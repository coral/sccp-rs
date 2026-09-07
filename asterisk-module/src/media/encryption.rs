//! Secret-free media-encryption policy negotiation.
//!
//! This module decides whether a media leg must remain clear or may use one
//! mutually supported encryption profile. It does not create, retain, or
//! install keying material. Capability discovery keeps “not reported” separate
//! from an explicit lack of support so required policy cannot silently
//! downgrade.

use std::fmt;
use std::str::FromStr;

use sccp_protocol::EncryptionMethod;
use thiserror::Error;

fn ordered_unique<T: Eq>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    values.into_iter().fold(Vec::new(), |mut ordered, value| {
        if !ordered.contains(&value) {
            ordered.push(value);
        }
        ordered
    })
}

/// Whether a media leg forbids, permits, or requires encryption.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MediaEncryptionRequirement {
    #[default]
    Off,
    Optional,
    Required,
}

/// One validated algorithm and master-key size pair.
///
/// This value contains negotiation metadata only. Keying material belongs to
/// the stream-establishment layer and must not be added here.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MediaEncryptionProfile {
    algorithm: EncryptionMethod,
    master_key_bits: u16,
}

impl MediaEncryptionProfile {
    pub const AES_128_HMAC_SHA1_32: Self = Self::known(EncryptionMethod::Aes128HmacSha1_32, 128);
    pub const AES_128_HMAC_SHA1_80: Self = Self::known(EncryptionMethod::Aes128HmacSha1_80, 128);
    pub const F8_128_HMAC_SHA1_32: Self = Self::known(EncryptionMethod::F8_128HmacSha1_32, 128);
    pub const F8_128_HMAC_SHA1_80: Self = Self::known(EncryptionMethod::F8_128HmacSha1_80, 128);
    pub const AEAD_AES_128_GCM: Self = Self::known(EncryptionMethod::AeadAes128Gcm, 128);
    pub const AEAD_AES_256_GCM: Self = Self::known(EncryptionMethod::AeadAes256Gcm, 256);

    const fn known(algorithm: EncryptionMethod, master_key_bits: u16) -> Self {
        Self {
            algorithm,
            master_key_bits,
        }
    }

    /// Validates a protocol algorithm and its advertised key size.
    pub const fn new(
        algorithm: EncryptionMethod,
        master_key_bits: u16,
    ) -> Result<Self, MediaEncryptionProfileError> {
        let expected_bits = match algorithm {
            EncryptionMethod::Aes128HmacSha1_32
            | EncryptionMethod::Aes128HmacSha1_80
            | EncryptionMethod::F8_128HmacSha1_32
            | EncryptionMethod::F8_128HmacSha1_80
            | EncryptionMethod::AeadAes128Gcm => 128,
            EncryptionMethod::AeadAes256Gcm => 256,
            EncryptionMethod::None => return Err(MediaEncryptionProfileError::ClearAlgorithm),
            EncryptionMethod::Unknown(_) => {
                return Err(MediaEncryptionProfileError::UnknownAlgorithm);
            }
        };
        if master_key_bits != expected_bits {
            return Err(MediaEncryptionProfileError::InvalidKeySize);
        }
        Ok(Self::known(algorithm, master_key_bits))
    }

    pub const fn algorithm(self) -> EncryptionMethod {
        self.algorithm
    }

    pub const fn master_key_bits(self) -> u16 {
        self.master_key_bits
    }
}

impl fmt::Display for MediaEncryptionProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match *self {
            Self::AES_128_HMAC_SHA1_32 => "aes-128-hmac-sha1-32",
            Self::AES_128_HMAC_SHA1_80 => "aes-128-hmac-sha1-80",
            Self::F8_128_HMAC_SHA1_32 => "f8-128-hmac-sha1-32",
            Self::F8_128_HMAC_SHA1_80 => "f8-128-hmac-sha1-80",
            Self::AEAD_AES_128_GCM => "aead-aes-128-gcm",
            Self::AEAD_AES_256_GCM => "aead-aes-256-gcm",
            _ => return Err(fmt::Error),
        })
    }
}

