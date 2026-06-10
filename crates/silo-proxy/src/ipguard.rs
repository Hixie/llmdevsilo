//! Address-level blocking.
//!
//! Every destination address is checked here before the proxy opens an
//! upstream connection: host names that are IP literals, and every address a
//! host name resolves to. The blocked ranges cover loopback, the private
//! RFC 1918 ranges, link-local, carrier-grade NAT, multicast, broadcast, and
//! the unspecified address for IPv4; loopback, unspecified, link-local,
//! unique-local, and multicast for IPv6. IPv4-mapped and IPv4-compatible
//! IPv6 addresses, and the NAT64 well-known prefix, are unwrapped to their
//! embedded IPv4 address and checked against the IPv4 rules.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why an address is blocked, for journaling and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockReason {
    Loopback,
    Private,
    LinkLocal,
    SharedCgnat,
    Multicast,
    Broadcast,
    Unspecified,
    UniqueLocal,
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockReason::Loopback => "loopback address",
            BlockReason::Private => "private address",
            BlockReason::LinkLocal => "link-local address",
            BlockReason::SharedCgnat => "carrier-grade NAT address",
            BlockReason::Multicast => "multicast address",
            BlockReason::Broadcast => "broadcast address",
            BlockReason::Unspecified => "unspecified address",
            BlockReason::UniqueLocal => "unique-local address",
        }
    }
}

/// Decides which addresses the proxy may connect to. Loopback is permitted
/// only when `allow_loopback` is set, which is a test-only affordance so
/// integration tests can reach local origin servers.
#[derive(Clone, Copy, Debug, Default)]
pub struct IpGuard {
    allow_loopback: bool,
}

impl IpGuard {
    pub fn new() -> Self {
        IpGuard {
            allow_loopback: false,
        }
    }

    pub fn with_loopback_allowed(allow: bool) -> Self {
        IpGuard {
            allow_loopback: allow,
        }
    }

    /// Returns the reason an address is blocked, or `None` if it is
    /// permitted.
    pub fn check(&self, addr: IpAddr) -> Option<BlockReason> {
        match addr {
            IpAddr::V4(v4) => self.check_v4(v4),
            IpAddr::V6(v6) => self.check_v6(v6),
        }
    }

    pub fn is_blocked(&self, addr: IpAddr) -> bool {
        self.check(addr).is_some()
    }

    fn check_v4(&self, addr: Ipv4Addr) -> Option<BlockReason> {
        let octets = addr.octets();
        if addr.is_loopback() {
            return if self.allow_loopback {
                None
            } else {
                Some(BlockReason::Loopback)
            };
        }
        if addr.is_unspecified() {
            return Some(BlockReason::Unspecified);
        }
        if addr == Ipv4Addr::BROADCAST {
            return Some(BlockReason::Broadcast);
        }
        // RFC 1918 private ranges.
        if octets[0] == 10
            || (octets[0] == 172 && (16..=31).contains(&octets[1]))
            || (octets[0] == 192 && octets[1] == 168)
        {
            return Some(BlockReason::Private);
        }
        // Link-local 169.254.0.0/16.
        if octets[0] == 169 && octets[1] == 254 {
            return Some(BlockReason::LinkLocal);
        }
        // Carrier-grade NAT 100.64.0.0/10.
        if octets[0] == 100 && (64..=127).contains(&octets[1]) {
            return Some(BlockReason::SharedCgnat);
        }
        // Multicast 224.0.0.0/4.
        if (224..=239).contains(&octets[0]) {
            return Some(BlockReason::Multicast);
        }
        None
    }

    fn check_v6(&self, addr: Ipv6Addr) -> Option<BlockReason> {
        if let Some(v4) = embedded_ipv4(addr) {
            return self.check_v4(v4);
        }
        if addr.is_loopback() {
            return if self.allow_loopback {
                None
            } else {
                Some(BlockReason::Loopback)
            };
        }
        if addr.is_unspecified() {
            return Some(BlockReason::Unspecified);
        }
        let segments = addr.segments();
        // Link-local fe80::/10.
        if (segments[0] & 0xffc0) == 0xfe80 {
            return Some(BlockReason::LinkLocal);
        }
        // Unique-local fc00::/7.
        if (segments[0] & 0xfe00) == 0xfc00 {
            return Some(BlockReason::UniqueLocal);
        }
        // Multicast ff00::/8.
        if (segments[0] & 0xff00) == 0xff00 {
            return Some(BlockReason::Multicast);
        }
        None
    }
}

/// Extracts an embedded IPv4 address from an IPv6 address when one is
/// present: IPv4-mapped (`::ffff:a.b.c.d`), IPv4-compatible
/// (`::a.b.c.d`, excluding `::` and `::1`), and the NAT64 well-known prefix
/// `64:ff9b::/96`.
pub fn embedded_ipv4(addr: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = addr.segments();
    // IPv4-mapped ::ffff:a.b.c.d.
    if segments[0..5] == [0, 0, 0, 0, 0] && segments[5] == 0xffff {
        return Some(last_v4(segments));
    }
    // NAT64 well-known prefix 64:ff9b::/96.
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        return Some(last_v4(segments));
    }
    // IPv4-compatible ::a.b.c.d, excluding the unspecified and loopback
    // addresses which carry their own meaning.
    if segments[0..6] == [0, 0, 0, 0, 0, 0] && !(segments[6] == 0 && segments[7] <= 1) {
        return Some(last_v4(segments));
    }
    None
}

