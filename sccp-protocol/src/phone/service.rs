//! Typed payloads returned by phone-hosted service applications.
//!
//! The SCCP application envelope carries routing identifiers separately from
//! its payload. Execute responses use documented XML schemas, while interactive
//! input and menu callbacks use a relative route followed by a standard URL
//! query string.

use std::fmt;

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::xml::{self as phone_xml, PhoneXmlError};
use crate::types::{ApplicationId, CallReference, LineInstance, TransactionId};

/// Maximum application payload accepted from an envelope, in bytes.
pub const MAX_PHONE_SERVICE_DATA_BYTES: usize = 2_000;
/// Maximum number of result items in an execute response.
pub const MAX_PHONE_SERVICE_RESPONSE_ITEMS: usize = 3;
/// Maximum number of decoded components in a submission route.
pub const MAX_PHONE_SERVICE_ROUTE_SEGMENTS: usize = 32;
/// Maximum UTF-8 byte length of one decoded route component.
pub const MAX_PHONE_SERVICE_ROUTE_COMPONENT_BYTES: usize = 1_024;
/// Maximum number of ordered name/value pairs retained from a submission.
pub const MAX_PHONE_SERVICE_SUBMITTED_VALUES: usize = 32;
/// Maximum UTF-8 byte length of a decoded submission parameter name.
pub const MAX_PHONE_SERVICE_PARAMETER_NAME_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a decoded submission parameter value.
pub const MAX_PHONE_SERVICE_PARAMETER_VALUE_BYTES: usize = 1_024;
/// Maximum character count of the data field in an execute result item.
pub const MAX_PHONE_SERVICE_RESPONSE_DATA_CHARS: usize = 256;
/// Maximum character count of the URL field in an execute result item.
pub const MAX_PHONE_SERVICE_RESPONSE_URL_CHARS: usize = 256;
/// Maximum character count of a structured application error message.
pub const MAX_PHONE_SERVICE_ERROR_MESSAGE_CHARS: usize = 256;

/// Envelope direction used to choose the permitted payload grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhoneServiceMessageKind {
    /// An application-data envelope, which may contain an interactive submission.
    Data,
    /// A response envelope, which accepts only structured responses or opaque data.
    Response,
}

/// Identifiers copied from the SCCP application envelope rather than inferred
/// from the submitted payload.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PhoneServiceRouting {
    pub application_id: ApplicationId,
    pub line_instance: LineInstance,
    pub call_reference: CallReference,
    pub transaction_id: TransactionId,
}

/// Additional selectors carried only by the extended application envelope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhoneServiceExtendedRouting {
    /// Sender-defined continuation marker retained without interpretation.
    pub sequence_flag: u32,
    /// Sender-defined display ordering hint retained without interpretation.
    pub display_priority: u32,
    /// Conference association from the extended envelope, or zero when absent.
    pub conference_id: u32,
    /// Instance discriminator for applications with concurrent executions.
    pub application_instance_id: u32,
    /// Sender-defined routing selector retained without interpretation.
    pub routing: u32,
}

/// One decoded application envelope and its typed or preserved payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhoneServiceEvent {
    /// Determines which payload grammar was accepted.
    pub kind: PhoneServiceMessageKind,
    pub routing: PhoneServiceRouting,
    /// Extended fields when the envelope used the extended message form.
    pub extended: Option<PhoneServiceExtendedRouting>,
    /// Decoded payload or exact bounded bytes for an unsupported payload.
    pub payload: PhoneServicePayload,
}

/// Supported application payloads after envelope decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhoneServicePayload {
    ExecuteResponse(CiscoIpPhoneResponse),
    Error(CiscoIpPhoneError),
    Submission(PhoneServiceSubmission),
    /// A syntactically valid but unsupported XML schema, a non-XML response,
    /// or binary application data. The protocol boundary already limits it.
    Opaque(Vec<u8>),
}

/// A bounded collection of results for previously requested execute actions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneResponse")]
pub struct CiscoIpPhoneResponse {
    #[serde(rename = "ResponseItem", default)]
    pub items: Vec<CiscoIpPhoneResponseItem>,
}

impl CiscoIpPhoneResponse {
    /// Enforces the response item count and text bounds before serialization.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        if self.items.len() > MAX_PHONE_SERVICE_RESPONSE_ITEMS {
            return Err(PhoneXmlError::LimitExceeded {
                kind: "phone-service response items",
                actual: self.items.len(),
                maximum: MAX_PHONE_SERVICE_RESPONSE_ITEMS,
            });
        }
        for item in &self.items {
            validate_response_text(
                "phone-service response data",
                &item.data,
                MAX_PHONE_SERVICE_RESPONSE_DATA_CHARS,
            )?;
            validate_response_text(
                "phone-service response URL",
                &item.url,
                MAX_PHONE_SERVICE_RESPONSE_URL_CHARS,
            )?;
        }
        Ok(())
    }
}

