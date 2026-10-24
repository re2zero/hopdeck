//! Exit-side allow rules: which targets a tunnel peer may open.
//!
//! Rule syntax (one string per entry):
//!   - `127.0.0.1`          exact IP, any port
//!   - `10.0.0.0/8:22`      CIDR + single port
//!   - `nas.local:5000`     exact domain + port
//!   - `*.corp.local:443`   wildcard subdomain + port
//!   - `::1`                bare IPv6, any port; `[::1]:22` for a port
//!
//! Matching is done on the name the peer *presented* (v1, approved design):
//! domains are matched before resolution and the exit resolves them with
//! its own DNS — that split-horizon behavior is the point of domain rules.
//! The resolved address is not re-checked in v1 (the exit's DNS is trusted;
//! a post-resolution CIDR re-check is the documented v2 upgrade). Default
//! policy is deny: an empty/absent allow list rejects every OPEN.

use std::net::IpAddr;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRule {
    Ip(IpAddr),
    Cidr(IpNet),
    /// Exact hostname, stored lowercased (IDN punycode input is the
    /// user's responsibility).
    Domain(String),
    /// `*.example.com` — matches subdomains only, not the apex.
    WildcardDomain(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowRule {
    host: HostRule,
    /// `None` = any port.
    port: Option<u16>,
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("empty rule")]
    Empty,
    #[error("invalid host pattern `{0}`")]
    InvalidHost(String),
    #[error("invalid CIDR `{0}`")]
    InvalidCidr(String),
    #[error("invalid port `{0}`")]
    InvalidPort(String),
}

impl AllowRule {
    /// Match the presented host (IP literal or hostname) and port.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        if let Some(p) = self.port {
            if p != port {
                return false;
            }
        }
        match &self.host {
            HostRule::Ip(ip) => host.parse::<IpAddr>().is_ok_and(|h| h == *ip),
            HostRule::Cidr(net) => host.parse::<IpAddr>().is_ok_and(|h| net.contains(h)),
            HostRule::Domain(domain) => host.eq_ignore_ascii_case(domain),
            HostRule::WildcardDomain(suffix) => {
                let host = host.to_ascii_lowercase();
                host.len() > suffix.len() + 1
                    && host.ends_with(suffix)
                    && host[..host.len() - suffix.len()].ends_with('.')
            }
        }
    }
}

impl FromStr for AllowRule {
    type Err = RuleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(RuleError::Empty);
        }
        // Bare IP literal (v4 or v6): any port. IPv6 carries colons, so it
        // must be recognized before the host:port split; a v6 rule with a
        // port restriction uses the bracketed form.
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(AllowRule {
                host: HostRule::Ip(ip),
                port: None,
            });
        }
        if let Some(rest) = s.strip_prefix('[') {
            let (host, port) = rest
                .split_once("]:")
                .ok_or_else(|| RuleError::InvalidHost(s.into()))?;
            let ip: IpAddr = host.parse().map_err(|_| RuleError::InvalidHost(s.into()))?;
            let port: u16 = port.parse().map_err(|_| RuleError::InvalidPort(s.into()))?;
            return Ok(AllowRule {
                host: HostRule::Ip(ip),
                port: Some(port),
            });
        }
        let (host_part, port) = match s.rsplit_once(':') {
            Some((host, port_str))
                if !port_str.is_empty() && port_str.chars().all(|c| c.is_ascii_digit()) =>
            {
                let port: u16 = port_str
                    .parse()
                    .map_err(|_| RuleError::InvalidPort(port_str.into()))?;
                (host, Some(port))
            }
            _ => (s, None),
        };
        let host = if let Some(rest) = host_part.strip_prefix("*.") {
            if rest.is_empty() || rest.contains('*') {
                return Err(RuleError::InvalidHost(host_part.into()));
            }
            HostRule::WildcardDomain(rest.to_ascii_lowercase())
        } else if host_part.contains('/') {
            HostRule::Cidr(IpNet::from_str(host_part)?)
        } else if host_part.contains('*') || host_part.contains(':') {
            return Err(RuleError::InvalidHost(host_part.into()));
        } else if let Ok(ip) = host_part.parse::<IpAddr>() {
            HostRule::Ip(ip)
        } else {
            validate_domain(host_part)?;
            HostRule::Domain(host_part.to_ascii_lowercase())
        };
        Ok(AllowRule { host, port })
    }
}

fn validate_domain(host: &str) -> Result<(), RuleError> {
    let valid = !host.is_empty()
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !host.contains("..");
    if valid {
        Ok(())
    } else {
        Err(RuleError::InvalidHost(host.into()))
    }
}

