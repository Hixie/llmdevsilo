//! DNS filter.
//!
//! A small UDP DNS server used by the Linux sandbox backends. It answers only
//! A and AAAA queries for allowlisted domains; it resolves them through the
//! host resolver and returns only addresses the IP guard permits. Every other
//! query — a name that is not allowlisted, or a record type other than A or
//! AAAA — gets `NXDOMAIN`, except that an allowlisted name queried for the
//! wrong record type gets an empty `NOERROR` answer.
//!
//! The packet-level handler [`DnsFilter::handle_query`] is independent of any
//! socket, so it can be unit-tested with hand-built query packets. Binding a
//! real UDP socket is done by [`DnsFilter::serve`], started from the proxy.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{DNSClass, RData, Record, RecordType};
use silo_core::error::ProxyError;
use silo_core::journal::{JournalEntry, JournalHandle, NetworkRecord};
use tokio::net::UdpSocket;

use crate::allowlist::DomainAllowlist;
use crate::ipguard::IpGuard;

const ANSWER_TTL_SECS: u32 = 30;

/// Resolves names to addresses. The default uses the host resolver; tests can
/// supply a fixed mapping.
#[async_trait::async_trait]
pub trait DnsResolver: Send + Sync {
    async fn resolve(&self, host: &str) -> Vec<IpAddr>;
}

/// Resolver backed by `tokio::net::lookup_host`.
pub struct SystemResolver;

