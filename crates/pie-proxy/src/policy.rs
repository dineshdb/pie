//! Egress policy: which hosts a sandboxed process is allowed to reach.
//!
//! Filtering happens at the *hostname* level, which needs no TLS interception:
//! a proxy sees the target in plaintext (the `CONNECT` authority, an absolute
//! request URI, or a SOCKS5 domain request) and can then relay the encrypted
//! bytes untouched.
//!
//! The engine is deliberately free of any I/O or server framework so the rules
//! can be tested exhaustively on their own.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// Ports reachable unless the configuration says otherwise.
///
/// A hostname allowlist is worthless if a client can `CONNECT` to any port on
/// an allowed host, so the port set is closed by default.
pub const DEFAULT_PORTS: [u16; 2] = [80, 443];

/// A host pattern from configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    /// Matches every host. Written `*`.
    Any,
    /// Matches one host exactly. Written `github.com` or `140.82.121.4`.
    ///
    /// Holds a parsed [`Host`] so IP literals compare numerically: two
    /// spellings of one address must not slip past each other.
    Exact(Host),
    /// Matches proper subdomains only. Written `*.github.com`, which matches
    /// `api.github.com` and `a.b.github.com` but *not* `github.com` itself.
    /// Stores the parent without its leading dot.
    Subdomains(String),
}

impl HostPattern {
    fn matches(&self, host: &Host) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(want) => want == host,
            // An IP literal has no subdomains; `*.x` must never match one.
            Self::Subdomains(parent) => match host {
                Host::Ip(_) => false,
                Host::Domain(name) => name
                    .strip_suffix(parent.as_str())
                    // The remainder must be a complete, non-empty label
                    // sequence, so neither `evil-github.com` nor `github.com`
                    // itself can pass `*.github.com`.
                    .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1),
            },
        }
    }
}

impl FromStr for HostPattern {
    type Err = PolicyError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let pattern = raw.trim();
        if pattern.is_empty() {
            return Err(PolicyError::EmptyPattern);
        }
        if pattern == "*" {
            return Ok(Self::Any);
        }
        if let Some(parent) = pattern.strip_prefix("*.") {
            return match Host::parse(parent) {
                // `*.1.2.3.4` cannot mean anything: IPs have no subdomains.
                Ok(Host::Domain(parent)) => Ok(Self::Subdomains(parent)),
                _ => Err(PolicyError::BadPattern(raw.to_string())),
            };
        }
        if pattern.contains('*') {
            // `*github.com` would also match `evilgithub.com`.
            return Err(PolicyError::BadPattern(raw.to_string()));
        }
        Host::parse(pattern)
            .map(Self::Exact)
            .map_err(|_| PolicyError::BadPattern(raw.to_string()))
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => write!(f, "*"),
            Self::Exact(host) => write!(f, "{host}"),
            Self::Subdomains(parent) => write!(f, "*.{parent}"),
        }
    }
}

/// Lowercases, drops a single trailing root dot, and unwraps IPv6 brackets.
///
/// Only *one* trailing dot is removed: `evil.com..` is malformed, not a
/// synonym for `evil.com`, and label validation should be the thing that says
/// so.
fn normalize_host(host: &str) -> String {
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let host = host.strip_suffix('.').unwrap_or(host);
    host.to_ascii_lowercase()
}

/// A connection target, as seen by the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Domain(String),
    Ip(IpAddr),
}

/// Longest legal DNS name, and longest legal label within one.
const MAX_NAME_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

