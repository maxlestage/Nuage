//! A server's address as a person types or pastes it (`localhost:8080`,
//! `nuage.example.com`, `HTTPS://Nuage.Example.com/admin/settings`), made
//! into the address the editor calls (`http://localhost:8080`,
//! `https://nuage.example.com`), and what kind of network it is on.
//!
//! Text is cut only at characters it was searched for, never at a byte count.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// `http://` or `https://`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

/// Where a host is, for what may cross the network in clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// This computer (`localhost`, `127.0.0.1`, `[::1]`).
    Loopback,
    /// A home or office network (`192.168.1.20`, `maison.local`, `nas`).
    Private,
    /// The Internet.
    Public,
}

/// A server's address, normalized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub scheme: Scheme,
    /// Lowercase; an IPv6 address without its brackets.
    pub host: String,
    pub port: Option<u16>,
    /// What followed the host and was dropped (`/admin`): shown under the field.
    pub dropped: Option<String>,
}

/// Why a text is not an address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddressError {
    Empty,
    /// Not `http` or `https` (`ftp`).
    Scheme(String),
    /// No host, or one with characters a host does not have.
    Host(String),
    /// Not a number from 1 to 65535.
    Port(String),
}

impl Address {
    /// `https://host[:port]`, what the editor calls.
    pub fn base(&self) -> String {
        let scheme = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        match self.port {
            Some(port) => format!("{scheme}://{host}:{port}"),
            None => format!("{scheme}://{host}"),
        }
    }

    pub fn class(&self) -> Class {
        class(&self.host)
    }

    /// The port called: the one written, else the scheme's.
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(match self.scheme {
            Scheme::Http => 80,
            Scheme::Https => 443,
        })
    }

    /// Plain `http://` to a host of the Internet: a token sent there crosses it in clear.
    pub fn is_cleartext_public(&self) -> bool {
        self.scheme == Scheme::Http && self.class() == Class::Public
    }
}

/// The address of `typed`: the scheme and host in lowercase, the port kept,
/// the path, query and trailing `/` dropped. Without a scheme, a host of
/// this computer or of the local network gets `http://`, any other `https://`.
pub fn normalize(typed: &str) -> Result<Address, AddressError> {
    let text = typed.trim();
    // A line pasted from a settings file.
    let text = text.strip_prefix("NUAGE_URL=").unwrap_or(text).trim().trim_matches(['"', '\'']).trim();
    if text.is_empty() {
        return Err(AddressError::Empty);
    }
    let (scheme, rest) = match text.split_once("://") {
        Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
            "http" => (Some(Scheme::Http), rest),
            "https" => (Some(Scheme::Https), rest),
            other => return Err(AddressError::Scheme(other.to_owned())),
        },
        None => (None, text),
    };
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    let dropped = Some(tail.trim_end_matches('/').to_owned()).filter(|t| !t.is_empty());
    let bad_host = || AddressError::Host(authority.to_owned());
    if authority.contains('@') || authority.chars().any(char::is_whitespace) {
        return Err(bad_host());
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        // [::1]:8080
        let (host, after) = inner.split_once(']').ok_or_else(bad_host)?;
        let host = host.parse::<Ipv6Addr>().map_err(|_| bad_host())?.to_string();
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':').ok_or_else(bad_host)?)),
        }
    } else if authority.matches(':').count() > 1 {
        // ::1, without brackets (and so without a port).
        (authority.parse::<Ipv6Addr>().map_err(|_| bad_host())?.to_string(), None)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_ascii_lowercase(), Some(port)),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    let name_ok = |h: &str| h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if host.is_empty() || !(host.contains(':') || name_ok(&host)) {
        return Err(bad_host());
    }
    let port = match port.filter(|p| !p.is_empty()) {
        None => None,
        Some(p) => Some(p.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| AddressError::Port(p.to_owned()))?),
    };
    let scheme = scheme.unwrap_or(match class(&host) {
        Class::Loopback | Class::Private => Scheme::Http,
        Class::Public => Scheme::Https,
    });
    Ok(Address { scheme, host, port, dropped })
}

/// Where `host` (lowercase, IPv6 without brackets) is.
pub fn class(host: &str) -> Class {
    let host = host.trim_end_matches('.');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip_class(ip);
    }
    if host == "localhost" || host.ends_with(".localhost") {
        Class::Loopback
    } else if host.ends_with(".local") || !host.contains('.') {
        // mDNS names, and single names a local network resolves (`nas`).
        Class::Private
    } else {
        Class::Public
    }
}

fn ip_class(ip: IpAddr) -> Class {
    match ip {
        IpAddr::V4(v4) => v4_class(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_class(v4);
            }
            let [first, ..] = v6.segments();
            if v6.is_loopback() || v6.is_unspecified() {
                Class::Loopback
            } else if first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80 {
                // fc00::/7 (unique local), fe80::/10 (link-local).
                Class::Private
            } else {
                Class::Public
            }
        }
    }
}

