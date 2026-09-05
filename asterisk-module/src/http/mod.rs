//! Policy-free registration of bounded Asterisk HTTP handlers.
//!
//! The native boundary copies method, route, path, remote address, ordered
//! query/header fields and body into bounded owned Rust values. Handler output
//! owns status, content type, headers and body. Unsafe route/text/NUL, invalid
//! header syntax, reserved framing headers, overflow, callback panic and native
//! errors fail closed.
//!
//! [`directory`] is lifecycle-registered by the loadable module at
//! `/sccp/directory`. [`authentication`] provides `/sccp/authenticate` as a
//! reusable policy boundary, but the module does not register it because no
//! credential repository or authorization policy is configured. Asterisk's
//! built-in HTTP listener must be enabled independently.
//!
//! Concrete backends own RAII registrations that invalidate routes, unregister
//! them, and wait for callbacks already in flight. Request and response bodies
//! never borrow backend-owned storage.

pub mod authentication;
pub mod directory;

use std::ffi::{CString, NulError};
use std::fmt;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use std::sync::Arc;

use sccp_protocol::PhoneXmlRefresh;
use thiserror::Error;

const ALLOW_GET: u32 = 1 << 0;
const ALLOW_POST: u32 = 1 << 1;
const ALLOW_HEAD: u32 = 1 << 2;
const ALLOW_PUT: u32 = 1 << 3;
const ALLOW_DELETE: u32 = 1 << 4;
const ALLOW_OPTIONS: u32 = 1 << 5;
const ALLOW_ALL: u32 =
    ALLOW_GET | ALLOW_POST | ALLOW_HEAD | ALLOW_PUT | ALLOW_DELETE | ALLOW_OPTIONS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpMethod {
    Get,
    Post,
    Head,
    Put,
    Delete,
    Options,
    Unknown,
}

impl HttpMethod {
    pub(crate) const fn flag(self) -> u32 {
        match self {
            Self::Get => ALLOW_GET,
            Self::Post => ALLOW_POST,
            Self::Head => ALLOW_HEAD,
            Self::Put => ALLOW_PUT,
            Self::Delete => ALLOW_DELETE,
            Self::Options => ALLOW_OPTIONS,
            Self::Unknown => 0,
        }
    }
}

/// A non-empty set of methods accepted by a registered handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpMethodSet(u32);

impl HttpMethodSet {
    pub const GET: Self = Self(ALLOW_GET);
    pub const POST: Self = Self(ALLOW_POST);
    pub const HEAD: Self = Self(ALLOW_HEAD);
    pub const PUT: Self = Self(ALLOW_PUT);
    pub const DELETE: Self = Self(ALLOW_DELETE);
    pub const OPTIONS: Self = Self(ALLOW_OPTIONS);
    pub const ALL: Self = Self(ALLOW_ALL);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, method: HttpMethod) -> bool {
        let flag = method.flag();
        flag != 0 && self.0 & flag == flag
    }

    pub const fn is_valid(self) -> bool {
        self.0 != 0 && self.0 & !ALLOW_ALL == 0
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn allow_header(self) -> String {
        let methods = [
            (ALLOW_GET, "GET"),
            (ALLOW_POST, "POST"),
            (ALLOW_HEAD, "HEAD"),
            (ALLOW_PUT, "PUT"),
            (ALLOW_DELETE, "DELETE"),
            (ALLOW_OPTIONS, "OPTIONS"),
        ]
        .into_iter()
        .filter_map(|(flag, name)| (self.0 & flag != 0).then_some(name))
        .collect::<Vec<_>>()
        .join(", ");
        format!("Allow: {methods}\r\n")
    }
}

/// Explicit upper bounds applied before a request enters Rust.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpLimits {
    pub max_body_bytes: usize,
    pub max_response_bytes: usize,
    pub max_path_bytes: usize,
    pub max_fields: usize,
    pub max_field_name_bytes: usize,
    pub max_field_value_bytes: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_path_bytes: 4096,
            max_fields: 256,
            max_field_name_bytes: 1024,
            max_field_value_bytes: 16 * 1024,
        }
    }
}

