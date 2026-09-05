//! Typed XML documents exchanged with phone services.
//!
//! Known document schemas go through this Serde boundary so size, encoding,
//! and document-type policy is applied consistently before a model is used.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use super::validation::{text_length_is_within, validate_count};
use crate::{ConferenceId, ParticipantId};

mod conference;
mod display;
mod document;
mod image;
mod menu;
mod status;
mod telemetry;

pub use conference::*;
pub use display::*;
pub use document::PhoneXmlDocument;
pub use image::*;
pub use menu::*;
pub use status::*;
pub use telemetry::*;

/// Maximum participants represented by a conference menu document.
pub const CONFERENCE_LIST_MAX_PARTICIPANTS: usize = 16;
/// Maximum encoded conference menu size, in bytes.
pub const CONFERENCE_LIST_MAX_BYTES: usize = 2_000;
/// Maximum entries accepted in a directory document.
pub const PHONE_DIRECTORY_MAX_ENTRIES: usize = 32;
/// Maximum encoded directory document size, in bytes.
pub const PHONE_DIRECTORY_MAX_BYTES: usize = 8_192;
/// Maximum selectable entries in a plain menu document.
pub const PHONE_MENU_MAX_ITEMS: usize = 100;
/// Maximum selectable entries in an icon menu document.
pub const PHONE_ICON_MENU_MAX_ITEMS: usize = 32;
/// Maximum embedded or referenced icons in an icon menu document.
pub const PHONE_ICON_MENU_MAX_ICONS: usize = 10;
/// Maximum encoded size shared by menu document families, in bytes.
pub const PHONE_MENU_MAX_BYTES: usize = 64 * 1_024;
/// Maximum Unicode character count for a text document body.
pub const PHONE_TEXT_MAX_CHARS: usize = 4_000;
/// Maximum encoded text document size, in bytes.
pub const PHONE_TEXT_MAX_BYTES: usize = 32 * 1_024;
/// Compatibility character bound for display profiles with smaller text capacity.
pub const PHONE_TEXT_LEGACY_MAX_CHARS: usize = 1_024;
/// Reserved application identifier used by text-display workflows.
pub const PHONE_TEXT_APPLICATION_ID: u32 = 9_089;
/// Maximum input controls in one input document.
pub const PHONE_INPUT_MAX_ITEMS: usize = 5;
/// Maximum encoded input document size, in bytes.
pub const PHONE_INPUT_MAX_BYTES: usize = 32 * 1_024;
/// Maximum actions in one execute document.
pub const PHONE_EXECUTE_MAX_ITEMS: usize = 3;
/// Maximum encoded execute document size, in bytes.
pub const PHONE_EXECUTE_MAX_BYTES: usize = 8 * 1_024;
/// Maximum decoded bitmap data in an inline image, in bytes.
pub const PHONE_IMAGE_BITMAP_MAX_BYTES: usize = 2_162;
/// Maximum touch entries in an inline-bitmap graphic menu.
pub const PHONE_GRAPHIC_MENU_MAX_ITEMS: usize = 12;
/// Maximum touch entries in an image-file graphic menu.
pub const PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS: usize = 32;
/// Maximum encoded size shared by image document families, in bytes.
pub const PHONE_IMAGE_MAX_BYTES: usize = 64 * 1_024;
/// Maximum decoded bitmap data in a status document, in bytes.
pub const PHONE_STATUS_BITMAP_MAX_BYTES: usize = 557;
/// Maximum encoded size shared by status document families, in bytes.
pub const PHONE_STATUS_MAX_BYTES: usize = 8 * 1_024;
/// Maximum encoded alarm telemetry size, in bytes.
pub const PHONE_ALARM_MAX_BYTES: usize = 2_048;
/// Maximum encoded location telemetry size, in bytes.
pub const PHONE_LOCATION_MAX_BYTES: usize = 2_404;
/// Reserved application identifier used by background-image workflows.
pub const PHONE_BACKGROUND_APPLICATION_ID: u32 = 9_086;
/// Maximum choices in a background-image list.
pub const PHONE_BACKGROUND_LIST_MAX_ITEMS: usize = 50;
/// Maximum encoded background-image list size, in bytes.
pub const PHONE_BACKGROUND_LIST_MAX_BYTES: usize = 32 * 1_024;
/// Maximum encoded background-control document size, in bytes.
pub const PHONE_BACKGROUND_CONTROL_MAX_BYTES: usize = 2_000;
/// Reserved application identifier used by ringtone workflows.
pub const PHONE_RINGTONE_APPLICATION_ID: u32 = 9_087;
/// Maximum encoded ringtone-control document size, in bytes.
pub const PHONE_RINGTONE_MAX_BYTES: usize = 2_000;
/// Maximum element nesting accepted before deserialization.
pub const PHONE_XML_MAX_NESTING_DEPTH: usize = 32;
const PHONE_DIRECTORY_TEXT_MAX_CHARS: usize = 32;
const PHONE_XML_URL_MAX_CHARS: usize = 256;

/// Schema, resource-limit, and serialization failures at the XML boundary.
#[derive(Debug, Error)]
pub enum PhoneXmlError {
    /// A collection or encoded document crossed its explicit resource bound.
    #[error("{kind} has {actual} entries or bytes; maximum is {maximum}")]
    LimitExceeded {
        kind: &'static str,
        actual: usize,
        maximum: usize,
    },
    /// Undeclared non-UTF-8 input reached a UTF-8-only boundary.
    #[error("phone XML is not valid UTF-8: {0}")]
    InvalidUtf8(#[source] std::str::Utf8Error),
    /// A document-type declaration was rejected before entity expansion.
    #[error("phone XML document types and entity declarations are not allowed")]
    DocumentTypeForbidden,
    /// An entity reference was undeclared or resolved to an invalid XML character.
    #[error("phone XML contains an invalid or undeclared entity reference")]
    InvalidEntity,
    /// A recognized alarm root did not satisfy its typed schema.
    #[error("supported phone alarm does not match its typed schema")]
    InvalidAlarmSchema,
    /// A recognized location root did not satisfy its typed schema.
    #[error("supported phone location information does not match its typed schema")]
    InvalidLocationSchema,
    /// Element depth crossed [`PHONE_XML_MAX_NESTING_DEPTH`].
    #[error("phone XML nesting exceeds the maximum depth of {maximum}")]
    NestingTooDeep { maximum: usize },
    /// Tokenization failed before typed deserialization.
    #[error("phone XML is malformed: {0}")]
    Malformed(#[source] quick_xml::Error),
    /// The XML was well-formed but did not match the selected model.
    #[error("phone XML does not match its typed schema: {0}")]
    Deserialize(#[source] quick_xml::DeError),
    /// The typed model could not be converted to XML.
    #[error("phone XML could not be serialized: {0}")]
    Serialize(#[source] quick_xml::SeError),
    /// A formatting sink failed while receiving a bounded serialized document.
    #[error("phone XML could not be written: {0}")]
    Write(#[source] fmt::Error),
    /// A model value violated a schema invariant beyond its Rust type.
    #[error("{field} must be {expected}")]
    InvalidField {
        field: &'static str,
        expected: &'static str,
    },
}

/// Parses bounded UTF-8, US-ASCII, or ISO-8859-1 XML using the encoding named
/// in the declaration. The XML reader, rather than an ad-hoc byte scan, owns
/// decoding, entity policy, and nesting validation.
pub fn from_bytes<T: DeserializeOwned>(
    document: &[u8],
    maximum_bytes: usize,
) -> Result<T, PhoneXmlError> {
    if document.len() > maximum_bytes {
        return Err(PhoneXmlError::LimitExceeded {
            kind: "phone XML document",
            actual: document.len(),
            maximum: maximum_bytes,
        });
    }
    // Without an explicit legacy declaration, raw non-UTF-8 input remains a
    // typed UTF-8 failure. quick-xml handles the declared ASCII/ISO decoder;
    // this guard prevents arbitrary malformed bytes from being reported as a
    // less useful generic deserialization error.
    if let Err(error) = std::str::from_utf8(document)
        && !declares_iso_8859_1(document)
    {
        return Err(PhoneXmlError::InvalidUtf8(error));
    }
    reject_document_type(document)?;
    quick_xml::de::from_reader(decoding_reader(document)).map_err(PhoneXmlError::Deserialize)
}

fn decoding_reader(document: &[u8]) -> quick_xml::encoding::DecodingReader<&[u8]> {
    let mut decoder = quick_xml::encoding::DecodingReader::new(document);
    let mut declaration_reader = Reader::from_reader(document);
    if let Ok(Event::Decl(declaration)) = declaration_reader.read_event()
        && declaration
            .encoding()
            .and_then(Result::ok)
            .is_some_and(|encoding| encoding.eq_ignore_ascii_case("iso-8859-1"))
        && let Some(encoding) = declaration.encoder()
    {
        decoder.set_encoding(encoding);
    }
    decoder
}

fn declares_iso_8859_1(document: &[u8]) -> bool {
    let mut reader = Reader::from_reader(document);
    let Ok(Event::Decl(declaration)) = reader.read_event() else {
        return false;
    };
    declaration
        .encoding()
        .and_then(Result::ok)
        .is_some_and(|encoding| encoding.eq_ignore_ascii_case("iso-8859-1"))
}

/// Serializes a known Serde model and rejects an oversized result.
pub fn to_string<T: Serialize>(
    document: &T,
    maximum_bytes: usize,
) -> Result<String, PhoneXmlError> {
    let xml = quick_xml::se::to_string(document).map_err(PhoneXmlError::Serialize)?;
    if xml.len() > maximum_bytes {
        return Err(PhoneXmlError::LimitExceeded {
            kind: "phone XML document",
            actual: xml.len(),
            maximum: maximum_bytes,
        });
    }
    Ok(xml)
}

/// Serializes through the bounded string boundary before touching the writer.
pub fn to_writer<T: Serialize>(
    mut writer: impl fmt::Write,
    document: &T,
    maximum_bytes: usize,
) -> Result<(), PhoneXmlError> {
    let xml = to_string(document, maximum_bytes)?;
    writer.write_str(&xml).map_err(PhoneXmlError::Write)
}

fn reject_document_type(document: &[u8]) -> Result<(), PhoneXmlError> {
    let mut reader = Reader::from_reader(decoding_reader(document));
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::DocType(_)) => return Err(PhoneXmlError::DocumentTypeForbidden),
            Ok(Event::Start(element)) => {
                validate_xml_attributes(&element)?;
                depth = depth.saturating_add(1);
                if depth > PHONE_XML_MAX_NESTING_DEPTH {
                    return Err(PhoneXmlError::NestingTooDeep {
                        maximum: PHONE_XML_MAX_NESTING_DEPTH,
                    });
                }
            }
            Ok(Event::Empty(element)) => validate_xml_attributes(&element)?,
            Ok(Event::GeneralRef(reference)) => {
                let reference = reference.xml_content(XmlVersion::Implicit1_0);
                let escaped = format!("&{reference};");
                let resolved = quick_xml::escape::unescape(&escaped)
                    .map_err(|_| PhoneXmlError::InvalidEntity)?;
                if !has_only_xml_characters(&resolved) {
                    return Err(PhoneXmlError::InvalidEntity);
                }
            }
            Ok(Event::Text(text)) => {
                let text = text.xml_content(XmlVersion::Implicit1_0);
                if !has_only_xml_characters(&text) {
                    return Err(PhoneXmlError::InvalidEntity);
                }
            }
            Ok(Event::CData(text)) => {
                let text = text.xml_content(XmlVersion::Implicit1_0);
                if !has_only_xml_characters(&text) {
                    return Err(PhoneXmlError::InvalidEntity);
                }
            }
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(PhoneXmlError::Malformed(error)),
        }
        buffer.clear();
    }
}

fn validate_xml_attributes(
    element: &quick_xml::events::BytesStart<'_>,
) -> Result<(), PhoneXmlError> {
    for attribute in element.attributes() {
        let attribute = attribute
            .map_err(quick_xml::Error::from)
            .map_err(PhoneXmlError::Malformed)?;
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| PhoneXmlError::InvalidEntity)?;
        if !has_only_xml_characters(&value) {
            return Err(PhoneXmlError::InvalidEntity);
        }
    }
    Ok(())
}

macro_rules! impl_validated_string_value {
    ($($value:ty),+ $(,)?) => {
        $(
            impl AsRef<str> for $value {
                fn as_ref(&self) -> &str {
                    self.as_str()
                }
            }

            impl TryFrom<String> for $value {
                type Error = PhoneXmlError;

                fn try_from(value: String) -> Result<Self, Self::Error> {
                    Self::new(value)
                }
            }

            impl FromStr for $value {
                type Err = PhoneXmlError;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    Self::new(value)
                }
            }
        )+
    };
}