/// Extensible execute result code that preserves unknown numeric values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PhoneExecuteStatus(pub u32);

impl PhoneExecuteStatus {
    pub const OK: Self = Self(0);
    pub const ERROR: Self = Self(1);
    pub const URI_NOT_FOUND: Self = Self(4);
    pub const NO_ACTIVE_CALL: Self = Self(6);

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Result metadata for one requested execute action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CiscoIpPhoneResponseItem {
    #[serde(rename = "@Status", alias = "Status")]
    pub status: PhoneExecuteStatus,
    #[serde(rename = "@Data", alias = "Data")]
    /// Result text constrained to [`MAX_PHONE_SERVICE_RESPONSE_DATA_CHARS`] characters.
    pub data: String,
    #[serde(rename = "@URL", alias = "URL")]
    /// Result URL constrained to [`MAX_PHONE_SERVICE_RESPONSE_URL_CHARS`] characters.
    pub url: String,
}

/// Extensible structured-error number that preserves unknown numeric values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PhoneServiceErrorCode(pub u32);

impl PhoneServiceErrorCode {
    pub const PARSING: Self = Self(1);
    pub const FRAMING: Self = Self(2);
    pub const INTERNAL_FILE: Self = Self(3);
    pub const AUTHENTICATION: Self = Self(4);

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Structured application error suitable for XML serialization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneError")]
pub struct CiscoIpPhoneError {
    #[serde(rename = "@Number", alias = "Number")]
    pub number: PhoneServiceErrorCode,
    #[serde(rename = "$text", default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

impl CiscoIpPhoneError {
    /// Enforces the error-message character bound before serialization.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_response_text(
            "phone-service error message",
            &self.message,
            MAX_PHONE_SERVICE_ERROR_MESSAGE_CHARS,
        )
    }
}

/// Percent-decoded interactive callback submitted by a phone application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhoneServiceSubmission {
    /// Percent-decoded route components in their original order.
    pub route: Vec<String>,
    /// Ordered form values. Duplicate names remain distinct.
    pub values: Vec<PhoneServiceSubmittedValue>,
}

impl PhoneServiceSubmission {
    /// Iterates matching values in submission order, including duplicates.
    pub fn values_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.values
            .iter()
            .filter(move |value| value.name == name)
            .map(|value| value.value.as_str())
    }
}

/// One submitted name/value pair whose value is redacted from diagnostics.
#[derive(Clone, Eq, PartialEq)]
pub struct PhoneServiceSubmittedValue {
    pub name: String,
    /// Percent-decoded value redacted from [`Debug`](std::fmt::Debug) output.
    pub value: String,
}

