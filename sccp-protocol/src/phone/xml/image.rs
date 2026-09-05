//! Image phone XML document family.

use super::*;

/// Validated hexadecimal bitmap data used by image-service documents.
///
/// The public value is binary. Serde converts it to and from the XML Schema
/// `hexBinary` lexical representation, accepting schema whitespace and either
/// letter case while emitting one stable uppercase representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhoneBitmapData(Vec<u8>);

impl PhoneBitmapData {
    /// Validates decoded bitmap bytes against [`PHONE_IMAGE_BITMAP_MAX_BYTES`].
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, PhoneXmlError> {
        let bytes = bytes.into();
        validate_count(
            "bitmap image data bytes",
            bytes.len(),
            PHONE_IMAGE_BITMAP_MAX_BYTES,
        )?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl Serialize for PhoneBitmapData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut encoded = String::with_capacity(self.0.len().saturating_mul(2));
        for byte in &self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        serializer.serialize_str(&encoded)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneBitmapData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = <String as serde::Deserialize>::deserialize(deserializer)?;
        let digits = encoded
            .bytes()
            .filter(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            .count();
        let digit_count = encoded.len().saturating_sub(digits);
        if digit_count % 2 != 0 {
            return Err(serde::de::Error::custom(
                "bitmap data must contain complete hexadecimal bytes",
            ));
        }
        let mut decoded = Vec::with_capacity(digit_count / 2);
        let mut high = None;
        for byte in encoded
            .bytes()
            .filter(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => {
                    return Err(serde::de::Error::custom("bitmap data must be hexadecimal"));
                }
            };
            if let Some(high) = high.take() {
                decoded.push((high << 4) | value);
            } else {
                high = Some(value);
            }
        }
        Self::new(decoded).map_err(serde::de::Error::custom)
    }
}

/// A schema-bounded image resource URL.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PhoneImageUrl(String);

impl PhoneImageUrl {
    /// Validates a non-empty image URL of at most 256 characters.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_optional_text("phone image URL", Some(&value), 1, PHONE_XML_URL_MAX_CHARS)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for PhoneImageUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// A TFTP URI accepted in a background-image selection list.
///
/// The phone's selection-list schema uses the opaque `TFTP:path` form.  The
/// authority-bearing `tftp://host/path` form and HTTP URLs are deliberately
/// rejected because they are not interchangeable on the handset.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PhoneBackgroundTftpUrl(String);

impl PhoneBackgroundTftpUrl {
    /// Validates an opaque `TFTP:path` URI naming a PNG resource.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_optional_text(
            "background image TFTP URI",
            Some(&value),
            1,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        let parsed = url::Url::parse(&value).map_err(|_| PhoneXmlError::InvalidField {
            field: "background image TFTP URI",
            expected: "a TFTP:path URI to a PNG image",
        })?;
        let has_tftp_prefix = value
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("tftp:"));
        let path = value.get(5..).unwrap_or_default();
        let is_png = path
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("png"));
        if !has_tftp_prefix
            || parsed.scheme() != "tftp"
            || !parsed.cannot_be_a_base()
            || parsed.host_str().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !valid_background_tftp_path(path)
            || !is_png
        {
            return Err(PhoneXmlError::InvalidField {
                field: "background image TFTP URI",
                expected: "a TFTP:path URI to a PNG image",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for PhoneBackgroundTftpUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneBackgroundTftpUrl(<redacted>)")
    }
}

impl Serialize for PhoneBackgroundTftpUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneBackgroundTftpUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

pub(super) fn valid_background_tftp_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['?', '#', '\\'])
        && path.split('/').all(|component| {
            valid_percent_encoding(component)
                && percent_encoding::percent_decode_str(component)
                    .decode_utf8()
                    .is_ok_and(|decoded| {
                        !decoded.is_empty()
                            && decoded != "."
                            && decoded != ".."
                            && !decoded.contains(['/', '\\'])
                            && !decoded.chars().any(char::is_control)
                    })
        })
}

pub(super) fn valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

