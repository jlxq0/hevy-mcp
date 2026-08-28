//! hevy-mcp: first-party streamable-HTTP MCP server for Hevy.
//!
//! Each MCP request carries a Hevy API key as its bearer token. Tool calls
//! forward that request-scoped value to the official Hevy REST API.

mod audit;
mod auth;
mod config;
mod hevy_client;
mod mcp;
mod metrics;
mod rate_limit;
mod session;
mod telemetry;
#[allow(dead_code)]
mod url_safety;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::auth::{AccessToken, bearer_auth};
use crate::config::Config;
use crate::hevy_client::HevyClient;
use crate::mcp::HevyMcpService;
use crate::rate_limit::{InitializeLimiter, Limiter};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    metrics::init();
    let config = Config::from_env()?;
    let bind_addr = config.bind_addr;
    let metrics_bind_addr = config.metrics_bind_addr;
    let app = build_app(config)?;

    let listener = TcpListener::bind(bind_addr).await?;
    info!(%bind_addr, "hevy-mcp listening (public)");

    let metrics_listener = TcpListener::bind(metrics_bind_addr).await?;
    info!(%metrics_bind_addr, "hevy-mcp metrics listening (internal)");
    let metrics_app = Router::new().route("/metrics", get(metrics::metrics_handler));

    tokio::select! {
        result = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()) => {
            result?;
        }
        result = axum::serve(metrics_listener, metrics_app)
            .with_graceful_shutdown(shutdown_signal()) => {
            result?;
        }
        () = shutdown_signal() => {}
    }
    Ok(())
}

fn build_app(config: Config) -> Result<Router> {
    let hevy = HevyClient::new(&config.hevy_base_url)?;
    let limiter = Arc::new(
        Limiter::new(
            config.rate_limit_reads_per_min,
            config.rate_limit_writes_per_min,
        )
        .ok_or_else(|| anyhow::anyhow!("rate-limit quotas must be greater than zero"))?,
    );
    Ok(build_router(config, hevy, limiter))
}

fn build_router(config: Config, hevy: HevyClient, limiter: Arc<Limiter>) -> Router {
    let initialize_limiter = Arc::new(InitializeLimiter::new(
        config.initialize_replenish,
        config.initialize_burst,
    ));

    let mcp_service = StreamableHttpService::new(
        move || Ok(HevyMcpService::new(hevy.clone(), Arc::clone(&limiter))),
        Arc::new(session::CappedSessionManager::new()),
        StreamableHttpServerConfig::default().with_allowed_hosts(config.allowed_hosts),
    );

    // Bearer auth must stay nested under /mcp. In axum 0.7 a `.layer()` on a
    // merged router becomes a catch-all, so unknown paths (including OAuth
    // well-known discovery) would 401 and Cursor would treat this as OAuth.
    let mcp_routes = Router::new()
        .fallback_service(mcp_service)
        .layer(middleware::from_fn_with_state(
            initialize_limiter,
            initialize_rate_limit,
        ))
        .layer(middleware::from_fn(bearer_auth));

    Router::new()
        .route("/health", get(health))
        .route(
            "/.well-known/oauth-authorization-server",
            any(oauth_probe_not_found),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            any(oauth_probe_not_found),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            any(oauth_probe_not_found),
        )
        .route(
            "/.well-known/openid-configuration",
            any(oauth_probe_not_found),
        )
        .route("/oauth-protected-resource/mcp", any(oauth_probe_not_found))
        .route("/openid-configuration", any(oauth_probe_not_found))
        .nest("/mcp", mcp_routes)
        .layer(TraceLayer::new_for_http())
}

async fn oauth_probe_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

async fn initialize_rate_limit(
    State(limiter): State<Arc<InitializeLimiter>>,
    request: Request<Body>,
    next: Next,
) -> axum::response::Response {
    if !is_fresh_mcp_session_request(&request) {
        return next.run(request).await;
    }
    let Some(token) = request.extensions().get::<AccessToken>() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "authenticated request missing token extension\n",
        )
            .into_response();
    };
    let bearer_hash = audit::token_hash(&token.0);
    if limiter.check(&bearer_hash).is_err() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many MCP initialize requests; try again later\n",
        )
            .into_response();
    }
    next.run(request).await
}

fn is_fresh_mcp_session_request(request: &Request<Body>) -> bool {
    request.method() == Method::POST && request.headers().get("mcp-session-id").is_none()
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("hevy_mcp=info,tower_http=info,axum=info,info"));
    let otel_layer = telemetry::try_build_otel_layer();
    let json_layer = std::env::var("HEVY_MCP_LOG_FORMAT").as_deref() == Ok("json");
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(otel_layer);
    if json_layer {
        registry.with(fmt::layer().json()).init();
    } else {
        registry.with(fmt::layer().compact()).init();
    }
}

