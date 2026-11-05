//! Select a private address for advertising a cluster listener.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

pub const CLUSTER_DEFAULT_PORT: u16 = 7441;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoLanAddr {
    pub candidate: Option<IpAddr>,
}

impl fmt::Display for NoLanAddr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(candidate) = self.candidate {
            write!(
                formatter,
                "no private LAN address found (candidate {candidate})"
            )?;
        } else {
            formatter.write_str("no private LAN address found")?;
        }
        formatter.write_str(". Next: remuda cluster listen --bind IP")
    }
}

impl std::error::Error for NoLanAddr {}

/// Return whether an address belongs to an RFC 1918, shared CGNAT, or ULA range.
pub fn is_private_lan(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_private()
                || (address.octets()[0] == 100
                    && (address.octets()[1] & 0b1100_0000) == 0b0100_0000)
        }
        IpAddr::V6(address) => (address.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Return whether an address is safe for automatic advertisement.
/// The route probe uses IPv4, so IPv4-mapped IPv6 addresses are not candidates.
pub fn is_auto_eligible(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private(),
        IpAddr::V6(address) => (address.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Select the default-route address only when it is safe to advertise on a LAN.
pub fn auto_advertise_addr() -> Result<SocketAddr, NoLanAddr> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|_| NoLanAddr { candidate: None })?;
    socket
        .connect(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 9))
        .map_err(|_| NoLanAddr { candidate: None })?;
    let candidate = socket.local_addr().ok().map(|address| address.ip());
    address_from_candidate(candidate)
}

fn address_from_candidate(candidate: Option<IpAddr>) -> Result<SocketAddr, NoLanAddr> {
    match candidate {
        Some(address) if is_auto_eligible(address) => {
            Ok(SocketAddr::new(address, CLUSTER_DEFAULT_PORT))
        }
        candidate => Err(NoLanAddr { candidate }),
    }
}

/// Return the local source address the routing table picks toward `destination`.
/// Connecting a UDP socket sends no packet; it only resolves the route.
pub fn route_source_toward(destination: SocketAddr) -> Option<IpAddr> {
    let unspecified: SocketAddr = match destination {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(unspecified).ok()?;
    socket.connect(destination).ok()?;
    socket.local_addr().ok().map(|address| address.ip())
}

/// Listener address for a node that is joining through `issuer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinListenerChoice {
    /// Keep automatic selection: it already picks the interface toward the issuer.
    Auto,
    /// Bind the interface that routes to the issuer, e.g. a VPN adapter that
    /// is not the default route.
    Toward(SocketAddr),
}

