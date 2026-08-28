//! Envelope-only audit logging.
//!
//! Every tool call emits a structured `tracing::info` event at target
//! `hevy_mcp::audit`. The cluster's Alloy collector ships these to Loki.
//!
//! ## What is and isn't logged
//!
//! Envelope fields **are** logged: `event`, `method` (MCP tool name),
//! `resource` (Hevy ID/date when relevant), `outcome`, `latency_ms`,
//! `result_count`, `error_class`, and `token_hash` (16 hex chars of
//! `sha256(bearer)`).
//!
//! Content fields **are not** logged: workout/routine titles, notes, request
//! bodies, bearer tokens, API keys, authorization headers, or any other
//! user-supplied free-form text from tool parameters.

use std::time::Instant;

use rmcp::ErrorData;
use sha2::{Digest, Sha256};
use tracing::info;

/// Coarse outcome class for an audit event. Stable strings for Grafana/Loki.
#[allow(dead_code)]
pub mod outcome {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
    pub const DENIED: &str = "denied";
    pub const NOT_FOUND: &str = "not_found";
    pub const INVALID: &str = "invalid";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const RATE_LIMITED: &str = "rate_limited";
}

/// First 16 hex chars of `sha256(bearer)` — a stable pseudonymous token id
/// for correlation without ever logging the token.
#[must_use]
pub fn token_hash(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..8])
}

/// JSON-RPC application code: caller exceeded their per-minute quota here,
/// before Hevy was called at all.
pub const RATE_LIMITED_CODE: i32 = -32029;

/// JSON-RPC application code: Hevy answered 429 to a call we forwarded.
pub const HEVY_RATE_LIMITED_CODE: i32 = -32012;

/// The class a caller and a dashboard both key on.
///
/// **This is the only place a JSON-RPC code becomes a class.** The audit
/// event's `outcome`, its `error_class` field, and the `data.class` a client
/// sees on the wire are all derived from this function, so they cannot
/// disagree about one event. They did: `emit_tool_audit` called both rate
/// limit codes `rate_limited` while this function knew only `-32029` and filed
/// `-32012` under `other`, so a Hevy 429 was rate-limited in one field of an
/// event and unclassified in another, and the series that would answer "how
/// often is a Hevy read rate-limited" did not exist.
#[must_use]
pub const fn error_class(err: &ErrorData) -> &'static str {
    class_for_code(err.code.0)
}

#[must_use]
pub const fn class_for_code(code: i32) -> &'static str {
    match code {
        -32700 => "parse",
        -32600 => "invalid_request",
        -32601 => "method_not_found",
        -32602 => "invalid_params",
        -32603 => "internal",
        RATE_LIMITED_CODE | HEVY_RATE_LIMITED_CODE => outcome::RATE_LIMITED,
        _ => "other",
    }
}

/// Emit a `tool_call` audit event. Call at the END of every tool body, on
/// both success and error paths. Also bumps the matching Prometheus metric.
pub fn tool_call(
    tool: &'static str,
    token_hash: &str,
    resource: Option<&str>,
    outcome: &'static str,
    started: Instant,
    result_count: Option<usize>,
    err_class: Option<&'static str>,
) {
    let elapsed = started.elapsed();
    // `resource` may be a raw, caller-supplied tool parameter (even on
    // validation-failure paths), so sanitise it before emission to stop an
    // attacker injecting newlines / fake `outcome=` fragments into logs.
    let safe_resource: Option<&str> = resource.map(|r| if is_safe_id(r) { r } else { "<invalid>" });
    info!(
        target: "hevy_mcp::audit",
        event = "tool_call",
        method = tool,
        token_hash,
        resource = safe_resource,
        outcome,
        latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        result_count,
        error_class = err_class,
    );
    crate::metrics::record_tool_call(tool, outcome, elapsed);
}

/// Audit-safe Hevy ID/date check: no whitespace/control chars, query, or
/// fragment, with a bounded length.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 1024
        && !id.contains(['?', '#'])
        && id.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ':' | '/' | '%')
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_is_16_hex_chars() {
        let h = token_hash("any-bearer-string");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn is_safe_id_accepts_hevy_ids_dates_and_emails() {
        assert!(is_safe_id("b459cba5-cd6d-463c-abd6-54f8eafcadcb"));
        assert!(is_safe_id("2026-08-17"));
        assert!(is_safe_id("alice@kampong.social"));
        assert!(!is_safe_id("has space"));
        assert!(!is_safe_id("inject\noutcome=ok"));
        assert!(!is_safe_id(""));
    }

    #[test]
    fn error_class_maps_known_codes() {
        assert_eq!(
            error_class(&ErrorData::internal_error("x", None)),
            "internal"
        );
        assert_eq!(
            error_class(&ErrorData::invalid_params("x", None)),
            "invalid_params"
        );
    }

    #[test]
    fn outcomes_are_stable_strings() {
        assert_eq!(outcome::OK, "ok");
        assert_eq!(outcome::ERROR, "error");
        assert_eq!(outcome::RATE_LIMITED, "rate_limited");
    }
}
