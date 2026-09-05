//! Direct-audio routing and explicit media-anchor ownership.
//!
//! [`DirectMediaPolicy`] fails closed: direct RTP requires enabled policy,
//! usable same-family endpoints in matching configured network scope, no active
//! NAT, no forced jitter buffer, and exact selected-codec compatibility.
//! [`MediaAnchorRegistry`] reference-counts independent recording and
//! announcement reasons. Announcement leases span the bounded playback window
//! and are released by completion or cancellation, preventing direct media from
//! re-entering while PBX audio is being generated.

use std::collections::HashMap;
use std::net::IpAddr;
#[cfg(any(feature = "asterisk-22", feature = "asterisk-latest", test))]
use std::time::Duration;

use sccp_protocol::MediaEndpoint;
use thiserror::Error;

use crate::config::{IpNetwork, NatMode};
use crate::media::addressing::canonical_ip_address;
use crate::runtime::backend::PbxCallId;

/// Policy inputs that must all be true before an RTP peer may bypass the
/// driver-owned Asterisk RTP instance.
#[derive(Clone, Copy, Debug)]
pub struct DirectMediaPolicy<'a> {
    pub enabled: bool,
    pub forced_jitter_buffer: bool,
    pub nat: NatMode,
    pub local_networks: &'a [IpNetwork],
}