/// An HTTP or HTTPS URL accepted by the background selection and preview application.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PhoneBackgroundHttpUrl(String);

impl PhoneBackgroundHttpUrl {
    /// Validates an absolute HTTP or HTTPS URL without credentials or a fragment.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_http_resource_url(
            "background image URL",
            "an absolute HTTP or HTTPS URL without credentials or a fragment",
            &value,
        )?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

pub(super) fn validate_http_resource_url(
    field: &'static str,
    expected: &'static str,
    value: &str,
) -> Result<(), PhoneXmlError> {
    validate_optional_text(field, Some(value), 1, PHONE_XML_URL_MAX_CHARS)?;
    if value
        .chars()
        .any(|character| character.is_ascii_whitespace() || character.is_ascii_control())
        || value.contains('\\')
        || !valid_percent_encoding(value)
    {
        return Err(PhoneXmlError::InvalidField { field, expected });
    }
    let parsed =
        url::Url::parse(value).map_err(|_| PhoneXmlError::InvalidField { field, expected })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(PhoneXmlError::InvalidField { field, expected });
    }
    Ok(())
}

impl fmt::Debug for PhoneBackgroundHttpUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneBackgroundHttpUrl(<redacted>)")
    }
}

impl Serialize for PhoneBackgroundHttpUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneBackgroundHttpUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// One ordered full-size/thumbnail pair in a background image list.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneImageListItem {
    #[serde(rename = "@Image")]
    pub thumbnail_url: PhoneBackgroundTftpUrl,
    #[serde(rename = "@URL")]
    pub image_url: PhoneBackgroundTftpUrl,
}

/// The ordered background choices pulled from a desktop `List.xml` resource.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneImageList", deny_unknown_fields)]
pub struct CiscoIpPhoneImageList {
    #[serde(rename = "ImageItem", default)]
    pub items: Vec<CiscoIpPhoneImageListItem>,
}

impl CiscoIpPhoneImageList {
    /// Builds and validates an ordered list of background choices.
    pub fn new(items: Vec<CiscoIpPhoneImageListItem>) -> Result<Self, PhoneXmlError> {
        let document = Self { items };
        document.validate()?;
        Ok(document)
    }

    /// Enforces [`PHONE_BACKGROUND_LIST_MAX_ITEMS`].
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_count(
            "background image choices",
            self.items.len(),
            PHONE_BACKGROUND_LIST_MAX_ITEMS,
        )
    }
}

/// Full-size and thumbnail URLs installed as the phone background.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneBackground {
    #[serde(rename = "image")]
    pub image_url: PhoneBackgroundHttpUrl,
    #[serde(rename = "icon")]
    pub thumbnail_url: PhoneBackgroundHttpUrl,
}

/// Background-selection application document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "setBackground", deny_unknown_fields)]
pub struct CiscoIpPhoneSetBackground {
    #[serde(rename = "background")]
    pub background: CiscoIpPhoneBackground,
}

impl CiscoIpPhoneSetBackground {
    /// Creates a background installation request from validated resource URLs.
    pub fn new(image_url: PhoneBackgroundHttpUrl, thumbnail_url: PhoneBackgroundHttpUrl) -> Self {
        Self {
            background: CiscoIpPhoneBackground {
                image_url,
                thumbnail_url,
            },
        }
    }

    /// Parses an installation request using [`PHONE_BACKGROUND_CONTROL_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum SetBackgroundEnvelope {
            #[serde(rename = "setBackground")]
            SetBackground(CiscoIpPhoneSetBackground),
        }

        let SetBackgroundEnvelope::SetBackground(document) =
            from_bytes(document, PHONE_BACKGROUND_CONTROL_MAX_BYTES)?;
        Ok(document)
    }

    /// Serializes an installation request using [`PHONE_BACKGROUND_CONTROL_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        to_string(self, PHONE_BACKGROUND_CONTROL_MAX_BYTES)
    }
}

/// Background-preview application document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "setBackgroundPreview", deny_unknown_fields)]
pub struct CiscoIpPhoneSetBackgroundPreview {
    #[serde(rename = "image")]
    pub image_url: PhoneBackgroundHttpUrl,
}

