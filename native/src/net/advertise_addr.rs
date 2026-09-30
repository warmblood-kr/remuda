//! Select a private address for advertising a cluster listener.

#[cfg(test)]
use std::net::{IpAddr, SocketAddr};

#[cfg(test)]
pub const CLUSTER_DEFAULT_PORT: u16 = 7441;

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoLanAddr {
    pub candidate: Option<IpAddr>,
}

#[cfg(test)]
pub fn is_private_lan(_address: IpAddr) -> bool {
    false
}

#[cfg(test)]
pub fn auto_bind() -> Result<SocketAddr, NoLanAddr> {
    Err(NoLanAddr { candidate: None })
}

#[cfg(test)]
mod tests {
    use super::{auto_bind, is_private_lan, CLUSTER_DEFAULT_PORT};
    #[cfg(test)]
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