#[allow(clippy::expect_used)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler at startup");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler at startup");
    tokio::select! {
        _ = sigterm.recv() => info!("received SIGTERM"),
        _ = sigint.recv() => info!("received SIGINT"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use tower::ServiceExt;
    use wiremock::matchers::{header as wiremock_header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn test_config(hevy_base_url: &str) -> Config {
        Config::new(hevy_base_url, SocketAddr::from(([0, 0, 0, 0], 3000))).unwrap()
    }

    fn router(hevy_base_url: &str) -> Router {
        build(test_config(hevy_base_url))
    }

    /// A router whose `allowed_hosts` came from configuration rather than the
    /// loopback default, the way `HEVY_MCP_ALLOWED_HOSTS` supplies it in the
    /// deployment.
    fn router_with_hosts(hevy_base_url: &str, hosts: &[&str]) -> Router {
        let mut config = test_config(hevy_base_url);
        config.allowed_hosts = hosts.iter().map(|host| (*host).to_owned()).collect();
        build(config)
    }

    fn build(config: Config) -> Router {
        let hevy = HevyClient::new(&config.hevy_base_url).unwrap();
        let limiter = Arc::new(Limiter::new(100_000, 100_000).unwrap());
        build_router(config, hevy, limiter)
    }

    /// Status of a bearer-carrying `initialize` sent with a given `Host`.
    async fn initialize_with_host(app: Router, host: &str) -> StatusCode {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, host)
                .header(header::AUTHORIZATION, "Bearer request-hevy-key")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    /// The published default must not carry any deployment's public origin.
    /// rmcp answers 403 to a Host outside `allowed_hosts`, so this is the
    /// check that a public origin now comes from the environment: on the
    /// default config it is just another rejected host.
    #[tokio::test]
    async fn default_allowed_hosts_are_loopback_only() {
        for (host, rejected) in [
            ("localhost", false),
            ("127.0.0.1", false),
            ("hevy-mcp.example", true),
            ("evil.example", true),
        ] {
            let status = initialize_with_host(router("https://api.hevyapp.com"), host).await;
            assert_eq!(
                status == StatusCode::FORBIDDEN,
                rejected,
                "{host} returned {status}"
            );
        }
    }

    /// The same origin is accepted once it is configured, which is what
    /// `HEVY_MCP_ALLOWED_HOSTS` does in the deployment.
    #[tokio::test]
    async fn a_configured_public_host_is_accepted() {
        let status = initialize_with_host(
            router_with_hosts("https://api.hevyapp.com", &["hevy-mcp.example"]),
            "hevy-mcp.example",
        )
        .await;
        assert_ne!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn health_is_public_without_a_hevy_key() {
        let response = router("https://api.hevyapp.com")
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    #[tokio::test]
    async fn mcp_without_bearer_returns_bare_401() {
        let response = router("https://api.hevyapp.com")
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
        assert!(!response.headers().contains_key("resource_metadata"));
    }

    #[tokio::test]
    async fn oauth_discovery_and_unknown_paths_are_plain_404() {
        for uri in [
            "/.well-known/oauth-authorization-server",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/openid-configuration",
            "/oauth-protected-resource/mcp",
            "/openid-configuration",
            "/no-such-path",
        ] {
            let response = router("https://api.hevyapp.com")
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            assert!(
                response.headers().get(header::WWW_AUTHENTICATE).is_none(),
                "{uri}"
            );
            assert!(
                !response.headers().contains_key("resource_metadata"),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn mcp_rejects_empty_and_non_bearer_authorization() {
        for authorization in ["Bearer ", "Basic abc"] {
            let response = router("https://api.hevyapp.com")
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/mcp")
                        .header(header::AUTHORIZATION, authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
        }
    }

    /// Drive a tool call and return the JSON-RPC envelope a client receives.
    async fn tool_call_envelope(app: Router, tool: &str) -> serde_json::Value {
        let initialize = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "hevy-mcp.example")
                    .header(header::AUTHORIZATION, "Bearer k")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let session_id = initialize.headers().get("mcp-session-id").unwrap().clone();
        let call = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "hevy-mcp.example")
                    .header(header::AUTHORIZATION, "Bearer k")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header("mcp-session-id", session_id)
                    .header("mcp-protocol-version", "2025-06-18")
                    .body(Body::from(format!(
                        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"{tool}","arguments":{{}}}}}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(call.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        let line = text.lines().rfind(|line| line.starts_with("data: {"));
        assert!(line.is_some(), "no JSON-RPC frame in response: {text}");
        serde_json::from_str(line.unwrap().trim_start_matches("data: ")).unwrap()
    }

    /// What a caller writes to tell "unreadable" from "empty".
    fn caller_sees_rate_limit(envelope: &serde_json::Value) -> bool {
        envelope
            .get("error")
            .and_then(|error| error.get("data"))
            .and_then(|data| data.get("class"))
            .and_then(serde_json::Value::as_str)
            == Some("rate_limited")
    }

    /// A rate limit and an empty read must not be the same silence.
    ///
    /// The 21:00 slot that reports whether a gym day went unlogged reads this
    /// boundary, so an error arriving as an empty result is a confident wrong
    /// answer rather than an absence somebody questions. Both rate limits are
    /// matched by one predicate on `data.class`; an empty list and a zero count
    /// are successful results and match nothing.
    #[tokio::test]
    async fn a_rate_limit_never_arrives_as_an_empty_read() {
        // Our own per-bearer limiter, which answers before Hevy is called.
        let hevy_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/workouts/count"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"workout_count": 7})),
            )
            .mount(&hevy_server)
            .await;
        let mut config = test_config(&hevy_server.uri());
        config.allowed_hosts = vec!["hevy-mcp.example".to_owned()];
        let hevy = HevyClient::new(&config.hevy_base_url).unwrap();
        let app = build_router(config, hevy, Arc::new(Limiter::new(1, 1).unwrap()));

        let first = tool_call_envelope(app.clone(), "count_workouts").await;
        assert!(first.get("result").is_some(), "first call: {first}");
        assert!(!caller_sees_rate_limit(&first));

        let throttled = tool_call_envelope(app, "count_workouts").await;
        assert!(throttled.get("result").is_none(), "expected an error");
        assert!(
            caller_sees_rate_limit(&throttled),
            "local rate limit is invisible to a caller matching data.class: {throttled}"
        );
        // `message` is what a human reads in a log and it must stay a sentence
        // with the retry interval in it. Folding it into the machine code once
        // reduced it to the bare token "rate_limited", which no test noticed.
        let message = throttled["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("minute"),
            "the retry interval is gone from the human message: {message:?}"
        );
        assert_ne!(
            message,
            throttled["error"]["data"]["code"]
                .as_str()
                .unwrap_or_default(),
            "human message collapsed into the machine code"
        );

        // Hevy's own 429, forwarded.
        let throttling_hevy = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/workouts/count"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&throttling_hevy)
            .await;
        let upstream = tool_call_envelope(
            router_with_hosts(&throttling_hevy.uri(), &["hevy-mcp.example"]),
            "count_workouts",
        )
        .await;
        assert!(
            caller_sees_rate_limit(&upstream),
            "upstream rate limit is invisible to the same predicate: {upstream}"
        );

        // An empty page and a zero count are results, and match nothing.
        let empty_hevy = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/workouts"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"page": 1, "page_count": 1, "workouts": []})),
            )
            .mount(&empty_hevy)
            .await;
        let empty = tool_call_envelope(
            router_with_hosts(&empty_hevy.uri(), &["hevy-mcp.example"]),
            "list_workouts",
        )
        .await;
        assert!(empty.get("result").is_some(), "empty read must be a result");
        assert!(!caller_sees_rate_limit(&empty));
    }

    #[tokio::test]
    async fn request_bearer_is_forwarded_to_hevy_user_info() {
        let hevy_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/user/info"))
            .and(wiremock_header("api-key", "request-hevy-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"id": "hevy-user"}
            })))
            .expect(1)
            .mount(&hevy_server)
            .await;

        let app = router_with_hosts(&hevy_server.uri(), &["hevy-mcp.example"]);
        let initialize = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "hevy-mcp.example")
                    .header(header::AUTHORIZATION, "Bearer request-hevy-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(initialize.status(), StatusCode::OK);
        let session_id = initialize.headers().get("mcp-session-id").unwrap().clone();

        let tool_call = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "hevy-mcp.example")
                    .header(header::AUTHORIZATION, "Bearer request-hevy-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header("mcp-session-id", session_id)
                    .header("mcp-protocol-version", "2025-06-18")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"whoami","arguments":{}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tool_call.status(), StatusCode::OK);
        let body = to_bytes(tool_call.into_body(), 64 * 1024).await.unwrap();
        assert!(!body.is_empty());
        hevy_server.verify().await;
    }
}
