//! hevy-mcp: first-party streamable-HTTP MCP server for Hevy.
//!
//! Logto authenticates MCP callers; a process-level Hevy Pro API key
//! authorizes calls to the official Hevy REST API.

mod audit;
mod auth;
mod config;
mod hevy_client;
mod last_used;
mod logto_oidc;
mod mcp;
mod metrics;
mod oauth_metadata;
mod oauth_proxy;
mod oauth_redirect;
mod rate_limit;
mod session;
mod telemetry;
mod token_introspect;
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
use axum::routing::{get, post};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::auth::{AccessToken, AuthState, bearer_auth};
use crate::config::Config;
use crate::hevy_client::HevyClient;
use crate::logto_oidc::LogtoValidationClient;
use crate::mcp::HevyMcpService;
use crate::oauth_metadata::{authorization_server_metadata, protected_resource_metadata, register};
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
    let logto = LogtoValidationClient::new(
        &config.authorization_server,
        config.accepted_token_audiences(),
    )?;
    let hevy = HevyClient::new(&config.hevy_base_url, config.hevy_api_key.clone())?;
    let auth_state = AuthState {
        config: config.clone(),
        logto,
        last_used: last_used::LastUsedTracker::new(),
    };
    let limiter = Arc::new(
        Limiter::new(
            config.rate_limit_reads_per_min,
            config.rate_limit_writes_per_min,
        )
        .ok_or_else(|| anyhow::anyhow!("rate-limit quotas must be greater than zero"))?,
    );
    Ok(build_router(config, auth_state, hevy, limiter))
}

fn build_router(
    config: Config,
    auth_state: AuthState,
    hevy: HevyClient,
    limiter: Arc<Limiter>,
) -> Router {
    let resource_host = parse_host(&config.resource_url);
    let mut allowed_hosts = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Some(host) = resource_host {
        allowed_hosts.push(host);
    }

    let mcp_service = StreamableHttpService::new(
        move || Ok(HevyMcpService::new(hevy.clone(), Arc::clone(&limiter))),
        Arc::new(session::CappedSessionManager::new()),
        StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts),
    );

    let initialize_limiter = Arc::new(InitializeLimiter::new(
        session::SESSION_KEEP_ALIVE,
        MAX_INITIALIZES_PER_IDENTITY,
    ));

    let mcp_routes = Router::new()
        .nest_service("/mcp", mcp_service)
        .route("/token/introspect", get(token_introspect::handler))
        .layer(middleware::from_fn_with_state(
            initialize_limiter,
            initialize_rate_limit,
        ))
        .layer(middleware::from_fn_with_state(
            auth_state.clone(),
            bearer_auth,
        ))
        .with_state(auth_state);

    Router::new()
        .route("/health", get(health))
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route("/register", post(register))
        .merge(
            Router::new()
                .route("/authorize", get(oauth_proxy::authorize))
                .route("/oauth/callback", get(oauth_proxy::callback))
                .route("/token", post(oauth_proxy::token))
                .with_state(oauth_proxy::OAuthProxyState::new(
                    &config.authorization_server,
                    &config.resource_url,
                    config.oauth_redirect_uris.clone(),
                )),
        )
        .merge(mcp_routes)
        .layer(TraceLayer::new_for_http())
        .with_state(config)
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
    let Some(identity) = request
        .extensions()
        .get::<crate::logto_oidc::AuthenticatedIdentity>()
    else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "authenticated request missing identity extension\n",
        )
            .into_response();
    };
    let bearer_hash = audit::token_hash(&token.0);
    if limiter
        .check(&bearer_hash, Some(identity.user_id.as_str()))
        .is_err()
    {
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

fn parse_host(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    (!authority.is_empty()).then(|| authority.to_owned())
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
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;

    fn test_config() -> Config {
        Config::new(
            "https://hevy-mcp.oddie.app",
            "https://login.kampong.social/oidc",
            "https://api.hevyapp.com",
            SocketAddr::from(([0, 0, 0, 0], 3000)),
        )
        .unwrap()
    }

    fn router(config: Config) -> Router {
        let logto = LogtoValidationClient::new(
            &config.authorization_server,
            config.accepted_token_audiences(),
        )
        .unwrap();
        let hevy = HevyClient::new(&config.hevy_base_url, config.hevy_api_key.clone()).unwrap();
        let auth_state = AuthState {
            config: config.clone(),
            logto,
            last_used: crate::last_used::LastUsedTracker::new(),
        };
        let limiter = Arc::new(Limiter::new(100_000, 100_000).unwrap());
        build_router(config, auth_state, hevy, limiter)
    }

    #[tokio::test]
    async fn health_is_public() {
        let response = router(test_config())
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mcp_without_token_returns_401_with_path_metadata() {
        let response = router(test_config())
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
        let challenge = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            challenge,
            r#"Bearer resource_metadata="https://hevy-mcp.oddie.app/.well-known/oauth-protected-resource/mcp""#
        );
    }

    #[tokio::test]
    async fn path_aware_metadata_has_canonical_resource() {
        let response = router(test_config())
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let metadata: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(metadata["resource"], "https://hevy-mcp.oddie.app/mcp");
    }

    #[tokio::test]
    async fn dcr_accepts_cursor_and_grok_uri_set() {
        let redirect_uris = vec![
            "cursor://anysphere.cursor-mcp/oauth/callback",
            "grokbot://mcp/oauth/callback",
            "http://localhost:8787/callback",
            "https://www.cursor.com/agents/mcp/oauth/callback",
        ];
        let mut config = test_config();
        config.dcr_client_id = Some("uw7dfhsvg6wq0p0eavk2i".to_owned());
        config.oauth_redirect_uris = redirect_uris.iter().map(|uri| (*uri).to_owned()).collect();
        let response = router(config)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "redirect_uris": redirect_uris })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let registration: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(registration["client_id"], "uw7dfhsvg6wq0p0eavk2i");
    }
}