impl_validated_string_value!(
    PhoneInputParameterName,
    PhoneExecuteUrl,
    PhoneImageUrl,
    PhoneBackgroundTftpUrl,
    PhoneBackgroundHttpUrl,
    PhoneRingtoneUrl,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_contract_rejects_a_valid_but_wrong_schema_root() {
        let menu =
            br#"<CiscoIPPhoneMenu><Title>Menu</Title><Prompt>Choose</Prompt></CiscoIPPhoneMenu>"#;

        assert!(matches!(
            CiscoIpPhoneText::from_xml(menu),
            Err(PhoneXmlError::InvalidField {
                field: "phone XML document root",
                ..
            })
        ));
    }

    #[test]
    fn typed_boundary_round_trips_escaped_menu_text() {
        let expected = CiscoIpPhoneMenu::new(
            "Support <East> & West",
            "Choose \"one\"",
            vec![CiscoIpPhoneMenuItem {
                name: Some("Alice & Bob".into()),
                url: Some("UserData:1:0:select/701?lot=east&side=west".into()),
            }],
        )
        .unwrap();
        let xml = to_string(&expected, 2_000).unwrap();
        assert!(xml.contains("Support &lt;East&gt; &amp; West"));
        assert_eq!(
            from_bytes::<CiscoIpPhoneMenu>(xml.as_bytes(), 2_000).unwrap(),
            expected
        );
    }

    #[test]
    fn typed_boundary_rejects_size_utf8_doctype_entities_and_malformed_xml() {
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(b"<CiscoIPPhoneMenu/>", 5),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(&[0xff], 5),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        let dtd = br#"<!DOCTYPE menu [<!ENTITY name "caller">]><CiscoIPPhoneMenu><Title>&name;</Title><Prompt/></CiscoIPPhoneMenu>"#;
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(dtd, 2_000),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        let external = br#"<!DOCTYPE menu SYSTEM "file:///untrusted/menu.dtd"><CiscoIPPhoneMenu><Title/><Prompt/></CiscoIPPhoneMenu>"#;
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(external, 2_000),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(
                b"<CiscoIPPhoneMenu><Title>&custom;</Title><Prompt/></CiscoIPPhoneMenu>",
                2_000,
            ),
            Err(PhoneXmlError::InvalidEntity)
        ));
        assert!(from_bytes::<CiscoIpPhoneMenu>(b"<CiscoIPPhoneMenu>", 2_000).is_err());

        let mut oversized = CiscoIpPhoneMenu::new("Menu", "Choose", Vec::new()).unwrap();
        oversized.title = Some("x".repeat(100));
        assert!(matches!(
            to_string(&oversized, 10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
    }

    fn complete_text_document() -> CiscoIpPhoneText {
        CiscoIpPhoneText {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("text-service".into()),
            on_focus_lost: Some("Notify:focus?state=lost&view=text".into()),
            on_focus_gained: Some("Notify:focus?state=gained".into()),
            on_minimized: Some("Notify:minimized".into()),
            on_closed: Some("Notify:closed".into()),
            title: Some("Message <East> & West".into()),
            prompt: Some("Read & refresh".into()),
            soft_keys: vec![CiscoIpPhoneSoftKeyItem {
                name: Some("Refresh".into()),
                position: PhoneSoftKeyPosition::new(1).unwrap(),
                url: Some("https://pbx.example/text?id=7&view=full".into()),
                url_down: Some("SoftKey:Update".into()),
            }],
            key_items: vec![CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some("SoftKey:Exit".into()),
                url_down: None,
            }],
            text: Some("Line one\nCafé <ready> & waiting\t✓".into()),
        }
    }

    #[test]
    fn text_document_round_trips_controls_order_utf8_and_escaping() {
        let expected = complete_text_document();
        let xml = expected.to_xml().unwrap();
        assert!(xml.contains("Message &lt;East&gt; &amp; West"));
        assert!(xml.contains("Café &lt;ready&gt; &amp; waiting"));
        assert!(xml.contains("id=7&amp;view=full"));
        assert!(xml.find("<SoftKeyItem>").unwrap() < xml.find("<KeyItem>").unwrap());
        assert!(xml.find("<KeyItem>").unwrap() < xml.find("<Text>").unwrap());
        assert_eq!(
            CiscoIpPhoneText::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let minimal = CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneText/>").unwrap();
        assert!(minimal.text.is_none());
        let empty =
            CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneText><Text></Text></CiscoIPPhoneText>")
                .unwrap();
        assert_eq!(empty.text.as_deref(), Some(""));
    }

    #[test]
    fn text_document_enforces_body_control_soft_key_and_refresh_bounds() {
        let exact = CiscoIpPhoneText::new("Title", "Prompt", "é".repeat(PHONE_TEXT_MAX_CHARS));
        assert!(exact.is_ok());
        assert!(matches!(
            CiscoIpPhoneText::new("Title", "Prompt", "x".repeat(PHONE_TEXT_MAX_CHARS + 1),),
            Err(PhoneXmlError::InvalidField {
                field: "phone text body",
                ..
            })
        ));
        let mut invalid = complete_text_document();
        invalid.text = Some("not\u{1} XML".into());
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField {
                field: "phone text body",
                ..
            })
        ));
        invalid = complete_text_document();
        invalid.soft_keys[0].position = PhoneSoftKeyPosition::new(16).unwrap();
        assert!(invalid.to_xml().is_ok());
        invalid = complete_text_document();
        invalid.soft_keys[0].url = Some("x".repeat(PHONE_XML_URL_MAX_CHARS + 1));
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));

        assert_eq!(PhoneServicePriority::LOW.wire(), 0);
        assert_eq!(PhoneServicePriority::NORMAL.wire(), 1);
        assert_eq!(PhoneServicePriority::HIGH.wire(), 2);
        assert_eq!(
            PhoneServicePriority::default(),
            PhoneServicePriority::NORMAL
        );
        assert!(PhoneServicePriority::new(3).is_err());
        let refresh = PhoneXmlRefresh::new(15, "https://pbx.example/text?page=2").unwrap();
        assert_eq!(refresh.delay_seconds(), 15);
        assert_eq!(refresh.url(), "https://pbx.example/text?page=2");
        assert_eq!(
            refresh.http_header_value(),
            "15;url=https://pbx.example/text?page=2"
        );
        assert!(PhoneXmlRefresh::new(0, "").is_err());
        assert!(PhoneXmlRefresh::new(0, "x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneXmlRefresh::new(0, "https://example.test/é").is_err());
        assert!(PhoneXmlRefresh::new(0, "https://example.test/not encoded").is_err());
        assert!(PhoneXmlRefresh::new(0, "https://example.test/\r\nInjected: yes").is_err());
    }

    #[test]
    fn text_parser_rejects_wrong_root_malformed_oversize_nesting_dtd_and_entities() {
        assert!(CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneMenu/>").is_err());
        assert!(
            CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneText><Unknown/></CiscoIPPhoneText>",)
                .is_err()
        );
        assert!(CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneText><Text>").is_err());
        assert!(matches!(
            CiscoIpPhoneText::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneText::from_xml(
                b"<!DOCTYPE text [<!ENTITY value 'secret'>]><CiscoIPPhoneText><Text>&value;</Text></CiscoIPPhoneText>",
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(
            CiscoIpPhoneText::from_xml(
                b"<CiscoIPPhoneText><Text>&unknown;</Text></CiscoIPPhoneText>",
            )
            .is_err()
        );
        assert!(matches!(
            complete_text_document().to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        let nested = format!(
            "<CiscoIPPhoneText>{}<Text>body</Text>{}</CiscoIPPhoneText>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneText::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_text_document(),
                PHONE_TEXT_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_input_document() -> CiscoIpPhoneInput {
        CiscoIpPhoneInput {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("conference-invite".into()),
            on_focus_lost: Some("Notify:input?focus=lost&view=invite".into()),
            on_focus_gained: Some("Notify:input?focus=gained".into()),
            on_minimized: Some("Notify:input?state=minimized".into()),
            on_closed: Some("Notify:input?state=closed".into()),
            title: Some("Invite <guest>".into()),
            prompt: Some("Enter name & number".into()),
            soft_keys: vec![CiscoIpPhoneSoftKeyItem {
                name: Some("Submit".into()),
                position: PhoneSoftKeyPosition::new(1).unwrap(),
                url: Some("SoftKey:Submit".into()),
                url_down: Some("Notify:submit?state=down".into()),
            }],
            key_items: vec![CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some("SoftKey:Exit".into()),
                url_down: None,
            }],
            url: "UserData:9091:0:conference/7/invite?source=phone&mode=full".into(),
            items: vec![
                CiscoIpPhoneInputItem {
                    display_name: Some("Number".into()),
                    parameter: PhoneInputParameterName::new("NUMBER").unwrap(),
                    flags: PhoneInputFlags::Telephone,
                    default_value: Some("+1 555 0100".into()),
                },
                CiscoIpPhoneInputItem {
                    display_name: Some("Name & team".into()),
                    parameter: PhoneInputParameterName::new("NAME&TEAM").unwrap(),
                    flags: PhoneInputFlags::AlphabeticPassword,
                    default_value: Some("Café <guest>".into()),
                },
            ],
        }
    }

    #[test]
    fn input_document_round_trips_every_control_in_schema_order_and_escapes_values() {
        let expected = complete_input_document();
        let xml = expected.to_xml().unwrap();
        assert!(xml.contains("Invite &lt;guest&gt;"));
        assert!(xml.contains("Enter name &amp; number"));
        assert!(xml.contains("NAME&amp;TEAM"));
        assert!(xml.contains("Café &lt;guest&gt;"));
        assert!(xml.contains("source=phone&amp;mode=full"));
        assert!(xml.find("<SoftKeyItem>").unwrap() < xml.find("<KeyItem>").unwrap());
        let submission = xml.find("<URL>UserData:").unwrap();
        assert!(xml.find("<KeyItem>").unwrap() < submission);
        assert!(submission < xml.find("<InputItem>").unwrap());
        assert_eq!(
            CiscoIpPhoneInput::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let minimal = CiscoIpPhoneInput::from_xml(
            b"<CiscoIPPhoneInput><URL>submit</URL></CiscoIPPhoneInput>",
        )
        .unwrap();
        assert!(minimal.items.is_empty());
        assert_eq!(minimal.url, "submit");
    }

    #[test]
    fn input_flags_round_trip_every_accepted_schema_value() {
        let codes = [
            "A", "T", "N", "E", "U", "L", "AP", "TP", "NP", "EP", "UP", "LP", "PA", "PT", "PN",
            "PE", "PU", "PL",
        ];
        for (flags, code) in PhoneInputFlags::ALL.into_iter().zip(codes) {
            let document = CiscoIpPhoneInput::new(
                "Input",
                "Enter value",
                "submit",
                vec![CiscoIpPhoneInputItem {
                    display_name: None,
                    parameter: PhoneInputParameterName::new("VALUE").unwrap(),
                    flags,
                    default_value: Some(String::new()),
                }],
            )
            .unwrap();
            let xml = document.to_xml().unwrap();
            assert!(xml.contains(&format!("<InputFlags>{code}</InputFlags>")));
            assert_eq!(
                CiscoIpPhoneInput::from_xml(xml.as_bytes()).unwrap(),
                document
            );
        }
    }

    #[test]
    fn input_document_enforces_field_collection_and_display_bounds() {
        assert!(PhoneInputParameterName::new("").is_err());
        assert!(PhoneInputParameterName::new("x".repeat(33)).is_err());
        assert!(PhoneInputParameterName::new("not\u{1}xml").is_err());

        let exact = CiscoIpPhoneInput::new(
            "t".repeat(32),
            "p".repeat(32),
            "u".repeat(PHONE_XML_URL_MAX_CHARS),
            vec![CiscoIpPhoneInputItem {
                display_name: Some("n".repeat(32)),
                parameter: PhoneInputParameterName::new("q".repeat(32)).unwrap(),
                flags: PhoneInputFlags::Numeric,
                default_value: Some("d".repeat(32)),
            }],
        );
        assert!(exact.is_ok());

        let too_many = (0..=PHONE_INPUT_MAX_ITEMS)
            .map(|index| CiscoIpPhoneInputItem {
                display_name: None,
                parameter: PhoneInputParameterName::new(format!("VALUE{index}")).unwrap(),
                flags: PhoneInputFlags::Alphabetic,
                default_value: None,
            })
            .collect();
        assert!(matches!(
            CiscoIpPhoneInput::new("Input", "Prompt", "submit", too_many),
            Err(PhoneXmlError::LimitExceeded {
                kind: "phone input fields",
                maximum: PHONE_INPUT_MAX_ITEMS,
                ..
            })
        ));

        for invalid in [
            CiscoIpPhoneInput::new("x".repeat(33), "Prompt", "submit", Vec::new()),
            CiscoIpPhoneInput::new("Input", "x".repeat(33), "submit", Vec::new()),
            CiscoIpPhoneInput::new("Input", "Prompt", "", Vec::new()),
            CiscoIpPhoneInput::new(
                "Input",
                "Prompt",
                "x".repeat(PHONE_XML_URL_MAX_CHARS + 1),
                Vec::new(),
            ),
        ] {
            assert!(invalid.is_err());
        }

        let mut invalid = complete_input_document();
        invalid.items[0].display_name = Some("x".repeat(33));
        assert!(invalid.to_xml().is_err());
        invalid = complete_input_document();
        invalid.items[0].default_value = Some("x".repeat(33));
        assert!(invalid.to_xml().is_err());
        assert!(PhoneSoftKeyPosition::new(0).is_err());
    }

    #[test]
    fn input_parser_rejects_wrong_root_unknown_flag_malformed_and_unsafe_documents() {
        assert!(CiscoIpPhoneInput::from_xml(b"<CiscoIPPhoneText/>").is_err());
        assert!(CiscoIpPhoneInput::from_xml(b"<CiscoIPPhoneInput/>").is_err());
        assert!(
            CiscoIpPhoneInput::from_xml(
                b"<CiscoIPPhoneInput><Unknown/><URL>submit</URL></CiscoIPPhoneInput>"
            )
            .is_err()
        );
        assert!(CiscoIpPhoneInput::from_xml(
            b"<CiscoIPPhoneInput><URL>submit</URL><InputItem><QueryStringParam>q</QueryStringParam><InputFlags>Q</InputFlags></InputItem></CiscoIPPhoneInput>"
        )
        .is_err());
        assert!(CiscoIpPhoneInput::from_xml(b"<CiscoIPPhoneInput><URL>").is_err());
        assert!(matches!(
            CiscoIpPhoneInput::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneInput::from_xml(
                b"<!DOCTYPE input [<!ENTITY value 'secret'>]><CiscoIPPhoneInput><URL>&value;</URL></CiscoIPPhoneInput>",
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(
            CiscoIpPhoneInput::from_xml(
                b"<CiscoIPPhoneInput><URL>&unknown;</URL></CiscoIPPhoneInput>"
            )
            .is_err()
        );
        assert!(matches!(
            complete_input_document().to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        let encoded = complete_input_document().to_xml().unwrap();
        assert!(matches!(
            CiscoIpPhoneInput::from_xml_with_limit(encoded.as_bytes(), 10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        let nested = format!(
            "<CiscoIPPhoneInput>{}<URL>submit</URL>{}</CiscoIPPhoneInput>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneInput::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_input_document(),
                PHONE_INPUT_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_execute_document() -> CiscoIpPhoneExecute {
        CiscoIpPhoneExecute::new(vec![
            CiscoIpPhoneExecuteItem::with_priority(
                "Key:Directories?name=Café&view=<all>",
                PhoneExecutePriority::LOW,
            )
            .unwrap(),
            CiscoIpPhoneExecuteItem::with_priority(
                "Application:PlacedCalls",
                PhoneExecutePriority::HIGH,
            )
            .unwrap(),
            CiscoIpPhoneExecuteItem::new("Init:Services").unwrap(),
        ])
        .unwrap()
    }

    #[test]
    fn execute_document_round_trips_order_optional_priority_utf8_and_escaping() {
        let expected = complete_execute_document();
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<CiscoIPPhoneExecute>"));
        assert!(xml.contains(
            r#"<ExecuteItem Priority="0" URL="Key:Directories?name=Café&amp;view=&lt;all&gt;"/>"#
        ));
        assert!(xml.contains(r#"<ExecuteItem Priority="2" URL="Application:PlacedCalls"/>"#));
        assert!(xml.contains(r#"<ExecuteItem URL="Init:Services"/>"#));
        assert_eq!(
            CiscoIpPhoneExecute::from_xml(xml.as_bytes()).unwrap(),
            expected
        );
        assert_eq!(
            expected
                .items
                .iter()
                .map(|item| item.url.as_str())
                .collect::<Vec<_>>(),
            [
                "Key:Directories?name=Café&view=<all>",
                "Application:PlacedCalls",
                "Init:Services",
            ]
        );
    }

    #[test]
    fn execute_document_enforces_action_priority_url_and_collection_bounds() {
        assert_eq!(PhoneExecutePriority::LOW.wire(), 0);
        assert_eq!(PhoneExecutePriority::NORMAL.wire(), 1);
        assert_eq!(PhoneExecutePriority::HIGH.wire(), 2);
        assert!(PhoneExecutePriority::new(3).is_err());
        assert!(PhoneExecuteUrl::new("").is_err());
        assert!(PhoneExecuteUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneExecuteUrl::new("not\u{1}xml").is_err());

        assert!(matches!(
            CiscoIpPhoneExecute::new(Vec::new()),
            Err(PhoneXmlError::InvalidField {
                field: "phone execute actions",
                ..
            })
        ));
        let maximum = (0..PHONE_EXECUTE_MAX_ITEMS)
            .map(|index| CiscoIpPhoneExecuteItem::new(format!("Key:KeyPad{index}")).unwrap())
            .collect();
        assert!(CiscoIpPhoneExecute::new(maximum).is_ok());
        assert!(matches!(
            CiscoIpPhoneExecute::new(vec![
                CiscoIpPhoneExecuteItem::new("https://example.test/one").unwrap(),
                CiscoIpPhoneExecuteItem::new("http://example.test/two").unwrap(),
            ]),
            Err(PhoneXmlError::InvalidField {
                field: "phone execute HTTP actions",
                ..
            })
        ));
        let too_many = (0..=PHONE_EXECUTE_MAX_ITEMS)
            .map(|index| CiscoIpPhoneExecuteItem::new(format!("Key:KeyPad{index}")).unwrap())
            .collect();
        assert!(matches!(
            CiscoIpPhoneExecute::new(too_many),
            Err(PhoneXmlError::LimitExceeded {
                kind: "phone execute actions",
                maximum: PHONE_EXECUTE_MAX_ITEMS,
                ..
            })
        ));
    }

    #[test]
    fn execute_parser_rejects_wrong_root_malformed_unsafe_and_oversized_documents() {
        assert!(CiscoIpPhoneExecute::from_xml(b"<CiscoIPPhoneMenu/>").is_err());
        assert!(CiscoIpPhoneExecute::from_xml(b"<CiscoIPPhoneExecute/>").is_err());
        assert!(CiscoIpPhoneExecute::from_xml(
            br#"<CiscoIPPhoneExecute><ExecuteItem Priority="3" URL="Init:Services"/></CiscoIPPhoneExecute>"#
        )
        .is_err());
        assert!(
            CiscoIpPhoneExecute::from_xml(
                br#"<CiscoIPPhoneExecute><ExecuteItem Priority="0"/></CiscoIPPhoneExecute>"#
            )
            .is_err()
        );
        assert!(
            CiscoIpPhoneExecute::from_xml(
                br#"<CiscoIPPhoneExecute><ExecuteItem URL=""/></CiscoIPPhoneExecute>"#
            )
            .is_err()
        );
        let oversized_url = format!(
            "<CiscoIPPhoneExecute><ExecuteItem URL=\"{}\"/></CiscoIPPhoneExecute>",
            "x".repeat(PHONE_XML_URL_MAX_CHARS + 1),
        );
        assert!(CiscoIpPhoneExecute::from_xml(oversized_url.as_bytes()).is_err());
        let too_many_actions = format!(
            "<CiscoIPPhoneExecute>{}</CiscoIPPhoneExecute>",
            r#"<ExecuteItem URL="Init:Services"/>"#.repeat(PHONE_EXECUTE_MAX_ITEMS + 1),
        );
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(too_many_actions.as_bytes()),
            Err(PhoneXmlError::LimitExceeded {
                kind: "phone execute actions",
                maximum: PHONE_EXECUTE_MAX_ITEMS,
                ..
            })
        ));
        assert!(CiscoIpPhoneExecute::from_xml(
            br#"<CiscoIPPhoneExecute><ExecuteItem Unknown="yes" URL="Init:Services"/></CiscoIPPhoneExecute>"#
        )
        .is_err());
        assert!(
            CiscoIpPhoneExecute::from_xml(
                b"<CiscoIPPhoneExecute><ExecuteItem URL=\"Init:Services\"></CiscoIPPhoneExecute>"
            )
            .is_err()
        );
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(
                br#"<!DOCTYPE execute [<!ENTITY action "Init:Services">]><CiscoIPPhoneExecute><ExecuteItem URL="&action;"/></CiscoIPPhoneExecute>"#,
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(
            CiscoIpPhoneExecute::from_xml(
                br#"<CiscoIPPhoneExecute><ExecuteItem URL="&unknown;"/></CiscoIPPhoneExecute>"#
            )
            .is_err()
        );
        let encoded = complete_execute_document().to_xml().unwrap();
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml_with_limit(encoded.as_bytes(), 10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            complete_execute_document().to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        let nested = format!(
            "<CiscoIPPhoneExecute>{}<ExecuteItem URL=\"Init:Services\"/>{}</CiscoIPPhoneExecute>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_execute_document(),
                PHONE_EXECUTE_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    #[test]
    fn declared_iso_8859_1_input_decodes_before_schema_validation() {
        let mut document = br#"<?xml version="1.0" encoding = 'ISO-8859-1'?><CiscoIPPhoneExecute><ExecuteItem URL="Key:Caf"#
            .to_vec();
        document.push(0xe9);
        document.extend_from_slice(br#""/></CiscoIPPhoneExecute>"#);
        let parsed = CiscoIpPhoneExecute::from_xml(&document).unwrap();
        assert_eq!(parsed.items[0].url.as_str(), "Key:Café");
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(&[b'<', 0xe9, b'>']),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
    }

    fn background_list_item(name: &str) -> CiscoIpPhoneImageListItem {
        CiscoIpPhoneImageListItem {
            thumbnail_url: PhoneBackgroundTftpUrl::new(format!(
                "TFTP:Desktops/320x212x16/TN-{name}.png"
            ))
            .unwrap(),
            image_url: PhoneBackgroundTftpUrl::new(format!("TFTP:Desktops/320x212x16/{name}.png"))
                .unwrap(),
        }
    }

    #[test]
    fn background_image_list_round_trips_order_attributes_and_escaping() {
        let expected = CiscoIpPhoneImageList::new(vec![
            background_list_item("Fountain"),
            background_list_item("Moon&Stars"),
        ])
        .unwrap();
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<CiscoIPPhoneImageList>"));
        assert!(xml.contains(
            r#"<ImageItem Image="TFTP:Desktops/320x212x16/TN-Fountain.png" URL="TFTP:Desktops/320x212x16/Fountain.png"/>"#
        ));
        assert!(xml.contains("TN-Moon&amp;Stars.png"));
        assert!(xml.find("Fountain.png").unwrap() < xml.find("Moon&amp;Stars.png").unwrap());
        assert_eq!(
            CiscoIpPhoneImageList::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let empty = CiscoIpPhoneImageList::from_xml(b"<CiscoIPPhoneImageList/>").unwrap();
        assert!(empty.items.is_empty());
    }

    #[test]
    fn background_control_documents_round_trip_exact_evidenced_roots_and_order() {
        let image =
            PhoneBackgroundHttpUrl::new("http://pbx.example/background.png?site=east&screen=main")
                .unwrap();
        let thumbnail =
            PhoneBackgroundHttpUrl::new("http://pbx.example/background-thumb.png").unwrap();
        let set = CiscoIpPhoneSetBackground::new(image.clone(), thumbnail);
        let xml = set.to_xml().unwrap();
        assert_eq!(
            xml,
            "<setBackground><background><image>http://pbx.example/background.png?site=east&amp;screen=main</image><icon>http://pbx.example/background-thumb.png</icon></background></setBackground>"
        );
        assert_eq!(
            CiscoIpPhoneSetBackground::from_xml(xml.as_bytes()).unwrap(),
            set
        );
        assert_eq!(
            PhoneBackgroundControlDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneBackgroundControlDocument::Set(set)
        );

        let preview = CiscoIpPhoneSetBackgroundPreview::new(image);
        let xml = preview.to_xml().unwrap();
        assert_eq!(
            xml,
            "<setBackgroundPreview><image>http://pbx.example/background.png?site=east&amp;screen=main</image></setBackgroundPreview>"
        );
        assert_eq!(
            CiscoIpPhoneSetBackgroundPreview::from_xml(xml.as_bytes()).unwrap(),
            preview
        );
        assert_eq!(
            PhoneBackgroundControlDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneBackgroundControlDocument::Preview(preview)
        );
    }

    #[test]
    fn background_urls_enforce_transport_shape_length_and_secret_safe_errors() {
        assert_eq!(
            PhoneBackgroundTftpUrl::new("TFTP:Desktops/800x480x24/Picture.PNG")
                .unwrap()
                .as_str(),
            "TFTP:Desktops/800x480x24/Picture.PNG"
        );
        assert_eq!(
            PhoneBackgroundHttpUrl::new("http://[2001:db8::1]:8080/image.png?size=full")
                .unwrap()
                .as_str(),
            "http://[2001:db8::1]:8080/image.png?size=full"
        );
        assert!(PhoneBackgroundHttpUrl::new("https://pbx.example/image.png").is_ok());
        for invalid in [
            "",
            "HTTP:Desktops/320x212x16/image.png",
            "TFTP://server/Desktops/image.png",
            "TFTP:/Desktops/image.png",
            "TFTP:Desktops/../image.png",
            "TFTP:Desktops/%2e%2e/image.png",
            "TFTP:Desktops/%2Fprivate/image.png",
            "TFTP:Desktops/%00private.png",
            "TFTP:Desktops/%Q0private.png",
            "TFTP:Desktops/image.jpg",
            "TFTP:Desktops/image.png?token=private",
            "TFTP:Desktops/image.png#private",
        ] {
            let error = PhoneBackgroundTftpUrl::new(invalid).unwrap_err();
            if !invalid.is_empty() {
                assert!(!error.to_string().contains(invalid));
            }
        }
        for invalid in [
            "",
            "ftp://pbx.example/private.png",
            "TFTP:Desktops/image.png",
            "background.png",
            "http://user:secret@pbx.example/private.png",
            "http://pbx.example/private.png#token",
        ] {
            let error = PhoneBackgroundHttpUrl::new(invalid).unwrap_err();
            if !invalid.is_empty() {
                assert!(!error.to_string().contains(invalid));
            }
        }
        assert!(PhoneBackgroundTftpUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneBackgroundHttpUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneBackgroundTftpUrl::new("TFTP:Desktops/not\u{1}xml.png").is_err());
        assert!(PhoneBackgroundHttpUrl::new("http://pbx.example/not\u{1}xml.png").is_err());
        assert!(
            !format!(
                "{:?}",
                PhoneBackgroundHttpUrl::new("http://private.example/secret.png").unwrap()
            )
            .contains("private.example")
        );
    }

    #[test]
    fn background_image_list_enforces_collection_and_document_bounds() {
        let maximum = (0..PHONE_BACKGROUND_LIST_MAX_ITEMS)
            .map(|index| background_list_item(&format!("image-{index}")))
            .collect();
        assert!(CiscoIpPhoneImageList::new(maximum).is_ok());

        let too_many = (0..=PHONE_BACKGROUND_LIST_MAX_ITEMS)
            .map(|index| background_list_item(&format!("image-{index}")))
            .collect();
        assert!(matches!(
            CiscoIpPhoneImageList::new(too_many),
            Err(PhoneXmlError::LimitExceeded {
                kind: "background image choices",
                maximum: PHONE_BACKGROUND_LIST_MAX_ITEMS,
                ..
            })
        ));

        let document = CiscoIpPhoneImageList::new(vec![background_list_item("image")]).unwrap();
        assert!(matches!(
            document.to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            CiscoIpPhoneImageList::from_xml(&vec![b'x'; PHONE_BACKGROUND_LIST_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        let preview =
            PhoneBackgroundControlDocument::Preview(CiscoIpPhoneSetBackgroundPreview::new(
                PhoneBackgroundHttpUrl::new("http://pbx.example/image.png").unwrap(),
            ));
        assert!(matches!(
            preview.to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
    }

    #[test]
    fn background_parser_rejects_wrong_roots_unknowns_malformed_and_unsafe_xml() {
        for invalid in [
            b"<CiscoIPPhoneMenu/>".as_slice(),
            b"<CiscoIPPhoneImageList><ImageItem Image=\"TFTP:Desktops/TN.png\"/></CiscoIPPhoneImageList>".as_slice(),
            b"<CiscoIPPhoneImageList><ImageItem Image=\"TFTP:Desktops/TN.png\" URL=\"TFTP:Desktops/image.png\" Unknown=\"yes\"/></CiscoIPPhoneImageList>".as_slice(),
            b"<CiscoIPPhoneImageList>".as_slice(),
        ] {
            assert!(CiscoIpPhoneImageList::from_xml(invalid).is_err());
        }
        assert!(CiscoIpPhoneSetBackground::from_xml(
            b"<setBackgroundPreview><image>http://pbx.example/image.png</image></setBackgroundPreview>"
        )
        .is_err());
        assert!(CiscoIpPhoneSetBackgroundPreview::from_xml(
            b"<setBackgroundPreview><image>ftp://pbx.example/image.png</image></setBackgroundPreview>"
        )
        .is_err());
        assert!(PhoneBackgroundControlDocument::from_xml(b"<getDeviceCaps/>").is_err());
        assert!(matches!(
            CiscoIpPhoneImageList::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneImageList::from_xml(
                br#"<!DOCTYPE images [<!ENTITY path "private">]><CiscoIPPhoneImageList><ImageItem Image="TFTP:Desktops/&path;-TN.png" URL="TFTP:Desktops/&path;.png"/></CiscoIPPhoneImageList>"#,
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(CiscoIpPhoneImageList::from_xml(
            br#"<CiscoIPPhoneImageList><ImageItem Image="TFTP:Desktops/&unknown;-TN.png" URL="TFTP:Desktops/image.png"/></CiscoIPPhoneImageList>"#,
        )
        .is_err());
        let nested = format!(
            "<CiscoIPPhoneImageList>{}<ImageItem Image=\"TFTP:Desktops/TN.png\" URL=\"TFTP:Desktops/image.png\"/>{}</CiscoIPPhoneImageList>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneImageList::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &CiscoIpPhoneImageList::new(vec![background_list_item("image")]).unwrap(),
                PHONE_BACKGROUND_LIST_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    #[test]
    fn ringtone_document_round_trips_exact_root_child_order_and_escaping() {
        let url =
            PhoneRingtoneUrl::new("http://pbx.example/ringtones/Classic.raw?site=east&set=primary")
                .unwrap();
        assert_eq!(
            url.as_str(),
            "http://pbx.example/ringtones/Classic.raw?site=east&set=primary"
        );
        assert_eq!(
            url.clone().into_string(),
            "http://pbx.example/ringtones/Classic.raw?site=east&set=primary"
        );
        let expected = CiscoIpPhoneSetRingTone::new(url);
        let xml = expected.to_xml().unwrap();
        assert_eq!(
            xml,
            "<setRingTone><ringTone>http://pbx.example/ringtones/Classic.raw?site=east&amp;set=primary</ringTone></setRingTone>"
        );
        assert_eq!(
            CiscoIpPhoneSetRingTone::from_xml(xml.as_bytes()).unwrap(),
            expected
        );
    }

    #[test]
    fn ringtone_url_enforces_transport_shape_length_and_secret_safe_errors() {
        assert_eq!(
            PhoneRingtoneUrl::new("http://[2001:db8::1]:8080/ringtones/Office.raw?locale=sv")
                .unwrap()
                .as_str(),
            "http://[2001:db8::1]:8080/ringtones/Office.raw?locale=sv"
        );
        for invalid in [
            "",
            "HTTP://pbx.example/ringtone.raw",
            "https://pbx.example/ringtone.raw",
            "TFTP:Ringlist.xml",
            "ringtone.raw",
            "http://user:secret@pbx.example/private.raw",
            "http://pbx.example/private.raw#secret",
            "http://pbx.example/not allowed.raw",
            "http://pbx.example/not\tallowed.raw",
            "http://pbx.example/not\\allowed.raw",
            "http://pbx.example/not%Q0allowed.raw",
        ] {
            let error = PhoneRingtoneUrl::new(invalid).unwrap_err();
            if !invalid.is_empty() {
                assert!(!error.to_string().contains(invalid));
            }
        }
        assert!(PhoneRingtoneUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneRingtoneUrl::new("http://pbx.example/not\u{1}xml.raw").is_err());
        assert!(
            !format!(
                "{:?}",
                PhoneRingtoneUrl::new("http://private.example/secret.raw").unwrap()
            )
            .contains("private.example")
        );
    }

    #[test]
    fn ringtone_parser_rejects_wrong_root_unknown_malformed_unsafe_and_bounded_xml() {
        for invalid in [
            b"<setBackground><ringTone>http://pbx.example/r.raw</ringTone></setBackground>"
                .as_slice(),
            b"<setRingTone/>".as_slice(),
            b"<setRingTone unknown=\"yes\"><ringTone>http://pbx.example/r.raw</ringTone></setRingTone>"
                .as_slice(),
            b"<setRingTone><ringTone>http://pbx.example/r.raw</ringTone><Unknown/></setRingTone>"
                .as_slice(),
            b"<setRingTone><ringTone>https://pbx.example/r.raw</ringTone></setRingTone>"
                .as_slice(),
            b"<setRingTone><ringTone>".as_slice(),
        ] {
            assert!(CiscoIpPhoneSetRingTone::from_xml(invalid).is_err());
        }
        assert!(matches!(
            CiscoIpPhoneSetRingTone::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneSetRingTone::from_xml(
                br#"<!DOCTYPE ringtone [<!ENTITY host "private.example">]><setRingTone><ringTone>http://&host;/r.raw</ringTone></setRingTone>"#,
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(
            CiscoIpPhoneSetRingTone::from_xml(
                b"<setRingTone><ringTone>http://&unknown;/r.raw</ringTone></setRingTone>",
            )
            .is_err()
        );
        assert!(matches!(
            CiscoIpPhoneSetRingTone::from_xml(&vec![b'x'; PHONE_RINGTONE_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded {
                maximum: PHONE_RINGTONE_MAX_BYTES,
                ..
            })
        ));

        let nested = format!(
            "<setRingTone>{}<ringTone>http://pbx.example/r.raw</ringTone>{}</setRingTone>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneSetRingTone::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        let document = CiscoIpPhoneSetRingTone::new(
            PhoneRingtoneUrl::new("http://pbx.example/r.raw").unwrap(),
        );
        assert!(matches!(
            document.to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(FailingWriter, &document, PHONE_RINGTONE_MAX_BYTES),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn image_soft_keys() -> Vec<CiscoIpPhoneSoftKeyItem> {
        vec![CiscoIpPhoneSoftKeyItem {
            name: Some("Select & view".into()),
            position: PhoneSoftKeyPosition::new(1).unwrap(),
            url: Some("SoftKey:Select?view=image&side=west".into()),
            url_down: Some("Notify:select?state=down".into()),
        }]
    }

    fn image_key_items() -> Vec<CiscoIpPhoneKeyItem> {
        vec![CiscoIpPhoneKeyItem {
            key: PhoneXmlKey::NavSelect,
            url: Some("Key:Select?view=image&side=west".into()),
            url_down: None,
        }]
    }

    fn complete_bitmap_image() -> CiscoIpPhoneImage {
        CiscoIpPhoneImage {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("image-service".into()),
            on_focus_lost: Some("Notify:image?focus=lost".into()),
            on_focus_gained: Some("Notify:image?focus=gained".into()),
            on_minimized: Some("Notify:image?state=minimized".into()),
            on_closed: Some("Notify:image?state=closed".into()),
            title: Some("Café <map> & menu".into()),
            prompt: Some("Choose & inspect".into()),
            soft_keys: image_soft_keys(),
            key_items: image_key_items(),
            location_x: Some(-1),
            location_y: Some(64),
            width: 133,
            height: 65,
            depth: 2,
            data: Some(PhoneBitmapData::new(vec![0x00, 0xab, 0xff]).unwrap()),
        }
    }

    fn complete_image_file() -> CiscoIpPhoneImageFile {
        CiscoIpPhoneImageFile {
            keypad_target: Some(PhoneKeypadTarget::Application),
            application_id: Some("image-file-service".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: Some("Notify:image-file?state=closed".into()),
            title: Some("Image <file>".into()),
            prompt: Some("Open & inspect".into()),
            soft_keys: image_soft_keys(),
            key_items: image_key_items(),
            location_x: Some(297),
            location_y: Some(-1),
            url: PhoneImageUrl::new("https://pbx.example/image.png?id=7&view=full").unwrap(),
        }
    }

    fn complete_graphic_menu() -> CiscoIpPhoneGraphicMenu {
        CiscoIpPhoneGraphicMenu {
            keypad_target: Some(PhoneKeypadTarget::ActiveCall),
            application_id: Some("graphic-menu".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some("Graphic menu".into()),
            prompt: Some("Choose a region".into()),
            soft_keys: image_soft_keys(),
            key_items: image_key_items(),
            location_x: Some(132),
            location_y: Some(-1),
            width: 1,
            height: 1,
            depth: 1,
            data: Some(PhoneBitmapData::new(vec![0x12, 0x34]).unwrap()),
            items: vec![CiscoIpPhoneMenuItem {
                name: Some("West <wing>".into()),
                url: Some("UserData:9095:0:image/west?floor=1&open=true".into()),
            }],
        }
    }

    fn complete_graphic_file_menu() -> CiscoIpPhoneGraphicFileMenu {
        CiscoIpPhoneGraphicFileMenu {
            keypad_target: None,
            application_id: Some("graphic-file-menu".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some("Floor plan".into()),
            prompt: Some("Touch a room".into()),
            soft_keys: image_soft_keys(),
            key_items: image_key_items(),
            location_x: Some(-1),
            location_y: Some(167),
            url: PhoneImageUrl::new("https://pbx.example/floor.png?site=east&floor=2").unwrap(),
            items: vec![CiscoIpPhoneTouchAreaMenuItem {
                name: Some("Room A & B".into()),
                url: Some("UserData:9095:0/room/a?mode=open&floor=2".into()),
                touch_area: Some(PhoneTouchArea {
                    x1: 4,
                    y1: 8,
                    x2: 90,
                    y2: 120,
                }),
            }],
        }
    }

    #[test]
    fn image_documents_round_trip_schema_order_hex_utf8_and_escaping() {
        let image = complete_bitmap_image();
        let xml = image.to_xml().unwrap();
        assert!(xml.contains("Café &lt;map&gt; &amp; menu"));
        assert!(xml.contains("<Data>00ABFF</Data>"));
        assert!(xml.find("<SoftKeyItem>").unwrap() < xml.find("<KeyItem>").unwrap());
        assert!(xml.find("<KeyItem>").unwrap() < xml.find("<LocationX>").unwrap());
        assert!(xml.find("<Depth>").unwrap() < xml.find("<Data>").unwrap());
        assert_eq!(CiscoIpPhoneImage::from_xml(xml.as_bytes()).unwrap(), image);
        assert_eq!(
            PhoneImageDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneImageDocument::Image(image)
        );

        let spaced_hex = b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>00 ab\nFF</Data></CiscoIPPhoneImage>";
        let parsed = CiscoIpPhoneImage::from_xml(spaced_hex).unwrap();
        assert_eq!(parsed.data.unwrap().as_bytes(), [0x00, 0xab, 0xff]);

        let image_file = complete_image_file();
        let xml = image_file.to_xml().unwrap();
        assert!(xml.contains("Image &lt;file&gt;"));
        assert!(xml.contains("id=7&amp;view=full"));
        assert!(xml.find("<KeyItem>").unwrap() < xml.find("<LocationX>").unwrap());
        let controls_end = xml.find("</KeyItem>").unwrap();
        let image_url = controls_end + xml[controls_end..].find("<URL>").unwrap();
        assert!(xml.find("<LocationY>").unwrap() < image_url);
        assert_eq!(
            PhoneImageDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneImageDocument::ImageFile(image_file)
        );

        let graphic = complete_graphic_menu();
        let xml = graphic.to_xml().unwrap();
        assert!(xml.contains("West &lt;wing&gt;"));
        assert!(xml.find("<Data>").unwrap() < xml.find("<MenuItem>").unwrap());
        assert_eq!(
            PhoneImageDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneImageDocument::GraphicMenu(graphic)
        );

        let graphic_file = complete_graphic_file_menu();
        let xml = graphic_file.to_xml().unwrap();
        assert!(xml.contains("Room A &amp; B"));
        assert!(xml.contains(r#"<TouchArea X1="4" Y1="8" X2="90" Y2="120"/>"#));
        let controls_end = xml.find("</KeyItem>").unwrap();
        let image_url = controls_end + xml[controls_end..].find("<URL>").unwrap();
        assert!(image_url < xml.find("<MenuItem>").unwrap());
        assert_eq!(
            PhoneImageDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneImageDocument::GraphicFileMenu(graphic_file)
        );
    }

    #[test]
    fn image_documents_enforce_exact_geometry_data_url_and_collection_bounds() {
        let mut image = complete_bitmap_image();
        assert!(image.validate().is_ok());
        image.location_x = Some(-2);
        assert!(image.validate().is_err());
        image.location_x = Some(133);
        assert!(image.validate().is_err());
        image.location_x = Some(0);
        image.location_y = Some(-2);
        assert!(image.validate().is_err());
        image.location_y = Some(65);
        assert!(image.validate().is_err());
        image.location_y = None;
        for (width, height, depth) in [
            (0, 1, 1),
            (134, 1, 1),
            (1, 0, 1),
            (1, 66, 1),
            (1, 1, 0),
            (1, 1, 3),
        ] {
            image.width = width;
            image.height = height;
            image.depth = depth;
            assert!(image.validate().is_err());
        }
        image.width = 1;
        image.height = 1;
        image.depth = 1;
        image.data = Some(PhoneBitmapData::new(vec![0; PHONE_IMAGE_BITMAP_MAX_BYTES]).unwrap());
        assert!(image.validate().is_ok());
        assert!(matches!(
            PhoneBitmapData::new(vec![0; PHONE_IMAGE_BITMAP_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded {
                kind: "bitmap image data bytes",
                maximum: PHONE_IMAGE_BITMAP_MAX_BYTES,
                ..
            })
        ));

        let mut image_file = complete_image_file();
        for x in [-2, 298] {
            image_file.location_x = Some(x);
            assert!(image_file.validate().is_err());
        }
        image_file.location_x = None;
        for y in [-2, 168] {
            image_file.location_y = Some(y);
            assert!(image_file.validate().is_err());
        }
        assert!(PhoneImageUrl::new("").is_err());
        assert!(PhoneImageUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneImageUrl::new("not\u{1}xml").is_err());

        let mut graphic = complete_graphic_menu();
        graphic.items = (0..PHONE_GRAPHIC_MENU_MAX_ITEMS)
            .map(|_| CiscoIpPhoneMenuItem {
                name: Some("x".repeat(64)),
                url: Some("x".repeat(PHONE_XML_URL_MAX_CHARS)),
            })
            .collect();
        assert!(graphic.validate().is_ok());
        graphic.items.push(CiscoIpPhoneMenuItem {
            name: None,
            url: None,
        });
        assert!(graphic.validate().is_err());
        graphic.items.truncate(1);
        graphic.items[0].name = Some("x".repeat(65));
        assert!(graphic.validate().is_err());

        let mut graphic_file = complete_graphic_file_menu();
        graphic_file.items = (0..PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS)
            .map(|_| CiscoIpPhoneTouchAreaMenuItem {
                name: Some("x".repeat(32)),
                url: Some("x".repeat(PHONE_XML_URL_MAX_CHARS)),
                touch_area: Some(PhoneTouchArea {
                    x1: u16::MIN,
                    y1: u16::MIN,
                    x2: u16::MAX,
                    y2: u16::MAX,
                }),
            })
            .collect();
        assert!(graphic_file.validate().is_ok());
        graphic_file.items.push(CiscoIpPhoneTouchAreaMenuItem {
            name: None,
            url: None,
            touch_area: None,
        });
        assert!(graphic_file.validate().is_err());
        graphic_file.items.truncate(1);
        graphic_file.items[0].name = Some("x".repeat(33));
        assert!(graphic_file.validate().is_err());
    }

    #[test]
    fn image_parsers_reject_wrong_roots_malformed_unsafe_nested_and_oversized_input() {
        assert!(
            CiscoIpPhoneImage::from_xml(
                b"<CiscoIPPhoneImageFile><URL>x</URL></CiscoIPPhoneImageFile>"
            )
            .is_err()
        );
        assert!(PhoneImageDocument::from_xml(b"<CiscoIPPhoneMenu/>").is_err());
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Unknown/></CiscoIPPhoneImage>").is_err());
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>123</Data></CiscoIPPhoneImage>").is_err());
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>zz</Data></CiscoIPPhoneImage>").is_err());
        assert!(CiscoIpPhoneGraphicFileMenu::from_xml(b"<CiscoIPPhoneGraphicFileMenu><URL>x</URL><MenuItem><TouchArea X1=\"bad\" Y1=\"0\" X2=\"1\" Y2=\"1\"/></MenuItem></CiscoIPPhoneGraphicFileMenu>").is_err());
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage>").is_err());
        assert!(matches!(
            CiscoIpPhoneImage::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneImage::from_xml(b"<!DOCTYPE image [<!ENTITY bits '00'>]><CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>&bits;</Data></CiscoIPPhoneImage>"),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>&unknown;</Data></CiscoIPPhoneImage>").is_err());

        let nested = format!(
            "<CiscoIPPhoneImage>{}<Width>1</Width><Height>1</Height><Depth>1</Depth>{}</CiscoIPPhoneImage>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneImage::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));
        assert!(matches!(
            PhoneImageDocument::from_xml(&vec![b'x'; PHONE_IMAGE_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            PhoneImageDocument::Image(complete_bitmap_image()).to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_graphic_file_menu(),
                PHONE_IMAGE_MAX_BYTES
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_bitmap_status() -> CiscoIpPhoneStatus {
        CiscoIpPhoneStatus {
            text: Some("Café <ready> & active".into()),
            timer_seconds: Some(15),
            location_x: Some(-1),
            location_y: Some(20),
            width: 106,
            height: 21,
            depth: 2,
            data: Some(PhoneBitmapData::new(vec![0x00, 0xab, 0xff]).unwrap()),
        }
    }

    fn complete_file_status() -> CiscoIpPhoneStatusFile {
        CiscoIpPhoneStatusFile {
            text: Some("Status <file> & refresh".into()),
            timer_seconds: Some(u16::MAX),
            location_x: Some(261),
            location_y: Some(-1),
            url: PhoneImageUrl::new("https://pbx.example/status.png?id=7&view=compact").unwrap(),
        }
    }

    #[test]
    fn status_documents_round_trip_icons_timers_order_utf8_and_escaping() {
        let bitmap = complete_bitmap_status();
        let xml = bitmap.to_xml().unwrap();
        assert!(xml.contains("Café &lt;ready&gt; &amp; active"));
        assert!(xml.contains("<Timer>15</Timer>"));
        assert!(xml.contains("<Data>00ABFF</Data>"));
        assert!(xml.find("<Text>").unwrap() < xml.find("<Timer>").unwrap());
        assert!(xml.find("<Timer>").unwrap() < xml.find("<LocationX>").unwrap());
        assert!(xml.find("<Depth>").unwrap() < xml.find("<Data>").unwrap());
        assert_eq!(
            CiscoIpPhoneStatus::from_xml(xml.as_bytes()).unwrap(),
            bitmap
        );
        assert_eq!(
            PhoneStatusDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneStatusDocument::Bitmap(bitmap)
        );

        let file = complete_file_status();
        let xml = file.to_xml().unwrap();
        assert!(xml.contains("Status &lt;file&gt; &amp; refresh"));
        assert!(xml.contains(&format!("<Timer>{}</Timer>", u16::MAX)));
        assert!(xml.contains("id=7&amp;view=compact"));
        assert!(xml.find("<LocationY>").unwrap() < xml.find("<URL>").unwrap());
        assert_eq!(
            PhoneStatusDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneStatusDocument::File(file)
        );

        let zero_timer = CiscoIpPhoneStatus::from_xml(
            b"<CiscoIPPhoneStatus><Timer>0</Timer><Width>1</Width><Height>1</Height><Depth>1</Depth><Data></Data></CiscoIPPhoneStatus>",
        )
        .unwrap();
        assert_eq!(zero_timer.timer_seconds, Some(0));
        assert_eq!(zero_timer.data.unwrap().as_bytes(), []);
        let absent_data = CiscoIpPhoneStatus::from_xml(
            b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth></CiscoIPPhoneStatus>",
        )
        .unwrap();
        assert!(absent_data.timer_seconds.is_none());
        assert!(absent_data.data.is_none());
    }

    #[test]
    fn status_documents_enforce_exact_text_geometry_icon_and_url_bounds() {
        let mut bitmap = complete_bitmap_status();
        bitmap.text = Some("x".repeat(32));
        assert!(bitmap.validate().is_ok());
        bitmap.text = Some("x".repeat(33));
        assert!(bitmap.validate().is_err());
        bitmap.text = None;
        for x in [-2, 106] {
            bitmap.location_x = Some(x);
            assert!(bitmap.validate().is_err());
        }
        bitmap.location_x = None;
        for y in [-2, 21] {
            bitmap.location_y = Some(y);
            assert!(bitmap.validate().is_err());
        }
        bitmap.location_y = None;
        for (width, height, depth) in [
            (0, 1, 1),
            (107, 1, 1),
            (1, 0, 1),
            (1, 22, 1),
            (1, 1, 0),
            (1, 1, 3),
        ] {
            bitmap.width = width;
            bitmap.height = height;
            bitmap.depth = depth;
            assert!(bitmap.validate().is_err());
        }
        bitmap.width = 1;
        bitmap.height = 1;
        bitmap.depth = 1;
        bitmap.data = Some(PhoneBitmapData::new(vec![0; PHONE_STATUS_BITMAP_MAX_BYTES]).unwrap());
        assert!(bitmap.validate().is_ok());
        bitmap.data =
            Some(PhoneBitmapData::new(vec![0; PHONE_STATUS_BITMAP_MAX_BYTES + 1]).unwrap());
        assert!(matches!(
            bitmap.validate(),
            Err(PhoneXmlError::LimitExceeded {
                kind: "phone status bitmap bytes",
                maximum: PHONE_STATUS_BITMAP_MAX_BYTES,
                ..
            })
        ));

        let mut file = complete_file_status();
        for x in [-2, 262] {
            file.location_x = Some(x);
            assert!(file.validate().is_err());
        }
        file.location_x = None;
        for y in [-2, 50] {
            file.location_y = Some(y);
            assert!(file.validate().is_err());
        }
        assert!(PhoneImageUrl::new("").is_err());
        assert!(PhoneImageUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn status_parsers_reject_wrong_roots_malformed_unsafe_nested_and_oversized_input() {
        assert!(
            CiscoIpPhoneStatus::from_xml(
                b"<CiscoIPPhoneStatusFile><URL>x</URL></CiscoIPPhoneStatusFile>"
            )
            .is_err()
        );
        assert!(PhoneStatusDocument::from_xml(b"<CiscoIPPhoneText/>").is_err());
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Unknown/></CiscoIPPhoneStatus>").is_err());
        assert!(
            CiscoIpPhoneStatus::from_xml(
                b"<CiscoIPPhoneStatus><Height>1</Height><Depth>1</Depth></CiscoIPPhoneStatus>"
            )
            .is_err()
        );
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>f</Data></CiscoIPPhoneStatus>").is_err());
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>zz</Data></CiscoIPPhoneStatus>").is_err());
        assert!(
            CiscoIpPhoneStatusFile::from_xml(
                b"<CiscoIPPhoneStatusFile><URL></URL></CiscoIPPhoneStatusFile>"
            )
            .is_err()
        );
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus>").is_err());
        assert!(matches!(
            CiscoIpPhoneStatus::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneStatus::from_xml(b"<!DOCTYPE status [<!ENTITY bits '00'>]><CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>&bits;</Data></CiscoIPPhoneStatus>"),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>&unknown;</Data></CiscoIPPhoneStatus>").is_err());

        let nested = format!(
            "<CiscoIPPhoneStatus>{}<Width>1</Width><Height>1</Height><Depth>1</Depth>{}</CiscoIPPhoneStatus>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneStatus::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));
        assert!(matches!(
            PhoneStatusDocument::from_xml(&vec![b'x'; PHONE_STATUS_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            PhoneStatusDocument::Bitmap(complete_bitmap_status()).to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_file_status(),
                PHONE_STATUS_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_alarm() -> CiscoIpPhoneAlarm {
        CiscoIpPhoneAlarm {
            alarm: CiscoIpPhoneAlarmEntry {
                name: LAST_OUT_OF_SERVICE_ALARM.into(),
                parameter_list: CiscoIpPhoneAlarmParameterList {
                    parameters: vec![
                        CiscoIpPhoneAlarmParameter::String(CiscoIpPhoneAlarmString {
                            name: "DeviceName".into(),
                            value: "SEP001122334455".into(),
                        }),
                        CiscoIpPhoneAlarmParameter::Enum(CiscoIpPhoneAlarmEnum {
                            name: "DHCPv4Status".into(),
                            value: 1,
                        }),
                        CiscoIpPhoneAlarmParameter::Enum(CiscoIpPhoneAlarmEnum {
                            name: "ReasonForOutOfService".into(),
                            value: 25,
                        }),
                        CiscoIpPhoneAlarmParameter::String(CiscoIpPhoneAlarmString {
                            name: "LastProtocolEventSent".into(),
                            value: "Sent:REGISTER <call-id> & route".into(),
                        }),
                        CiscoIpPhoneAlarmParameter::String(CiscoIpPhoneAlarmString {
                            name: "LastProtocolEventReceived".into(),
                            value: String::new(),
                        }),
                    ],
                },
            },
        }
    }

    #[test]
    fn alarm_schema_round_trips_ordered_typed_parameters_and_accessors() {
        let expected = complete_alarm();
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with(
            "<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList>"
        ));
        assert!(xml.contains("Sent:REGISTER &lt;call-id&gt; &amp; route"));
        assert!(xml.find("DeviceName").unwrap() < xml.find("DHCPv4Status").unwrap());
        assert!(
            xml.find("ReasonForOutOfService").unwrap() < xml.find("LastProtocolEventSent").unwrap()
        );
        let decoded = CiscoIpPhoneAlarm::from_xml(xml.as_bytes()).unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(decoded.reason_for_out_of_service(), Some(25));
        assert_eq!(decoded.enumeration("DHCPv4Status"), Some(1));
        assert_eq!(decoded.string("DeviceName"), Some("SEP001122334455"));
        assert_eq!(decoded.string("LastProtocolEventReceived"), Some(""));
        assert_eq!(decoded.string("Unknown"), None);
        let telemetry = parse_phone_alarm(xml.as_bytes()).unwrap();
        assert!(matches!(
            &telemetry,
            PhoneAlarmTelemetry::LastOutOfService(alarm) if alarm == &expected
        ));
        assert_eq!(
            telemetry.summary(),
            Some(PhoneAlarmSummary {
                kind: PhoneAlarmKind::LastOutOfService,
                reason_for_out_of_service: Some(25),
            })
        );
    }

    #[test]
    fn unknown_alarm_schemas_remain_bounded_lossless_and_secret_safe() {
        for unknown in [
            b"<x-cisco-alarm/>".as_slice(),
            b"<x-cisco-alarm><Alarm Name=\"DeviceTroubleshootingReport\"><ParameterList><String name=\"Token\">secret-value</String></ParameterList></Alarm></x-cisco-alarm>".as_slice(),
            b"<vendor-alarm><Credential>secret-value</Credential></vendor-alarm>".as_slice(),
        ] {
            let PhoneAlarmTelemetry::Opaque(opaque) = parse_phone_alarm(unknown).unwrap() else {
                panic!("unknown alarm schema must remain opaque");
            };
            assert_eq!(opaque.as_bytes(), unknown);
            let debug = format!("{opaque:?}");
            assert!(!debug.contains("secret-value"));
            assert!(debug.contains(&unknown.len().to_string()));
            assert_eq!(opaque.clone().into_bytes(), unknown);
        }

        let opaque = parse_phone_alarm(b"<vendor-alarm/>").unwrap();
        assert!(opaque.is_opaque());
        assert_eq!(opaque.summary(), None);

        let known = complete_alarm();
        let debug = format!("{known:?}");
        assert!(!debug.contains("SEP001122334455"));
        assert!(!debug.contains("call-id"));
        assert!(debug.contains(LAST_OUT_OF_SERVICE_ALARM));
        assert_eq!(
            format!("{:?}", known.alarm.parameter_list),
            "CiscoIpPhoneAlarmParameterList { parameter_count: 5 }"
        );
    }

    #[test]
    fn known_alarm_validation_rejects_ambiguity_unsafe_values_and_size_overflow() {
        let mut alarm = complete_alarm();
        alarm
            .alarm
            .parameter_list
            .parameters
            .push(CiscoIpPhoneAlarmParameter::Enum(CiscoIpPhoneAlarmEnum {
                name: "DeviceName".into(),
                value: 2,
            }));
        assert!(matches!(
            alarm.validate(),
            Err(PhoneXmlError::InvalidField {
                field: "phone alarm parameter names",
                ..
            })
        ));

        alarm = complete_alarm();
        match &mut alarm.alarm.parameter_list.parameters[0] {
            CiscoIpPhoneAlarmParameter::String(device) => device.name.clear(),
            CiscoIpPhoneAlarmParameter::Enum(_) => panic!("first parameter must be a string"),
        }
        assert!(alarm.validate().is_err());
        match &mut alarm.alarm.parameter_list.parameters[0] {
            CiscoIpPhoneAlarmParameter::String(device) => {
                device.name = "DeviceName".into();
                device.value = "not\u{1}xml".into();
            }
            CiscoIpPhoneAlarmParameter::Enum(_) => panic!("first parameter must be a string"),
        }
        assert!(alarm.validate().is_err());
        match &mut alarm.alarm.parameter_list.parameters[0] {
            CiscoIpPhoneAlarmParameter::String(device) => {
                device.value = "sensitive-value".repeat(PHONE_ALARM_MAX_BYTES);
            }
            CiscoIpPhoneAlarmParameter::Enum(_) => panic!("first parameter must be a string"),
        }
        let error = alarm.to_xml().unwrap_err();
        assert!(!error.to_string().contains("sensitive-value"));

        let duplicate = b"<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList><String name=\"DeviceName\">first-secret</String><String name=\"DeviceName\">second-secret</String></ParameterList></Alarm></x-cisco-alarm>";
        let error = parse_phone_alarm(duplicate).unwrap_err();
        assert!(!error.to_string().contains("first-secret"));
        assert!(!error.to_string().contains("second-secret"));
    }

    #[test]
    fn alarm_parser_rejects_malformed_known_unsafe_and_oversized_documents() {
        assert!(parse_phone_alarm(b"<x-cisco-alarm>").is_err());
        assert!(matches!(
            parse_phone_alarm(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            parse_phone_alarm(b"<!DOCTYPE alarm [<!ENTITY value 'secret'>]><x-cisco-alarm><Alarm Name=\"Unknown\"><ParameterList><String name=\"Value\">&value;</String></ParameterList></Alarm></x-cisco-alarm>"),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(parse_phone_alarm(b"<x-cisco-alarm><Alarm Name=\"Unknown\"><ParameterList><String name=\"Value\">&unknown;</String></ParameterList></Alarm></x-cisco-alarm>").is_err());
        assert!(parse_phone_alarm(b"<vendor-alarm><Value>&#1;</Value></vendor-alarm>").is_err());
        assert!(parse_phone_alarm(b"<vendor-alarm value=\"&#1;\"/>").is_err());
        assert!(parse_phone_alarm(b"<vendor-alarm>not\x01xml</vendor-alarm>").is_err());
        assert!(parse_phone_alarm(b"<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList><Binary name=\"Value\">00</Binary></ParameterList></Alarm></x-cisco-alarm>").is_err());
        assert!(
            parse_phone_alarm(
                b"<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"/></x-cisco-alarm>"
            )
            .is_err()
        );
        let invalid_enum = b"<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList><Enum name=\"ReasonForOutOfService\">secret-enum</Enum></ParameterList></Alarm></x-cisco-alarm>";
        let error = parse_phone_alarm(invalid_enum).unwrap_err();
        assert!(!error.to_string().contains("secret-enum"));

        let nested = format!(
            "<x-cisco-alarm>{}<Alarm Name=\"Unknown\"/>{}</x-cisco-alarm>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            parse_phone_alarm(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));
        assert!(matches!(
            parse_phone_alarm(&vec![b'x'; PHONE_ALARM_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded {
                maximum: PHONE_ALARM_MAX_BYTES,
                ..
            })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(FailingWriter, &complete_alarm(), PHONE_ALARM_MAX_BYTES),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_location() -> CiscoIpPhoneLocationInformation {
        CiscoIpPhoneLocationInformation {
            wifi: CiscoIpPhoneWifiLocation {
                bssid: PhoneBssid::parse("e8:ed:f3:10:29:fd").unwrap(),
                ssid: "Café <voice> & data".into(),
                access_point_name: "West wing <3>".into(),
            },
            off_premises: Some(CiscoIpPhoneOffPremises::new()),
        }
    }

    #[test]
    fn location_schema_round_trips_typed_address_fields_order_and_escaping() {
        let expected = complete_location();
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<Interface1><wifi><BSSID>E8:ED:F3:10:29:FD</BSSID>"));
        assert!(xml.contains("<SSID>Café &lt;voice&gt; &amp; data</SSID>"));
        assert!(xml.contains("<APName>West wing &lt;3&gt;</APName>"));
        assert!(xml.find("</wifi>").unwrap() < xml.find("<OffPrem").unwrap());
        assert_eq!(
            CiscoIpPhoneLocationInformation::from_xml(xml.as_bytes()).unwrap(),
            expected
        );
        assert_eq!(
            expected.wifi.bssid.octets(),
            [0xe8, 0xed, 0xf3, 0x10, 0x29, 0xfd]
        );
        assert_eq!(expected.wifi.bssid.to_string(), "E8:ED:F3:10:29:FD");
        assert!(expected.is_off_premises());

        let telemetry = parse_phone_location(xml.as_bytes()).unwrap();
        assert_eq!(
            telemetry.summary(),
            Some(PhoneLocationSummary {
                kind: PhoneLocationKind::WirelessInterface,
                off_premises: true,
            })
        );

        let on_premises = CiscoIpPhoneLocationInformation::from_xml(
            b"<Interface1><wifi><BSSID>00:11:22:33:44:55</BSSID><SSID></SSID><APName/></wifi></Interface1>",
        )
        .unwrap();
        assert!(!on_premises.is_off_premises());
        assert_eq!(on_premises.wifi.ssid, "");
        assert_eq!(on_premises.wifi.access_point_name, "");
    }

    #[test]
    fn location_models_enforce_address_marker_text_and_document_bounds() {
        for invalid in [
            "00:11:22:33:44",
            "00:11:22:33:44:555",
            "00-11-22-33-44-55",
            "00:11:22:33:44:gg",
            "private-address",
        ] {
            let error = PhoneBssid::parse(invalid).unwrap_err();
            assert!(!error.to_string().contains(invalid));
        }

        let mut location = complete_location();
        location.wifi.ssid = "é".repeat(16);
        assert!(location.validate().is_ok());
        location.wifi.ssid.push('é');
        assert!(matches!(
            location.validate(),
            Err(PhoneXmlError::InvalidField {
                field: "phone location SSID",
                expected: "at most 32 bytes",
            })
        ));

        location = complete_location();
        location.wifi.access_point_name = "private-name".repeat(PHONE_LOCATION_MAX_BYTES);
        let error = location.to_xml().unwrap_err();
        assert!(!error.to_string().contains("private-name"));

        let nonempty_marker = b"<Interface1><wifi><BSSID>00:11:22:33:44:55</BSSID><SSID>voice</SSID><APName>west</APName></wifi><OffPrem>private-location</OffPrem></Interface1>";
        let error = parse_phone_location(nonempty_marker).unwrap_err();
        assert!(!error.to_string().contains("private-location"));
    }

    #[test]
    fn unknown_location_schemas_are_bounded_lossless_and_secret_safe() {
        for unknown in [
            b"<Interface2><wifi><BSSID>00:11:22:33:44:55</BSSID></wifi></Interface2>".as_slice(),
            b"<DeviceLocation><CivicAddress>private-building</CivicAddress></DeviceLocation>"
                .as_slice(),
        ] {
            let telemetry = parse_phone_location(unknown).unwrap();
            let PhoneLocationTelemetry::Opaque(opaque) = &telemetry else {
                panic!("unsupported location schema must remain opaque");
            };
            assert_eq!(opaque.as_bytes(), unknown);
            assert_eq!(opaque.clone().into_bytes(), unknown);
            assert_eq!(telemetry.summary(), None);
            assert!(telemetry.is_opaque());
            let debug = format!("{telemetry:?}");
            assert!(!debug.contains("private-building"));
            assert!(!debug.contains("00:11:22:33:44:55"));
            assert!(debug.contains(&unknown.len().to_string()));
        }

        let debug = format!("{:?}", complete_location());
        assert!(!debug.contains("Café"));
        assert!(!debug.contains("West wing"));
        assert!(!debug.contains("E8:ED:F3:10:29:FD"));
    }

    #[test]
    fn location_parser_rejects_malformed_known_unsafe_and_oversized_documents() {
        for invalid in [
            b"<Interface1>".as_slice(),
            b"<Interface1><wifi><BSSID>private-address</BSSID><SSID>private-network</SSID><APName>private-access-point</APName></wifi></Interface1>".as_slice(),
            b"<Interface1><wifi><BSSID>00:11:22:33:44:55</BSSID><SSID>voice</SSID><APName>west</APName><Credential>private-secret</Credential></wifi></Interface1>".as_slice(),
            b"<Interface1><OffPrem/></Interface1>".as_slice(),
            b"<Interface1><wifi><BSSID>00:11:22:33:44:55</BSSID><SSID>one</SSID><SSID>two</SSID><APName>west</APName></wifi></Interface1>".as_slice(),
        ] {
            let error = parse_phone_location(invalid).unwrap_err();
            let error = error.to_string();
            assert!(!error.contains("private-address"));
            assert!(!error.contains("private-network"));
            assert!(!error.contains("private-access-point"));
            assert!(!error.contains("private-secret"));
        }
        assert!(matches!(
            parse_phone_location(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            parse_phone_location(b"<!DOCTYPE Interface2 [<!ENTITY location 'private'>]><Interface2>&location;</Interface2>"),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(matches!(
            parse_phone_location(b"<Interface2>&undeclared;</Interface2>"),
            Err(PhoneXmlError::InvalidEntity)
        ));
        assert!(parse_phone_location(b"<Interface2>&#1;</Interface2>").is_err());
        assert!(parse_phone_location(b"<Interface2>not\x01xml</Interface2>").is_err());

        let nested = format!(
            "<Interface2>{}{}</Interface2>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            parse_phone_location(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));
        assert!(matches!(
            parse_phone_location(&vec![b'x'; PHONE_LOCATION_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded {
                maximum: PHONE_LOCATION_MAX_BYTES,
                ..
            })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_location(),
                PHONE_LOCATION_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_menu() -> CiscoIpPhoneMenu {
        CiscoIpPhoneMenu {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("menu-west".into()),
            on_focus_lost: Some("Notify:focus?state=lost&side=west".into()),
            on_focus_gained: Some("Notify:focus?state=gained".into()),
            on_minimized: Some("Notify:minimized".into()),
            on_closed: Some("Notify:closed".into()),
            title: Some("Support <East> & West".into()),
            prompt: Some("Choose A & B".into()),
            soft_keys: vec![CiscoIpPhoneSoftKeyItem {
                name: Some("Open & inspect".into()),
                position: PhoneSoftKeyPosition::new(1).unwrap(),
                url: Some("SoftKey:Select?a=1&b=2".into()),
                url_down: Some("SoftKey:SelectDown".into()),
            }],
            key_items: vec![CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some("SoftKey:Cancel".into()),
                url_down: None,
            }],
            items: vec![CiscoIpPhoneMenuItem {
                name: Some("Alice <Admin> & Bob".into()),
                url: Some("UserData:7:0:open/a?x=1&y=2".into()),
            }],
        }
    }

    #[test]
    fn basic_menu_round_trips_complete_display_controls_in_schema_order() {
        let expected = complete_menu();
        let xml = expected.to_xml().unwrap();
        assert!(xml.contains("Support &lt;East&gt; &amp; West"));
        assert!(xml.contains("Alice &lt;Admin&gt; &amp; Bob"));
        assert!(xml.contains("x=1&amp;y=2"));
        assert!(xml.find("<SoftKeyItem>").unwrap() < xml.find("<KeyItem>").unwrap());
        assert!(xml.find("<KeyItem>").unwrap() < xml.find("<MenuItem>").unwrap());
        assert_eq!(
            CiscoIpPhoneMenu::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let minimal = CiscoIpPhoneMenu::from_xml(b"<CiscoIPPhoneMenu/>").unwrap();
        assert!(minimal.title.is_none());
        assert!(minimal.items.is_empty());
    }

    #[test]
    fn bitmap_and_resource_icon_menus_round_trip_exact_icon_families() {
        let bitmap = CiscoIpPhoneIconMenu::new(
            "Conference & staff",
            "Choose <one>",
            vec![CiscoIpPhoneIconMenuItem {
                name: Some("Taylor & team".into()),
                url: Some("UserData:1:0:participant/7?view=a&b=c".into()),
                icon_index: Some(2),
            }],
            vec![CiscoIpPhoneIconItem {
                index: 2,
                width: 16,
                height: 10,
                depth: 2,
                data: Some("000FF0".into()),
            }],
        )
        .unwrap();
        let xml = bitmap.to_xml().unwrap();
        assert!(xml.find("<MenuItem>").unwrap() < xml.find("<IconItem>").unwrap());
        assert!(xml.find("<Width>").unwrap() < xml.find("<Height>").unwrap());
        assert!(xml.contains("Conference &amp; staff"));
        assert_eq!(
            CiscoIpPhoneIconMenu::from_xml(xml.as_bytes()).unwrap(),
            bitmap
        );

        let resources = CiscoIpPhoneIconFileMenu {
            keypad_target: Some(PhoneKeypadTarget::ActiveCall),
            application_id: Some("conference-list".into()),
            on_focus_lost: None,
            on_focus_gained: Some("Notify:focus".into()),
            on_minimized: None,
            on_closed: Some("SoftKey:Exit".into()),
            icon_index: Some(4),
            title: Some(CiscoIpPhoneIconTitle {
                icon_index: Some(5),
                text: "Locked & secure".into(),
            }),
            prompt: Some("Choose a participant".into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            items: vec![CiscoIpPhoneIconMenuItem {
                name: Some("Alex <Host>".into()),
                url: Some("UserData:1:0:participant/1".into()),
                icon_index: Some(5),
            }],
            icons: vec![CiscoIpPhoneIconFileItem {
                index: 5,
                url: "Resource:Icon.SecureCall?shade=dark&size=small".into(),
            }],
        };
        let xml = resources.to_xml().unwrap();
        assert!(xml.contains("<Title IconIndex=\"5\">Locked &amp; secure</Title>"));
        assert!(xml.contains("shade=dark&amp;size=small"));
        assert_eq!(
            CiscoIpPhoneIconFileMenu::from_xml(xml.as_bytes()).unwrap(),
            resources
        );
    }

    #[test]
    fn menu_models_reject_every_collection_text_url_position_and_icon_bound() {
        let mut basic = complete_menu();
        basic.items = vec![basic.items[0].clone(); PHONE_MENU_MAX_ITEMS + 1];
        assert!(matches!(
            basic.to_xml(),
            Err(PhoneXmlError::LimitExceeded {
                kind: "menu items",
                ..
            })
        ));

        let mut invalid = complete_menu();
        invalid.items[0].name = Some("x".repeat(65));
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        invalid = complete_menu();
        invalid.items[0].url = Some("x".repeat(PHONE_XML_URL_MAX_CHARS + 1));
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        invalid = complete_menu();
        invalid.application_id = Some(String::new());
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        invalid = complete_menu();
        invalid.on_closed = Some(String::new());
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        invalid = complete_menu();
        invalid.soft_keys[0].position = PhoneSoftKeyPosition::new(16).unwrap();
        assert!(invalid.to_xml().is_ok());
        invalid = complete_menu();
        invalid.soft_keys = vec![invalid.soft_keys[0].clone(); 17];
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        invalid = complete_menu();
        invalid.key_items = vec![invalid.key_items[0].clone(); 33];
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        let item = CiscoIpPhoneIconMenuItem {
            name: Some("Item".into()),
            url: Some("SoftKey:Select".into()),
            icon_index: Some(0),
        };
        let icon = CiscoIpPhoneIconItem {
            index: 0,
            width: 1,
            height: 1,
            depth: 1,
            data: Some("00".into()),
        };
        let mut icon_menu =
            CiscoIpPhoneIconMenu::new("Icons", "Choose", vec![item.clone()], vec![icon.clone()])
                .unwrap();
        icon_menu.items = vec![item.clone(); PHONE_ICON_MENU_MAX_ITEMS + 1];
        assert!(matches!(
            icon_menu.to_xml(),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        icon_menu =
            CiscoIpPhoneIconMenu::new("Icons", "Choose", vec![item.clone()], vec![icon.clone()])
                .unwrap();
        icon_menu.icons = vec![icon.clone(); PHONE_ICON_MENU_MAX_ICONS + 1];
        assert!(matches!(
            icon_menu.to_xml(),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        for invalid_icon in [
            CiscoIpPhoneIconItem {
                width: 0,
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                height: 11,
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                depth: 3,
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                data: Some("0".into()),
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                data: Some("GG".into()),
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                data: Some("00".repeat(41)),
                ..icon
            },
        ] {
            assert!(
                CiscoIpPhoneIconMenu::new(
                    "Icons",
                    "Choose",
                    vec![item.clone()],
                    vec![invalid_icon]
                )
                .is_err()
            );
        }
        let mut invalid_item = item;
        invalid_item.icon_index = Some(10);
        assert!(
            CiscoIpPhoneIconMenu::new("Icons", "Choose", vec![invalid_item], vec![icon]).is_err()
        );

        let mut file_menu = CiscoIpPhoneIconFileMenu {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            icon_index: None,
            title: None,
            prompt: None,
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            items: Vec::new(),
            icons: vec![CiscoIpPhoneIconFileItem {
                index: 10,
                url: "Resource:Icon.Hold".into(),
            }],
        };
        assert!(matches!(
            file_menu.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        file_menu.icons[0].index = 0;
        file_menu.icons[0].url.clear();
        assert!(matches!(
            file_menu.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
    }

    #[test]
    fn menu_parsers_reject_wrong_roots_unknown_fields_malformed_input_and_writer_failure() {
        assert!(CiscoIpPhoneMenu::from_xml(b"<CiscoIPPhoneIconMenu/>").is_err());
        assert!(CiscoIpPhoneIconMenu::from_xml(b"<CiscoIPPhoneMenu/>").is_err());
        assert!(CiscoIpPhoneIconFileMenu::from_xml(b"<CiscoIPPhoneIconMenu/>").is_err());
        assert!(
            CiscoIpPhoneMenu::from_xml(b"<CiscoIPPhoneMenu><Unknown/></CiscoIPPhoneMenu>",)
                .is_err()
        );
        assert!(CiscoIpPhoneIconMenu::from_xml(b"<CiscoIPPhoneIconMenu>").is_err());
        assert!(
            CiscoIpPhoneIconFileMenu::from_xml(b"<!DOCTYPE menu><CiscoIPPhoneIconFileMenu/>",)
                .is_err()
        );
        assert!(matches!(
            CiscoIpPhoneMenu::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            complete_menu().to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(FailingWriter, &complete_menu(), PHONE_MENU_MAX_BYTES),
            Err(PhoneXmlError::Write(_))
        ));
    }

    #[test]
    fn conference_lists_round_trip_menu_and_icon_families_with_typed_actions() {
        let conference_id = ConferenceId::new(41);
        let participants = [
            ConferenceListEntry {
                participant_id: ParticipantId::new(7),
                name: "Alex <Host> & Co".into(),
                number: "2100".into(),
                moderator: true,
                muted: false,
            },
            ConferenceListEntry {
                participant_id: ParticipantId::new(8),
                name: String::new(),
                number: "2200".into(),
                moderator: false,
                muted: true,
            },
            ConferenceListEntry {
                participant_id: ParticipantId::new(9),
                name: "Casey".into(),
                number: "2300".into(),
                moderator: false,
                muted: false,
            },
        ];
        for family in [ConferenceMenuFamily::Menu, ConferenceMenuFamily::IconMenu] {
            let expected =
                ConferenceListDocument::new(conference_id, &participants, family).unwrap();
            let xml = expected.to_xml().unwrap();
            assert!(xml.contains("Alex &lt;Host&gt; &amp; Co"));
            let decoded = ConferenceListDocument::from_xml(xml.as_bytes(), family).unwrap();
            assert_eq!(decoded, expected);
            assert_eq!(
                decoded.actions().collect::<Vec<_>>(),
                [
                    ConferenceListAction::Participant {
                        conference_id,
                        participant_id: ParticipantId::new(7),
                    },
                    ConferenceListAction::Participant {
                        conference_id,
                        participant_id: ParticipantId::new(8),
                    },
                    ConferenceListAction::Participant {
                        conference_id,
                        participant_id: ParticipantId::new(9),
                    },
                    ConferenceListAction::End { conference_id },
                ]
            );
        }
    }

    #[test]
    fn conference_participant_actions_round_trip_both_families_and_removal_policy() {
        let conference_id = ConferenceId::new(41);
        let mut participant = ConferenceListEntry {
            participant_id: ParticipantId::new(8),
            name: "Alex <Admin> & Co".into(),
            number: "2200".into(),
            moderator: false,
            muted: false,
        };
        for family in [ConferenceMenuFamily::Menu, ConferenceMenuFamily::IconMenu] {
            let expected = ConferenceParticipantActionsDocument::new(
                conference_id,
                &participant,
                true,
                false,
                family,
            )
            .unwrap();
            let xml = expected.to_xml().unwrap();
            let decoded =
                ConferenceParticipantActionsDocument::from_xml(xml.as_bytes(), family).unwrap();
            assert_eq!(decoded, expected);
            assert_eq!(
                decoded.actions().collect::<Vec<_>>(),
                [
                    ConferenceListAction::Mute {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                    ConferenceListAction::Remove {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                    ConferenceListAction::Promote {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                ]
            );

            participant.muted = true;
            let not_removable = ConferenceParticipantActionsDocument::new(
                conference_id,
                &participant,
                false,
                false,
                family,
            )
            .unwrap();
            assert_eq!(
                not_removable.actions().collect::<Vec<_>>(),
                [
                    ConferenceListAction::Unmute {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                    ConferenceListAction::Promote {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                ]
            );
            participant.moderator = true;
            let demotable = ConferenceParticipantActionsDocument::new(
                conference_id,
                &participant,
                false,
                true,
                family,
            )
            .unwrap();
            assert_eq!(
                demotable.actions().collect::<Vec<_>>(),
                [ConferenceListAction::Demote {
                    conference_id,
                    participant_id: participant.participant_id,
                }]
            );
            let sole_moderator = ConferenceParticipantActionsDocument::new(
                conference_id,
                &participant,
                false,
                false,
                family,
            )
            .unwrap();
            assert!(sole_moderator.actions().next().is_none());
            participant.moderator = false;
            participant.muted = false;
        }
    }

    #[test]
    fn conference_lists_reject_limits_malformed_actions_and_wrong_family() {
        let participants = vec![
            ConferenceListEntry {
                participant_id: ParticipantId::new(1),
                name: "Participant".into(),
                number: String::new(),
                moderator: false,
                muted: false,
            };
            CONFERENCE_LIST_MAX_PARTICIPANTS + 1
        ];
        assert!(matches!(
            ConferenceListDocument::new(
                ConferenceId::new(1),
                &participants,
                ConferenceMenuFamily::Menu,
            ),
            Err(PhoneXmlError::LimitExceeded {
                kind: "conference participants",
                ..
            })
        ));
        assert!(ConferenceListAction::parse("conference/1/participant/not-a-number").is_none());
        assert!(ConferenceListAction::parse("conference/1/remove/7").is_none());
        assert_eq!(
            ConferenceListAction::parse("conference/1/participant/7/remove"),
            Some(ConferenceListAction::Remove {
                conference_id: ConferenceId::new(1),
                participant_id: ParticipantId::new(7),
            })
        );
        assert_eq!(
            ConferenceListAction::from_route(&[
                "conference".into(),
                "1".into(),
                "participant".into(),
                "7".into(),
                "remove".into(),
            ]),
            Some(ConferenceListAction::Remove {
                conference_id: ConferenceId::new(1),
                participant_id: ParticipantId::new(7),
            })
        );
        for (operation, expected) in [
            (
                "promote",
                ConferenceListAction::Promote {
                    conference_id: ConferenceId::new(1),
                    participant_id: ParticipantId::new(7),
                },
            ),
            (
                "demote",
                ConferenceListAction::Demote {
                    conference_id: ConferenceId::new(1),
                    participant_id: ParticipantId::new(7),
                },
            ),
        ] {
            let route = [
                "conference".into(),
                "1".into(),
                "participant".into(),
                "7".into(),
                operation.into(),
            ];
            assert_eq!(ConferenceListAction::from_route(&route), Some(expected));
        }

        let menu = ConferenceListDocument::new(
            ConferenceId::new(1),
            &participants[..1],
            ConferenceMenuFamily::Menu,
        )
        .unwrap()
        .to_xml()
        .unwrap();
        assert!(
            ConferenceListDocument::from_xml(menu.as_bytes(), ConferenceMenuFamily::IconMenu)
                .is_err()
        );
        assert!(
            ConferenceListDocument::from_xml(
                b"<!DOCTYPE menu><CiscoIPPhoneMenu/>",
                ConferenceMenuFamily::Menu,
            )
            .is_err()
        );
    }

    #[test]
    fn directory_schema_round_trips_entries_controls_attributes_and_escaping() {
        let expected = CiscoIpPhoneDirectory {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("directory-west".into()),
            on_focus_lost: Some("Notify:focus?state=lost&view=all".into()),
            on_focus_gained: None,
            on_minimized: None,
            on_closed: Some("SoftKey:Exit".into()),
            title: Some("R&D <West>".into()),
            prompt: Some("Choose A & B".into()),
            soft_keys: vec![CiscoIpPhoneSoftKeyItem {
                name: Some("Next".into()),
                position: PhoneSoftKeyPosition::new(3).unwrap(),
                url: Some("http://pbx.test/directory?page=2&query=R%26D".into()),
                url_down: None,
            }],
            key_items: vec![CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some("SoftKey:Cancel".into()),
                url_down: None,
            }],
            entries: vec![CiscoIpPhoneDirectoryEntry {
                name: Some("Alice <Admin> & Bob".into()),
                telephone: Some("1001&2".into()),
            }],
        };

        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<CiscoIPPhoneDirectory"));
        assert!(xml.contains("keypadTarget=\"applicationCall\""));
        assert!(xml.contains("R&amp;D &lt;West&gt;"));
        assert!(xml.contains("Alice &lt;Admin&gt; &amp; Bob"));
        assert_eq!(
            CiscoIpPhoneDirectory::from_xml(xml.as_bytes()).unwrap(),
            expected
        );
    }

    #[test]
    fn directory_schema_accepts_the_minimal_document_and_optionally_empty_fields() {
        let xml = b"<CiscoIPPhoneDirectory><Title/><Prompt/><DirectoryEntry><Name/><Telephone/></DirectoryEntry></CiscoIPPhoneDirectory>";
        let document = CiscoIpPhoneDirectory::from_xml(xml).unwrap();
        assert_eq!(document.title.as_deref(), Some(""));
        assert_eq!(document.prompt.as_deref(), Some(""));
        assert_eq!(document.entries.len(), 1);
        assert_eq!(document.entries[0].name.as_deref(), Some(""));
        assert_eq!(document.entries[0].telephone.as_deref(), Some(""));
    }

    #[test]
    fn directory_schema_enforces_entry_text_control_and_document_bounds() {
        let too_many = vec![
            CiscoIpPhoneDirectoryEntry {
                name: Some("Name".into()),
                telephone: Some("1000".into()),
            };
            PHONE_DIRECTORY_MAX_ENTRIES + 1
        ];
        assert!(matches!(
            CiscoIpPhoneDirectory::new("Directory", "Choose", too_many),
            Err(PhoneXmlError::LimitExceeded {
                kind: "directory entries",
                ..
            })
        ));

        let invalid = CiscoIpPhoneDirectory::new(
            "Directory",
            "Choose",
            vec![CiscoIpPhoneDirectoryEntry {
                name: Some("x".repeat(PHONE_DIRECTORY_TEXT_MAX_CHARS + 1)),
                telephone: Some("1000".into()),
            }],
        )
        .unwrap_err();
        assert!(matches!(invalid, PhoneXmlError::InvalidField { .. }));

        assert!(PhoneSoftKeyPosition::new(0).is_err());
        assert!(PhoneSoftKeyPosition::new(-1).is_ok());
        assert!(PhoneSoftKeyPosition::new(16).is_ok());
        assert!(PhoneSoftKeyPosition::new(17).is_err());

        assert!(
            CiscoIpPhoneDirectory::from_xml(b"<!DOCTYPE directory><CiscoIPPhoneDirectory/>",)
                .is_err()
        );
        assert!(matches!(
            CiscoIpPhoneDirectory::from_xml(&vec![b'x'; PHONE_DIRECTORY_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
    }
}