impl HttpLimits {
    pub fn validate(self) -> Result<Self, HttpError> {
        if self.max_body_bytes == 0
            || self.max_response_bytes == 0
            || self.max_path_bytes == 0
            || self.max_fields == 0
            || self.max_field_name_bytes == 0
            || self.max_field_value_bytes == 0
        {
            return Err(HttpError::ZeroLimit);
        }
        Ok(self)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HttpField {
    pub name: String,
    pub value: String,
}

impl fmt::Debug for HttpField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpField")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HttpRequest {
    pub method: HttpMethod,
    /// Configured route, including its leading slash.
    pub route: String,
    /// Full decoded request path, excluding query parameters.
    pub path: String,
    pub remote_address: String,
    /// Query parameters in request order, including repeated names.
    pub query: Vec<HttpField>,
    /// Headers in request order, including repeated names.
    pub headers: Vec<HttpField>,
    pub body: Vec<u8>,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("route", &self.route)
            .field("path", &self.path)
            .field("remote_address", &self.remote_address)
            .field("query", &self.query)
            .field("headers", &self.headers)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// An owned response returned by a handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    status: u16,
    content_type: String,
    headers: Vec<HttpField>,
    body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(
        status: u16,
        content_type: impl Into<String>,
        body: impl Into<Vec<u8>>,
    ) -> Result<Self, HttpError> {
        if !(200..=599).contains(&status) {
            return Err(HttpError::InvalidStatus(status));
        }
        let content_type = content_type.into();
        validate_content_type(&content_type)?;
        Ok(Self {
            status,
            content_type,
            headers: Vec::new(),
            body: body.into(),
        })
    }

    pub fn text(status: u16, body: impl Into<String>) -> Result<Self, HttpError> {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            body.into().into_bytes(),
        )
    }

    pub const fn status(&self) -> u16 {
        self.status
    }

    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    pub fn headers(&self) -> &[HttpField] {
        &self.headers
    }

    /// Adds a response header after validating its RFC token name and
    /// injection-safe field value. Framing and content headers remain owned by
    /// the native HTTP adapter and cannot be overridden here.
    pub fn with_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, HttpError> {
        let field = HttpField {
            name: name.into(),
            value: value.into(),
        };
        validate_response_header(&field)?;
        self.headers.push(field);
        Ok(self)
    }

    /// Adds the handset text-service refresh instruction as HTTP metadata.
    /// Refresh is deliberately not represented as an XML element.
    pub fn with_phone_xml_refresh(self, refresh: &PhoneXmlRefresh) -> Result<Self, HttpError> {
        self.with_header("Refresh", refresh.http_header_value())
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn validate_for(&self, limits: HttpLimits) -> Result<(), HttpResponseError> {
        if self.body.len() > limits.max_response_bytes {
            return Err(HttpResponseError::BodyTooLarge);
        }
        if self.content_type.len() > limits.max_field_value_bytes {
            return Err(HttpResponseError::ContentTypeTooLarge);
        }
        if self.headers.len() > limits.max_fields {
            return Err(HttpResponseError::TooManyHeaders);
        }
        let mut header_bytes = "Content-Type: \r\n".len() + self.content_type.len();
        for field in &self.headers {
            if field.name.len() > limits.max_field_name_bytes
                || field.value.len() > limits.max_field_value_bytes
            {
                return Err(HttpResponseError::HeaderTooLarge);
            }
            validate_response_header(field).map_err(|_| HttpResponseError::InvalidHeader)?;
            header_bytes = header_bytes
                .checked_add(field.name.len())
                .and_then(|length| length.checked_add(field.value.len()))
                .and_then(|length| length.checked_add(4))
                .ok_or(HttpResponseError::HeadersTooLarge)?;
        }
        if header_bytes > limits.max_response_bytes {
            return Err(HttpResponseError::HeadersTooLarge);
        }
        Ok(())
    }
}

/// A typed request-framing failure, before a handler is invoked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) enum HttpFramingError {
    InvalidContentLength,
    UnsupportedTransferEncoding,
    BodyTooLarge,
}