impl fmt::Debug for PhoneServiceSubmittedValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PhoneServiceSubmittedValue")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// Failures while validating or decoding an application payload.
#[derive(Debug, Error)]
pub enum PhoneServiceError {
    /// A structured XML payload was malformed or violated its schema bounds.
    #[error(transparent)]
    Xml(#[from] PhoneXmlError),
    #[error("phone-service submission is not valid UTF-8")]
    InvalidUtf8,
    #[error("phone-service submission contains invalid percent encoding")]
    InvalidPercentEncoding,
    #[error("phone-service submission contains a forbidden control character")]
    ControlCharacter,
    /// The route exceeded [`MAX_PHONE_SERVICE_ROUTE_SEGMENTS`].
    #[error("phone-service submission route has {actual} components; maximum is {maximum}")]
    TooManyRouteSegments { actual: usize, maximum: usize },
    /// The query exceeded [`MAX_PHONE_SERVICE_SUBMITTED_VALUES`].
    #[error("phone-service submission has {actual} values; maximum is {maximum}")]
    TooManyValues { actual: usize, maximum: usize },
    #[error("phone-service submission has an empty parameter name")]
    EmptyParameterName,
    #[error("phone-service submission has an empty route component")]
    EmptyRouteComponent,
    /// A decoded route, name, or value exceeded its corresponding byte limit.
    #[error("phone-service submission {kind} has {actual} bytes; maximum is {maximum}")]
    ComponentTooLong {
        kind: &'static str,
        actual: usize,
        maximum: usize,
    },
}

#[derive(Deserialize)]
enum PhoneServiceXmlSchema {
    #[serde(rename = "CiscoIPPhoneResponse")]
    Response {
        #[serde(rename = "ResponseItem", default)]
        items: Vec<CiscoIpPhoneResponseItem>,
    },
    #[serde(rename = "CiscoIPPhoneError")]
    Error {
        #[serde(rename = "@Number", alias = "Number")]
        number: PhoneServiceErrorCode,
        #[serde(rename = "$text", default)]
        message: String,
    },
    #[serde(other)]
    Unknown,
}

/// Parse application data according to its message kind. Response envelopes
/// accept the two documented XML schemas; data envelopes additionally accept
/// interactive route/query submissions.
pub fn parse_phone_service_payload(
    data: &[u8],
    kind: PhoneServiceMessageKind,
) -> Result<PhoneServicePayload, PhoneServiceError> {
    if data.len() > MAX_PHONE_SERVICE_DATA_BYTES {
        return Err(PhoneXmlError::LimitExceeded {
            kind: "phone-service data",
            actual: data.len(),
            maximum: MAX_PHONE_SERVICE_DATA_BYTES,
        }
        .into());
    }
    let trimmed = trim_protocol_padding(data);
    if trimmed.is_empty() {
        return Ok(PhoneServicePayload::Opaque(data.to_vec()));
    }

    if trimmed.first() == Some(&b'<') {
        return match phone_xml::from_bytes::<PhoneServiceXmlSchema>(
            trimmed,
            MAX_PHONE_SERVICE_DATA_BYTES,
        )? {
            PhoneServiceXmlSchema::Response { items } => {
                let response = CiscoIpPhoneResponse { items };
                response.validate()?;
                Ok(PhoneServicePayload::ExecuteResponse(response))
            }
            PhoneServiceXmlSchema::Error { number, message } => {
                let error = CiscoIpPhoneError { number, message };
                error.validate()?;
                Ok(PhoneServicePayload::Error(error))
            }
            PhoneServiceXmlSchema::Unknown => Ok(PhoneServicePayload::Opaque(data.to_vec())),
        };
    }

    if kind == PhoneServiceMessageKind::Response {
        return Ok(PhoneServicePayload::Opaque(data.to_vec()));
    }
    let Ok(text) = std::str::from_utf8(trimmed) else {
        return Ok(PhoneServicePayload::Opaque(data.to_vec()));
    };
    parse_submission(text).map(PhoneServicePayload::Submission)
}

fn validate_response_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), PhoneXmlError> {
    if value.chars().count() > maximum {
        Err(PhoneXmlError::InvalidField {
            field,
            expected: "within the documented phone-service text bound",
        })
    } else {
        Ok(())
    }
}

