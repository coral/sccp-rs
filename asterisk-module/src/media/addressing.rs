//! Deterministic signaling and media address selection.

use std::io;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::config::{AdvertisedAddresses, ExternalAddress, IpNetwork, NatMode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}

impl AddressFamily {
    pub fn of(address: IpAddr) -> Self {
        match canonical_ip_address(address) {
            IpAddr::V4(_) => Self::Ipv4,
            IpAddr::V6(_) => Self::Ipv6,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NatReason {
    Forced,
    SignalingPeerOutsideLocalNetworks,
    ReportedAddressMismatch,
    NotDetected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NatDecision {
    pub active: bool,
    pub reason: NatReason,
}

#[derive(Clone, Copy, Debug)]
pub struct AddressSelectionPolicy<'a> {
    pub nat: NatMode,
    pub local_networks: &'a [IpNetwork],
    pub advertised: &'a AdvertisedAddresses,
    pub external: ResolvedExternalAddresses,
}

impl AddressSelectionPolicy<'_> {
    pub fn nat_decision(self, signaling_peer: IpAddr, reported: Option<IpAddr>) -> NatDecision {
        let signaling_peer = canonical_ip_address(signaling_peer);
        let reported = reported.map(canonical_ip_address);
        match self.nat {
            NatMode::On => NatDecision {
                active: true,
                reason: NatReason::Forced,
            },
            NatMode::Off => NatDecision {
                active: false,
                reason: NatReason::Forced,
            },
            NatMode::Auto | NatMode::AutoOff | NatMode::AutoOn => {
                if !self.local_networks.is_empty()
                    && !self
                        .local_networks
                        .iter()
                        .any(|network| network_contains(network, signaling_peer))
                {
                    return NatDecision {
                        active: true,
                        reason: NatReason::SignalingPeerOutsideLocalNetworks,
                    };
                }
                if reported.is_some_and(|reported| reported != signaling_peer) {
                    return NatDecision {
                        active: true,
                        reason: NatReason::ReportedAddressMismatch,
                    };
                }
                NatDecision {
                    active: false,
                    reason: NatReason::NotDetected,
                }
            }
        }
    }

    /// Select the endpoint at which the phone can actually receive media.
    pub fn phone_peer(
        self,
        signaling_peer: IpAddr,
        reported: IpAddr,
        registration_reported: Option<IpAddr>,
    ) -> Result<(IpAddr, NatDecision), AddressSelectionError> {
        let signaling_peer = canonical_ip_address(signaling_peer);
        let reported = canonical_ip_address(reported);
        let decision = self.nat_decision(signaling_peer, registration_reported);
        let selected = if decision.active {
            signaling_peer
        } else {
            reported
        };
        validate_endpoint(selected)?;
        if AddressFamily::of(selected) != AddressFamily::of(signaling_peer) {
            return Err(AddressSelectionError::AddressFamilyMismatch {
                expected: AddressFamily::of(signaling_peer),
                found: AddressFamily::of(selected),
            });
        }
        Ok((selected, decision))
    }

    /// Select the local RTP address that should be placed on the wire to the
    /// phone. The Asterisk RTP socket remains bound to `local`; this only
    /// chooses its reachable advertised address.
    pub fn advertised_media(
        self,
        local: IpAddr,
        signaling_peer: IpAddr,
        reported: Option<IpAddr>,
    ) -> Result<(IpAddr, NatDecision), AddressSelectionError> {
        let local = canonical_ip_address(local);
        let signaling_peer = canonical_ip_address(signaling_peer);
        let decision = self.nat_decision(signaling_peer, reported);
        let family = AddressFamily::of(signaling_peer);
        let candidate = if decision.active {
            self.external.for_family(family)
        } else {
            None
        }
        .filter(|candidate| usable_for_peer(*candidate, signaling_peer))
        .or_else(|| {
            advertised_for_family(self.advertised, family)
                .filter(|candidate| usable_for_peer(*candidate, signaling_peer))
        })
        .unwrap_or(local);
        validate_endpoint(candidate)?;
        if AddressFamily::of(candidate) != family {
            return Err(AddressSelectionError::AddressFamilyMismatch {
                expected: family,
                found: AddressFamily::of(candidate),
            });
        }
        if !usable_for_peer(candidate, signaling_peer) {
            return Err(AddressSelectionError::UnusableAddress(candidate));
        }
        Ok((candidate, decision))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResolvedExternalAddresses {
    pub ipv4: Option<IpAddr>,
    pub ipv6: Option<IpAddr>,
}

impl ResolvedExternalAddresses {
    pub fn from_address(address: IpAddr) -> Self {
        match address {
            IpAddr::V4(_) => Self {
                ipv4: Some(address),
                ipv6: None,
            },
            IpAddr::V6(_) => Self {
                ipv4: None,
                ipv6: Some(address),
            },
        }
    }

    pub const fn for_family(self, family: AddressFamily) -> Option<IpAddr> {
        match family {
            AddressFamily::Ipv4 => self.ipv4,
            AddressFamily::Ipv6 => self.ipv6,
        }
    }
}

pub trait HostResolver: Send + Sync {
    fn resolve(&self, hostname: &str) -> io::Result<Vec<IpAddr>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemHostResolver;

impl HostResolver for SystemHostResolver {
    fn resolve(&self, hostname: &str) -> io::Result<Vec<IpAddr>> {
        (hostname, 0)
            .to_socket_addrs()
            .map(|addresses| addresses.map(|address| address.ip()).collect())
    }
}

#[derive(Debug)]
pub struct ExternalAddressCache<R> {
    resolver: R,
    hostname: Option<String>,
    resolved: ResolvedExternalAddresses,
    expires_at: Option<Instant>,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct ExternalAddressCheckpoint {
    hostname: Option<String>,
    resolved: ResolvedExternalAddresses,
    expires_at: Option<Instant>,
}

impl<R> ExternalAddressCache<R>
where
    R: HostResolver,
{
    pub fn new(resolver: R) -> Self {
        Self {
            resolver,
            hostname: None,
            resolved: ResolvedExternalAddresses::default(),
            expires_at: None,
        }
    }

    pub const fn current(&self) -> ResolvedExternalAddresses {
        self.resolved
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn refresh_deadline(&self) -> Option<Instant> {
        self.expires_at
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn checkpoint(&self) -> ExternalAddressCheckpoint {
        ExternalAddressCheckpoint {
            hostname: self.hostname.clone(),
            resolved: self.resolved,
            expires_at: self.expires_at,
        }
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn restore(&mut self, checkpoint: ExternalAddressCheckpoint) {
        self.hostname = checkpoint.hostname;
        self.resolved = checkpoint.resolved;
        self.expires_at = checkpoint.expires_at;
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn reset_to(&mut self, fallback: ResolvedExternalAddresses) {
        self.hostname = None;
        self.expires_at = None;
        self.resolved = fallback;
    }

    /// Refresh an external address when its configured lifetime expires.
    /// Failed hostname refreshes preserve the last usable result and remain
    /// immediately retryable.
    pub fn refresh(
        &mut self,
        external: Option<&ExternalAddress>,
        now: Instant,
    ) -> Result<ResolvedExternalAddresses, ExternalResolutionError> {
        match external {
            None => {
                self.hostname = None;
                self.resolved = ResolvedExternalAddresses::default();
                self.expires_at = None;
                Ok(self.resolved)
            }
            Some(ExternalAddress::Address(address)) => {
                validate_endpoint(*address).map_err(|_| {
                    ExternalResolutionError::NoUsableAddress("configured address".into())
                })?;
                self.hostname = None;
                self.resolved = ResolvedExternalAddresses::from_address(*address);
                self.expires_at = None;
                Ok(self.resolved)
            }
            Some(ExternalAddress::Hostname {
                name,
                refresh_seconds,
            }) => {
                if self.hostname.as_deref() == Some(name)
                    && self.expires_at.is_some_and(|deadline| now < deadline)
                {
                    return Ok(self.resolved);
                }
                let addresses = self.resolver.resolve(name).map_err(|source| {
                    ExternalResolutionError::Lookup {
                        hostname: name.clone(),
                        source,
                    }
                })?;
                let mut resolved = ResolvedExternalAddresses::default();
                for address in addresses {
                    if validate_endpoint(address).is_err() {
                        continue;
                    }
                    match address {
                        IpAddr::V4(_) if resolved.ipv4.is_none() => resolved.ipv4 = Some(address),
                        IpAddr::V6(_) if resolved.ipv6.is_none() => resolved.ipv6 = Some(address),
                        _ => {}
                    }
                }
                if resolved == ResolvedExternalAddresses::default() {
                    return Err(ExternalResolutionError::NoUsableAddress(name.clone()));
                }
                self.hostname = Some(name.clone());
                self.resolved = resolved;
                self.expires_at = Some(now + Duration::from_secs(u64::from(*refresh_seconds)));
                Ok(self.resolved)
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum ExternalResolutionError {
    #[error("unable to resolve external hostname {hostname}: {source}")]
    Lookup {
        hostname: String,
        #[source]
        source: io::Error,
    },
    #[error("external address source {0} returned no usable unicast address")]
    NoUsableAddress(String),
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum AddressSelectionError {
    #[error("address {0} is not a usable unicast endpoint")]
    UnusableAddress(IpAddr),
    #[error("address family mismatch: expected {expected:?}, found {found:?}")]
    AddressFamilyMismatch {
        expected: AddressFamily,
        found: AddressFamily,
    },
}

fn advertised_for_family(addresses: &AdvertisedAddresses, family: AddressFamily) -> Option<IpAddr> {
    match family {
        AddressFamily::Ipv4 => addresses.ipv4.map(IpAddr::V4),
        AddressFamily::Ipv6 => addresses.ipv6.map(IpAddr::V6),
    }
}

/// Normalize IPv4-mapped IPv6 addresses so dual-stack listeners make the
/// same family decisions as native IPv4 listeners.
pub fn canonical_ip_address(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    }
}

fn validate_endpoint(address: IpAddr) -> Result<(), AddressSelectionError> {
    let usable = match address {
        IpAddr::V4(address) => {
            !address.is_unspecified() && !address.is_broadcast() && !address.is_multicast()
        }
        IpAddr::V6(address) => {
            !address.is_unspecified() && !address.is_multicast() && !address.is_unicast_link_local()
        }
    };
    usable
        .then_some(())
        .ok_or(AddressSelectionError::UnusableAddress(address))
}

fn usable_for_peer(candidate: IpAddr, peer: IpAddr) -> bool {
    validate_endpoint(candidate).is_ok()
        && match (candidate, peer) {
            (IpAddr::V4(candidate), IpAddr::V4(peer)) => {
                !candidate.is_loopback() || peer.is_loopback()
            }
            (IpAddr::V6(candidate), IpAddr::V6(peer)) => {
                !candidate.is_loopback() || peer.is_loopback()
            }
            _ => false,
        }
}

fn network_contains(network: &IpNetwork, address: IpAddr) -> bool {
    match (network.address, address) {
        (IpAddr::V4(network_address), IpAddr::V4(address)) if network.prefix <= 32 => {
            let mask = if network.prefix == 0 {
                0
            } else {
                u32::MAX << (32 - network.prefix)
            };
            u32::from(network_address) & mask == u32::from(address) & mask
        }
        (IpAddr::V6(network_address), IpAddr::V6(address)) if network.prefix <= 128 => {
            let mask = if network.prefix == 0 {
                0
            } else {
                u128::MAX << (128 - network.prefix)
            };
            u128::from(network_address) & mask == u128::from(address) & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Mutex;

    use super::*;

    fn networks() -> Vec<IpNetwork> {
        vec![
            IpNetwork {
                address: "10.0.0.0".parse().unwrap(),
                prefix: 8,
            },
            IpNetwork {
                address: "2001:db8:10::".parse().unwrap(),
                prefix: 48,
            },
        ]
    }

    fn advertised() -> AdvertisedAddresses {
        AdvertisedAddresses {
            ipv4: Some("10.0.0.1".parse().unwrap()),
            ipv6: Some("2001:db8:10::1".parse().unwrap()),
        }
    }

    fn policy<'a>(
        nat: NatMode,
        networks: &'a [IpNetwork],
        advertised: &'a AdvertisedAddresses,
        external: ResolvedExternalAddresses,
    ) -> AddressSelectionPolicy<'a> {
        AddressSelectionPolicy {
            nat,
            local_networks: networks,
            advertised,
            external,
        }
    }

    #[test]
    fn forced_nat_modes_override_topology_and_auto_states_redetect() {
        let networks = networks();
        let advertised = advertised();
        let local = "10.1.2.3".parse().unwrap();
        let remote = "198.51.100.20".parse().unwrap();
        assert!(
            !policy(NatMode::Off, &networks, &advertised, Default::default())
                .nat_decision(remote, Some(local))
                .active
        );
        assert!(
            policy(NatMode::On, &networks, &advertised, Default::default())
                .nat_decision(local, Some(local))
                .active
        );
        for mode in [NatMode::Auto, NatMode::AutoOff, NatMode::AutoOn] {
            assert!(
                policy(mode, &networks, &advertised, Default::default())
                    .nat_decision(remote, Some(remote))
                    .active
            );
        }
    }

    #[test]
    fn automatic_nat_detects_reported_address_mismatch() {
        let networks = networks();
        let advertised = advertised();
        let decision = policy(NatMode::Auto, &networks, &advertised, Default::default())
            .nat_decision(
                "10.0.0.20".parse().unwrap(),
                Some("192.168.1.20".parse().unwrap()),
            );
        assert_eq!(decision.reason, NatReason::ReportedAddressMismatch);
        assert!(decision.active);
    }

    #[test]
    fn automatic_nat_without_local_networks_uses_address_mismatch_only() {
        let advertised = advertised();
        let selection = policy(
            NatMode::Auto,
            &[],
            &advertised,
            ResolvedExternalAddresses::default(),
        );
        let peer = "198.51.100.20".parse().unwrap();
        assert!(!selection.nat_decision(peer, Some(peer)).active);
        assert!(
            selection
                .nat_decision(peer, Some("192.168.1.20".parse().unwrap()))
                .active
        );
    }

    #[test]
    fn phone_peer_uses_signaling_peer_only_when_nat_is_active() {
        let networks = networks();
        let advertised = advertised();
        let selection = policy(NatMode::Auto, &networks, &advertised, Default::default());
        assert_eq!(
            selection
                .phone_peer(
                    "198.51.100.20".parse().unwrap(),
                    "192.168.1.20".parse().unwrap(),
                    Some("192.168.1.20".parse().unwrap()),
                )
                .unwrap()
                .0,
            "198.51.100.20".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            selection
                .phone_peer(
                    "10.0.0.20".parse().unwrap(),
                    "10.0.0.20".parse().unwrap(),
                    Some("10.0.0.20".parse().unwrap()),
                )
                .unwrap()
                .0,
            "10.0.0.20".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn advertised_media_prefers_external_only_for_nat_active_station() {
        let networks = networks();
        let advertised = advertised();
        let external = ResolvedExternalAddresses::from_address("203.0.113.10".parse().unwrap());
        let selection = policy(NatMode::Auto, &networks, &advertised, external);
        assert_eq!(
            selection
                .advertised_media(
                    "10.0.0.5".parse().unwrap(),
                    "198.51.100.20".parse().unwrap(),
                    Some("192.168.1.20".parse().unwrap())
                )
                .unwrap()
                .0,
            "203.0.113.10".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            selection
                .advertised_media(
                    "10.0.0.5".parse().unwrap(),
                    "10.0.0.20".parse().unwrap(),
                    Some("10.0.0.20".parse().unwrap())
                )
                .unwrap()
                .0,
            "10.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn loopback_default_never_replaces_a_reachable_native_media_address() {
        let networks = networks();
        let advertised = AdvertisedAddresses::default();
        let selection = policy(
            NatMode::Off,
            &networks,
            &advertised,
            ResolvedExternalAddresses::default(),
        );
        assert_eq!(
            selection
                .advertised_media(
                    "10.0.0.5".parse().unwrap(),
                    "10.0.0.20".parse().unwrap(),
                    Some("10.0.0.20".parse().unwrap()),
                )
                .unwrap()
                .0,
            "10.0.0.5".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn family_mismatch_and_unusable_endpoints_fail_closed() {
        let networks = networks();
        let advertised = AdvertisedAddresses {
            ipv4: None,
            ipv6: None,
        };
        let selection = policy(NatMode::Off, &networks, &advertised, Default::default());
        assert!(matches!(
            selection.advertised_media(
                "2001:db8::10".parse().unwrap(),
                "10.0.0.20".parse().unwrap(),
                None,
            ),
            Err(AddressSelectionError::AddressFamilyMismatch { .. })
        ));
        assert_eq!(
            selection.phone_peer(
                "10.0.0.20".parse().unwrap(),
                "0.0.0.0".parse().unwrap(),
                Some("10.0.0.20".parse().unwrap()),
            ),
            Err(AddressSelectionError::UnusableAddress(
                "0.0.0.0".parse().unwrap()
            ))
        );
        assert_eq!(
            selection.phone_peer(
                "2001:db8::20".parse().unwrap(),
                "fe80::20".parse().unwrap(),
                Some("2001:db8::20".parse().unwrap()),
            ),
            Err(AddressSelectionError::UnusableAddress(
                "fe80::20".parse().unwrap()
            ))
        );
    }

    #[derive(Debug)]
    struct FakeResolver {
        results: Mutex<Vec<io::Result<Vec<IpAddr>>>>,
    }

    impl HostResolver for FakeResolver {
        fn resolve(&self, _hostname: &str) -> io::Result<Vec<IpAddr>> {
            self.results.lock().unwrap().remove(0)
        }
    }

    #[test]
    fn hostname_cache_respects_refresh_and_preserves_last_good_on_failure() {
        let resolver = FakeResolver {
            results: Mutex::new(vec![
                Ok(vec![
                    "203.0.113.10".parse().unwrap(),
                    "2001:db8::10".parse().unwrap(),
                ]),
                Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
            ]),
        };
        let mut cache = ExternalAddressCache::new(resolver);
        let start = Instant::now();
        let configured = ExternalAddress::Hostname {
            name: "pbx.example.test".into(),
            refresh_seconds: 60,
        };
        let first = cache.refresh(Some(&configured), start).unwrap();
        assert_eq!(first.ipv4, Some("203.0.113.10".parse().unwrap()));
        assert_eq!(
            cache
                .refresh(Some(&configured), start + Duration::from_secs(59))
                .unwrap(),
            first
        );
        assert!(
            cache
                .refresh(Some(&configured), start + Duration::from_secs(60))
                .is_err()
        );
        assert_eq!(cache.current(), first);
    }

    #[test]
    fn address_and_clear_configuration_replace_hostname_cache() {
        let resolver = FakeResolver {
            results: Mutex::new(Vec::new()),
        };
        let mut cache = ExternalAddressCache::new(resolver);
        let start = Instant::now();
        assert_eq!(
            cache
                .refresh(
                    Some(&ExternalAddress::Address("203.0.113.20".parse().unwrap())),
                    start
                )
                .unwrap()
                .ipv4,
            Some("203.0.113.20".parse().unwrap())
        );
        assert_eq!(
            cache.refresh(None, start).unwrap(),
            ResolvedExternalAddresses::default()
        );
    }

    #[test]
    fn socket_address_family_is_preserved_by_policy_inputs() {
        let ipv4: SocketAddr = "192.0.2.10:2000".parse().unwrap();
        let ipv6: SocketAddr = "[2001:db8::10]:2000".parse().unwrap();
        assert_eq!(AddressFamily::of(ipv4.ip()), AddressFamily::Ipv4);
        assert_eq!(AddressFamily::of(ipv6.ip()), AddressFamily::Ipv6);
        assert_eq!(
            AddressFamily::of("::ffff:192.0.2.10".parse().unwrap()),
            AddressFamily::Ipv4
        );
    }
}