impl CiscoIpPhoneSetBackgroundPreview {
    /// Creates a preview request from a validated image URL.
    pub const fn new(image_url: PhoneBackgroundHttpUrl) -> Self {
        Self { image_url }
    }

    /// Parses a preview request using [`PHONE_BACKGROUND_CONTROL_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum PreviewEnvelope {
            #[serde(rename = "setBackgroundPreview")]
            Preview(CiscoIpPhoneSetBackgroundPreview),
        }

        let PreviewEnvelope::Preview(document) =
            from_bytes(document, PHONE_BACKGROUND_CONTROL_MAX_BYTES)?;
        Ok(document)
    }

    /// Serializes a preview request using [`PHONE_BACKGROUND_CONTROL_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        to_string(self, PHONE_BACKGROUND_CONTROL_MAX_BYTES)
    }
}

/// Either application document accepted by the background-control service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhoneBackgroundControlDocument {
    Set(CiscoIpPhoneSetBackground),
    Preview(CiscoIpPhoneSetBackgroundPreview),
}

impl PhoneBackgroundControlDocument {
    /// Detects and parses either supported background-control root.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum BackgroundEnvelope {
            #[serde(rename = "setBackground")]
            Set(CiscoIpPhoneSetBackground),
            #[serde(rename = "setBackgroundPreview")]
            Preview(CiscoIpPhoneSetBackgroundPreview),
        }

        Ok(
            match from_bytes(document, PHONE_BACKGROUND_CONTROL_MAX_BYTES)? {
                BackgroundEnvelope::Set(document) => Self::Set(document),
                BackgroundEnvelope::Preview(document) => Self::Preview(document),
            },
        )
    }

    /// Serializes the selected document using the default control byte bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.to_xml_with_limit(PHONE_BACKGROUND_CONTROL_MAX_BYTES)
    }

    /// Serializes the selected document within a caller-selected byte limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        match self {
            Self::Set(document) => to_string(document, maximum_bytes),
            Self::Preview(document) => to_string(document, maximum_bytes),
        }
    }
}

/// An HTTP resource URL accepted by the ringtone-selection application.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PhoneRingtoneUrl(String);

impl PhoneRingtoneUrl {
    /// Validates an absolute lowercase-scheme HTTP URL without credentials or a fragment.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        if !value.starts_with("http://") {
            return Err(PhoneXmlError::InvalidField {
                field: "ringtone HTTP URL",
                expected: "an absolute lowercase HTTP URL without credentials or a fragment",
            });
        }
        validate_http_resource_url(
            "ringtone HTTP URL",
            "an absolute lowercase HTTP URL without credentials or a fragment",
            &value,
        )?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for PhoneRingtoneUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneRingtoneUrl(<redacted>)")
    }
}

impl Serialize for PhoneRingtoneUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneRingtoneUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Ringtone-selection application document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "setRingTone", deny_unknown_fields)]
pub struct CiscoIpPhoneSetRingTone {
    #[serde(rename = "ringTone")]
    pub ringtone_url: PhoneRingtoneUrl,
}

impl CiscoIpPhoneSetRingTone {
    /// Creates a ringtone request from a validated HTTP resource URL.
    pub const fn new(ringtone_url: PhoneRingtoneUrl) -> Self {
        Self { ringtone_url }
    }

    /// Parses a ringtone request using [`PHONE_RINGTONE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum RingToneEnvelope {
            #[serde(rename = "setRingTone")]
            RingTone(CiscoIpPhoneSetRingTone),
        }

        let RingToneEnvelope::RingTone(document) = from_bytes(document, PHONE_RINGTONE_MAX_BYTES)?;
        Ok(document)
    }

    /// Serializes a ringtone request using [`PHONE_RINGTONE_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.to_xml_with_limit(PHONE_RINGTONE_MAX_BYTES)
    }

    /// Serializes a ringtone request within a caller-selected byte limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        to_string(self, maximum_bytes)
    }
}

