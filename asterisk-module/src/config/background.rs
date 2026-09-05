//! Validates static resources and resolves dynamic patterns from SCCP device types.

use std::fmt;

use sccp_protocol::{CiscoIpPhoneSetBackground, DeviceType, PhoneBackgroundHttpUrl, PhoneXmlError};
use thiserror::Error;

const MAX_DYNAMIC_BACKGROUND_PATTERN_CHARS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundThumbnailSource {
    Explicit,
    Derived,
    Dynamic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceBackgroundSelection {
    Static(DeviceBackground),
    Dynamic(DynamicBackgroundPattern),
}

impl DeviceBackgroundSelection {
    pub const fn is_dynamic(&self) -> bool {
        matches!(self, Self::Dynamic(_))
    }

    pub fn resolve(
        &self,
        device_type: Option<DeviceType>,
    ) -> Result<Option<ResolvedDeviceBackground>, DeviceBackgroundError> {
        match (self, device_type) {
            (Self::Static(background), device_type) => Ok(background.resolve_for(device_type)),
            (Self::Dynamic(_), None) => Ok(None),
            (Self::Dynamic(pattern), Some(device_type)) => pattern.resolve(device_type),
        }
    }
}

impl From<DeviceBackground> for DeviceBackgroundSelection {
    fn from(background: DeviceBackground) -> Self {
        Self::Static(background)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct DynamicBackgroundPattern(String);

impl DynamicBackgroundPattern {
    pub fn new(value: impl Into<String>) -> Result<Self, DeviceBackgroundError> {
        let value = value.into();
        let characters = value.chars().count();
        if characters > MAX_DYNAMIC_BACKGROUND_PATTERN_CHARS {
            return Err(DeviceBackgroundError::DynamicPatternTooLong {
                characters,
                maximum: MAX_DYNAMIC_BACKGROUND_PATTERN_CHARS,
            });
        }
        if !value.contains("{W}") || !value.contains("{H}") {
            return Err(DeviceBackgroundError::MissingDynamicDimensions);
        }
        let pattern = Self(value);
        for profile in BACKGROUND_PROFILES {
            pattern.resolve_profile(profile)?;
        }
        Ok(pattern)
    }

    pub fn resolve(
        &self,
        device_type: DeviceType,
    ) -> Result<Option<ResolvedDeviceBackground>, DeviceBackgroundError> {
        match background_profile(device_type) {
            Some(profile) => self.resolve_profile(profile).map(Some),
            None => Ok(None),
        }
    }

    fn resolve_profile(
        &self,
        profile: BackgroundProfile,
    ) -> Result<ResolvedDeviceBackground, DeviceBackgroundError> {
        match profile {
            BackgroundProfile::Set {
                full,
                thumbnail,
                bit_depth,
                format,
            } => DeviceBackground::validated(
                self.render(full, bit_depth, format)?,
                self.render(thumbnail, bit_depth, format)?,
                BackgroundThumbnailSource::Dynamic,
            )
            .map(ResolvedDeviceBackground::Set),
            BackgroundProfile::Display {
                dimensions,
                bit_depth,
                format,
            } => self
                .render(dimensions, bit_depth, format)
                .map(ResolvedDeviceBackground::Display),
        }
    }

    fn render(
        &self,
        dimensions: BackgroundDimensions,
        bit_depth: u8,
        format: BackgroundFormat,
    ) -> Result<PhoneBackgroundHttpUrl, DeviceBackgroundError> {
        let value = self
            .0
            .replace("{FORMAT}", format.as_str())
            .replace("{W}", &dimensions.width.to_string())
            .replace("{H}", &dimensions.height.to_string())
            .replace("{B}", &bit_depth.to_string());
        if value
            .chars()
            .any(|character| matches!(character, '{' | '}'))
        {
            return Err(DeviceBackgroundError::InvalidDynamicPlaceholder);
        }
        PhoneBackgroundHttpUrl::new(value)
            .map_err(|source| DeviceBackgroundError::InvalidDynamicResource { source })
    }
}

impl fmt::Debug for DynamicBackgroundPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DynamicBackgroundPattern(<redacted>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceBackground {
    image_url: PhoneBackgroundHttpUrl,
    thumbnail_url: PhoneBackgroundHttpUrl,
    thumbnail_source: BackgroundThumbnailSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedDeviceBackground {
    Set(DeviceBackground),
    Display(PhoneBackgroundHttpUrl),
}

impl DeviceBackground {
    pub fn new(
        image_url: PhoneBackgroundHttpUrl,
        thumbnail_url: Option<PhoneBackgroundHttpUrl>,
    ) -> Result<Self, DeviceBackgroundError> {
        let (thumbnail_url, thumbnail_source) = match thumbnail_url {
            Some(url) => (url, BackgroundThumbnailSource::Explicit),
            None => (
                derive_thumbnail_url(&image_url)?,
                BackgroundThumbnailSource::Derived,
            ),
        };
        Self::validated(image_url, thumbnail_url, thumbnail_source)
    }

    fn validated(
        image_url: PhoneBackgroundHttpUrl,
        thumbnail_url: PhoneBackgroundHttpUrl,
        thumbnail_source: BackgroundThumbnailSource,
    ) -> Result<Self, DeviceBackgroundError> {
        let background = Self {
            image_url,
            thumbnail_url,
            thumbnail_source,
        };
        CiscoIpPhoneSetBackground::new(
            background.image_url.clone(),
            background.thumbnail_url.clone(),
        )
        .to_xml()
        .map_err(|source| DeviceBackgroundError::InvalidControlDocument { source })?;
        Ok(background)
    }

    pub const fn image_url(&self) -> &PhoneBackgroundHttpUrl {
        &self.image_url
    }

    pub const fn thumbnail_url(&self) -> &PhoneBackgroundHttpUrl {
        &self.thumbnail_url
    }

    pub const fn thumbnail_source(&self) -> BackgroundThumbnailSource {
        self.thumbnail_source
    }

    pub(crate) fn resolve_for(
        &self,
        device_type: Option<DeviceType>,
    ) -> Option<ResolvedDeviceBackground> {
        match device_type {
            None => Some(ResolvedDeviceBackground::Set(self.clone())),
            Some(device_type) => {
                background_profile(device_type).map(|profile| profile.resolve_static(self.clone()))
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum DeviceBackgroundError {
    #[error("an explicit thumbnail URL is required when the image URL has no filename")]
    MissingImageFilename,
    #[error("the derived thumbnail URL is invalid")]
    InvalidDerivedThumbnail {
        #[source]
        source: PhoneXmlError,
    },
    #[error("the background URL pair does not fit the phone control document")]
    InvalidControlDocument {
        #[source]
        source: PhoneXmlError,
    },
    #[error("a dynamic background pattern must contain both {{W}} and {{H}}")]
    MissingDynamicDimensions,
    #[error("a dynamic background pattern contains an unknown or unmatched placeholder")]
    InvalidDynamicPlaceholder,
    #[error("a dynamic background pattern does not produce valid HTTP or HTTPS URLs")]
    InvalidDynamicResource {
        #[source]
        source: PhoneXmlError,
    },
    #[error("a dynamic background pattern contains {characters} characters; maximum is {maximum}")]
    DynamicPatternTooLong { characters: usize, maximum: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackgroundDimensions {
    width: u16,
    height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackgroundFormat {
    Png,
    Xml,
}

impl BackgroundFormat {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Xml => "xml",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackgroundProfile {
    Set {
        full: BackgroundDimensions,
        thumbnail: BackgroundDimensions,
        bit_depth: u8,
        format: BackgroundFormat,
    },
    Display {
        dimensions: BackgroundDimensions,
        bit_depth: u8,
        format: BackgroundFormat,
    },
}

impl BackgroundProfile {
    fn resolve_static(self, background: DeviceBackground) -> ResolvedDeviceBackground {
        match self {
            Self::Set { .. } => ResolvedDeviceBackground::Set(background),
            Self::Display { .. } => {
                ResolvedDeviceBackground::Display(background.image_url().clone())
            }
        }
    }
}

const BACKGROUND_PROFILE_95_34_1: BackgroundProfile = BackgroundProfile::Set {
    full: BackgroundDimensions {
        width: 95,
        height: 34,
    },
    thumbnail: BackgroundDimensions {
        width: 23,
        height: 8,
    },
    bit_depth: 1,
    format: BackgroundFormat::Png,
};
const BACKGROUND_PROFILE_128_59_1: BackgroundProfile = BackgroundProfile::Display {
    dimensions: BackgroundDimensions {
        width: 128,
        height: 59,
    },
    bit_depth: 1,
    format: BackgroundFormat::Xml,
};
const BACKGROUND_PROFILE_133_65_2: BackgroundProfile = BackgroundProfile::Display {
    dimensions: BackgroundDimensions {
        width: 133,
        height: 65,
    },
    bit_depth: 2,
    format: BackgroundFormat::Xml,
};
const BACKGROUND_PROFILE_320_196_4: BackgroundProfile = BackgroundProfile::Set {
    full: BackgroundDimensions {
        width: 320,
        height: 196,
    },
    thumbnail: BackgroundDimensions {
        width: 80,
        height: 49,
    },
    bit_depth: 4,
    format: BackgroundFormat::Png,
};
const BACKGROUND_PROFILE_320_212_12: BackgroundProfile = BackgroundProfile::Set {
    full: BackgroundDimensions {
        width: 320,
        height: 212,
    },
    thumbnail: BackgroundDimensions {
        width: 80,
        height: 53,
    },
    bit_depth: 12,
    format: BackgroundFormat::Png,
};
const BACKGROUND_PROFILE_320_212_16: BackgroundProfile = BackgroundProfile::Set {
    full: BackgroundDimensions {
        width: 320,
        height: 212,
    },
    thumbnail: BackgroundDimensions {
        width: 80,
        height: 53,
    },
    bit_depth: 16,
    format: BackgroundFormat::Png,
};
const BACKGROUND_PROFILE_320_216_16: BackgroundProfile = BackgroundProfile::Set {
    full: BackgroundDimensions {
        width: 320,
        height: 216,
    },
    thumbnail: BackgroundDimensions {
        width: 80,
        height: 53,
    },
    bit_depth: 16,
    format: BackgroundFormat::Png,
};
const BACKGROUND_PROFILE_800_600_16: BackgroundProfile = BackgroundProfile::Set {
    full: BackgroundDimensions {
        width: 800,
        height: 600,
    },
    thumbnail: BackgroundDimensions {
        width: 800,
        height: 600,
    },
    bit_depth: 16,
    format: BackgroundFormat::Png,
};
const BACKGROUND_PROFILE_640_480_24: BackgroundProfile = BackgroundProfile::Set {
    full: BackgroundDimensions {
        width: 640,
        height: 480,
    },
    thumbnail: BackgroundDimensions {
        width: 123,
        height: 111,
    },
    bit_depth: 24,
    format: BackgroundFormat::Png,
};
const BACKGROUND_PROFILES: [BackgroundProfile; 9] = [
    BACKGROUND_PROFILE_95_34_1,
    BACKGROUND_PROFILE_128_59_1,
    BACKGROUND_PROFILE_133_65_2,
    BACKGROUND_PROFILE_320_196_4,
    BACKGROUND_PROFILE_320_212_12,
    BACKGROUND_PROFILE_320_212_16,
    BACKGROUND_PROFILE_320_216_16,
    BACKGROUND_PROFILE_800_600_16,
    BACKGROUND_PROFILE_640_480_24,
];

fn background_profile(device_type: DeviceType) -> Option<BackgroundProfile> {
    match device_type {
        DeviceType::Cisco7906 | DeviceType::Cisco7911 => Some(BACKGROUND_PROFILE_95_34_1),
        DeviceType::Cisco7920 => Some(BACKGROUND_PROFILE_128_59_1),
        DeviceType::Cisco7940 | DeviceType::Cisco7960 => Some(BACKGROUND_PROFILE_133_65_2),
        DeviceType::Cisco7941
        | DeviceType::Cisco7941Ge
        | DeviceType::Cisco7942
        | DeviceType::Cisco7961
        | DeviceType::Cisco7961Ge
        | DeviceType::Cisco7962 => Some(BACKGROUND_PROFILE_320_196_4),
        DeviceType::Cisco7970 | DeviceType::Cisco7971 | DeviceType::CiscoIpCommunicator => {
            Some(BACKGROUND_PROFILE_320_212_12)
        }
        DeviceType::Cisco7945 | DeviceType::Cisco7965 => Some(BACKGROUND_PROFILE_320_212_16),
        DeviceType::Cisco7975 => Some(BACKGROUND_PROFILE_320_216_16),
        DeviceType::Cisco7985 => Some(BACKGROUND_PROFILE_800_600_16),
        DeviceType::Cisco8941 | DeviceType::Cisco8945 => Some(BACKGROUND_PROFILE_640_480_24),
        DeviceType::Undefined | DeviceType::NotDefined | DeviceType::Unknown(_) => {
            Some(BACKGROUND_PROFILE_320_212_16)
        }
        DeviceType::Phone30SpPlus
        | DeviceType::Phone12SpPlus
        | DeviceType::Phone12Sp
        | DeviceType::Phone12
        | DeviceType::Phone30Vip
        | DeviceType::Cisco7910
        | DeviceType::Cisco7935
        | DeviceType::Vgc
        | DeviceType::Ata186
        | DeviceType::Ata188
        | DeviceType::Virtual30SpPlus
        | DeviceType::PhoneApplication
        | DeviceType::AnalogAccess
        | DeviceType::DigitalAccessPri
        | DeviceType::DigitalAccessT1
        | DeviceType::DigitalAccessTitan2
        | DeviceType::AnalogAccessElvis
        | DeviceType::DigitalAccessLennon
        | DeviceType::ConferenceBridge
        | DeviceType::ConferenceBridgeYoko
        | DeviceType::ConferenceBridgeDixieland
        | DeviceType::ConferenceBridgeSummit
        | DeviceType::H225
        | DeviceType::H323Phone
        | DeviceType::H323Trunk
        | DeviceType::MusicOnHold
        | DeviceType::Pilot
        | DeviceType::TapiPort
        | DeviceType::TapiRoutePoint
        | DeviceType::VoiceInbox
        | DeviceType::VoiceInboxAdmin
        | DeviceType::LineAnnunciator
        | DeviceType::SoftwareMtpDixieland
        | DeviceType::CiscoMediaServer
        | DeviceType::ConferenceBridgeFlint
        | DeviceType::RouteList
        | DeviceType::LoadSimulator
        | DeviceType::MediaTerminationPoint
        | DeviceType::MediaTerminationPointYoko
        | DeviceType::MediaTerminationPointDixieland
        | DeviceType::MediaTerminationPointSummit
        | DeviceType::MgcpStation
        | DeviceType::MgcpTrunk
        | DeviceType::RasProxy
        | DeviceType::CiscoAddon7914
        | DeviceType::Trunk
        | DeviceType::Annunciator
        | DeviceType::MonitorBridge
        | DeviceType::Recorder
        | DeviceType::MonitorBridgeYoko
        | DeviceType::SipTrunk
        | DeviceType::CiscoAddon7915_12
        | DeviceType::CiscoAddon7915_24
        | DeviceType::CiscoAddon7916_12
        | DeviceType::CiscoAddon7916_24
        | DeviceType::NokiaESeries
        | DeviceType::Cisco7931
        | DeviceType::Cisco7921
        | DeviceType::NokiaIcc
        | DeviceType::Cisco7937
        | DeviceType::Cisco7925
        | DeviceType::Cisco6921
        | DeviceType::Cisco6941
        | DeviceType::Cisco6961
        | DeviceType::Cisco6901
        | DeviceType::Cisco6911
        | DeviceType::Cisco6945
        | DeviceType::Cisco7926
        | DeviceType::Cisco7905
        | DeviceType::Cisco7912
        | DeviceType::Cisco7902
        | DeviceType::Cisco7936
        | DeviceType::AnalogGateway
        | DeviceType::BriGateway
        | DeviceType::Spa521s
        | DeviceType::Spa524sg
        | DeviceType::Spa502g
        | DeviceType::Spa504g
        | DeviceType::Spa525g
        | DeviceType::Spa508g
        | DeviceType::Spa509g
        | DeviceType::Spa525g2
        | DeviceType::Spa303g
        | DeviceType::Spa512g
        | DeviceType::Spa514g
        | DeviceType::AddonSpa500s
        | DeviceType::AddonSpa500ds
        | DeviceType::AddonSpa932ds => None,
    }
}

fn derive_thumbnail_url(
    image_url: &PhoneBackgroundHttpUrl,
) -> Result<PhoneBackgroundHttpUrl, DeviceBackgroundError> {
    let value = image_url.as_str();
    let path_end = value.find('?').unwrap_or(value.len());
    let path = &value[..path_end];
    let authority_end = path
        .find("://")
        .and_then(|index| index.checked_add(3))
        .ok_or(DeviceBackgroundError::MissingImageFilename)?;
    let resource_start = path[authority_end..]
        .find('/')
        .and_then(|index| authority_end.checked_add(index + 1))
        .ok_or(DeviceBackgroundError::MissingImageFilename)?;
    let filename_start = path[resource_start..]
        .rfind('/')
        .map_or(resource_start, |index| resource_start + index + 1);
    let filename = &path[filename_start..];
    if filename.is_empty() {
        return Err(DeviceBackgroundError::MissingImageFilename);
    }
    let insertion = filename
        .rfind('.')
        .map_or(path_end, |index| filename_start + index);
    let mut thumbnail = String::with_capacity(value.len() + "_thumb".len());
    thumbnail.push_str(&value[..insertion]);
    thumbnail.push_str("_thumb");
    thumbnail.push_str(&value[insertion..]);
    PhoneBackgroundHttpUrl::new(thumbnail)
        .map_err(|source| DeviceBackgroundError::InvalidDerivedThumbnail { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn background(image: &str) -> DeviceBackground {
        DeviceBackground::new(PhoneBackgroundHttpUrl::new(image).unwrap(), None).unwrap()
    }

    fn dynamic_pattern() -> DynamicBackgroundPattern {
        DynamicBackgroundPattern::new(
            "https://images.example.test/render.{FORMAT}?w={W}&h={H}&bitdepth={B}",
        )
        .unwrap()
    }

    #[test]
    fn thumbnail_derivation_inserts_suffix_before_extension_and_query() {
        let cases = [
            (
                "http://assets.example.test/phones/topkek.png",
                "http://assets.example.test/phones/topkek_thumb.png",
            ),
            (
                "http://assets.example.test/phones/topkek.jpg?rev=2",
                "http://assets.example.test/phones/topkek_thumb.jpg?rev=2",
            ),
            (
                "http://assets.example.test/phones/topkek",
                "http://assets.example.test/phones/topkek_thumb",
            ),
        ];
        for (image, thumbnail) in cases {
            let background = background(image);
            assert_eq!(background.thumbnail_url().as_str(), thumbnail);
            assert_eq!(
                background.thumbnail_source(),
                BackgroundThumbnailSource::Derived
            );
        }
    }

    #[test]
    fn automatic_thumbnail_requires_a_filename() {
        for url in [
            "http://assets.example.test",
            "http://assets.example.test?image=desk",
            "http://assets.example.test/phones/",
        ] {
            let image = PhoneBackgroundHttpUrl::new(url).unwrap();
            assert!(matches!(
                DeviceBackground::new(image, None),
                Err(DeviceBackgroundError::MissingImageFilename)
            ));
        }
    }

    #[test]
    fn explicit_thumbnail_accepts_a_dynamic_image_endpoint() {
        let image = PhoneBackgroundHttpUrl::new("http://assets.example.test/phones/").unwrap();
        let thumbnail =
            PhoneBackgroundHttpUrl::new("http://assets.example.test/thumbnail?id=desk").unwrap();
        let background = DeviceBackground::new(image, Some(thumbnail.clone())).unwrap();
        assert_eq!(background.thumbnail_url(), &thumbnail);
        assert_eq!(
            background.thumbnail_source(),
            BackgroundThumbnailSource::Explicit
        );
    }

    #[test]
    fn rejects_a_url_pair_that_cannot_fit_the_control_document() {
        let value = format!("http://x/{}", "😀".repeat(247));
        let image = PhoneBackgroundHttpUrl::new(value).unwrap();
        assert!(matches!(
            DeviceBackground::new(image.clone(), Some(image)),
            Err(DeviceBackgroundError::InvalidControlDocument { .. })
        ));
    }

    #[test]
    fn dynamic_profiles_resolve_full_and_thumbnail_resources() {
        let cases = [
            (DeviceType::Cisco7906, (95, 34, 23, 8, 1), "png"),
            (DeviceType::Cisco7911, (95, 34, 23, 8, 1), "png"),
            (DeviceType::Cisco7941, (320, 196, 80, 49, 4), "png"),
            (DeviceType::Cisco7941Ge, (320, 196, 80, 49, 4), "png"),
            (DeviceType::Cisco7942, (320, 196, 80, 49, 4), "png"),
            (DeviceType::Cisco7961, (320, 196, 80, 49, 4), "png"),
            (DeviceType::Cisco7961Ge, (320, 196, 80, 49, 4), "png"),
            (DeviceType::Cisco7962, (320, 196, 80, 49, 4), "png"),
            (DeviceType::Cisco7970, (320, 212, 80, 53, 12), "png"),
            (DeviceType::Cisco7971, (320, 212, 80, 53, 12), "png"),
            (
                DeviceType::CiscoIpCommunicator,
                (320, 212, 80, 53, 12),
                "png",
            ),
            (DeviceType::Cisco7945, (320, 212, 80, 53, 16), "png"),
            (DeviceType::Cisco7965, (320, 212, 80, 53, 16), "png"),
            (DeviceType::Cisco7975, (320, 216, 80, 53, 16), "png"),
            (DeviceType::Cisco7985, (800, 600, 800, 600, 16), "png"),
            (DeviceType::Cisco8941, (640, 480, 123, 111, 24), "png"),
            (DeviceType::Cisco8945, (640, 480, 123, 111, 24), "png"),
            (DeviceType::Undefined, (320, 212, 80, 53, 16), "png"),
            (DeviceType::NotDefined, (320, 212, 80, 53, 16), "png"),
            (DeviceType::Unknown(123_456), (320, 212, 80, 53, 16), "png"),
        ];
        let pattern = dynamic_pattern();
        for (device_type, (width, height, thumb_width, thumb_height, bit_depth), format) in cases {
            let Some(ResolvedDeviceBackground::Set(background)) =
                pattern.resolve(device_type).unwrap()
            else {
                panic!("selectable profile must resolve to a set-background request");
            };
            assert_eq!(
                background.image_url().as_str(),
                format!(
                    "https://images.example.test/render.{format}?w={width}&h={height}&bitdepth={bit_depth}"
                )
            );
            assert_eq!(
                background.thumbnail_url().as_str(),
                format!(
                    "https://images.example.test/render.{format}?w={thumb_width}&h={thumb_height}&bitdepth={bit_depth}"
                )
            );
            assert_eq!(
                background.thumbnail_source(),
                BackgroundThumbnailSource::Dynamic
            );
        }
    }

    #[test]
    fn legacy_dynamic_profiles_resolve_to_xml_display_resources() {
        let cases = [
            (DeviceType::Cisco7920, 128, 59, 1),
            (DeviceType::Cisco7940, 133, 65, 2),
            (DeviceType::Cisco7960, 133, 65, 2),
        ];
        let pattern = dynamic_pattern();
        for (device_type, width, height, bit_depth) in cases {
            let Some(ResolvedDeviceBackground::Display(image_url)) =
                pattern.resolve(device_type).unwrap()
            else {
                panic!("legacy profile must resolve to an XML display request");
            };
            assert_eq!(
                image_url.as_str(),
                format!(
                    "https://images.example.test/render.xml?w={width}&h={height}&bitdepth={bit_depth}"
                )
            );
        }
    }

    #[test]
    fn dynamic_patterns_reject_missing_or_unknown_placeholders() {
        assert!(matches!(
            DynamicBackgroundPattern::new("https://images.example.test/render?w={W}"),
            Err(DeviceBackgroundError::MissingDynamicDimensions)
        ));
        assert!(matches!(
            DynamicBackgroundPattern::new(
                "https://images.example.test/render?w={W}&h={H}&fit={MODE}"
            ),
            Err(DeviceBackgroundError::InvalidDynamicPlaceholder)
        ));
    }

    #[test]
    fn unsupported_device_types_do_not_resolve_dynamic_resources() {
        assert!(
            dynamic_pattern()
                .resolve(DeviceType::Cisco7931)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn dynamic_pattern_debug_output_redacts_the_resource() {
        assert!(!format!("{:?}", dynamic_pattern()).contains("images.example"));
    }

    #[test]
    fn dynamic_pattern_length_accepts_the_limit_and_rejects_the_next_character() {
        let suffix = "?w={W}&h={H}";
        let prefix = "https://x.test/";
        let fill = MAX_DYNAMIC_BACKGROUND_PATTERN_CHARS - prefix.len() - suffix.len();
        let maximum = format!("{prefix}{}{suffix}", "a".repeat(fill));
        assert_eq!(
            maximum.chars().count(),
            MAX_DYNAMIC_BACKGROUND_PATTERN_CHARS
        );
        assert!(DynamicBackgroundPattern::new(maximum.clone()).is_ok());
        assert!(matches!(
            DynamicBackgroundPattern::new(format!("{maximum}a")),
            Err(DeviceBackgroundError::DynamicPatternTooLong {
                characters,
                maximum
            }) if characters == MAX_DYNAMIC_BACKGROUND_PATTERN_CHARS + 1
                && maximum == MAX_DYNAMIC_BACKGROUND_PATTERN_CHARS
        ));
    }
}
