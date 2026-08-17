//! Process-level configuration. Hevy credentials are request-scoped and are
//! deliberately absent from this module.

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::{Context, Result};

const ENV_HEVY_BASE_URL: &str = "HEVY_MCP_HEVY_BASE_URL";
const ENV_BIND_ADDR: &str = "HEVY_MCP_BIND_ADDR";
const ENV_METRICS_BIND_ADDR: &str = "HEVY_MCP_METRICS_BIND_ADDR";
const ENV_POD_IP: &str = "POD_IP";
const ENV_RATE_LIMIT_READS: &str = "HEVY_MCP_RATE_LIMIT_READS_PER_MIN";
const ENV_RATE_LIMIT_WRITES: &str = "HEVY_MCP_RATE_LIMIT_WRITES_PER_MIN";
const ENV_ALLOWED_HOSTS: &str = "HEVY_MCP_ALLOWED_HOSTS";

const DEFAULT_HEVY_BASE_URL: &str = "https://api.hevyapp.com";
const DEFAULT_RATE_LIMIT_READS: u32 = 60;
const DEFAULT_RATE_LIMIT_WRITES: u32 = 30;
const DEFAULT_ALLOWED_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1", "hevy-mcp.oddie.app"];

#[derive(Clone, Debug)]
pub struct Config {
    /// Hevy API base URL without a trailing slash.
    pub hevy_base_url: String,
    pub bind_addr: SocketAddr,
    pub metrics_bind_addr: SocketAddr,
    pub rate_limit_reads_per_min: u32,
    pub rate_limit_writes_per_min: u32,
    pub allowed_hosts: Vec<String>,
}

impl Config {
    pub fn new(hevy_base_url: impl Into<String>, bind_addr: SocketAddr) -> Result<Self> {
        let hevy_base_url = strip_trailing_slash(hevy_base_url.into());
        validate_url(&hevy_base_url, ENV_HEVY_BASE_URL)?;
        Ok(Self {
            hevy_base_url,
            bind_addr,
            metrics_bind_addr: SocketAddr::from(([127, 0, 0, 1], 9090)),
            rate_limit_reads_per_min: DEFAULT_RATE_LIMIT_READS,
            rate_limit_writes_per_min: DEFAULT_RATE_LIMIT_WRITES,
            allowed_hosts: default_allowed_hosts(),
        })
    }

    pub fn from_env() -> Result<Self> {
        let hevy_base_url =
            std::env::var(ENV_HEVY_BASE_URL).unwrap_or_else(|_| DEFAULT_HEVY_BASE_URL.to_owned());
        let bind_addr_string =
            std::env::var(ENV_BIND_ADDR).unwrap_or_else(|_| "0.0.0.0:3000".to_owned());
        let bind_addr = SocketAddr::from_str(&bind_addr_string)
            .with_context(|| format!("invalid {ENV_BIND_ADDR}: {bind_addr_string}"))?;

        let mut config = Self::new(hevy_base_url, bind_addr)?;
        config.metrics_bind_addr = resolve_metrics_bind_addr(
            std::env::var(ENV_METRICS_BIND_ADDR).ok().as_deref(),
            std::env::var(ENV_POD_IP).ok().as_deref(),
        )?;
        config.rate_limit_reads_per_min =
            parse_rate_limit(ENV_RATE_LIMIT_READS, DEFAULT_RATE_LIMIT_READS)?;
        config.rate_limit_writes_per_min =
            parse_rate_limit(ENV_RATE_LIMIT_WRITES, DEFAULT_RATE_LIMIT_WRITES)?;
        config.allowed_hosts = parse_allowed_hosts(std::env::var(ENV_ALLOWED_HOSTS).ok())?;
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

fn parse_allowed_hosts(raw: Option<String>) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(default_allowed_hosts());
    };
    let hosts: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect();
    if hosts.is_empty() {
        anyhow::bail!("{ENV_ALLOWED_HOSTS} must contain at least one host");
    }
    if hosts
        .iter()
        .any(|host| host.chars().any(char::is_whitespace))
    {
        anyhow::bail!("{ENV_ALLOWED_HOSTS} entries must not contain whitespace");
    }
    Ok(hosts)
}

fn default_allowed_hosts() -> Vec<String> {
    DEFAULT_ALLOWED_HOSTS
        .iter()
        .map(|host| (*host).to_owned())
        .collect()
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
            "https://api.hevyapp.com/",
            SocketAddr::from(([0, 0, 0, 0], 3000)),
        )
        .unwrap()
    }

    #[test]
    fn constructor_normalizes_url_and_sets_public_host_defaults() {
        let config = config();
        assert_eq!(config.hevy_base_url, "https://api.hevyapp.com");
        assert!(config.allowed_hosts.iter().any(|host| host == "localhost"));
        assert!(
            config
                .allowed_hosts
                .iter()
                .any(|host| host == "hevy-mcp.oddie.app")
        );
    }

    #[test]
    fn parses_explicit_allowed_hosts() {
        assert_eq!(
            parse_allowed_hosts(Some("one.test, two.test".to_owned())).unwrap(),
            vec!["one.test", "two.test"]
        );
        assert!(parse_allowed_hosts(Some(" , ".to_owned())).is_err());
    }

    #[test]
    fn rejects_non_absolute_urls() {
        assert!(!is_absolute_http_uri("api.hevyapp.com"));
        assert!(is_absolute_http_uri("https://api.hevyapp.com"));
    }
}
