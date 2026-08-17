//! Process-level configuration.

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::{Context, Result};

use crate::oauth_redirect;

const ENV_RESOURCE_URL: &str = "HEVY_MCP_RESOURCE_URL";
const ENV_AUTH_SERVER_URL: &str = "HEVY_MCP_AUTHORIZATION_SERVER";
const ENV_HEVY_BASE_URL: &str = "HEVY_MCP_HEVY_BASE_URL";
const ENV_HEVY_API_KEY: &str = "HEVY_API_KEY";
const ENV_HEVY_API_KEY_ALIAS: &str = "HEVY_MCP_API_KEY";
const ENV_BIND_ADDR: &str = "HEVY_MCP_BIND_ADDR";
const ENV_METRICS_BIND_ADDR: &str = "HEVY_MCP_METRICS_BIND_ADDR";
const ENV_POD_IP: &str = "POD_IP";
const ENV_INTROSPECTION_CLIENT_ID: &str = "HEVY_MCP_LOGTO_CLIENT_ID";
const ENV_INTROSPECTION_CLIENT_SECRET: &str = "HEVY_MCP_LOGTO_CLIENT_SECRET";
const ENV_DCR_CLIENT_ID: &str = "HEVY_MCP_DCR_CLIENT_ID";
const ENV_RATE_LIMIT_READS: &str = "HEVY_MCP_RATE_LIMIT_READS_PER_MIN";
const ENV_RATE_LIMIT_WRITES: &str = "HEVY_MCP_RATE_LIMIT_WRITES_PER_MIN";
const ENV_TRUSTED_PROXY_HOPS: &str = "HEVY_MCP_TRUSTED_PROXY_HOPS";

const DEFAULT_HEVY_BASE_URL: &str = "https://api.hevyapp.com";
const DEFAULT_RATE_LIMIT_READS: u32 = 60;
const DEFAULT_RATE_LIMIT_WRITES: u32 = 30;
const DEFAULT_TRUSTED_PROXY_HOPS: usize = 1;

#[derive(Clone)]
pub struct Config {
    /// Public origin without `/mcp` or a trailing slash.
    pub resource_url: String,
    /// Logto OIDC issuer without a trailing slash.
    pub authorization_server: String,
    /// Hevy API base URL without a trailing slash.
    pub hevy_base_url: String,
    /// Optional Hevy Pro API key. Missing is a supported boot state.
    pub hevy_api_key: Option<String>,
    pub bind_addr: SocketAddr,
    pub metrics_bind_addr: SocketAddr,
    pub introspection: Option<IntrospectionCredentials>,
    pub rate_limit_reads_per_min: u32,
    pub rate_limit_writes_per_min: u32,
    pub trusted_proxy_hops: usize,
    pub dcr_client_id: Option<String>,
    pub oauth_redirect_uris: Vec<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("resource_url", &self.resource_url)
            .field("authorization_server", &self.authorization_server)
            .field("hevy_base_url", &self.hevy_base_url)
            .field(
                "hevy_api_key",
                &self.hevy_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("bind_addr", &self.bind_addr)
            .field("metrics_bind_addr", &self.metrics_bind_addr)
            .field("introspection", &self.introspection)
            .field("rate_limit_reads_per_min", &self.rate_limit_reads_per_min)
            .field("rate_limit_writes_per_min", &self.rate_limit_writes_per_min)
            .field("trusted_proxy_hops", &self.trusted_proxy_hops)
            .field("dcr_client_id", &self.dcr_client_id)
            .field("oauth_redirect_uris", &self.oauth_redirect_uris)
            .finish()
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct IntrospectionCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for IntrospectionCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntrospectionCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl Config {
    pub fn new(
        resource_url: impl Into<String>,
        authorization_server: impl Into<String>,
        hevy_base_url: impl Into<String>,
        bind_addr: SocketAddr,
    ) -> Result<Self> {
        let resource_url = strip_trailing_slash(resource_url.into());
        let authorization_server = strip_trailing_slash(authorization_server.into());
        let hevy_base_url = strip_trailing_slash(hevy_base_url.into());
        validate_url(&resource_url, ENV_RESOURCE_URL)?;
        validate_url(&authorization_server, ENV_AUTH_SERVER_URL)?;
        validate_url(&hevy_base_url, ENV_HEVY_BASE_URL)?;
        Ok(Self {
            resource_url,
            authorization_server,
            hevy_base_url,
            hevy_api_key: None,
            bind_addr,
            metrics_bind_addr: SocketAddr::from(([127, 0, 0, 1], 9090)),
            introspection: None,
            rate_limit_reads_per_min: DEFAULT_RATE_LIMIT_READS,
            rate_limit_writes_per_min: DEFAULT_RATE_LIMIT_WRITES,
            trusted_proxy_hops: DEFAULT_TRUSTED_PROXY_HOPS,
            dcr_client_id: None,
            oauth_redirect_uris: Vec::new(),
        })
    }

    #[must_use]
    pub fn with_introspection(mut self, credentials: IntrospectionCredentials) -> Self {
        self.introspection = Some(credentials);
        self
    }

    /// Logto tokens may identify either the origin or the canonical MCP URL.
    pub fn accepted_token_audiences(&self) -> Vec<String> {
        vec![
            self.resource_url.clone(),
            crate::oauth_metadata::mcp_resource(&self.resource_url),
        ]
    }

    pub fn from_env() -> Result<Self> {
        let resource_url = require_env(ENV_RESOURCE_URL)?;
        let authorization_server = require_env(ENV_AUTH_SERVER_URL)?;
        let hevy_base_url =
            std::env::var(ENV_HEVY_BASE_URL).unwrap_or_else(|_| DEFAULT_HEVY_BASE_URL.to_owned());
        let bind_addr_string =
            std::env::var(ENV_BIND_ADDR).unwrap_or_else(|_| "0.0.0.0:3000".to_owned());
        let bind_addr = SocketAddr::from_str(&bind_addr_string)
            .with_context(|| format!("invalid {ENV_BIND_ADDR}: {bind_addr_string}"))?;

        let mut config = Self::new(resource_url, authorization_server, hevy_base_url, bind_addr)?;
        config.metrics_bind_addr = resolve_metrics_bind_addr(
            std::env::var(ENV_METRICS_BIND_ADDR).ok().as_deref(),
            std::env::var(ENV_POD_IP).ok().as_deref(),
        )?;
        config.hevy_api_key =
            optional_env(ENV_HEVY_API_KEY).or_else(|| optional_env(ENV_HEVY_API_KEY_ALIAS));
        config.rate_limit_reads_per_min =
            parse_rate_limit(ENV_RATE_LIMIT_READS, DEFAULT_RATE_LIMIT_READS)?;
        config.rate_limit_writes_per_min =
            parse_rate_limit(ENV_RATE_LIMIT_WRITES, DEFAULT_RATE_LIMIT_WRITES)?;
        config.trusted_proxy_hops = parse_trusted_proxy_hops()?;
        config.dcr_client_id = optional_env(ENV_DCR_CLIENT_ID);
        config.oauth_redirect_uris = parse_redirect_uris_env()?;

        if let (Ok(client_id), Ok(client_secret)) = (
            std::env::var(ENV_INTROSPECTION_CLIENT_ID),
            std::env::var(ENV_INTROSPECTION_CLIENT_SECRET),
        ) {
            config = config.with_introspection(IntrospectionCredentials {
                client_id,
                client_secret,
            });
        }
        Ok(config)
    }
}

fn resolve_metrics_bind_addr(
    explicit_addr: Option<&str>,
    pod_ip: Option<&str>,
) -> Result<SocketAddr> {
    let address = explicit_addr.map_or_else(
        || pod_ip.map_or_else(|| "127.0.0.1:9090".to_owned(), |ip| format!("{ip}:9090")),
        str::to_owned,
    );
    SocketAddr::from_str(&address)
        .with_context(|| format!("invalid {ENV_METRICS_BIND_ADDR}: {address}"))
}

fn require_env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("required env var {key} is not set"))
}