fn parse_submission(text: &str) -> Result<PhoneServiceSubmission, PhoneServiceError> {
    if text.chars().any(char::is_control) || text.contains('#') {
        return Err(PhoneServiceError::ControlCharacter);
    }
    let (route, query) = text
        .split_once('?')
        .map_or((text, None), |(route, query)| (route, Some(query)));
    let route = if route.is_empty() {
        Vec::new()
    } else {
        route
            .split('/')
            .map(|component| {
                decode_component(
                    component,
                    "route component",
                    MAX_PHONE_SERVICE_ROUTE_COMPONENT_BYTES,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    if route.len() > MAX_PHONE_SERVICE_ROUTE_SEGMENTS {
        return Err(PhoneServiceError::TooManyRouteSegments {
            actual: route.len(),
            maximum: MAX_PHONE_SERVICE_ROUTE_SEGMENTS,
        });
    }
    let values = query.map_or_else(|| Ok(Vec::new()), parse_form_values)?;
    Ok(PhoneServiceSubmission { route, values })
}

fn parse_form_values(query: &str) -> Result<Vec<PhoneServiceSubmittedValue>, PhoneServiceError> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    for field in query.split('&') {
        if field.is_empty() {
            return Err(PhoneServiceError::EmptyParameterName);
        }
        let (name, value) = field.split_once('=').unwrap_or((field, ""));
        validate_encoded_component(name)?;
        validate_encoded_component(value)?;
        percent_decode_str(name)
            .decode_utf8()
            .map_err(|_| PhoneServiceError::InvalidUtf8)?;
        percent_decode_str(value)
            .decode_utf8()
            .map_err(|_| PhoneServiceError::InvalidUtf8)?;
    }

    let values = form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| {
            if name.is_empty() {
                return Err(PhoneServiceError::EmptyParameterName);
            }
            validate_component_length(
                "parameter name",
                &name,
                MAX_PHONE_SERVICE_PARAMETER_NAME_BYTES,
            )?;
            validate_component_length(
                "parameter value",
                &value,
                MAX_PHONE_SERVICE_PARAMETER_VALUE_BYTES,
            )?;
            reject_decoded_controls(&name)?;
            reject_decoded_controls(&value)?;
            Ok(PhoneServiceSubmittedValue {
                name: name.into_owned(),
                value: value.into_owned(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values.len() > MAX_PHONE_SERVICE_SUBMITTED_VALUES {
        return Err(PhoneServiceError::TooManyValues {
            actual: values.len(),
            maximum: MAX_PHONE_SERVICE_SUBMITTED_VALUES,
        });
    }
    Ok(values)
}

fn decode_component(
    encoded: &str,
    kind: &'static str,
    maximum: usize,
) -> Result<String, PhoneServiceError> {
    if encoded.is_empty() {
        return Err(PhoneServiceError::EmptyRouteComponent);
    }
    validate_encoded_component(encoded)?;
    let decoded = percent_decode_str(encoded)
        .decode_utf8()
        .map_err(|_| PhoneServiceError::InvalidUtf8)?;
    validate_component_length(kind, &decoded, maximum)?;
    reject_decoded_controls(&decoded)?;
    Ok(decoded.into_owned())
}

fn validate_encoded_component(encoded: &str) -> Result<(), PhoneServiceError> {
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(pair) = bytes.get(index + 1..index + 3) else {
                return Err(PhoneServiceError::InvalidPercentEncoding);
            };
            if !pair.iter().all(u8::is_ascii_hexdigit) {
                return Err(PhoneServiceError::InvalidPercentEncoding);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn validate_component_length(
    kind: &'static str,
    component: &str,
    maximum: usize,
) -> Result<(), PhoneServiceError> {
    if component.len() > maximum {
        return Err(PhoneServiceError::ComponentTooLong {
            kind,
            actual: component.len(),
            maximum,
        });
    }
    Ok(())
}

fn reject_decoded_controls(component: &str) -> Result<(), PhoneServiceError> {
    if component.chars().any(char::is_control) {
        Err(PhoneServiceError::ControlCharacter)
    } else {
        Ok(())
    }
}

fn trim_protocol_padding(mut data: &[u8]) -> &[u8] {
    while data.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
        data = &data[1..];
    }
    while data
        .last()
        .is_some_and(|byte| *byte == 0 || byte.is_ascii_whitespace())
    {
        data = &data[..data.len() - 1];
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_execute_responses_and_errors_use_the_xml_boundary() {
        let payload = parse_phone_service_payload(
            br#"<CiscoIPPhoneResponse><ResponseItem Status="0" Data="Taylor &amp; Co" URL="Play:chime.raw"/><ResponseItem Status="6" Data="No Active Call" URL="SendDigits:12"/></CiscoIPPhoneResponse>"#,
            PhoneServiceMessageKind::Response,
        )
        .unwrap();
        let PhoneServicePayload::ExecuteResponse(response) = payload else {
            panic!("expected typed execute response");
        };
        assert_eq!(response.items.len(), 2);
        assert_eq!(response.items[0].status, PhoneExecuteStatus::OK);
        assert_eq!(response.items[0].data, "Taylor & Co");
        assert_eq!(response.items[1].status, PhoneExecuteStatus::NO_ACTIVE_CALL);

        assert_eq!(
            parse_phone_service_payload(
                br#"<CiscoIPPhoneError Number="4">Authentication failed</CiscoIPPhoneError>"#,
                PhoneServiceMessageKind::Response,
            )
            .unwrap(),
            PhoneServicePayload::Error(CiscoIpPhoneError {
                number: PhoneServiceErrorCode::AUTHENTICATION,
                message: "Authentication failed".into(),
            })
        );
    }

    #[test]
    fn submitted_route_and_form_values_are_decoded_in_order() {
        let payload = parse_phone_service_payload(
            b"invite/desk%20one?NUMBER=555%2A12&NAME=Fran%C3%A7ois&NOTE=a+b&NOTE=second",
            PhoneServiceMessageKind::Data,
        )
        .unwrap();
        let PhoneServicePayload::Submission(submission) = payload else {
            panic!("expected typed submission");
        };
        assert_eq!(submission.route, ["invite", "desk one"]);
        assert_eq!(
            submission
                .values
                .iter()
                .map(|value| (value.name.as_str(), value.value.as_str()))
                .collect::<Vec<_>>(),
            [
                ("NUMBER", "555*12"),
                ("NAME", "François"),
                ("NOTE", "a b"),
                ("NOTE", "second"),
            ]
        );
        assert_eq!(
            submission.values_named("NOTE").collect::<Vec<_>>(),
            ["a b", "second"]
        );
        let debug = format!("{:?}", submission.values[0]);
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("555"));
    }

    #[test]
    fn opaque_payloads_are_reserved_for_unknown_schemas() {
        let unknown = b"<VendorPhoneResult><Value>one</Value></VendorPhoneResult>";
        assert_eq!(
            parse_phone_service_payload(unknown, PhoneServiceMessageKind::Response).unwrap(),
            PhoneServicePayload::Opaque(unknown.to_vec())
        );
        assert_eq!(
            parse_phone_service_payload(b"Success", PhoneServiceMessageKind::Response).unwrap(),
            PhoneServicePayload::Opaque(b"Success".to_vec())
        );
        assert_eq!(
            parse_phone_service_payload(&[0xff, 0x00], PhoneServiceMessageKind::Data).unwrap(),
            PhoneServicePayload::Opaque(vec![0xff, 0x00])
        );
    }

    #[test]
    fn malformed_xml_encoding_and_bounds_fail_closed_without_values_in_errors() {
        for malformed in [
            b"<CiscoIPPhoneResponse>".as_slice(),
            b"<!DOCTYPE x><CiscoIPPhoneResponse/>".as_slice(),
            b"<VendorPhoneResult>".as_slice(),
        ] {
            assert!(
                parse_phone_service_payload(malformed, PhoneServiceMessageKind::Response).is_err()
            );
        }

        for malformed in [
            "invite?PIN=secret%",
            "invite?PIN=secret%GG",
            "invite?PIN=%FF",
            "invite?PIN=secret%0Avalue",
            "invite?=secret",
            "invite//desk?PIN=secret",
            "invite?PIN=secret#fragment",
        ] {
            let error =
                parse_phone_service_payload(malformed.as_bytes(), PhoneServiceMessageKind::Data)
                    .unwrap_err()
                    .to_string();
            assert!(!error.contains("secret"), "{error}");
        }

        assert!(
            parse_phone_service_payload(
                &vec![b'x'; MAX_PHONE_SERVICE_DATA_BYTES + 1],
                PhoneServiceMessageKind::Data,
            )
            .is_err()
        );
        let too_many = format!(
            "route?{}",
            (0..=MAX_PHONE_SERVICE_SUBMITTED_VALUES)
                .map(|index| format!("p{index}=x"))
                .collect::<Vec<_>>()
                .join("&")
        );
        assert!(
            parse_phone_service_payload(too_many.as_bytes(), PhoneServiceMessageKind::Data,)
                .is_err()
        );
        let too_many_items = format!(
            "<CiscoIPPhoneResponse>{}</CiscoIPPhoneResponse>",
            r#"<ResponseItem Status="0" Data="ok" URL="Init:Services"/>"#
                .repeat(MAX_PHONE_SERVICE_RESPONSE_ITEMS + 1)
        );
        assert!(
            parse_phone_service_payload(
                too_many_items.as_bytes(),
                PhoneServiceMessageKind::Response,
            )
            .is_err()
        );
    }

    #[test]
    fn every_submission_collection_and_component_bound_is_enforced() {
        let too_many_route_segments =
            std::iter::repeat_n("x", MAX_PHONE_SERVICE_ROUTE_SEGMENTS + 1)
                .collect::<Vec<_>>()
                .join("/");
        assert!(matches!(
            parse_phone_service_payload(
                too_many_route_segments.as_bytes(),
                PhoneServiceMessageKind::Data,
            ),
            Err(PhoneServiceError::TooManyRouteSegments { .. })
        ));

        let long_route = "x".repeat(MAX_PHONE_SERVICE_ROUTE_COMPONENT_BYTES + 1);
        assert!(matches!(
            parse_phone_service_payload(long_route.as_bytes(), PhoneServiceMessageKind::Data),
            Err(PhoneServiceError::ComponentTooLong {
                kind: "route component",
                ..
            })
        ));

        let long_name = format!(
            "route?{}=value",
            "n".repeat(MAX_PHONE_SERVICE_PARAMETER_NAME_BYTES + 1)
        );
        assert!(matches!(
            parse_phone_service_payload(long_name.as_bytes(), PhoneServiceMessageKind::Data),
            Err(PhoneServiceError::ComponentTooLong {
                kind: "parameter name",
                ..
            })
        ));

        let long_value = format!(
            "route?name={}",
            "v".repeat(MAX_PHONE_SERVICE_PARAMETER_VALUE_BYTES + 1)
        );
        assert!(matches!(
            parse_phone_service_payload(long_value.as_bytes(), PhoneServiceMessageKind::Data),
            Err(PhoneServiceError::ComponentTooLong {
                kind: "parameter value",
                ..
            })
        ));
    }
}