impl DirectMediaPolicy<'_> {
    pub fn route(
        self,
        phone: IpAddr,
        peer: Option<IpAddr>,
        nat_active: bool,
        peer_supports_codec: bool,
    ) -> DirectMediaRoute {
        let Some(peer) = peer else {
            return DirectMediaRoute::Anchored(DirectMediaRejection::InvalidEndpoint);
        };
        match self.validate(phone, peer, nat_active, peer_supports_codec) {
            Ok(()) => DirectMediaRoute::Direct,
            Err(reason) => DirectMediaRoute::Anchored(reason),
        }
    }

    pub fn validate(
        self,
        phone: IpAddr,
        peer: IpAddr,
        nat_active: bool,
        peer_supports_codec: bool,
    ) -> Result<(), DirectMediaRejection> {
        let phone = canonical_ip_address(phone);
        let peer = canonical_ip_address(peer);
        if !self.enabled {
            return Err(DirectMediaRejection::Disabled);
        }
        if self.forced_jitter_buffer {
            return Err(DirectMediaRejection::JitterBuffer);
        }
        if nat_active || self.nat == NatMode::On {
            return Err(DirectMediaRejection::Nat);
        }
        if !usable_unicast(phone) || !usable_unicast(peer) {
            return Err(DirectMediaRejection::InvalidEndpoint);
        }
        if !same_address_family(phone, peer) {
            return Err(DirectMediaRejection::AddressFamily);
        }
        if self.is_local(phone) != self.is_local(peer) {
            return Err(DirectMediaRejection::Topology);
        }
        if !peer_supports_codec {
            return Err(DirectMediaRejection::Codec);
        }
        Ok(())
    }

    fn is_local(self, address: IpAddr) -> bool {
        self.local_networks
            .iter()
            .any(|network| network_contains(network, address))
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DirectMediaRejection {
    #[error("direct media is disabled")]
    Disabled,
    #[error("direct media is unavailable with a forced jitter buffer")]
    JitterBuffer,
    #[error("direct media is unavailable across NAT")]
    Nat,
    #[error("direct media endpoint is not usable unicast")]
    InvalidEndpoint,
    #[error("direct media endpoints use different address families")]
    AddressFamily,
    #[error("direct media endpoints are in incompatible network scopes")]
    Topology,
    #[error("direct media peer does not support the selected codec")]
    Codec,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectMediaRoute {
    Direct,
    Anchored(DirectMediaRejection),
}

pub fn direct_failure_anchor(
    failed: MediaEndpoint,
    local_anchor: Option<MediaEndpoint>,
    anchoring_required: bool,
) -> Option<MediaEndpoint> {
    let anchor = local_anchor?;
    if anchoring_required
        || (failed.address == anchor.address
            && failed.rtp_port == anchor.rtp_port
            && failed.codec == anchor.codec)
    {
        None
    } else {
        Some(anchor)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MediaAnchorReason {
    Recording,
    Announcement,
}

/// Duration of the driver-owned conference tone playback window.
#[cfg(any(feature = "asterisk-22", feature = "asterisk-latest", test))]
pub(crate) const CONFERENCE_ANNOUNCEMENT_PLAYBACK_WINDOW: Duration = Duration::from_millis(750);

/// Reference-counted reasons that require a call to stay on the PBX-owned RTP
/// path. Independent services may acquire the same reason concurrently.
#[derive(Debug, Default)]
pub struct MediaAnchorRegistry {
    counts: HashMap<(PbxCallId, MediaAnchorReason), usize>,
}

#[derive(Debug)]
pub struct MediaAnchorRestores<C> {
    calls: HashMap<PbxCallId, C>,
}

impl<C> Default for MediaAnchorRestores<C> {
    fn default() -> Self {
        Self {
            calls: HashMap::new(),
        }
    }
}

impl<C> MediaAnchorRestores<C> {
    pub fn remember(&mut self, call_id: PbxCallId, call: C) {
        self.calls.entry(call_id).or_insert(call);
    }

    pub fn get(&self, call_id: PbxCallId) -> Option<&C> {
        self.calls.get(&call_id)
    }

    pub fn remove_call(&mut self, call_id: PbxCallId) -> Option<C> {
        self.calls.remove(&call_id)
    }
}

impl MediaAnchorRegistry {
    pub fn acquire(&mut self, call_id: PbxCallId, reason: MediaAnchorReason) {
        *self.counts.entry((call_id, reason)).or_default() += 1;
    }

    pub fn release(&mut self, call_id: PbxCallId, reason: MediaAnchorReason) -> bool {
        let key = (call_id, reason);
        let Some(count) = self.counts.get_mut(&key) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.counts.remove(&key);
        }
        true
    }

    pub fn is_anchored(&self, call_id: PbxCallId) -> bool {
        self.counts
            .keys()
            .any(|(candidate, _)| *candidate == call_id)
    }

    pub fn is_anchored_for_other_reason(
        &self,
        call_id: PbxCallId,
        excluded: MediaAnchorReason,
    ) -> bool {
        self.counts.iter().any(|((candidate, reason), count)| {
            *candidate == call_id && (*reason != excluded || *count > 1)
        })
    }

    pub fn remove_call(&mut self, call_id: PbxCallId) {
        self.counts
            .retain(|(candidate, _), _| *candidate != call_id);
    }
}

fn same_address_family(left: IpAddr, right: IpAddr) -> bool {
    matches!(
        (left, right),
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
    )
}

fn usable_unicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_unspecified()
                && !address.is_broadcast()
                && !address.is_multicast()
                && !address.is_loopback()
                && !address.is_link_local()
        }
        IpAddr::V6(address) => {
            !address.is_unspecified()
                && !address.is_multicast()
                && !address.is_loopback()
                && !address.is_unicast_link_local()
        }
    }
}

