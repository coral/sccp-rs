//! Bounded HTTP service for phone authentication callbacks.
//!
//! The service accepts only `GET` with no body and the exact `UserID`,
//! `Password`, and `devicename` query fields parsed by
//! [`sccp_protocol::PhoneAuthenticationRequest`]. Unknown, repeated, malformed,
//! or oversized values fail closed. Responses are the phone's bounded plain-text
//! authorization tokens; credentials and form values are redacted from errors
//! and `Debug` output.
//!
//! [`PhoneAuthenticationProvider`] deliberately supplies the missing repository
//! and authorization policy. [`register_phone_authentication_http`] is therefore
//! a library integration point, not a route installed by module startup.

use sccp_protocol::{
    PHONE_AUTHENTICATION_MAX_QUERY_BYTES, PHONE_AUTHENTICATION_MAX_RESPONSE_BYTES,
    PhoneAuthenticationRequest, PhoneAuthenticationResponse,
};
use thiserror::Error;

use crate::http::{
    HttpBackend, HttpError, HttpLimits, HttpMethod, HttpMethodSet, HttpRequest, HttpResponse,
};

pub const PHONE_AUTHENTICATION_HTTP_PATH: &str = "/sccp/authenticate";

/// Supplies authorization policy without coupling the HTTP boundary to a
/// credential repository.
pub trait PhoneAuthenticationProvider: Send + Sync + 'static {
    fn authorize(
        &self,
        request: &PhoneAuthenticationRequest,
    ) -> Result<bool, PhoneAuthenticationProviderError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PhoneAuthenticationProviderError {
    #[error("phone authentication policy is temporarily unavailable")]
    Unavailable,
}

pub struct PhoneAuthenticationHttpService<P> {
    provider: P,
}

impl<P> PhoneAuthenticationHttpService<P>
where
    P: PhoneAuthenticationProvider,
{
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn handle(&self, request: HttpRequest) -> HttpResponse {
        if request.method != HttpMethod::Get || !request.body.is_empty() {
            return decision_response(405, PhoneAuthenticationResponse::Unauthorized);
        }
        let authentication = match PhoneAuthenticationRequest::from_fields(
            request
                .query
                .iter()
                .map(|field| (field.name.as_str(), field.value.as_str())),
        ) {
            Ok(request) => request,
            Err(_) => {
                return decision_response(400, PhoneAuthenticationResponse::Unauthorized);
            }
        };
        match self.provider.authorize(&authentication) {
            Ok(true) => decision_response(200, PhoneAuthenticationResponse::Authorized),
            Ok(false) => decision_response(200, PhoneAuthenticationResponse::Unauthorized),
            Err(_) => decision_response(503, PhoneAuthenticationResponse::Unauthorized),
        }
    }
}

pub fn register_phone_authentication_http<P, B>(
    provider: P,
    backend: B,
) -> Result<B::Registration, HttpError>
where
    P: PhoneAuthenticationProvider,
    B: HttpBackend,
{
    let service = PhoneAuthenticationHttpService::new(provider);
    backend.register(
        PHONE_AUTHENTICATION_HTTP_PATH,
        "SCCP phone authentication",
        false,
        HttpMethodSet::GET,
        HttpLimits {
            max_body_bytes: 1,
            max_response_bytes: PHONE_AUTHENTICATION_MAX_RESPONSE_BYTES,
            max_path_bytes: 128,
            max_fields: 3,
            max_field_name_bytes: 10,
            max_field_value_bytes: PHONE_AUTHENTICATION_MAX_QUERY_BYTES,
        },
        move |request| service.handle(request),
    )
}