fn v4_class(v4: Ipv4Addr) -> Class {
    let [a, b, ..] = v4.octets();
    if v4.is_loopback() || v4.is_unspecified() {
        Class::Loopback
    } else if v4.is_private() || v4.is_link_local() || (a == 100 && b & 0xc0 == 64) {
        // 10/8, 172.16/12, 192.168/16, 169.254/16, and 100.64/10 (carrier-grade NAT, Tailscale).
        Class::Private
    } else {
        Class::Public
    }
}

/// A Heroku app (it only picks the help texts).
pub fn is_heroku(host: &str) -> bool {
    host.trim_end_matches('.').ends_with(".herokuapp.com")
}

/// This computer's address on its local network, as other devices reach it.
/// No packet is sent: connecting a UDP socket only picks the route.
pub fn lan_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 80)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(typed: &str) -> String {
        normalize(typed).map(|a| a.base()).unwrap_or_else(|e| format!("{e:?}"))
    }

    #[test]
    fn addresses_as_people_type_them() {
        for (typed, address) in [
            ("localhost:8080", "http://localhost:8080"),
            ("  127.0.0.1:8080/ ", "http://127.0.0.1:8080"),
            ("192.168.1.20:8080", "http://192.168.1.20:8080"),
            ("172.20.1.5:8080", "http://172.20.1.5:8080"),
            ("10.0.0.7", "http://10.0.0.7"),
            ("[::1]:8080", "http://[::1]:8080"),
            ("::1", "http://[::1]"),
            ("nas:8080", "http://nas:8080"),
            ("nuage.example.com", "https://nuage.example.com"),
            ("172.32.0.1", "https://172.32.0.1"),
            ("HTTPS://Nuage.Example.com/", "https://nuage.example.com"),
            ("HTTP://LOCALHOST:8080", "http://localhost:8080"),
            ("https://nuage-f48a.herokuapp.com/admin/settings", "https://nuage-f48a.herokuapp.com"),
            ("http://maison.local:8080/fr/app/", "http://maison.local:8080"),
            ("nuage.example.com:8443?x=1", "https://nuage.example.com:8443"),
            ("NUAGE_URL=\"https://n.example.com\"", "https://n.example.com"),
            ("http://nuage.example.com", "http://nuage.example.com"),
        ] {
            assert_eq!(base(typed), address, "{typed}");
        }
        let pasted = normalize("https://nuage-f48a.herokuapp.com/admin/settings/").unwrap();
        assert_eq!(pasted.dropped.as_deref(), Some("/admin/settings"));
        assert_eq!(normalize("localhost:8080/").unwrap().dropped, None);
        assert_eq!(normalize("été.fr/é").map_err(|e| matches!(e, AddressError::Host(_))), Err(true));
        for (typed, error) in [
            ("", AddressError::Empty),
            ("   ", AddressError::Empty),
            ("ftp://x", AddressError::Scheme("ftp".into())),
            ("http://", AddressError::Host(String::new())),
            ("https:///admin", AddressError::Host(String::new())),
            ("my server", AddressError::Host("my server".into())),
            ("user:pass@host", AddressError::Host("user:pass@host".into())),
            ("localhost:99999", AddressError::Port("99999".into())),
            ("localhost:0", AddressError::Port("0".into())),
            ("[::1", AddressError::Host("[::1".into())),
        ] {
            assert_eq!(normalize(typed), Err(error), "{typed:?}");
        }
    }

    #[test]
    fn what_may_cross_the_network_in_clear() {
        for host in ["localhost", "127.0.0.1", "127.8.9.1", "::1", "0.0.0.0", "app.localhost"] {
            assert_eq!(class(host), Class::Loopback, "{host}");
        }
        for host in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.10",
            "169.254.1.1",
            "100.100.1.1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:192.168.1.2",
            "maison.local",
            "nas",
        ] {
            assert_eq!(class(host), Class::Private, "{host}");
        }
        for host in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "2001:db8::1", "nuage.example.com", "x.herokuapp.com"] {
            assert_eq!(class(host), Class::Public, "{host}");
        }
        assert!(normalize("http://nuage.example.com").unwrap().is_cleartext_public());
        assert!(!normalize("http://192.168.1.2").unwrap().is_cleartext_public());
        assert!(!normalize("nuage.example.com").unwrap().is_cleartext_public());
        assert!(is_heroku("nuage-f48a.herokuapp.com") && !is_heroku("herokuapp.com.example.org"));
        assert_eq!(normalize("https://x").unwrap().port_or_default(), 443);
    }
}