impl IpNet {
    fn from_str(s: &str) -> Result<Self, RuleError> {
        let (addr_part, prefix_part) = s
            .split_once('/')
            .ok_or_else(|| RuleError::InvalidCidr(s.into()))?;
        let addr: IpAddr = addr_part
            .parse()
            .map_err(|_| RuleError::InvalidCidr(s.into()))?;
        let prefix: u8 = prefix_part
            .parse()
            .map_err(|_| RuleError::InvalidCidr(s.into()))?;
        let max = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix > max {
            return Err(RuleError::InvalidCidr(s.into()));
        }
        Ok(IpNet { addr, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let prefix = self.prefix as u32;
                let mask = if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                };
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let prefix = self.prefix as u32;
                let mask = if prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - prefix)
                };
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// First-match-wins is not needed for correctness (matching is
/// independent per rule), so the exit simply checks the list in order.
#[derive(Debug, Clone, Default)]
pub struct AllowList {
    rules: Vec<AllowRule>,
}

impl AllowList {
    pub fn from_strings(rules: &[String]) -> Result<Self, RuleError> {
        Ok(AllowList {
            rules: rules
                .iter()
                .map(|r| AllowRule::from_str(r))
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Exit-side verdict for an OPEN. `ok: false` comes with a coarse,
    /// non-leaking reason.
    pub fn check(&self, host: &str, port: u16) -> Result<(), &'static str> {
        if host.is_empty() {
            return Err("empty target host");
        }
        if self.rules.iter().any(|r| r.matches(host, port)) {
            Ok(())
        } else {
            Err("target denied by exit allow rules")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(rules: &[&str]) -> AllowList {
        AllowList::from_strings(&rules.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .expect("valid rules")
    }

    #[test]
    fn exact_ip_any_port() {
        let l = list(&["127.0.0.1"]);
        assert!(l.check("127.0.0.1", 22).is_ok());
        assert!(l.check("127.0.0.1", 8080).is_ok());
        assert!(l.check("127.0.0.2", 22).is_err());
        assert!(l.check("other.host", 22).is_err());
    }

    #[test]
    fn cidr_with_port() {
        let l = list(&["10.0.0.0/8:22"]);
        assert!(l.check("10.1.2.3", 22).is_ok());
        assert!(l.check("10.255.0.1", 22).is_ok());
        assert!(l.check("10.1.2.3", 23).is_err(), "port restricted");
        assert!(l.check("192.168.0.1", 22).is_err());
        assert!(l.check("nas.local", 22).is_err());
    }

    #[test]
    fn ipv4_prefix_boundaries() {
        let l = list(&["192.168.1.0/24"]);
        assert!(l.check("192.168.1.255", 1).is_ok());
        assert!(l.check("192.168.2.0", 1).is_err());
    }

    #[test]
    fn domain_exact_case_insensitive() {
        let l = list(&["nas.local:5000"]);
        assert!(l.check("nas.local", 5000).is_ok());
        assert!(l.check("NAS.LOCAL", 5000).is_ok());
        assert!(l.check("sub.nas.local", 5000).is_err());
        assert!(l.check("nas.local", 5001).is_err());
    }

    #[test]
    fn wildcard_matches_subdomains_only() {
        let l = list(&["*.corp.local:443"]);
        assert!(l.check("a.corp.local", 443).is_ok());
        assert!(l.check("deep.b.corp.local", 443).is_ok());
        assert!(l.check("corp.local", 443).is_err(), "apex excluded");
        assert!(l.check("evilcorp.local", 443).is_err(), "suffix spoof");
        assert!(l.check("a.corp.local", 8443).is_err());
    }

    #[test]
    fn default_deny_when_empty() {
        let l = AllowList::default();
        assert!(l.is_empty());
        assert!(l.check("anything", 1).is_err());
    }

    #[test]
    fn invalid_rules_are_rejected() {
        for bad in [
            "",
            "*.",
            "10.0.0.0/33",
            "10.0.0.0/8:notaport",
            "ba*d.host",
            ".leading.dot",
            "double..dot",
            "10.0.0.0/8:",
        ] {
            assert!(
                AllowList::from_strings(&[bad.to_string()]).is_err(),
                "`{bad}` must not parse"
            );
        }
    }

    #[test]
    fn bare_ipv6_and_v4_rules() {
        let l = list(&["::1", "10.0.0.1"]);
        assert!(l.check("::1", 22).is_ok());
        assert!(l.check("10.0.0.1", 443).is_ok());
        assert!(l.check("::2", 22).is_err());
    }

    #[test]
    fn ipv6_cidr_works() {
        let l = list(&["fd00::/8:22"]);
        assert!(l.check("fd00::1", 22).is_ok());
        assert!(l.check("fd00::1", 23).is_err());
        assert!(l.check("fe80::1", 22).is_err());
    }

    #[test]
    fn bracketed_ipv6_with_port() {
        let l = list(&["[::1]:8080"]);
        assert!(l.check("::1", 8080).is_ok());
        assert!(l.check("::1", 22).is_err());
    }

    #[test]
    fn v4_against_v6_rule_never_matches() {
        let l = list(&["fd00::/8"]);
        assert!(l.check("192.168.1.1", 22).is_err());
    }

    #[test]
    fn loopback_rule_set_from_design_example() {
        let l = list(&["127.0.0.1", "10.0.0.0/8:22", "*.corp.local:443"]);
        assert!(l.check("127.0.0.1", 18099).is_ok());
        assert!(l.check("10.8.15.62", 22).is_ok());
        assert!(l.check("10.8.15.62", 445).is_err());
        assert!(l.check("git.corp.local", 443).is_ok());
        assert!(l.check("git.corp.local", 80).is_err());
    }
}