impl Host {
    /// Parses and normalizes a host, classifying IP literals.
    ///
    /// Deliberately an allowlist: anything that is not a plain IP literal or a
    /// letter-digit-hyphen domain is rejected. A denylist of "bad characters"
    /// is how filters get bypassed — `user@evil.com` and `%65vil.com` both look
    /// like harmless domains to a matcher but are something else entirely to
    /// whatever eventually opens the socket.
    pub fn parse(host: &str) -> Result<Self, PolicyError> {
        let normalized = normalize_host(host);
        if normalized.is_empty() {
            return Err(PolicyError::EmptyHost);
        }

        if let Ok(ip) = normalized.parse::<IpAddr>() {
            // `::ffff:127.0.0.1` and `127.0.0.1` are the same address and must
            // therefore be the same `Host`, or a rule naming one would not
            // cover the other.
            let ip = match ip {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
                v4 => v4,
            };
            return Ok(Self::Ip(ip));
        }

        let bad = || PolicyError::BadHost(host.to_string());
        if normalized.len() > MAX_NAME_LEN {
            return Err(bad());
        }
        // Non-ASCII is refused rather than guessed at: matching a Unicode rule
        // against a punycode target (or the reverse) silently never fires, so
        // rules must be written in their A-label form.
        if !normalized
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(bad());
        }
        for label in normalized.split('.') {
            if label.is_empty() || label.len() > MAX_LABEL_LEN {
                return Err(bad());
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(bad());
            }
        }
        // An IP address in a shape Rust's parser rejects but `getaddrinfo`
        // accepts (`127.1`, `2130706433`, `0x7f.0.0.1`) would sail through as a
        // "domain" that matches no rule, then resolve to the address anyway.
        let last = normalized.rsplit('.').next().unwrap_or_default();
        if last.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        if normalized
            .split('.')
            .any(|label| label.starts_with("0x") || label.starts_with("0X"))
        {
            return Err(bad());
        }

        Ok(Self::Domain(normalized))
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Domain(name) => write!(f, "{name}"),
            Self::Ip(ip) => write!(f, "{ip}"),
        }
    }
}

/// Where a client wants to connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: Host,
    pub port: u16,
}

impl Target {
    pub fn new(host: Host, port: u16) -> Self {
        Self { host, port }
    }

    /// Parses an `authority` such as `github.com:443`, `1.2.3.4:80` or
    /// `[::1]:443`.
    ///
    /// `default_port` applies when the authority carries no port; `None`
    /// rejects such an authority. Guessing a port is not harmless — whoever
    /// opens the socket may guess differently, and then the port that was
    /// checked is not the port that gets used.
    pub fn parse_authority(
        authority: &str,
        default_port: Option<u16>,
    ) -> Result<Self, PolicyError> {
        let authority = authority.trim();
        if authority.is_empty() {
            return Err(PolicyError::EmptyHost);
        }

        // IPv6 authorities are bracketed, so scan for the port after the
        // closing bracket only.
        let split_at = match authority.rfind(']') {
            Some(bracket) => authority[bracket..].find(':').map(|i| bracket + i),
            None => authority.rfind(':'),
        };

        let (host, port) = match split_at {
            Some(i) => {
                let port = &authority[i + 1..];
                let port = port
                    .parse::<u16>()
                    .map_err(|_| PolicyError::BadPort(port.to_string()))?;
                if port == 0 {
                    return Err(PolicyError::BadPort(port.to_string()));
                }
                (&authority[..i], port)
            }
            None => (
                authority,
                default_port.ok_or_else(|| PolicyError::MissingPort(authority.to_string()))?,
            ),
        };

        Ok(Self::new(Host::parse(host)?, port))
    }
}

/// Lets the proxy hand the *checked* destination straight to the connector.
///
/// Fallible on purpose: if a validated host cannot be expressed as a connector
/// target, the caller must refuse the request rather than let the connector
/// pick its own destination.
impl TryFrom<Target> for rama::net::address::HostWithPort {
    type Error = PolicyError;

    fn try_from(target: Target) -> Result<Self, Self::Error> {
        let host = match &target.host {
            Host::Ip(ip) => rama::net::address::Host::Address(*ip),
            Host::Domain(name) => rama::net::address::Domain::from_str(name)
                .map(rama::net::address::Host::Name)
                .map_err(|_| PolicyError::BadHost(name.clone()))?,
        };
        Ok(Self::new(host, target.port))
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Ip(ip) if ip.is_ipv6() => write!(f, "[{ip}]:{}", self.port),
            host => write!(f, "{host}:{}", self.port),
        }
    }
}

/// How the policy treats a target that matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No rules configured: everything is allowed. Standalone convenience.
    AllowAll,
    /// An allow list exists, so anything unlisted is denied.
    Allowlist,
    /// Only deny rules exist, so anything unlisted is allowed.
    Blocklist,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::AllowAll => "allow-all",
            Self::Allowlist => "allowlist",
            Self::Blocklist => "blocklist",
        };
        f.write_str(name)
    }
}

