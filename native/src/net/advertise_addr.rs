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
        match self.candidate {
            Some(candidate) => write!(
                formatter,
                "no private LAN address found (candidate {candidate})"
            ),
            None => formatter.write_str("no private LAN address found"),
        }
    }
}

impl std::error::Error for NoLanAddr {}

/// Return whether an address belongs to a private LAN or shared CGNAT range.
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

/// Select the default-route address only when it is safe to advertise on a LAN.
pub fn auto_bind() -> Result<SocketAddr, NoLanAddr> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|_| NoLanAddr { candidate: None })?;
    socket
        .connect(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 9))
        .map_err(|_| NoLanAddr { candidate: None })?;
    let candidate = socket.local_addr().ok().map(|address| address.ip());
    match candidate {
        Some(address) if is_private_lan(address) => {
            Ok(SocketAddr::new(address, CLUSTER_DEFAULT_PORT))
        }
        candidate => Err(NoLanAddr { candidate }),
    }
}

#[cfg(test)]
mod tests {
    use super::{auto_bind, is_private_lan, CLUSTER_DEFAULT_PORT};
    use std::net::{IpAddr, SocketAddr};

    #[test]
    fn private_lan_address_classification_matches_private_and_cgnat_ranges() {
        let private = [
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.2",
            "100.64.0.1",
            "100.127.255.254",
            "fc00::1",
            "fdff::1",
        ];
        for address in private {
            let address: IpAddr = address.parse().unwrap();
            assert!(is_private_lan(address), "expected private LAN: {address}");
        }

        let not_private = [
            "127.0.0.1",
            "169.254.1.2",
            "8.8.8.8",
            "192.0.2.1",
            "::1",
            "fe80::1",
            "2001:4860:4860::8888",
        ];
        for address in not_private {
            let address: IpAddr = address.parse().unwrap();
            assert!(!is_private_lan(address), "expected non-private: {address}");
        }
    }

    #[test]
    fn auto_bind_returns_private_address_or_typed_no_lan_error() {
        match auto_bind() {
            Ok(address) => {
                assert!(is_private_lan(address.ip()));
                assert_eq!(address.port(), CLUSTER_DEFAULT_PORT);
            }
            Err(error) => {
                assert!(error
                    .candidate
                    .is_none_or(|candidate| !is_private_lan(candidate)));
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