fn decision_response(status: u16, response: PhoneAuthenticationResponse) -> HttpResponse {
    HttpResponse::new(status, "text/plain; charset=utf-8", response.to_bytes())
        .expect("static phone authentication response is valid")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use sccp_protocol::DeviceId;

    use super::*;
    use crate::http::HttpField;

    #[derive(Clone)]
    struct FakeProvider {
        decision: Result<bool, PhoneAuthenticationProviderError>,
        received: Arc<Mutex<Vec<(String, String, DeviceId)>>>,
    }

    impl PhoneAuthenticationProvider for FakeProvider {
        fn authorize(
            &self,
            request: &PhoneAuthenticationRequest,
        ) -> Result<bool, PhoneAuthenticationProviderError> {
            self.received.lock().unwrap().push((
                request.user_id.expose_secret().to_owned(),
                request.password.expose_secret().to_owned(),
                request.device_id.clone(),
            ));
            self.decision
        }
    }

    fn request(method: HttpMethod, fields: &[(&str, &str)]) -> HttpRequest {
        HttpRequest {
            method,
            route: PHONE_AUTHENTICATION_HTTP_PATH.into(),
            path: PHONE_AUTHENTICATION_HTTP_PATH.into(),
            remote_address: "192.0.2.10:40000".into(),
            query: fields
                .iter()
                .map(|(name, value)| HttpField {
                    name: (*name).into(),
                    value: (*value).into(),
                })
                .collect(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn service_returns_exact_typed_authorized_and_denied_tokens() {
        for (decision, expected) in [
            (true, PhoneAuthenticationResponse::Authorized),
            (false, PhoneAuthenticationResponse::Unauthorized),
        ] {
            let received = Arc::new(Mutex::new(Vec::new()));
            let service = PhoneAuthenticationHttpService::new(FakeProvider {
                decision: Ok(decision),
                received: Arc::clone(&received),
            });
            let response = service.handle(request(
                HttpMethod::Get,
                &[
                    ("UserID", "private-user"),
                    ("Password", "private-password"),
                    ("devicename", "SEP001122334455"),
                ],
            ));
            assert_eq!(response.status(), 200);
            assert_eq!(response.content_type(), "text/plain; charset=utf-8");
            assert_eq!(response.body(), expected.as_bytes());
            assert_eq!(
                received.lock().unwrap().as_slice(),
                &[(
                    "private-user".into(),
                    "private-password".into(),
                    DeviceId::new("SEP001122334455").unwrap(),
                )]
            );
        }
    }

    #[test]
    fn malformed_method_body_and_policy_failure_fail_closed_without_disclosure() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let service = PhoneAuthenticationHttpService::new(FakeProvider {
            decision: Err(PhoneAuthenticationProviderError::Unavailable),
            received: Arc::clone(&received),
        });

        let mut wrong_method = request(HttpMethod::Post, &[]);
        wrong_method.body = b"private-password".to_vec();
        let response = service.handle(wrong_method);
        assert_eq!(response.status(), 405);
        assert_eq!(response.body(), b"UN-AUTHORIZED");

        let response = service.handle(request(
            HttpMethod::Get,
            &[
                ("UserID", "private-user"),
                ("Password", "private-password"),
                ("DeviceName", "SEP001122334455"),
            ],
        ));
        assert_eq!(response.status(), 400);
        assert_eq!(response.body(), b"UN-AUTHORIZED");
        assert!(received.lock().unwrap().is_empty());

        let response = service.handle(request(
            HttpMethod::Get,
            &[
                ("UserID", "private-user"),
                ("Password", "private-password"),
                ("devicename", "SEP001122334455"),
            ],
        ));
        assert_eq!(response.status(), 503);
        assert_eq!(response.body(), b"UN-AUTHORIZED");
        let debug = format!(
            "{:?}",
            request(
                HttpMethod::Get,
                &[
                    ("UserID", "private-user"),
                    ("Password", "private-password"),
                    ("devicename", "SEP001122334455"),
                ],
            )
        );
        assert!(!debug.contains("private-user"));
        assert!(!debug.contains("private-password"));
    }

    #[cfg(not(any(feature = "asterisk-22", feature = "asterisk-latest")))]
    #[test]
    fn development_registration_is_explicitly_unavailable() {
        let result = register_phone_authentication_http(
            FakeProvider {
                decision: Ok(false),
                received: Arc::new(Mutex::new(Vec::new())),
            },
            crate::http::UnavailableHttp,
        );
        assert!(matches!(result, Err(HttpError::Unavailable)));
    }
}