/// Why a target was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The port is outside the allowed set.
    PortNotAllowed,
    /// A deny rule matched.
    DeniedHost,
    /// Allowlist mode, and nothing matched.
    NotAllowed,
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::PortNotAllowed => "port not allowed",
            Self::DeniedHost => "host explicitly denied",
            Self::NotAllowed => "host not in allow list",
        };
        f.write_str(text)
    }
}

/// The verdict for one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(DenyReason),
}

impl Decision {
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    NoRules,
    MissingPort(String),
    EmptyPattern,
    BadPattern(String),
    EmptyHost,
    BadHost(String),
    BadPort(String),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRules => write!(
                f,
                "no rules given: pass at least one allow or deny rule, or `*` to allow everything"
            ),
            Self::MissingPort(a) => write!(f, "authority `{a}` has no port"),
            Self::EmptyPattern => write!(f, "host pattern is empty"),
            Self::BadPattern(p) => write!(
                f,
                "invalid host pattern `{p}`: use an exact host, `*.example.com`, or `*`"
            ),
            Self::EmptyHost => write!(f, "host is empty"),
            Self::BadHost(h) => write!(f, "invalid host `{h}`"),
            Self::BadPort(p) => write!(f, "invalid port `{p}`"),
        }
    }
}

impl std::error::Error for PolicyError {}

/// Parses a list of raw patterns.
fn parse_patterns<I>(patterns: I) -> Result<Vec<HostPattern>, PolicyError>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    patterns
        .into_iter()
        .map(|p| p.as_ref().parse::<HostPattern>())
        .collect()
}

/// An egress policy: deny rules, allow rules, and a port set.
///
/// Deny always wins over allow, so a broad allow rule cannot resurrect a host
/// that was explicitly denied.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    allow: Vec<HostPattern>,
    deny: Vec<HostPattern>,
    ports: Vec<u16>,
}

impl Policy {
    /// A policy that allows everything, for standalone/debugging use.
    pub fn allow_all() -> Self {
        Self {
            allow: Vec::new(),
            deny: Vec::new(),
            ports: Vec::new(),
        }
    }

    /// Builds a policy from raw pattern strings.
    ///
    /// Refuses an empty rule set: a proxy with no rules forwards everything,
    /// and that must be asked for on purpose ([`Policy::allow_all`] or an
    /// explicit `*` rule), never arrived at by forgetting a flag.
    pub fn new<A, D>(allow: A, deny: D) -> Result<Self, PolicyError>
    where
        A: IntoIterator,
        A::Item: AsRef<str>,
        D: IntoIterator,
        D::Item: AsRef<str>,
    {
        let allow = parse_patterns(allow)?;
        let deny = parse_patterns(deny)?;
        if allow.is_empty() && deny.is_empty() {
            return Err(PolicyError::NoRules);
        }
        Ok(Self {
            allow,
            deny,
            ports: Vec::new(),
        })
    }

    /// Restricts the reachable ports. Empty keeps [`DEFAULT_PORTS`].
    pub fn with_ports(mut self, ports: impl IntoIterator<Item = u16>) -> Self {
        self.ports = ports.into_iter().filter(|p| *p != 0).collect();
        self.ports.sort_unstable();
        self.ports.dedup();
        self
    }

    /// Ports this policy permits, or `None` when ports are unrestricted.
    ///
    /// An explicit port list is always enforced. Without one, host rules imply
    /// the default ports, while a rule-less (transparent) policy restricts
    /// nothing.
    pub fn ports(&self) -> Option<&[u16]> {
        if !self.ports.is_empty() {
            return Some(&self.ports);
        }
        match self.mode() {
            Mode::AllowAll => None,
            Mode::Allowlist | Mode::Blocklist => Some(&DEFAULT_PORTS),
        }
    }

    /// Whether unmatched targets are allowed or denied.
    pub fn mode(&self) -> Mode {
        if !self.allow.is_empty() {
            return Mode::Allowlist;
        }
        if self.deny.is_empty() {
            Mode::AllowAll
        } else {
            Mode::Blocklist
        }
    }