/// A rectangular selection region in a graphic-file menu.
#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneTouchArea {
    #[serde(rename = "@X1")]
    pub x1: u16,
    #[serde(rename = "@Y1")]
    pub y1: u16,
    #[serde(rename = "@X2")]
    pub x2: u16,
    #[serde(rename = "@Y2")]
    pub y2: u16,
}

/// One optional label, action, and selection region in a graphic-file menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneTouchAreaMenuItem {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "TouchArea", default, skip_serializing_if = "Option::is_none")]
    pub touch_area: Option<PhoneTouchArea>,
}

/// A complete inline-bitmap image document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneImage", deny_unknown_fields)]
pub struct CiscoIpPhoneImage {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=132`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=64`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "Width")]
    /// Bitmap width in pixels, constrained to `1..=133`.
    pub width: u16,
    #[serde(rename = "Height")]
    /// Bitmap height in pixels, constrained to `1..=65`.
    pub height: u16,
    #[serde(rename = "Depth")]
    /// Bitmap bit depth, constrained to `1..=2`.
    pub depth: u16,
    #[serde(rename = "Data", default, skip_serializing_if = "Option::is_none")]
    pub data: Option<PhoneBitmapData>,
}

impl CiscoIpPhoneImage {
    /// Validates display metadata, pixel geometry, and decoded bitmap size.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_image_display(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_bitmap_image(
            self.location_x,
            self.location_y,
            self.width,
            self.height,
            self.depth,
            self.data.as_ref(),
        )
    }

    /// Detects, parses, and validates an image root within [`PHONE_IMAGE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum ImageEnvelope {
            #[serde(rename = "CiscoIPPhoneImage")]
            Image(CiscoIpPhoneImage),
        }
        let ImageEnvelope::Image(document) = from_bytes(document, PHONE_IMAGE_MAX_BYTES)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the image within [`PHONE_IMAGE_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_IMAGE_MAX_BYTES)
    }
}

/// A complete URL-backed image document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneImageFile", deny_unknown_fields)]
pub struct CiscoIpPhoneImageFile {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=297`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=167`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "URL")]
    pub url: PhoneImageUrl,
}

impl CiscoIpPhoneImageFile {
    /// Validates display metadata and the optional image origin.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_image_display(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_file_image_location(self.location_x, self.location_y)
    }

    /// Detects, parses, and validates an image-file root within [`PHONE_IMAGE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum ImageFileEnvelope {
            #[serde(rename = "CiscoIPPhoneImageFile")]
            ImageFile(CiscoIpPhoneImageFile),
        }
        let ImageFileEnvelope::ImageFile(document) = from_bytes(document, PHONE_IMAGE_MAX_BYTES)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the image-file document within its default bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_IMAGE_MAX_BYTES)
    }
}

/// A complete inline-bitmap graphic menu document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneGraphicMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneGraphicMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=132`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=64`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "Width")]
    /// Bitmap width in pixels, constrained to `1..=133`.
    pub width: u16,
    #[serde(rename = "Height")]
    /// Bitmap height in pixels, constrained to `1..=65`.
    pub height: u16,
    #[serde(rename = "Depth")]
    /// Bitmap bit depth, constrained to `1..=2`.
    pub depth: u16,
    #[serde(rename = "Data", default, skip_serializing_if = "Option::is_none")]
    pub data: Option<PhoneBitmapData>,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneMenuItem>,
}

impl CiscoIpPhoneGraphicMenu {
    /// Validates display metadata, bitmap geometry, and selectable-item bounds.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_image_display(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_bitmap_image(
            self.location_x,
            self.location_y,
            self.width,
            self.height,
            self.depth,
            self.data.as_ref(),
        )?;
        validate_count(
            "graphic menu items",
            self.items.len(),
            PHONE_GRAPHIC_MENU_MAX_ITEMS,
        )?;
        for item in &self.items {
            validate_optional_text("graphic menu item name", item.name.as_deref(), 0, 64)?;
            validate_optional_text(
                "graphic menu item URL",
                item.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
        }
        Ok(())
    }

    /// Detects, parses, and validates a graphic menu within [`PHONE_IMAGE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum GraphicMenuEnvelope {
            #[serde(rename = "CiscoIPPhoneGraphicMenu")]
            GraphicMenu(CiscoIpPhoneGraphicMenu),
        }
        let GraphicMenuEnvelope::GraphicMenu(document) =
            from_bytes(document, PHONE_IMAGE_MAX_BYTES)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the graphic menu within its default bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_IMAGE_MAX_BYTES)
    }
}

