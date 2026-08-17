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
use crate::rate_limit::{InitializeLimiter, Limiter, MAX_INITIALIZES_PER_IDENTITY};

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
    let mcp_service = StreamableHttpService::new(
        move || Ok(HevyMcpService::new(hevy.clone(), Arc::clone(&limiter))),
        Arc::new(session::CappedSessionManager::new()),
        StreamableHttpServerConfig::default().with_allowed_hosts(config.allowed_hosts),
    );

    let initialize_limiter = Arc::new(InitializeLimiter::new(
        session::SESSION_KEEP_ALIVE,
        MAX_INITIALIZES_PER_IDENTITY,
    ));

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
        let config = test_config(hevy_base_url);
        let hevy = HevyClient::new(&config.hevy_base_url).unwrap();
        let limiter = Arc::new(Limiter::new(100_000, 100_000).unwrap());
        build_router(config, hevy, limiter)
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

        let app = router(&hevy_server.uri());
        let initialize = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "hevy-mcp.oddie.app")
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
                    .header(header::HOST, "hevy-mcp.oddie.app")
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
