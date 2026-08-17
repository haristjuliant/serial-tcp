//! Deciding who is allowed to connect.
//!
//! Binding to `0.0.0.0` is all or nothing: either the machine is unreachable
//! from the network or every host on it can knock. An allowlist is the missing
//! middle, and it matters most for the serial ports themselves — those speak raw
//! bytes or RFC 2217 and cannot carry a token, so *where a connection comes
//! from* is the only thing left to judge them by.
//!
//! Prefix matching is a mask and a comparison, so it is written out here rather
//! than pulling in a crate for it.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An address, or a network of them: `192.168.9.5`, `192.168.8.0/22`, `::1/128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    base: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            // A bare address is a network of exactly one host.
            None => (text, None),
        };

        let base: IpAddr = addr
            .trim()
            .parse()
            .with_context(|| format!("{addr} is not an IP address"))?;
        // Normalise ::ffff:192.168.1.1 to 192.168.1.1 so a v4 rule matches a
        // v4 client that arrived over a dual-stack socket.
        let base = base.to_canonical();

        let width = if base.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(text) => text
                .trim()
                .parse::<u8>()
                .with_context(|| format!("{text} is not a prefix length"))?,
            None => width,
        };

        if prefix > width {
            bail!("/{prefix} is too long for {base}, the most it can be is /{width}");
        }

        Ok(Self { base, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, ip.to_canonical()) {
            (IpAddr::V4(base), IpAddr::V4(ip)) => {
                mask_v4(base, self.prefix) == mask_v4(ip, self.prefix)
            }
            (IpAddr::V6(base), IpAddr::V6(ip)) => {
                mask_v6(base, self.prefix) == mask_v6(ip, self.prefix)
            }
            // A v4 rule never matches a real v6 client, or the other way round.
            _ => false,
        }
    }
}

fn mask_v4(addr: Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from(addr);
    if prefix == 0 {
        0
    } else {
        bits & (u32::MAX << (32 - prefix))
    }
}

fn mask_v6(addr: Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from(addr);
    if prefix == 0 {
        0
    } else {
        bits & (u128::MAX << (128 - prefix))
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.base, self.prefix)
    }
}

impl Serialize for Cidr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Self::parse(&text).map_err(|e| serde::de::Error::custom(format!("{e:#}")))
    }
}

/// Who may connect. Empty means everybody.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    rules: Vec<Cidr>,
}

impl Allowlist {
    pub fn new(rules: Vec<Cidr>) -> Self {
        Self { rules }
    }

    /// Parse a list of `--allow` arguments, reporting which one was wrong.
    pub fn parse(values: &[String]) -> Result<Self> {
        let rules = values
            .iter()
            .map(|v| Cidr::parse(v))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::new(rules))
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[Cidr] {
        &self.rules
    }

    pub fn permits(&self, ip: IpAddr) -> bool {
        // Never lock the machine out of its own dashboard. An allowlist is about
        // the network, and shutting out `127.0.0.1` is only ever an accident.
        if ip.to_canonical().is_loopback() {
            return true;
        }
        self.rules.is_empty() || self.rules.iter().any(|rule| rule.contains(ip))
    }