/// Choose the joiner's listener from the route source toward the issuer and
/// the automatic choice. Only auto-eligible addresses are used, so this never
/// widens exposure beyond private ranges.
pub fn join_listener_choice(
    route_source: Option<IpAddr>,
    auto: Result<SocketAddr, NoLanAddr>,
) -> JoinListenerChoice {
    match route_source {
        Some(source)
            if is_auto_eligible(source) && auto.map(|address| address.ip()) != Ok(source) =>
        {
            JoinListenerChoice::Toward(SocketAddr::new(source, CLUSTER_DEFAULT_PORT))
        }
        _ => JoinListenerChoice::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        address_from_candidate, auto_advertise_addr, is_auto_eligible, is_private_lan,
        join_listener_choice, route_source_toward, JoinListenerChoice, NoLanAddr,
        CLUSTER_DEFAULT_PORT,
    };
    use std::net::{IpAddr, SocketAddr};

    fn auto(address: &str) -> Result<SocketAddr, NoLanAddr> {
        Ok(address.parse().unwrap())
    }

    fn no_lan(candidate: Option<&str>) -> Result<SocketAddr, NoLanAddr> {
        Err(NoLanAddr {
            candidate: candidate.map(|address| address.parse().unwrap()),
        })
    }

    #[test]
    fn join_listener_binds_the_vpn_interface_when_the_default_route_is_elsewhere() {
        // Home laptop: default route is home Wi-Fi, the issuer is reached via the VPN adapter.
        assert_eq!(
            join_listener_choice(Some("10.8.0.5".parse().unwrap()), auto("192.168.0.20:7441")),
            JoinListenerChoice::Toward("10.8.0.5:7441".parse().unwrap())
        );
        // Default route is not private at all (Windows with a public or CGNAT route).
        assert_eq!(
            join_listener_choice(
                Some("172.20.1.9".parse().unwrap()),
                no_lan(Some("203.0.113.7"))
            ),
            JoinListenerChoice::Toward("172.20.1.9:7441".parse().unwrap())
        );
        assert_eq!(
            join_listener_choice(Some("fd00::5".parse().unwrap()), no_lan(None)),
            JoinListenerChoice::Toward("[fd00::5]:7441".parse().unwrap())
        );
    }

    #[test]
    fn join_listener_keeps_auto_when_routes_agree_or_the_source_is_not_bindable() {
        // Same subnet: the issuer route and the default route share an interface.
        assert_eq!(
            join_listener_choice(
                Some("192.168.0.20".parse().unwrap()),
                auto("192.168.0.20:7441")
            ),
            JoinListenerChoice::Auto
        );
        // Loopback test issuers, CGNAT, link-local, public and unknown sources never widen exposure.
        for source in [
            "127.0.0.1",
            "100.64.0.9",
            "169.254.3.4",
            "203.0.113.7",
            "::1",
        ] {
            assert_eq!(
                join_listener_choice(Some(source.parse().unwrap()), auto("192.168.0.20:7441")),
                JoinListenerChoice::Auto,
                "{source}"
            );
            assert_eq!(
                join_listener_choice(Some(source.parse().unwrap()), no_lan(None)),
                JoinListenerChoice::Auto,
                "{source}"
            );
        }
        assert_eq!(
            join_listener_choice(None, auto("192.168.0.20:7441")),
            JoinListenerChoice::Auto
        );
    }

    #[test]
    fn route_source_toward_loopback_is_loopback() {
        assert_eq!(
            route_source_toward("127.0.0.1:7441".parse().unwrap()),
            Some("127.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn auto_classifier_excludes_cgnat_and_checks_boundaries() {
        let private = [
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.2",
            "fc00::1",
            "fdff::1",
        ];
        for address in private {
            let address: IpAddr = address.parse().unwrap();
            assert!(
                is_auto_eligible(address),
                "expected auto eligible: {address}"
            );
        }

        let not_private = [
            "127.0.0.1",
            "169.254.1.2",
            "172.15.255.254",
            "172.32.0.1",
            "100.63.255.254",
            "100.64.0.1",
            "100.127.255.254",
            "100.128.0.1",
            "8.8.8.8",
            "192.0.2.1",
            "::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "2001:4860:4860::8888",
        ];
        for address in not_private {
            let address: IpAddr = address.parse().unwrap();
            assert!(
                !is_auto_eligible(address),
                "expected not auto eligible: {address}"
            );
        }
        assert!(is_private_lan("100.64.0.1".parse().unwrap()));
    }

    #[test]
    fn no_lan_address_suggests_an_explicit_bind_next_step() {
        let error = NoLanAddr {
            candidate: Some("8.8.8.8".parse().unwrap()),
        };
        assert_eq!(
            error.to_string(),
            "no private LAN address found (candidate 8.8.8.8). Next: remuda cluster listen --bind IP"
        );
    }

    #[test]
    fn auto_advertisement_refuses_only_missing_or_ineligible_candidates() {
        for candidate in [
            Some("8.8.8.8".parse().unwrap()),
            Some("100.64.0.1".parse().unwrap()),
            None,
        ] {
            let error =
                address_from_candidate(candidate).expect_err("candidate is not auto eligible");
            assert!(error
                .to_string()
                .contains("Next: remuda cluster listen --bind IP"));
        }
        let eligible = address_from_candidate(Some("192.168.1.20".parse().unwrap()))
            .expect("a selected eligible route is sufficient");
        assert_eq!(eligible, "192.168.1.20:7441".parse().unwrap());
    }

    #[test]
    fn auto_advertise_addr_returns_private_address_or_typed_no_lan_error() {
        match auto_advertise_addr() {
            Ok(address) => {
                assert!(is_auto_eligible(address.ip()));
                assert_eq!(address.port(), CLUSTER_DEFAULT_PORT);
            }
            Err(error) => {
                assert!(error
                    .candidate
                    .is_none_or(|candidate| !is_auto_eligible(candidate)));
            }
        }
    }

    #[test]
    fn cluster_default_port_is_7441() {
        assert_eq!(CLUSTER_DEFAULT_PORT, 7441);
        let address: SocketAddr = "192.0.2.4:7441".parse().unwrap();
        assert_eq!(address.port(), CLUSTER_DEFAULT_PORT);
    }
}