impl FromStr for MediaEncryptionProfile {
    type Err = MediaEncryptionProfileError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "aes-128-hmac-sha1-32" => Ok(Self::AES_128_HMAC_SHA1_32),
            "aes-128-hmac-sha1-80" => Ok(Self::AES_128_HMAC_SHA1_80),
            "f8-128-hmac-sha1-32" => Ok(Self::F8_128_HMAC_SHA1_32),
            "f8-128-hmac-sha1-80" => Ok(Self::F8_128_HMAC_SHA1_80),
            "aead-aes-128-gcm" => Ok(Self::AEAD_AES_128_GCM),
            "aead-aes-256-gcm" => Ok(Self::AEAD_AES_256_GCM),
            _ => Err(MediaEncryptionProfileError::UnknownAlgorithm),
        }
    }
}

/// A station-provided algorithm and key size before validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdvertisedEncryptionProfile {
    pub algorithm: EncryptionMethod,
    pub master_key_bits: u16,
}

impl TryFrom<AdvertisedEncryptionProfile> for MediaEncryptionProfile {
    type Error = MediaEncryptionProfileError;

    fn try_from(value: AdvertisedEncryptionProfile) -> Result<Self, Self::Error> {
        Self::new(value.algorithm, value.master_key_bits)
    }
}

impl From<MediaEncryptionProfile> for AdvertisedEncryptionProfile {
    fn from(value: MediaEncryptionProfile) -> Self {
        Self {
            algorithm: value.algorithm,
            master_key_bits: value.master_key_bits,
        }
    }
}

/// The station's audio-encryption advertisement state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum StationEncryptionCapabilities {
    /// The station did not provide an audio algorithm advertisement.
    #[default]
    NotReported,
    NotCapable,
    Supported(Vec<AdvertisedEncryptionProfile>),
}

/// Encryption profiles that the local media and wire adapters can represent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalEncryptionCapabilities {
    profiles: Vec<MediaEncryptionProfile>,
}

impl LocalEncryptionCapabilities {
    /// Builds a local support set while retaining its deterministic order.
    pub fn new(profiles: impl IntoIterator<Item = MediaEncryptionProfile>) -> Self {
        Self {
            profiles: ordered_unique(profiles),
        }
    }

    /// Profiles whose master keys fit the current 16-byte wire container.
    pub fn wire_representable() -> Self {
        Self::new([
            MediaEncryptionProfile::AES_128_HMAC_SHA1_32,
            MediaEncryptionProfile::AES_128_HMAC_SHA1_80,
            MediaEncryptionProfile::F8_128_HMAC_SHA1_32,
            MediaEncryptionProfile::F8_128_HMAC_SHA1_80,
            MediaEncryptionProfile::AEAD_AES_128_GCM,
        ])
    }

    pub fn profiles(&self) -> &[MediaEncryptionProfile] {
        &self.profiles
    }
}

/// Inputs to one media-encryption negotiation decision.
///
/// The caller is responsible for resolving these values from one call-owned
/// configuration and station generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioEncryptionAdmission {
    policy: MediaEncryptionPolicy,
    station: StationEncryptionCapabilities,
    local: LocalEncryptionCapabilities,
}

impl AudioEncryptionAdmission {
    pub fn new(
        policy: MediaEncryptionPolicy,
        station: StationEncryptionCapabilities,
        local: LocalEncryptionCapabilities,
    ) -> Self {
        Self {
            policy,
            station,
            local,
        }
    }

    pub fn decide(&self) -> Result<MediaEncryptionDecision, MediaEncryptionNegotiationError> {
        negotiate_media_encryption(&self.policy, &self.station, &self.local)
    }
}

/// Validated configuration in deterministic preference order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaEncryptionPolicy {
    requirement: MediaEncryptionRequirement,
    preferences: Vec<MediaEncryptionProfile>,
}

impl Default for MediaEncryptionPolicy {
    fn default() -> Self {
        Self {
            requirement: MediaEncryptionRequirement::Off,
            preferences: Vec::new(),
        }
    }
}