    pub fn allow_rules(&self) -> &[HostPattern] {
        &self.allow
    }

    pub fn deny_rules(&self) -> &[HostPattern] {
        &self.deny
    }

    /// Evaluates a target. Port first, then deny rules, then mode.
    pub fn evaluate(&self, target: &Target) -> Decision {
        let mode = self.mode();
        if let Some(ports) = self.ports()
            && !ports.contains(&target.port)
        {
            return Decision::Deny(DenyReason::PortNotAllowed);
        }
        if self.deny.iter().any(|p| p.matches(&target.host)) {
            return Decision::Deny(DenyReason::DeniedHost);
        }
        match mode {
            Mode::AllowAll | Mode::Blocklist => Decision::Allow,
            Mode::Allowlist if self.allow.iter().any(|p| p.matches(&target.host)) => {
                Decision::Allow
            }
            Mode::Allowlist => Decision::Deny(DenyReason::NotAllowed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(authority: &str) -> Target {
        Target::parse_authority(authority, Some(443)).expect(authority)
    }

    fn policy(allow: &[&str], deny: &[&str]) -> Policy {
        Policy::new(allow.iter().copied(), deny.iter().copied()).expect("valid policy")
    }

    #[test]
    fn exact_pattern_matches_only_that_host() {
        let p = policy(&["github.com"], &[]);
        assert!(p.evaluate(&target("github.com:443")).is_allowed());
        assert!(!p.evaluate(&target("api.github.com:443")).is_allowed());
    }

    #[test]
    fn subdomain_pattern_does_not_match_the_parent() {
        let p = policy(&["*.github.com"], &[]);
        assert!(p.evaluate(&target("api.github.com:443")).is_allowed());
        assert!(p.evaluate(&target("a.b.github.com:443")).is_allowed());
        assert!(!p.evaluate(&target("github.com:443")).is_allowed());
    }

    /// The classic wildcard bypass: a prefix that merely ends with the parent.
    #[test]
    fn subdomain_pattern_rejects_lookalike_hosts() {
        let p = policy(&["*.github.com", "github.com"], &[]);
        for host in [
            "evil-github.com",
            "evilgithub.com",
            "notgithub.com",
            "github.com.evil.com",
            "xgithub.com",
        ] {
            let t = target(&format!("{host}:443"));
            assert!(!p.evaluate(&t).is_allowed(), "{host} must not be allowed");
        }
    }

    #[test]
    fn matching_ignores_case_and_trailing_dot() {
        let p = policy(&["GitHub.com", "*.Example.COM"], &[]);
        assert!(p.evaluate(&target("github.com:443")).is_allowed());
        assert!(p.evaluate(&target("GITHUB.COM:443")).is_allowed());
        assert!(p.evaluate(&target("github.com.:443")).is_allowed());
        assert!(p.evaluate(&target("API.example.com:443")).is_allowed());
    }

    #[test]
    fn deny_beats_allow() {
        let p = policy(&["*.github.com"], &["evil.github.com"]);
        assert!(p.evaluate(&target("api.github.com:443")).is_allowed());
        assert_eq!(
            p.evaluate(&target("evil.github.com:443")),
            Decision::Deny(DenyReason::DeniedHost)
        );
    }

    #[test]
    fn deny_beats_allow_all_wildcard() {
        let p = policy(&["*"], &["*.evil.com"]);
        assert!(p.evaluate(&target("anything.org:443")).is_allowed());
        assert!(!p.evaluate(&target("c2.evil.com:443")).is_allowed());
    }

    #[test]
    fn no_rules_allows_everything_including_odd_ports() {
        let p = Policy::allow_all();
        assert_eq!(p.mode(), Mode::AllowAll);
        assert!(p.evaluate(&target("anything.internal:9999")).is_allowed());
    }

    #[test]
    fn only_deny_rules_means_blocklist() {
        let p = policy(&[], &["*.evil.com"]);
        assert_eq!(p.mode(), Mode::Blocklist);
        assert!(p.evaluate(&target("github.com:443")).is_allowed());
        assert!(!p.evaluate(&target("c2.evil.com:443")).is_allowed());
    }

    /// An IP literal has no subdomains, so `*.x` must not cover it, and in
    /// allowlist mode it needs to be named explicitly.
    #[test]
    fn ip_literals_need_an_explicit_rule() {
        let p = policy(&["*.github.com"], &[]);
        assert!(!p.evaluate(&target("140.82.121.4:443")).is_allowed());
        assert!(
            !p.evaluate(&target("[2606:50c0:8000::153]:443"))
                .is_allowed()
        );

        let p = policy(&["140.82.121.4"], &[]);
        assert!(p.evaluate(&target("140.82.121.4:443")).is_allowed());
    }

    #[test]
    fn wildcard_pattern_never_matches_an_ip() {
        let pattern: HostPattern = "*.example.com".parse().unwrap();
        assert!(!pattern.matches(&Host::parse("127.0.0.1").unwrap()));
        // An IP-shaped wildcard is meaningless and must not parse at all.
        assert!("*.127.0.0.1".parse::<HostPattern>().is_err());
        assert!("*.2606:50c0::153".parse::<HostPattern>().is_err());
    }

    #[test]
    fn ports_are_closed_by_default() {
        let p = policy(&["github.com"], &[]);
        assert_eq!(p.ports(), Some(&DEFAULT_PORTS[..]));
        assert!(p.evaluate(&target("github.com:443")).is_allowed());
        assert!(p.evaluate(&target("github.com:80")).is_allowed());
        assert_eq!(
            p.evaluate(&target("github.com:22")),
            Decision::Deny(DenyReason::PortNotAllowed)
        );
    }

    #[test]
    fn ports_can_be_widened() {
        let p = policy(&["github.com"], &[]).with_ports([443, 22, 22, 0]);
        assert_eq!(p.ports(), Some(&[22, 443][..]));
        assert!(p.evaluate(&target("github.com:22")).is_allowed());
        assert!(!p.evaluate(&target("github.com:80")).is_allowed());
    }

    #[test]
    fn port_check_precedes_deny_and_allow() {
        let p = policy(&["github.com"], &["github.com"]);
        assert_eq!(
            p.evaluate(&target("github.com:1234")),
            Decision::Deny(DenyReason::PortNotAllowed)
        );
    }

    #[test]
    fn authority_parsing_covers_ipv6_and_default_port() {
        assert_eq!(target("github.com:8080").port, 8080);
        assert_eq!(target("github.com").port, 443);
        let t = target("[2606:50c0:8000::153]:443");
        assert_eq!(t.host, Host::Ip("2606:50c0:8000::153".parse().unwrap()));
        assert_eq!(t.port, 443);
        // Unbracketed IPv6 must be rejected, not silently truncated into a
        // bogus domain such as `2606:50c0:8000:` with port 153.
        let t = Target::parse_authority("2606:50c0:8000::153", Some(443));
        assert!(t.is_err(), "{t:?}");
    }

    #[test]
    fn authority_parsing_rejects_junk() {
        for bad in ["", "  ", "github.com:0", "github.com:99999", "github.com:x"] {
            assert!(
                Target::parse_authority(bad, Some(443)).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn display_round_trips_authority() {
        assert_eq!(target("GitHub.com:443").to_string(), "github.com:443");
        assert_eq!(
            target("[2606:50c0:8000::153]:443").to_string(),
            "[2606:50c0:8000::153]:443"
        );
    }

    #[test]
    fn dangerous_patterns_are_rejected() {
        for bad in ["", "  ", "*github.com", "*.", "*.*", "a*b.com"] {
            assert!(
                bad.parse::<HostPattern>().is_err(),
                "{bad} must be rejected"
            );
        }
        assert_eq!("*".parse::<HostPattern>().unwrap(), HostPattern::Any);
    }

    /// `user@host` renders back as `user@host`, so a matcher that accepts it
    /// judges a "domain" nobody will ever dial, while the connector strips the
    /// userinfo and reaches the real host.
    #[test]
    fn userinfo_in_an_authority_is_rejected() {
        for authority in [
            "x@127.0.0.1:443",
            "@127.0.0.1:443",
            "user:pass@github.com:443",
            "x@secret.github.com:443",
        ] {
            assert!(
                Target::parse_authority(authority, Some(443)).is_err(),
                "{authority} must be rejected"
            );
        }
    }

    /// Percent-encoding hides the real host from the matcher but not from
    /// whatever decodes it later.
    #[test]
    fn percent_encoded_hosts_are_rejected() {
        for host in [
            "%31%32%37.0.0.1",
            "127%2E0%2E0%2E1",
            "e%76il.com",
            "a%00b.com",
        ] {
            assert!(Host::parse(host).is_err(), "{host} must be rejected");
        }
    }

    /// `::ffff:127.0.0.1` is the same address as `127.0.0.1`; a rule naming one
    /// must cover the other.
    #[test]
    fn ipv4_mapped_ipv6_is_the_same_host_as_its_ipv4_form() {
        assert_eq!(
            Host::parse("::ffff:127.0.0.1").unwrap(),
            Host::parse("127.0.0.1").unwrap()
        );
        assert_eq!(
            Host::parse("[::ffff:7f00:1]").unwrap(),
            Host::parse("127.0.0.1").unwrap()
        );

        let p = policy(&["*"], &["127.0.0.1"]);
        for authority in [
            "127.0.0.1:443",
            "[::ffff:127.0.0.1]:443",
            "[::ffff:7f00:1]:443",
        ] {
            assert_eq!(
                p.evaluate(&target(authority)),
                Decision::Deny(DenyReason::DeniedHost),
                "{authority} must stay denied"
            );
        }
    }

    /// These are IP addresses to `getaddrinfo` but "domains" to a strict
    /// parser, so accepting them as domains would let them match no rule and
    /// then resolve to the address anyway.
    #[test]
    fn alternate_ip_encodings_are_rejected() {
        for host in [
            "127.1",
            "2130706433",
            "0177.0.0.1",
            "0x7f.0.0.1",
            "0x7f000001",
            "192.168.1",
        ] {
            assert!(Host::parse(host).is_err(), "{host} must be rejected");
        }
    }

    #[test]
    fn non_ascii_hosts_are_rejected_so_rules_cannot_silently_miss() {
        for host in ["☃.com", "gÜnther.com", "exаmple.com"] {
            assert!(Host::parse(host).is_err(), "{host} must be rejected");
        }
        // The punycode form is the way to write such a rule.
        assert!(Host::parse("xn--n3h.com").is_ok());
    }

    #[test]
    fn malformed_labels_are_rejected() {
        for host in [
            ".evil.com",
            "evil.com..",
            "a..github.com",
            "..github.com",
            "-evil.com",
            "evil-.com",
            "a b.com",
            "a\tb.com",
        ] {
            assert!(Host::parse(host).is_err(), "{host} must be rejected");
        }
    }

    /// An authority without a port is ambiguous; whoever dials it may choose a
    /// different default than whoever checked it.
    #[test]
    fn authority_without_a_port_can_be_refused() {
        assert!(Target::parse_authority("github.com", None).is_err());
        assert_eq!(
            Target::parse_authority("github.com", Some(443))
                .unwrap()
                .port,
            443
        );
    }

    #[test]
    fn an_explicit_port_list_is_enforced_even_without_host_rules() {
        let p = Policy::allow_all().with_ports([443]);
        assert!(p.evaluate(&target("anything.org:443")).is_allowed());
        assert_eq!(
            p.evaluate(&target("anything.org:80")),
            Decision::Deny(DenyReason::PortNotAllowed)
        );
    }

    /// Forgetting the rules must be an error, not an open proxy.
    #[test]
    fn a_rule_less_policy_must_be_asked_for_explicitly() {
        let empty: [&str; 0] = [];
        assert_eq!(Policy::new(empty, empty).unwrap_err(), PolicyError::NoRules);
        // Transparency is available, but only by name.
        assert_eq!(Policy::allow_all().mode(), Mode::AllowAll);
        // As is an explicit "everything".
        assert_eq!(policy(&["*"], &[]).mode(), Mode::Allowlist);
    }

    #[test]
    fn pattern_display_round_trips() {
        for pattern in ["*", "github.com", "*.github.com"] {
            let parsed: HostPattern = pattern.parse().unwrap();
            assert_eq!(parsed.to_string(), pattern);
        }
    }
}