fn last_v4(segments: [u16; 8]) -> Ipv4Addr {
    let high = segments[6];
    let low = segments[7];
    Ipv4Addr::new(
        (high >> 8) as u8,
        (high & 0xff) as u8,
        (low >> 8) as u8,
        (low & 0xff) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[test]
    fn blocks_ipv4_ranges() {
        let guard = IpGuard::new();
        assert_eq!(guard.check(ip("127.0.0.1")), Some(BlockReason::Loopback));
        assert_eq!(
            guard.check(ip("127.255.255.254")),
            Some(BlockReason::Loopback)
        );
        assert_eq!(guard.check(ip("10.0.0.1")), Some(BlockReason::Private));
        assert_eq!(
            guard.check(ip("10.255.255.255")),
            Some(BlockReason::Private)
        );
        assert_eq!(guard.check(ip("172.16.0.1")), Some(BlockReason::Private));
        assert_eq!(
            guard.check(ip("172.31.255.255")),
            Some(BlockReason::Private)
        );
        assert_eq!(guard.check(ip("192.168.1.1")), Some(BlockReason::Private));
        assert_eq!(guard.check(ip("169.254.1.1")), Some(BlockReason::LinkLocal));
        assert_eq!(
            guard.check(ip("100.64.0.1")),
            Some(BlockReason::SharedCgnat)
        );
        assert_eq!(
            guard.check(ip("100.127.255.255")),
            Some(BlockReason::SharedCgnat)
        );
        assert_eq!(guard.check(ip("224.0.0.1")), Some(BlockReason::Multicast));
        assert_eq!(
            guard.check(ip("239.255.255.255")),
            Some(BlockReason::Multicast)
        );
        assert_eq!(
            guard.check(ip("255.255.255.255")),
            Some(BlockReason::Broadcast)
        );
        assert_eq!(guard.check(ip("0.0.0.0")), Some(BlockReason::Unspecified));
    }

    #[test]
    fn permits_public_ipv4() {
        let guard = IpGuard::new();
        assert_eq!(guard.check(ip("8.8.8.8")), None);
        assert_eq!(guard.check(ip("1.1.1.1")), None);
        assert_eq!(guard.check(ip("172.15.0.1")), None);
        assert_eq!(guard.check(ip("172.32.0.1")), None);
        assert_eq!(guard.check(ip("192.169.0.1")), None);
        assert_eq!(guard.check(ip("100.63.255.255")), None);
        assert_eq!(guard.check(ip("100.128.0.0")), None);
        assert_eq!(guard.check(ip("223.255.255.255")), None);
    }

    #[test]
    fn blocks_ipv6_ranges() {
        let guard = IpGuard::new();
        assert_eq!(guard.check(ip("::1")), Some(BlockReason::Loopback));
        assert_eq!(guard.check(ip("::")), Some(BlockReason::Unspecified));
        assert_eq!(guard.check(ip("fe80::1")), Some(BlockReason::LinkLocal));
        assert_eq!(guard.check(ip("febf::1")), Some(BlockReason::LinkLocal));
        assert_eq!(guard.check(ip("fc00::1")), Some(BlockReason::UniqueLocal));
        assert_eq!(
            guard.check(ip("fd12:3456::1")),
            Some(BlockReason::UniqueLocal)
        );
        assert_eq!(guard.check(ip("ff00::1")), Some(BlockReason::Multicast));
        assert_eq!(guard.check(ip("ff02::1")), Some(BlockReason::Multicast));
    }

    #[test]
    fn permits_public_ipv6() {
        let guard = IpGuard::new();
        assert_eq!(guard.check(ip("2606:4700:4700::1111")), None);
        assert_eq!(guard.check(ip("2001:4860:4860::8888")), None);
    }

    #[test]
    fn unwraps_ipv4_mapped_and_compatible() {
        let guard = IpGuard::new();
        // ::ffff:127.0.0.1 maps to loopback.
        assert_eq!(
            guard.check(ip("::ffff:127.0.0.1")),
            Some(BlockReason::Loopback)
        );
        assert_eq!(
            guard.check(ip("::ffff:10.0.0.1")),
            Some(BlockReason::Private)
        );
        // ::ffff:8.8.8.8 maps to a public address.
        assert_eq!(guard.check(ip("::ffff:8.8.8.8")), None);
        // NAT64 64:ff9b::a.b.c.d.
        assert_eq!(
            guard.check(ip("64:ff9b::10.0.0.1")),
            Some(BlockReason::Private)
        );
        assert_eq!(guard.check(ip("64:ff9b::8.8.8.8")), None);
        // IPv4-compatible ::a.b.c.d (not ::, not ::1).
        assert_eq!(guard.check(ip("::192.168.0.1")), Some(BlockReason::Private));
    }

    #[test]
    fn embedded_ipv4_excludes_unspecified_and_loopback() {
        assert_eq!(embedded_ipv4(Ipv6Addr::from_str("::").unwrap()), None);
        assert_eq!(embedded_ipv4(Ipv6Addr::from_str("::1").unwrap()), None);
        assert_eq!(
            embedded_ipv4(Ipv6Addr::from_str("::ffff:1.2.3.4").unwrap()),
            Some(Ipv4Addr::new(1, 2, 3, 4))
        );
    }

    #[test]
    fn loopback_allowed_when_enabled() {
        let guard = IpGuard::with_loopback_allowed(true);
        assert_eq!(guard.check(ip("127.0.0.1")), None);
        assert_eq!(guard.check(ip("::1")), None);
        assert_eq!(guard.check(ip("::ffff:127.0.0.1")), None);
        // Other private ranges stay blocked even with loopback allowed.
        assert_eq!(guard.check(ip("10.0.0.1")), Some(BlockReason::Private));
    }
}