fn optional_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn validate_url(url: &str, key: &str) -> Result<()> {
    if !is_absolute_http_uri(url) {
        anyhow::bail!("{key} must be an absolute http(s) URL, got: {url}");
    }
    Ok(())
}

pub fn is_absolute_http_uri(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() > "https://".len()
        && !url.chars().any(char::is_whitespace)
}

fn parse_rate_limit(key: &str, default: u32) -> Result<u32> {
    match std::env::var(key) {
        Err(_) => Ok(default),
        Ok(raw) => {
            let value: u32 = raw
                .trim()
                .parse()
                .with_context(|| format!("{key} must be a positive integer, got: {raw}"))?;
            if value == 0 {
                anyhow::bail!("{key} must be > 0");
            }
            Ok(value)
        }
    }
}

fn parse_redirect_uris_env() -> Result<Vec<String>> {
    match std::env::var(oauth_redirect::ENV_OAUTH_REDIRECT_URIS) {
        Ok(raw) => oauth_redirect::parse_allowlist(&raw, oauth_redirect::ENV_OAUTH_REDIRECT_URIS),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(error) => Err(error)
            .with_context(|| format!("invalid {}", oauth_redirect::ENV_OAUTH_REDIRECT_URIS)),
    }
}

fn parse_trusted_proxy_hops() -> Result<usize> {
    std::env::var(ENV_TRUSTED_PROXY_HOPS).map_or_else(
        |_| Ok(DEFAULT_TRUSTED_PROXY_HOPS),
        |raw| {
            raw.trim().parse().with_context(|| {
                format!("{ENV_TRUSTED_PROXY_HOPS} must be a non-negative integer, got: {raw}")
            })
        },
    )
}

fn strip_trailing_slash(mut value: String) -> String {
    while value.ends_with('/') {
        value.pop();
    }
    value
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::new(
            "https://hevy-mcp.oddie.app/",
            "https://login.kampong.social/oidc/",
            "https://api.hevyapp.com/",
            SocketAddr::from(([0, 0, 0, 0], 3000)),
        )
        .unwrap()
    }

    #[test]
    fn constructor_normalizes_urls_and_accepts_both_audiences() {
        let config = config();
        assert_eq!(config.resource_url, "https://hevy-mcp.oddie.app");
        assert_eq!(config.hevy_base_url, "https://api.hevyapp.com");
        assert_eq!(
            config.accepted_token_audiences(),
            vec![
                "https://hevy-mcp.oddie.app",
                "https://hevy-mcp.oddie.app/mcp"
            ]
        );
    }

    #[test]
    fn debug_redacts_hevy_key() {
        let mut config = config();
        config.hevy_api_key = Some("never-print-this-key".to_owned());
        let debug = format!("{config:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("never-print-this-key"));
    }

    #[test]
    fn rejects_non_absolute_urls() {
        assert!(!is_absolute_http_uri("hevy-mcp.oddie.app"));
        assert!(is_absolute_http_uri("https://hevy-mcp.oddie.app"));
    }
}