/// A typed handler-response validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) enum HttpResponseError {
    BodyTooLarge,
    ContentTypeTooLarge,
    TooManyHeaders,
    HeaderTooLarge,
    HeadersTooLarge,
    InvalidHeader,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) trait HttpHandler: Send + Sync {
    fn handle(&self, request: HttpRequest) -> HttpResponse;
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
impl<F> HttpHandler for F
where
    F: Fn(HttpRequest) -> HttpResponse + Send + Sync,
{
    fn handle(&self, request: HttpRequest) -> HttpResponse {
        self(request)
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) type SharedHttpHandler = Arc<dyn HttpHandler>;

/// Backend port for typed HTTP route registration.
pub trait HttpBackend: Clone + Send + Sync + 'static {
    type Registration: Send + 'static;

    fn register<F>(
        &self,
        path: &str,
        description: &str,
        has_subtree: bool,
        methods: HttpMethodSet,
        limits: HttpLimits,
        handler: F,
    ) -> Result<Self::Registration, HttpError>
    where
        F: Fn(HttpRequest) -> HttpResponse + Send + Sync + 'static;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UnavailableHttp;

#[cfg(test)]
impl HttpBackend for UnavailableHttp {
    type Registration = ();

    fn register<F>(
        &self,
        _path: &str,
        _description: &str,
        _has_subtree: bool,
        _methods: HttpMethodSet,
        _limits: HttpLimits,
        _handler: F,
    ) -> Result<Self::Registration, HttpError>
    where
        F: Fn(HttpRequest) -> HttpResponse + Send + Sync + 'static,
    {
        Err(HttpError::Unavailable)
    }
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("{field} contains a NUL byte")]
    InvalidText {
        field: &'static str,
        #[source]
        source: NulError,
    },

    #[error("HTTP path must be an absolute non-root path using safe URL-segment characters")]
    InvalidPath,

    #[error("{field} must not be empty")]
    EmptyText { field: &'static str },

    #[error("at least one HTTP method must be enabled")]
    EmptyMethods,

    #[error("HTTP bounds must all be greater than zero")]
    ZeroLimit,

    #[error("HTTP path exceeds the configured request-path limit")]
    PathExceedsLimit,

    #[error("HTTP response status {0} is outside 200..=599")]
    InvalidStatus(u16),

    #[error("HTTP response content type is empty or contains invalid characters")]
    InvalidContentType,

    #[error("HTTP response header name is not a valid RFC token")]
    InvalidHeaderName,

    #[error("HTTP response header value contains invalid characters")]
    InvalidHeaderValue,

    #[error("HTTP response header {0} is managed by the native HTTP adapter")]
    ReservedResponseHeader(String),

    #[error("unable to register Asterisk HTTP route")]
    RegistrationFailed,

    #[error("Asterisk HTTP support is unavailable in development builds")]
    Unavailable,
}

pub fn registered_path(path: &str) -> Result<(String, String), HttpError> {
    if path.len() < 2
        || !path.starts_with('/')
        || path.ends_with('/')
        || !path.is_ascii()
        || path.split('/').skip(1).any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(HttpError::InvalidPath);
    }
    let route = path.to_owned();
    let native = CString::new(&path[1..]).map_err(|source| HttpError::InvalidText {
        field: "path",
        source,
    })?;
    Ok((native.into_string().expect("validated ASCII path"), route))
}

pub fn nonempty_text(field: &'static str, value: &str) -> Result<String, HttpError> {
    if value.is_empty() {
        return Err(HttpError::EmptyText { field });
    }
    CString::new(value)
        .map_err(|source| HttpError::InvalidText { field, source })
        .map(|value| value.into_string().expect("input originated as UTF-8"))
}

fn validate_content_type(value: &str) -> Result<(), HttpError> {
    if value.is_empty()
        || value.contains(['\r', '\n', '\0'])
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        Err(HttpError::InvalidContentType)
    } else {
        Ok(())
    }
}