fn network_contains(network: &IpNetwork, address: IpAddr) -> bool {
    match (network.address, address) {
        (IpAddr::V4(network_address), IpAddr::V4(address)) if network.prefix <= 32 => {
            let prefix = u32::from(network_address);
            let address = u32::from(address);
            let mask = if network.prefix == 0 {
                0
            } else {
                u32::MAX << (32 - network.prefix)
            };
            prefix & mask == address & mask
        }
        (IpAddr::V6(network_address), IpAddr::V6(address)) if network.prefix <= 128 => {
            let prefix = u128::from(network_address);
            let address = u128::from(address);
            let mask = if network.prefix == 0 {
                0
            } else {
                u128::MAX << (128 - network.prefix)
            };
            prefix & mask == address & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn networks() -> Vec<IpNetwork> {
        vec![
            IpNetwork {
                address: "10.0.0.0".parse().unwrap(),
                prefix: 8,
            },
            IpNetwork {
                address: "2001:db8:1::".parse().unwrap(),
                prefix: 48,
            },
        ]
    }

    fn policy(networks: &[IpNetwork]) -> DirectMediaPolicy<'_> {
        DirectMediaPolicy {
            enabled: true,
            forced_jitter_buffer: false,
            nat: NatMode::Auto,
            local_networks: networks,
        }
    }

    #[test]
    fn allows_same_scope_exact_codec_for_ipv4_and_ipv6() {
        let networks = networks();
        for (phone, peer) in [
            ("10.1.1.10", "10.2.2.20"),
            ("::ffff:10.1.1.10", "10.2.2.20"),
            ("198.51.100.10", "203.0.113.20"),
            ("2001:db8:1::10", "2001:db8:1::20"),
        ] {
            assert_eq!(
                policy(&networks).validate(
                    phone.parse().unwrap(),
                    peer.parse().unwrap(),
                    false,
                    true,
                ),
                Ok(())
            );
        }
    }

    #[test]
    fn rejects_disabled_nat_mixed_scope_family_and_codec() {
        let networks = networks();
        let mut policy = policy(&networks);
        policy.enabled = false;
        assert_eq!(
            policy.validate(
                "10.1.1.10".parse().unwrap(),
                "10.2.2.20".parse().unwrap(),
                false,
                true,
            ),
            Err(DirectMediaRejection::Disabled)
        );
        policy.enabled = true;
        policy.forced_jitter_buffer = true;
        assert_eq!(
            policy.validate(
                "10.1.1.10".parse().unwrap(),
                "10.2.2.20".parse().unwrap(),
                false,
                true,
            ),
            Err(DirectMediaRejection::JitterBuffer)
        );
        policy.forced_jitter_buffer = false;
        assert_eq!(
            policy.validate(
                "10.1.1.10".parse().unwrap(),
                "10.2.2.20".parse().unwrap(),
                true,
                true,
            ),
            Err(DirectMediaRejection::Nat)
        );
        assert_eq!(
            policy.validate(
                "10.1.1.10".parse().unwrap(),
                "203.0.113.20".parse().unwrap(),
                false,
                true,
            ),
            Err(DirectMediaRejection::Topology)
        );
        assert_eq!(
            policy.validate(
                "10.1.1.10".parse().unwrap(),
                "2001:db8:1::20".parse().unwrap(),
                false,
                true,
            ),
            Err(DirectMediaRejection::AddressFamily)
        );
        assert_eq!(
            policy.validate(
                "10.1.1.10".parse().unwrap(),
                "10.2.2.20".parse().unwrap(),
                false,
                false,
            ),
            Err(DirectMediaRejection::Codec)
        );
    }

    #[test]
    fn rejects_unusable_endpoints_and_forced_nat_modes() {
        let networks = networks();
        for endpoint in ["0.0.0.0", "127.0.0.1", "169.254.1.1", "224.0.0.1"] {
            assert_eq!(
                policy(&networks).validate(
                    endpoint.parse().unwrap(),
                    "10.2.2.20".parse().unwrap(),
                    false,
                    true,
                ),
                Err(DirectMediaRejection::InvalidEndpoint)
            );
        }
        assert_eq!(
            DirectMediaPolicy {
                enabled: true,
                forced_jitter_buffer: false,
                nat: NatMode::On,
                local_networks: &networks,
            }
            .validate(
                "10.1.1.10".parse().unwrap(),
                "10.2.2.20".parse().unwrap(),
                false,
                true,
            ),
            Err(DirectMediaRejection::Nat)
        );
        assert_eq!(
            DirectMediaPolicy {
                enabled: true,
                forced_jitter_buffer: false,
                nat: NatMode::AutoOn,
                local_networks: &networks,
            }
            .validate(
                "10.1.1.10".parse().unwrap(),
                "10.2.2.20".parse().unwrap(),
                true,
                true,
            ),
            Err(DirectMediaRejection::Nat)
        );
    }

    #[test]
    fn every_direct_rejection_selects_anchored_media() {
        let networks = networks();
        let policy = policy(&networks);
        for (peer, nat_active, codec, expected) in [
            (None, false, true, DirectMediaRejection::InvalidEndpoint),
            (
                Some("203.0.113.20".parse().unwrap()),
                false,
                true,
                DirectMediaRejection::Topology,
            ),
            (
                Some("10.2.2.20".parse().unwrap()),
                true,
                true,
                DirectMediaRejection::Nat,
            ),
            (
                Some("10.2.2.20".parse().unwrap()),
                false,
                false,
                DirectMediaRejection::Codec,
            ),
        ] {
            assert_eq!(
                policy.route("10.1.1.10".parse().unwrap(), peer, nat_active, codec,),
                DirectMediaRoute::Anchored(expected),
            );
        }
        assert_eq!(
            policy.route(
                "10.1.1.10".parse().unwrap(),
                Some("10.2.2.20".parse().unwrap()),
                false,
                true,
            ),
            DirectMediaRoute::Direct,
        );
    }

    #[test]
    fn independent_anchor_reasons_are_reference_counted_and_call_scoped() {
        let call = PbxCallId(7);
        let other = PbxCallId(8);
        let mut anchors = MediaAnchorRegistry::default();
        anchors.acquire(call, MediaAnchorReason::Recording);
        anchors.acquire(call, MediaAnchorReason::Recording);
        anchors.acquire(call, MediaAnchorReason::Announcement);
        anchors.acquire(other, MediaAnchorReason::Recording);
        assert!(anchors.is_anchored(call));
        assert!(anchors.is_anchored(other));
        assert!(anchors.is_anchored_for_other_reason(call, MediaAnchorReason::Announcement));
        assert!(!anchors.is_anchored_for_other_reason(other, MediaAnchorReason::Recording));

        assert!(anchors.release(call, MediaAnchorReason::Recording));
        assert!(anchors.release(call, MediaAnchorReason::Recording));
        assert!(anchors.is_anchored(call));
        assert!(anchors.release(call, MediaAnchorReason::Announcement));
        assert!(!anchors.is_anchored(call));
        assert!(!anchors.release(call, MediaAnchorReason::Announcement));

        anchors.remove_call(other);
        assert!(!anchors.is_anchored(other));
    }

    #[test]
    fn direct_restore_plan_survives_until_the_final_anchor_reason_releases() {
        let call = PbxCallId(7);
        let mut anchors = MediaAnchorRegistry::default();
        let mut restores = MediaAnchorRestores::default();
        restores.remember(call, "direct");
        anchors.acquire(call, MediaAnchorReason::Announcement);
        anchors.acquire(call, MediaAnchorReason::Recording);

        assert!(anchors.release(call, MediaAnchorReason::Announcement));
        assert!(anchors.is_anchored(call));
        assert_eq!(restores.get(call), Some(&"direct"));

        assert!(anchors.release(call, MediaAnchorReason::Recording));
        assert!(!anchors.is_anchored(call));
        assert_eq!(restores.remove_call(call), Some("direct"));
        assert_eq!(restores.get(call), None);

        let call = PbxCallId(8);
        restores.remember(call, "direct");
        anchors.acquire(call, MediaAnchorReason::Recording);
        anchors.acquire(call, MediaAnchorReason::Announcement);

        assert!(anchors.release(call, MediaAnchorReason::Recording));
        assert!(anchors.is_anchored(call));
        assert_eq!(restores.get(call), Some(&"direct"));

        assert!(anchors.release(call, MediaAnchorReason::Announcement));
        assert!(!anchors.is_anchored(call));
        assert_eq!(restores.remove_call(call), Some("direct"));
    }

    #[test]
    fn conference_announcement_has_a_bounded_playback_window() {
        assert_eq!(
            CONFERENCE_ANNOUNCEMENT_PLAYBACK_WINDOW,
            Duration::from_millis(750)
        );
    }

    #[test]
    fn only_a_failed_direct_route_gets_one_local_anchor_retry() {
        let endpoint = |address: &str, port| MediaEndpoint {
            address: address.parse().unwrap(),
            rtp_port: port,
            rtcp_port: port + 1,
            codec: sccp_protocol::Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 0,
        };
        let direct = endpoint("198.51.100.20", 20_000);
        let anchor = endpoint("192.0.2.10", 10_000);

        assert_eq!(
            direct_failure_anchor(direct, Some(anchor), false),
            Some(anchor)
        );
        assert_eq!(direct_failure_anchor(anchor, Some(anchor), false), None);
        assert_eq!(direct_failure_anchor(direct, Some(anchor), true), None);
        assert_eq!(direct_failure_anchor(direct, None, false), None);
    }
}
