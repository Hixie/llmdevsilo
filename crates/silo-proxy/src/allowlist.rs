//! Domain allowlist matching.
//!
//! Entries are either exact host names (`example.com`) or wildcard entries
//! (`*.example.com`), where a wildcard matches the base domain and every
//! subdomain. Matching is case-insensitive and tolerant of a single trailing
//! dot on either side.

/// Lower-cases a host name and strips a single trailing dot.
pub fn normalize_host(host: &str) -> String {
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    trimmed.to_ascii_lowercase()
}

#[derive(Clone, Debug)]
enum Entry {
    Exact(String),
    /// Wildcard `*.suffix`: matches `suffix` and any `*.suffix`.
    Wildcard(String),
}

/// Compiled set of allowlist entries.
#[derive(Clone, Debug, Default)]
pub struct DomainAllowlist {
    entries: Vec<Entry>,
}

impl DomainAllowlist {
    pub fn new(domains: &[String]) -> Self {
        let mut entries = Vec::new();
        for raw in domains {
            let normalized = normalize_host(raw);
            if let Some(suffix) = normalized.strip_prefix("*.") {
                entries.push(Entry::Wildcard(suffix.to_string()));
            } else {
                entries.push(Entry::Exact(normalized));
            }
        }
        DomainAllowlist { entries }
    }

    /// Whether `host` is allowed by any entry.
    pub fn allows(&self, host: &str) -> bool {
        let host = normalize_host(host);
        self.entries.iter().any(|entry| match entry {
            Entry::Exact(name) => name == &host,
            Entry::Wildcard(suffix) => {
                host == *suffix
                    || host
                        .strip_suffix(suffix)
                        .map(|prefix| prefix.ends_with('.'))
                        .unwrap_or(false)
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> DomainAllowlist {
        DomainAllowlist::new(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn exact_match_only() {
        let allow = list(&["example.com"]);
        assert!(allow.allows("example.com"));
        assert!(!allow.allows("www.example.com"));
        assert!(!allow.allows("notexample.com"));
    }

    #[test]
    fn wildcard_matches_base_and_subdomains() {
        let allow = list(&["*.example.com"]);
        assert!(allow.allows("example.com"));
        assert!(allow.allows("www.example.com"));
        assert!(allow.allows("a.b.example.com"));
        assert!(!allow.allows("example.org"));
        assert!(!allow.allows("notexample.com"));
        assert!(!allow.allows("badexample.com"));
    }

    #[test]
    fn case_insensitive_and_trailing_dot_tolerant() {
        let allow = list(&["Example.COM"]);
        assert!(allow.allows("example.com"));
        assert!(allow.allows("EXAMPLE.com"));
        assert!(allow.allows("example.com."));

        let wild = list(&["*.Example.com."]);
        assert!(wild.allows("WWW.example.com"));
        assert!(wild.allows("www.example.com."));
    }

    #[test]
    fn empty_list_allows_nothing() {
        let allow = DomainAllowlist::default();
        assert!(!allow.allows("example.com"));
        assert!(allow.is_empty());
    }

    #[test]
    fn wildcard_suffix_boundary() {
        let allow = list(&["*.foo.com"]);
        // "xfoo.com" must not match: the character before the suffix must be
        // a dot.
        assert!(!allow.allows("xfoo.com"));
        assert!(allow.allows("x.foo.com"));
    }
}