/// A complete URL-backed graphic menu document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneGraphicFileMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneGraphicFileMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=297`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=167`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "URL")]
    pub url: PhoneImageUrl,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneTouchAreaMenuItem>,
}

impl CiscoIpPhoneGraphicFileMenu {
    /// Validates display metadata, image origin, and touch-area item bounds.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_image_display(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_file_image_location(self.location_x, self.location_y)?;
        validate_count(
            "graphic-file menu items",
            self.items.len(),
            PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS,
        )?;
        for item in &self.items {
            validate_optional_text("graphic-file menu item name", item.name.as_deref(), 0, 32)?;
            validate_optional_text(
                "graphic-file menu item URL",
                item.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
        }
        Ok(())
    }

    /// Detects, parses, and validates a graphic-file menu within its default bound.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum GraphicFileMenuEnvelope {
            #[serde(rename = "CiscoIPPhoneGraphicFileMenu")]
            GraphicFileMenu(CiscoIpPhoneGraphicFileMenu),
        }
        let GraphicFileMenuEnvelope::GraphicFileMenu(document) =
            from_bytes(document, PHONE_IMAGE_MAX_BYTES)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the graphic-file menu within its default bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_IMAGE_MAX_BYTES)
    }
}

/// Any accepted image-service document family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhoneImageDocument {
    Image(CiscoIpPhoneImage),
    ImageFile(CiscoIpPhoneImageFile),
    GraphicMenu(CiscoIpPhoneGraphicMenu),
    GraphicFileMenu(CiscoIpPhoneGraphicFileMenu),
}

impl PhoneImageDocument {
    /// Applies the invariants for the selected image family.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        match self {
            Self::Image(document) => document.validate(),
            Self::ImageFile(document) => document.validate(),
            Self::GraphicMenu(document) => document.validate(),
            Self::GraphicFileMenu(document) => document.validate(),
        }
    }

    /// Detects the root and parses any supported image family.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum ImageDocumentEnvelope {
            #[serde(rename = "CiscoIPPhoneImage")]
            Image(CiscoIpPhoneImage),
            #[serde(rename = "CiscoIPPhoneImageFile")]
            ImageFile(CiscoIpPhoneImageFile),
            #[serde(rename = "CiscoIPPhoneGraphicMenu")]
            GraphicMenu(CiscoIpPhoneGraphicMenu),
            #[serde(rename = "CiscoIPPhoneGraphicFileMenu")]
            GraphicFileMenu(CiscoIpPhoneGraphicFileMenu),
        }
        let document = match from_bytes(document, PHONE_IMAGE_MAX_BYTES)? {
            ImageDocumentEnvelope::Image(document) => Self::Image(document),
            ImageDocumentEnvelope::ImageFile(document) => Self::ImageFile(document),
            ImageDocumentEnvelope::GraphicMenu(document) => Self::GraphicMenu(document),
            ImageDocumentEnvelope::GraphicFileMenu(document) => Self::GraphicFileMenu(document),
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the selected family using [`PHONE_IMAGE_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.to_xml_with_limit(PHONE_IMAGE_MAX_BYTES)
    }

    /// Validates and serializes the selected family within a caller-selected limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        self.validate()?;
        match self {
            Self::Image(document) => to_string(document, maximum_bytes),
            Self::ImageFile(document) => to_string(document, maximum_bytes),
            Self::GraphicMenu(document) => to_string(document, maximum_bytes),
            Self::GraphicFileMenu(document) => to_string(document, maximum_bytes),
        }
    }
}