impl MediaEncryptionPolicy {
    /// Builds a policy and removes repeated profiles without changing order.
    pub fn new(
        requirement: MediaEncryptionRequirement,
        preferences: impl IntoIterator<Item = MediaEncryptionProfile>,
    ) -> Result<Self, MediaEncryptionPolicyError> {
        let preferences = ordered_unique(preferences);
        match (requirement, preferences.is_empty()) {
            (MediaEncryptionRequirement::Off, false) => {
                Err(MediaEncryptionPolicyError::ProfilesWhileOff)
            }
            (MediaEncryptionRequirement::Optional | MediaEncryptionRequirement::Required, true) => {
                Err(MediaEncryptionPolicyError::NoProfiles)
            }
            _ => Ok(Self {
                requirement,
                preferences,
            }),
        }
    }

    pub const fn requirement(&self) -> MediaEncryptionRequirement {
        self.requirement
    }

    pub fn preferences(&self) -> &[MediaEncryptionProfile] {
        &self.preferences
    }
}

/// Secret-free result consumed by later stream establishment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaEncryptionDecision {
    Clear,
    Encrypted(MediaEncryptionProfile),
}

/// Applies configured preference order to a station advertisement.
pub fn negotiate_media_encryption(
    policy: &MediaEncryptionPolicy,
    station: &StationEncryptionCapabilities,
    local: &LocalEncryptionCapabilities,
) -> Result<MediaEncryptionDecision, MediaEncryptionNegotiationError> {
    if policy.requirement == MediaEncryptionRequirement::Off {
        return Ok(MediaEncryptionDecision::Clear);
    }

    let advertised = match station {
        StationEncryptionCapabilities::NotReported => {
            return unavailable_decision(policy.requirement, CapabilityUnavailable::NotReported);
        }
        StationEncryptionCapabilities::NotCapable => {
            return unavailable_decision(policy.requirement, CapabilityUnavailable::NotCapable);
        }
        StationEncryptionCapabilities::Supported(advertised) => advertised,
    };
    let supported = advertised
        .iter()
        .copied()
        .map(MediaEncryptionProfile::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    if !policy
        .preferences
        .iter()
        .any(|profile| local.profiles.contains(profile))
    {
        return unavailable_decision(
            policy.requirement,
            CapabilityUnavailable::NoLocallySupportedProfile,
        );
    }
    if let Some(selected) = policy
        .preferences
        .iter()
        .copied()
        .filter(|profile| local.profiles.contains(profile))
        .find(|profile| supported.contains(profile))
    {
        return Ok(MediaEncryptionDecision::Encrypted(selected));
    }
    unavailable_decision(policy.requirement, CapabilityUnavailable::NoMutualProfile)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapabilityUnavailable {
    NotReported,
    NotCapable,
    NoLocallySupportedProfile,
    NoMutualProfile,
}

fn unavailable_decision(
    requirement: MediaEncryptionRequirement,
    unavailable: CapabilityUnavailable,
) -> Result<MediaEncryptionDecision, MediaEncryptionNegotiationError> {
    match requirement {
        MediaEncryptionRequirement::Optional => Ok(MediaEncryptionDecision::Clear),
        MediaEncryptionRequirement::Required => Err(match unavailable {
            CapabilityUnavailable::NotReported => {
                MediaEncryptionNegotiationError::CapabilitiesNotReported
            }
            CapabilityUnavailable::NotCapable => MediaEncryptionNegotiationError::StationNotCapable,
            CapabilityUnavailable::NoLocallySupportedProfile => {
                MediaEncryptionNegotiationError::NoLocallySupportedProfile
            }
            CapabilityUnavailable::NoMutualProfile => {
                MediaEncryptionNegotiationError::NoMutualProfile
            }
        }),
        MediaEncryptionRequirement::Off => Ok(MediaEncryptionDecision::Clear),
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MediaEncryptionProfileError {
    #[error("clear media is not an encryption profile")]
    ClearAlgorithm,
    #[error("the media-encryption algorithm is unknown")]
    UnknownAlgorithm,
    #[error("the media-encryption key size is invalid for its algorithm")]
    InvalidKeySize,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MediaEncryptionPolicyError {
    #[error("media-encryption profiles cannot be configured while encryption is off")]
    ProfilesWhileOff,
    #[error("optional or required media encryption needs at least one allowed profile")]
    NoProfiles,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MediaEncryptionNegotiationError {
    #[error("the station did not report audio-encryption capabilities")]
    CapabilitiesNotReported,
    #[error("the station reported that audio encryption is unavailable")]
    StationNotCapable,
    #[error("configured policy and station capabilities have no mutual encryption profile")]
    NoMutualProfile,
    #[error("configured policy has no encryption profile supported by the local media adapter")]
    NoLocallySupportedProfile,
    #[error(transparent)]
    InvalidAdvertisement(#[from] MediaEncryptionProfileError),
}

#[cfg(test)]
mod tests {

    use super::*;

    fn policy(
        requirement: MediaEncryptionRequirement,
        profiles: &[MediaEncryptionProfile],
    ) -> MediaEncryptionPolicy {
        MediaEncryptionPolicy::new(requirement, profiles.iter().copied()).unwrap()
    }

    fn supported(profiles: &[MediaEncryptionProfile]) -> StationEncryptionCapabilities {
        StationEncryptionCapabilities::Supported(profiles.iter().copied().map(Into::into).collect())
    }

    fn local() -> LocalEncryptionCapabilities {
        LocalEncryptionCapabilities::wire_representable()
    }

    #[test]
    fn known_algorithms_accept_only_their_defined_master_key_size() {
        for profile in [
            MediaEncryptionProfile::AES_128_HMAC_SHA1_32,
            MediaEncryptionProfile::AES_128_HMAC_SHA1_80,
            MediaEncryptionProfile::F8_128_HMAC_SHA1_32,
            MediaEncryptionProfile::F8_128_HMAC_SHA1_80,
            MediaEncryptionProfile::AEAD_AES_128_GCM,
            MediaEncryptionProfile::AEAD_AES_256_GCM,
        ] {
            assert_eq!(
                MediaEncryptionProfile::new(profile.algorithm(), profile.master_key_bits()),
                Ok(profile)
            );
            assert_eq!(
                MediaEncryptionProfile::new(
                    profile.algorithm(),
                    profile.master_key_bits().saturating_sub(1),
                ),
                Err(MediaEncryptionProfileError::InvalidKeySize)
            );
        }
        assert_eq!(
            MediaEncryptionProfile::new(EncryptionMethod::None, 0),
            Err(MediaEncryptionProfileError::ClearAlgorithm)
        );
        assert_eq!(
            MediaEncryptionProfile::new(EncryptionMethod::Unknown(99), 128),
            Err(MediaEncryptionProfileError::UnknownAlgorithm)
        );
    }

    #[test]
    fn policy_is_explicit_and_normalizes_repeated_preferences() {
        assert_eq!(
            MediaEncryptionPolicy::default().requirement(),
            MediaEncryptionRequirement::Off
        );
        assert_eq!(
            MediaEncryptionPolicy::new(
                MediaEncryptionRequirement::Off,
                [MediaEncryptionProfile::AES_128_HMAC_SHA1_80]
            ),
            Err(MediaEncryptionPolicyError::ProfilesWhileOff)
        );
        assert_eq!(
            MediaEncryptionPolicy::new(MediaEncryptionRequirement::Required, []),
            Err(MediaEncryptionPolicyError::NoProfiles)
        );
        let policy = MediaEncryptionPolicy::new(
            MediaEncryptionRequirement::Optional,
            [
                MediaEncryptionProfile::AEAD_AES_128_GCM,
                MediaEncryptionProfile::AES_128_HMAC_SHA1_80,
                MediaEncryptionProfile::AEAD_AES_128_GCM,
            ],
        )
        .unwrap();
        assert_eq!(
            policy.preferences(),
            &[
                MediaEncryptionProfile::AEAD_AES_128_GCM,
                MediaEncryptionProfile::AES_128_HMAC_SHA1_80,
            ]
        );
    }

    #[test]
    fn profile_names_round_trip_without_permissive_aliases() {
        for profile in [
            MediaEncryptionProfile::AES_128_HMAC_SHA1_32,
            MediaEncryptionProfile::AES_128_HMAC_SHA1_80,
            MediaEncryptionProfile::F8_128_HMAC_SHA1_32,
            MediaEncryptionProfile::F8_128_HMAC_SHA1_80,
            MediaEncryptionProfile::AEAD_AES_128_GCM,
            MediaEncryptionProfile::AEAD_AES_256_GCM,
        ] {
            assert_eq!(profile.to_string().parse(), Ok(profile));
        }
        assert_eq!(
            "future-profile".parse::<MediaEncryptionProfile>(),
            Err(MediaEncryptionProfileError::UnknownAlgorithm)
        );
    }

    #[test]
    fn local_wire_capabilities_exclude_profiles_that_need_larger_keys() {
        let local = LocalEncryptionCapabilities::wire_representable();
        assert!(
            local
                .profiles()
                .contains(&MediaEncryptionProfile::AEAD_AES_128_GCM)
        );
        assert!(
            !local
                .profiles()
                .contains(&MediaEncryptionProfile::AEAD_AES_256_GCM)
        );

        let station = supported(&[MediaEncryptionProfile::AEAD_AES_256_GCM]);
        let required = policy(
            MediaEncryptionRequirement::Required,
            &[MediaEncryptionProfile::AEAD_AES_256_GCM],
        );
        assert_eq!(
            negotiate_media_encryption(&required, &station, &local),
            Err(MediaEncryptionNegotiationError::NoLocallySupportedProfile)
        );
        let optional = policy(
            MediaEncryptionRequirement::Optional,
            &[MediaEncryptionProfile::AEAD_AES_256_GCM],
        );
        assert_eq!(
            negotiate_media_encryption(&optional, &station, &local),
            Ok(MediaEncryptionDecision::Clear)
        );
    }

    #[test]
    fn configured_order_wins_over_advertisement_order() {
        let policy = policy(
            MediaEncryptionRequirement::Required,
            &[
                MediaEncryptionProfile::AEAD_AES_128_GCM,
                MediaEncryptionProfile::AES_128_HMAC_SHA1_80,
            ],
        );
        let station = supported(&[
            MediaEncryptionProfile::AES_128_HMAC_SHA1_80,
            MediaEncryptionProfile::AEAD_AES_128_GCM,
        ]);
        assert_eq!(
            negotiate_media_encryption(&policy, &station, &local()),
            Ok(MediaEncryptionDecision::Encrypted(
                MediaEncryptionProfile::AEAD_AES_128_GCM
            ))
        );
    }

    #[test]
    fn optional_policy_stays_clear_when_capabilities_are_unavailable() {
        let policy = policy(
            MediaEncryptionRequirement::Optional,
            &[MediaEncryptionProfile::AES_128_HMAC_SHA1_80],
        );
        for capabilities in [
            StationEncryptionCapabilities::NotReported,
            StationEncryptionCapabilities::NotCapable,
            supported(&[MediaEncryptionProfile::AEAD_AES_128_GCM]),
        ] {
            assert_eq!(
                negotiate_media_encryption(&policy, &capabilities, &local()),
                Ok(MediaEncryptionDecision::Clear)
            );
        }
    }

    #[test]
    fn required_policy_rejects_every_unavailable_state() {
        let policy = policy(
            MediaEncryptionRequirement::Required,
            &[MediaEncryptionProfile::AES_128_HMAC_SHA1_80],
        );
        for (capabilities, expected) in [
            (
                StationEncryptionCapabilities::NotReported,
                MediaEncryptionNegotiationError::CapabilitiesNotReported,
            ),
            (
                StationEncryptionCapabilities::NotCapable,
                MediaEncryptionNegotiationError::StationNotCapable,
            ),
            (
                supported(&[MediaEncryptionProfile::AEAD_AES_128_GCM]),
                MediaEncryptionNegotiationError::NoMutualProfile,
            ),
        ] {
            assert_eq!(
                negotiate_media_encryption(&policy, &capabilities, &local()),
                Err(expected)
            );
        }
    }

    #[test]
    fn malformed_or_unknown_advertisements_fail_before_selection() {
        let policy = policy(
            MediaEncryptionRequirement::Optional,
            &[MediaEncryptionProfile::AES_128_HMAC_SHA1_80],
        );
        for advertised in [
            AdvertisedEncryptionProfile {
                algorithm: EncryptionMethod::Unknown(42),
                master_key_bits: 128,
            },
            AdvertisedEncryptionProfile {
                algorithm: EncryptionMethod::None,
                master_key_bits: 0,
            },
            AdvertisedEncryptionProfile {
                algorithm: EncryptionMethod::Aes128HmacSha1_80,
                master_key_bits: 256,
            },
        ] {
            let capabilities = StationEncryptionCapabilities::Supported(vec![
                MediaEncryptionProfile::AES_128_HMAC_SHA1_80.into(),
                advertised,
            ]);
            assert!(matches!(
                negotiate_media_encryption(&policy, &capabilities, &local()),
                Err(MediaEncryptionNegotiationError::InvalidAdvertisement(_))
            ));
        }
    }

    #[test]
    fn off_policy_never_consults_an_untrusted_advertisement() {
        let capabilities =
            StationEncryptionCapabilities::Supported(vec![AdvertisedEncryptionProfile {
                algorithm: EncryptionMethod::Unknown(u32::MAX),
                master_key_bits: u16::MAX,
            }]);
        assert_eq!(
            negotiate_media_encryption(
                &MediaEncryptionPolicy::default(),
                &capabilities,
                &LocalEncryptionCapabilities::default(),
            ),
            Ok(MediaEncryptionDecision::Clear)
        );
    }

    #[test]
    fn negotiation_outputs_and_errors_contain_metadata_only() {
        let decision =
            MediaEncryptionDecision::Encrypted(MediaEncryptionProfile::AES_128_HMAC_SHA1_80);
        let rendered = format!("{decision:?}");
        assert!(rendered.contains("Aes128HmacSha1_80"));
        assert!(!rendered.contains("key:") && !rendered.contains("salt:"));

        let error = negotiate_media_encryption(
            &policy(
                MediaEncryptionRequirement::Required,
                &[MediaEncryptionProfile::AES_128_HMAC_SHA1_80],
            ),
            &StationEncryptionCapabilities::NotReported,
            &local(),
        )
        .unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("key:") && !rendered.contains("salt:"));
    }

    #[test]
    fn admission_snapshot_keeps_required_rejection_and_clear_fallback_deterministic() {
        let required = AudioEncryptionAdmission::new(
            policy(
                MediaEncryptionRequirement::Required,
                &[MediaEncryptionProfile::AES_128_HMAC_SHA1_80],
            ),
            StationEncryptionCapabilities::NotReported,
            LocalEncryptionCapabilities::default(),
        );
        let before = required.clone();
        assert_eq!(
            required.decide(),
            Err(MediaEncryptionNegotiationError::CapabilitiesNotReported)
        );
        assert_eq!(required, before);

        for requirement in [
            MediaEncryptionRequirement::Off,
            MediaEncryptionRequirement::Optional,
        ] {
            let policy = if requirement == MediaEncryptionRequirement::Off {
                MediaEncryptionPolicy::default()
            } else {
                policy(requirement, &[MediaEncryptionProfile::AES_128_HMAC_SHA1_80])
            };
            let admission = AudioEncryptionAdmission::new(
                policy,
                StationEncryptionCapabilities::NotReported,
                LocalEncryptionCapabilities::default(),
            );
            assert_eq!(admission.decide(), Ok(MediaEncryptionDecision::Clear));
            assert_eq!(admission.decide(), Ok(MediaEncryptionDecision::Clear));
        }
    }
}
