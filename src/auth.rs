//! Axum middleware for per-request Hevy API-key authentication.

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

/// The raw Hevy API key extracted from `Authorization: Bearer <key>`.
/// Debug output is always redacted.
#[derive(Clone)]
pub struct AccessToken(pub String);

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AccessToken").field(&"<redacted>").finish()
    }
}

/// Attach a non-empty bearer value to the request without validating it
/// against Hevy. Tool calls forward the value as Hevy's `api-key` header.
pub async fn bearer_auth(mut request: Request<Body>, next: Next) -> Response {
    let Some(token) = extract_bearer(request.headers().get(header::AUTHORIZATION)) else {
        return unauthorized();
    };
    request.extensions_mut().insert(AccessToken(token));
    next.run(request).await
}

/// Extract a case-sensitive RFC 6750 Bearer value.
fn extract_bearer(header: Option<&HeaderValue>) -> Option<String> {
    let raw = header?.to_str().ok()?.trim();
    let (scheme, value) = raw.split_once(' ')?;
    if scheme.as_bytes().ct_eq(b"Bearer").unwrap_u8() != 1 {
        return None;
    }
    let token = value.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}

fn unauthorized() -> Response {
    // Static API key, not RFC 6750 OAuth. A Bearer challenge makes Cursor
    // start OAuth discovery. Return a bare 401.
    (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn extracts_well_formed_bearer() {
        let header = HeaderValue::from_static("Bearer hevy-key");
        assert_eq!(extract_bearer(Some(&header)).as_deref(), Some("hevy-key"));
    }

    #[test]
    fn rejects_lowercase_scheme() {
        assert!(extract_bearer(Some(&HeaderValue::from_static("bearer key"))).is_none());
    }

    #[test]
    fn rejects_basic_scheme() {
        assert!(extract_bearer(Some(&HeaderValue::from_static("Basic key"))).is_none());
    }

    #[test]
    fn rejects_empty_token() {
        assert!(extract_bearer(Some(&HeaderValue::from_static("Bearer "))).is_none());
    }

    #[test]
    fn trims_whitespace_around_token() {
        let header = HeaderValue::from_static("Bearer   key   ");
        assert_eq!(extract_bearer(Some(&header)).as_deref(), Some("key"));
    }

    #[test]
    fn unauthorized_has_no_www_authenticate() {
        let response = unauthorized();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
    }
}