fn validate_response_header(field: &HttpField) -> Result<(), HttpError> {
    if field.name.is_empty()
        || !field.name.is_ascii()
        || !field.name.bytes().all(is_http_token_byte)
    {
        return Err(HttpError::InvalidHeaderName);
    }
    if [
        "content-type",
        "content-length",
        "transfer-encoding",
        "connection",
    ]
    .iter()
    .any(|reserved| field.name.eq_ignore_ascii_case(reserved))
    {
        return Err(HttpError::ReservedResponseHeader(field.name.clone()));
    }
    if !field.value.is_ascii()
        || field
            .value
            .bytes()
            .any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
    {
        return Err(HttpError::InvalidHeaderValue);
    }
    Ok(())
}

const fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) const fn http_status_title(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Response",
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn request_body_length(
    headers: &[HttpField],
    maximum: usize,
) -> Result<usize, HttpFramingError> {
    let mut parsed_length = None;
    for field in headers {
        if field.name.eq_ignore_ascii_case("transfer-encoding")
            && !field.value.trim_ascii().eq_ignore_ascii_case("identity")
        {
            return Err(HttpFramingError::UnsupportedTransferEncoding);
        }
        if !field.name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let value = field.value.trim_ascii();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(HttpFramingError::InvalidContentLength);
        }
        let value = value
            .parse::<usize>()
            .map_err(|_| HttpFramingError::InvalidContentLength)?;
        if parsed_length.is_some_and(|previous| previous != value) {
            return Err(HttpFramingError::InvalidContentLength);
        }
        parsed_length = Some(value);
    }
    let length = parsed_length.unwrap_or(0);
    if length > maximum {
        Err(HttpFramingError::BodyTooLarge)
    } else {
        Ok(length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> Vec<HttpField> {
        pairs
            .iter()
            .map(|(name, value)| HttpField {
                name: (*name).to_owned(),
                value: (*value).to_owned(),
            })
            .collect()
    }

    #[test]
    fn request_framing_returns_typed_errors() {
        assert_eq!(request_body_length(&[], 10), Ok(0));
        assert_eq!(
            request_body_length(
                &fields(&[("Content-Length", " 7\t"), ("content-length", "7")]),
                10,
            ),
            Ok(7)
        );
        assert_eq!(
            request_body_length(&fields(&[("Content-Length", "7x")]), 10),
            Err(HttpFramingError::InvalidContentLength)
        );
        assert_eq!(
            request_body_length(
                &fields(&[("Content-Length", "7"), ("Content-Length", "8")]),
                10,
            ),
            Err(HttpFramingError::InvalidContentLength)
        );
        assert_eq!(
            request_body_length(&fields(&[("Transfer-Encoding", "chunked")]), 10),
            Err(HttpFramingError::UnsupportedTransferEncoding)
        );
        assert_eq!(
            request_body_length(&fields(&[("Content-Length", "11")]), 10),
            Err(HttpFramingError::BodyTooLarge)
        );
    }

    #[test]
    fn typed_handler_owns_request_and_response() {
        let handler: SharedHttpHandler = Arc::new(|request: HttpRequest| {
            assert_eq!(request.method, HttpMethod::Post);
            assert_eq!(request.route, "/provision");
            assert_eq!(request.path, "/provision/SEP001/config.xml");
            assert_eq!(request.body, vec![0, 1, 2, 0xff]);
            HttpResponse::new(201, "application/octet-stream", vec![0, 3, 0xff]).unwrap()
        });
        let response = handler.handle(HttpRequest {
            method: HttpMethod::Post,
            route: "/provision".to_owned(),
            path: "/provision/SEP001/config.xml".to_owned(),
            remote_address: "192.0.2.5".to_owned(),
            query: fields(&[("model", "7962"), ("model", "fallback")]),
            headers: fields(&[("Content-Type", "application/octet-stream")]),
            body: vec![0, 1, 2, 0xff],
        });
        assert_eq!(response.status(), 201);
        assert_eq!(response.content_type(), "application/octet-stream");
        assert_eq!(response.body(), &[0, 3, 0xff]);
    }

    #[test]
    fn validates_paths_limits_methods_and_responses() {
        let methods = HttpMethodSet::GET.union(HttpMethodSet::DELETE);
        assert!(methods.contains(HttpMethod::Get));
        assert!(methods.contains(HttpMethod::Delete));
        assert!(!methods.contains(HttpMethod::Post));
        assert!(!methods.contains(HttpMethod::Unknown));
        assert_eq!(methods.allow_header(), "Allow: GET, DELETE\r\n");

        for path in [
            "",
            "/",
            "relative",
            "/bad/../path",
            "/bad path",
            "/trailing/",
        ] {
            assert!(matches!(registered_path(path), Err(HttpError::InvalidPath)));
        }
        assert_eq!(
            registered_path("/phones/config-v2").unwrap(),
            (
                "phones/config-v2".to_owned(),
                "/phones/config-v2".to_owned()
            )
        );
        assert!(matches!(
            HttpLimits {
                max_body_bytes: 0,
                ..HttpLimits::default()
            }
            .validate(),
            Err(HttpError::ZeroLimit)
        ));
        assert!(matches!(
            HttpResponse::new(199, "text/plain", Vec::new()),
            Err(HttpError::InvalidStatus(199))
        ));
        assert!(matches!(
            HttpResponse::new(200, "text/plain\r\nInjected: yes", Vec::new()),
            Err(HttpError::InvalidContentType)
        ));
        assert_eq!(http_status_title(201), "Created");
        assert_eq!(http_status_title(299), "Response");
    }

    #[test]
    fn response_validation_is_typed_and_bounded() {
        let response = HttpResponse::text(200, "ok")
            .unwrap()
            .with_header("X-Test", "safe")
            .unwrap();
        assert_eq!(
            response.validate_for(HttpLimits {
                max_response_bytes: 1,
                ..HttpLimits::default()
            }),
            Err(HttpResponseError::BodyTooLarge)
        );
        assert_eq!(
            response.validate_for(HttpLimits {
                max_fields: 0,
                ..HttpLimits::default()
            }),
            Err(HttpResponseError::TooManyHeaders)
        );

        let invalid = HttpResponse {
            status: 200,
            content_type: "text/plain".to_owned(),
            headers: vec![HttpField {
                name: "X-Test".to_owned(),
                value: "safe\r\nInjected: yes".to_owned(),
            }],
            body: b"ok".to_vec(),
        };
        assert_eq!(
            invalid.validate_for(HttpLimits::default()),
            Err(HttpResponseError::InvalidHeader)
        );
    }

    #[test]
    fn refresh_header_remains_typed_http_metadata() {
        let refresh = PhoneXmlRefresh::new(15, "https://pbx.example/text?page=2").unwrap();
        let response = HttpResponse::new(
            200,
            "text/xml; charset=utf-8",
            b"<CiscoIPPhoneText><Text>next</Text></CiscoIPPhoneText>".to_vec(),
        )
        .unwrap()
        .with_phone_xml_refresh(&refresh)
        .unwrap();
        assert_eq!(
            response.headers(),
            &[HttpField {
                name: "Refresh".to_owned(),
                value: "15;url=https://pbx.example/text?page=2".to_owned(),
            }]
        );
    }

    #[cfg(feature = "development")]
    #[test]
    fn public_api_is_explicitly_unavailable_without_native_linkage() {
        let result = UnavailableHttp.register(
            "/provision",
            "Provisioning",
            true,
            HttpMethodSet::GET,
            HttpLimits::default(),
            |_| HttpResponse::text(200, "ok").unwrap(),
        );
        assert!(matches!(result, Err(HttpError::Unavailable)));
    }
}