#[async_trait::async_trait]
impl DnsResolver for SystemResolver {
    async fn resolve(&self, host: &str) -> Vec<IpAddr> {
        match tokio::net::lookup_host(format!("{host}:0")).await {
            Ok(addrs) => addrs.map(|a| a.ip()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Filters DNS queries against a domain allowlist and the IP guard.
pub struct DnsFilter {
    allowlist: DomainAllowlist,
    guard: IpGuard,
    resolver: Arc<dyn DnsResolver>,
    journal: JournalHandle,
}

impl DnsFilter {
    pub fn new(
        allowlist: DomainAllowlist,
        guard: IpGuard,
        resolver: Arc<dyn DnsResolver>,
        journal: JournalHandle,
    ) -> Self {
        DnsFilter {
            allowlist,
            guard,
            resolver,
            journal,
        }
    }

    /// Parses a raw query packet, applies the policy, and returns a raw
    /// response packet.
    pub async fn handle_query(&self, packet: &[u8]) -> Result<Vec<u8>, ProxyError> {
        let request =
            Message::from_vec(packet).map_err(|e| ProxyError::Setup(format!("dns parse: {e}")))?;
        let mut response = Message::new();
        response
            .set_id(request.id())
            .set_message_type(MessageType::Response)
            .set_op_code(OpCode::Query)
            .set_recursion_desired(request.recursion_desired())
            .set_recursion_available(true)
            .set_authoritative(true);
        for query in request.queries() {
            response.add_query(query.clone());
        }

        let Some(query) = request.queries().first().cloned() else {
            response.set_response_code(ResponseCode::FormErr);
            return response
                .to_vec()
                .map_err(|e| ProxyError::Setup(format!("dns encode: {e}")));
        };

        let name = query.name().to_ascii();
        let host = name.trim_end_matches('.').to_ascii_lowercase();
        let qtype = query.query_type();

        if !self.allowlist.allows(&host) {
            self.journal_block(&host, "domain not allowlisted");
            response.set_response_code(ResponseCode::NXDomain);
            return response
                .to_vec()
                .map_err(|e| ProxyError::Setup(format!("dns encode: {e}")));
        }

        if qtype != RecordType::A && qtype != RecordType::AAAA {
            // Allowlisted name, unsupported record type: empty NOERROR.
            response.set_response_code(ResponseCode::NoError);
            return response
                .to_vec()
                .map_err(|e| ProxyError::Setup(format!("dns encode: {e}")));
        }

        let want_v4 = qtype == RecordType::A;
        let addrs = self.resolver.resolve(&host).await;
        let mut answered = 0u64;
        for addr in addrs {
            if self.guard.is_blocked(addr) {
                continue;
            }
            match (want_v4, addr) {
                (true, IpAddr::V4(v4)) => {
                    let mut record = Record::from_rdata(
                        query.name().clone(),
                        ANSWER_TTL_SECS,
                        RData::A(A::from(v4)),
                    );
                    record.set_dns_class(DNSClass::IN);
                    response.add_answer(record);
                    answered += 1;
                }
                (false, IpAddr::V6(v6)) => {
                    let mut record = Record::from_rdata(
                        query.name().clone(),
                        ANSWER_TTL_SECS,
                        RData::AAAA(AAAA::from(v6)),
                    );
                    record.set_dns_class(DNSClass::IN);
                    response.add_answer(record);
                    answered += 1;
                }
                _ => {}
            }
        }
        response.set_response_code(ResponseCode::NoError);
        self.journal.append(JournalEntry::Network {
            record: NetworkRecord {
                host: host.clone(),
                port: 53,
                method: Some("DNS".into()),
                allowed: true,
                bytes_received: answered,
                note: Some(format!("{} answers", answered)),
                ..NetworkRecord::default()
            },
        });
        response
            .to_vec()
            .map_err(|e| ProxyError::Setup(format!("dns encode: {e}")))
    }

    fn journal_block(&self, host: &str, note: &str) {
        self.journal.append(JournalEntry::Network {
            record: NetworkRecord {
                host: host.to_string(),
                port: 53,
                method: Some("DNS".into()),
                allowed: false,
                note: Some(note.to_string()),
                ..NetworkRecord::default()
            },
        });
    }

    /// Binds a UDP socket and serves until `shutdown` resolves. Returns the
    /// bound address through `bound`.
    pub async fn serve(
        self: Arc<Self>,
        listen: SocketAddr,
    ) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), ProxyError> {
        let socket = UdpSocket::bind(listen).await?;
        let addr = socket.local_addr()?;
        let socket = Arc::new(socket);
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            loop {
                let (len, peer) = match socket.recv_from(&mut buf).await {
                    Ok(pair) => pair,
                    Err(_) => continue,
                };
                let request = buf[..len].to_vec();
                let filter = self.clone();
                let socket = socket.clone();
                tokio::spawn(async move {
                    if let Ok(reply) = filter.handle_query(&request).await {
                        let _ = socket.send_to(&reply, peer).await;
                    }
                });
            }
        });
        Ok((addr, handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Query;
    use hickory_proto::rr::Name;
    use silo_core::clock::{FakeClock, SharedClock};
    use silo_core::journal::JournalWriter;
    use std::str::FromStr;
    use std::sync::Arc as StdArc;

    struct FixedResolver(Vec<IpAddr>);

    #[async_trait::async_trait]
    impl DnsResolver for FixedResolver {
        async fn resolve(&self, _host: &str) -> Vec<IpAddr> {
            self.0.clone()
        }
    }

    fn journal() -> (JournalHandle, StdArc<std::sync::Mutex<Vec<u8>>>) {
        let clock: SharedClock = StdArc::new(FakeClock::default());
        let (writer, buf) = JournalWriter::in_memory(clock);
        (JournalHandle::new(writer), buf)
    }

    fn query_packet(host: &str, rtype: RecordType) -> Vec<u8> {
        let mut message = Message::new();
        message.set_id(0x1234).set_recursion_desired(true);
        let mut query = Query::new();
        query
            .set_name(Name::from_ascii(host).unwrap())
            .set_query_type(rtype)
            .set_query_class(DNSClass::IN);
        message.add_query(query);
        message.to_vec().unwrap()
    }

    fn allowlist(items: &[&str]) -> DomainAllowlist {
        DomainAllowlist::new(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[tokio::test]
    async fn allowlisted_a_query_returns_permitted_addresses() {
        let (journal, _buf) = journal();
        let resolver = StdArc::new(FixedResolver(vec![
            IpAddr::from_str("203.0.113.10").unwrap(),
            IpAddr::from_str("10.0.0.5").unwrap(),
        ]));
        let filter = DnsFilter::new(
            allowlist(&["example.com"]),
            IpGuard::new(),
            resolver,
            journal,
        );
        let packet = query_packet("example.com.", RecordType::A);
        let reply = filter.handle_query(&packet).await.unwrap();
        let message = Message::from_vec(&reply).unwrap();
        assert_eq!(message.response_code(), ResponseCode::NoError);
        // Only the public address is returned; 10.0.0.5 is filtered out.
        let answers: Vec<_> = message
            .answers()
            .iter()
            .filter_map(|r| match r.data() {
                Some(RData::A(a)) => Some(a.0),
                _ => None,
            })
            .collect();
        assert_eq!(answers, vec![std::net::Ipv4Addr::new(203, 0, 113, 10)]);
    }

    #[tokio::test]
    async fn non_allowlisted_name_is_nxdomain() {
        let (journal, _buf) = journal();
        let resolver = StdArc::new(FixedResolver(vec![]));
        let filter = DnsFilter::new(
            allowlist(&["example.com"]),
            IpGuard::new(),
            resolver,
            journal,
        );
        let packet = query_packet("evil.test.", RecordType::A);
        let reply = filter.handle_query(&packet).await.unwrap();
        let message = Message::from_vec(&reply).unwrap();
        assert_eq!(message.response_code(), ResponseCode::NXDomain);
        assert!(message.answers().is_empty());
    }

    #[tokio::test]
    async fn wrong_record_type_for_allowed_name_is_empty_noerror() {
        let (journal, _buf) = journal();
        let resolver = StdArc::new(FixedResolver(vec![]));
        let filter = DnsFilter::new(
            allowlist(&["example.com"]),
            IpGuard::new(),
            resolver,
            journal,
        );
        let packet = query_packet("example.com.", RecordType::MX);
        let reply = filter.handle_query(&packet).await.unwrap();
        let message = Message::from_vec(&reply).unwrap();
        assert_eq!(message.response_code(), ResponseCode::NoError);
        assert!(message.answers().is_empty());
    }

    #[tokio::test]
    async fn aaaa_query_returns_only_v6() {
        let (journal, _buf) = journal();
        let resolver = StdArc::new(FixedResolver(vec![
            IpAddr::from_str("203.0.113.10").unwrap(),
            IpAddr::from_str("2606:4700::1").unwrap(),
        ]));
        let filter = DnsFilter::new(
            allowlist(&["*.example.com"]),
            IpGuard::new(),
            resolver,
            journal,
        );
        let packet = query_packet("www.example.com.", RecordType::AAAA);
        let reply = filter.handle_query(&packet).await.unwrap();
        let message = Message::from_vec(&reply).unwrap();
        let answers: Vec<_> = message
            .answers()
            .iter()
            .filter_map(|r| match r.data() {
                Some(RData::AAAA(a)) => Some(a.0),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            vec![std::net::Ipv6Addr::from_str("2606:4700::1").unwrap()]
        );
    }

    #[tokio::test]
    async fn served_socket_answers_real_queries() {
        let (journal, _buf) = journal();
        let resolver = StdArc::new(FixedResolver(
            vec![IpAddr::from_str("203.0.113.1").unwrap()],
        ));
        let filter = StdArc::new(DnsFilter::new(
            allowlist(&["example.com"]),
            IpGuard::new(),
            resolver,
            journal,
        ));
        let (addr, _task) = filter.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let packet = query_packet("example.com.", RecordType::A);
        client.send_to(&packet, addr).await.unwrap();
        let mut buf = vec![0u8; 1500];
        let (len, _) = client.recv_from(&mut buf).await.unwrap();
        let message = Message::from_vec(&buf[..len]).unwrap();
        assert_eq!(message.id(), 0x1234);
        assert_eq!(message.answers().len(), 1);
    }
}