    pub fn describe(&self) -> String {
        if self.rules.is_empty() {
            "anywhere".to_owned()
        } else {
            self.rules
                .iter()
                .map(Cidr::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// This machine's address on the network it routes through.
///
/// Nothing is actually sent — a connectionless UDP socket just makes the OS pick
/// the interface it would use, which is the address a user needs to type into
/// another machine. Used only to print a helpful hint at startup.
pub fn primary_local_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 80)).ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

/// The /24 around an address, as a suggestion for `--allow`.
pub fn suggest_subnet(ip: IpAddr) -> Option<String> {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            Some(format!("{a}.{b}.{c}.0/24"))
        }
        IpAddr::V6(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("test address")
    }

    #[test]
    fn a_bare_address_matches_only_itself() {
        let rule = Cidr::parse("192.168.9.104").unwrap();
        assert_eq!(rule.to_string(), "192.168.9.104/32");
        assert!(rule.contains(ip("192.168.9.104")));
        assert!(!rule.contains(ip("192.168.9.105")));
    }

    /// The case the user asked for: one rule covering a whole /22.
    #[test]
    fn a_slash_22_covers_its_whole_range() {
        let rule = Cidr::parse("192.168.8.0/22").unwrap();

        assert!(rule.contains(ip("192.168.8.0")));
        assert!(rule.contains(ip("192.168.9.104")));
        assert!(rule.contains(ip("192.168.11.255")));

        assert!(!rule.contains(ip("192.168.7.255")), "just below the range");
        assert!(!rule.contains(ip("192.168.12.0")), "just above the range");
        assert!(!rule.contains(ip("10.0.0.1")));
    }

    #[test]
    fn common_prefixes_match_what_people_expect() {
        assert!(
            Cidr::parse("10.0.0.0/8")
                .unwrap()
                .contains(ip("10.255.1.2"))
        );
        assert!(!Cidr::parse("10.0.0.0/8").unwrap().contains(ip("11.0.0.1")));
        assert!(
            Cidr::parse("192.168.1.0/24")
                .unwrap()
                .contains(ip("192.168.1.77"))
        );
        assert!(
            !Cidr::parse("192.168.1.0/24")
                .unwrap()
                .contains(ip("192.168.2.77"))
        );
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(ip("8.8.8.8")));
    }

    /// A dual-stack listener reports v4 clients as ::ffff:a.b.c.d, which must
    /// still match the v4 rule the user wrote.
    #[test]
    fn v4_mapped_v6_clients_match_v4_rules() {
        let rule = Cidr::parse("192.168.8.0/22").unwrap();
        assert!(rule.contains(ip("::ffff:192.168.9.104")));
        assert!(!rule.contains(ip("::ffff:10.0.0.1")));
    }

    #[test]
    fn v6_rules_work_too() {
        let rule = Cidr::parse("fd00::/8").unwrap();
        assert!(rule.contains(ip("fd12:3456::1")));
        assert!(!rule.contains(ip("2001:db8::1")));
        assert!(!rule.contains(ip("192.168.1.1")), "different family");
    }

    #[test]
    fn nonsense_is_rejected_with_a_reason() {
        assert!(Cidr::parse("not-an-ip").is_err());
        assert!(Cidr::parse("192.168.1.0/33").is_err());
        assert!(Cidr::parse("192.168.1.0/abc").is_err());
        assert!(Cidr::parse("::1/129").is_err());
    }

    #[test]
    fn an_empty_allowlist_permits_everything() {
        let list = Allowlist::default();
        assert!(list.permits(ip("8.8.8.8")));
        assert!(list.permits(ip("192.168.1.1")));
        assert_eq!(list.describe(), "anywhere");
    }

    #[test]
    fn an_allowlist_admits_only_its_networks() {
        let list = Allowlist::parse(&["192.168.8.0/22".to_owned(), "10.0.0.5".to_owned()]).unwrap();

        assert!(list.permits(ip("192.168.9.104")));
        assert!(list.permits(ip("10.0.0.5")));
        assert!(!list.permits(ip("10.0.0.6")));
        assert!(!list.permits(ip("172.16.0.1")));
    }

    /// Locking yourself out of your own machine is never what the flag meant.
    #[test]
    fn loopback_is_always_allowed() {
        let list = Allowlist::parse(&["10.0.0.0/8".to_owned()]).unwrap();
        assert!(list.permits(ip("127.0.0.1")));
        assert!(list.permits(ip("::1")));
        assert!(list.permits(ip("::ffff:127.0.0.1")));
    }

    #[test]
    fn rules_survive_a_json_round_trip() {
        let rule = Cidr::parse("192.168.8.0/22").unwrap();
        let json = serde_json::to_string(&rule).unwrap();
        assert_eq!(json, "\"192.168.8.0/22\"");
        assert_eq!(serde_json::from_str::<Cidr>(&json).unwrap(), rule);
    }

    #[test]
    fn a_subnet_suggestion_is_the_slash_24_around_an_address() {
        assert_eq!(
            suggest_subnet(ip("192.168.9.104")).as_deref(),
            Some("192.168.9.0/24")
        );
    }
}
