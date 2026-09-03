use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use url::Url;

use crate::error::ConfigError;
use crate::key_pool::KeyPool;

const DEFAULT_FIRECRAWL_URL: &str = "https://api.firecrawl.dev";
const DEFAULT_BRAVE_URL: &str = "https://api.search.brave.com";
const DEFAULT_TAVILY_URL: &str = "https://api.tavily.com";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_SEARCH_CACHE_TTL_SECS: u64 = 600;
const DEFAULT_SCRAPE_CACHE_TTL_SECS: u64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchUpstream {
    Firecrawl,
    Brave,
    Tavily,
}

impl SearchUpstream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Firecrawl => "firecrawl",
            Self::Brave => "brave",
            Self::Tavily => "tavily",
        }
    }
}

impl FromStr for SearchUpstream {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "firecrawl" => Ok(Self::Firecrawl),
            "brave" => Ok(Self::Brave),
            "tavily" => Ok(Self::Tavily),
            _ => Err(ConfigError::Invalid),
        }
    }
}

#[derive(Clone)]
pub struct CacheConfig {
    endpoint: Url,
    bucket: String,
    region: String,
    #[allow(dead_code)]
    access_key_id: String,
    #[allow(dead_code)]
    secret_access_key: String,
    search_ttl: Duration,
    scrape_ttl: Duration,
}

impl fmt::Debug for CacheConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheConfig")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field("search_ttl", &self.search_ttl)
            .field("scrape_ttl", &self.scrape_ttl)
            .finish()
    }
}

impl CacheConfig {
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn search_ttl(&self) -> Duration {
        self.search_ttl
    }

    pub fn scrape_ttl(&self) -> Duration {
        self.scrape_ttl
    }

    #[allow(dead_code)]
    pub(crate) fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    #[allow(dead_code)]
    pub(crate) fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }
}

#[derive(Clone)]
pub struct Config {
    token: String,
    search_upstream: SearchUpstream,
    firecrawl_upstream_url: Url,
    brave_upstream_url: Url,
    tavily_upstream_url: Url,
    bind_addr: SocketAddr,
    firecrawl_keys: KeyPool,
    brave_keys: Option<KeyPool>,
    tavily_keys: Option<KeyPool>,
    cache: Option<CacheConfig>,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("token", &"[REDACTED]")
            .field("search_upstream", &self.search_upstream)
            .field("firecrawl_upstream_url", &self.firecrawl_upstream_url)
            .field("brave_upstream_url", &self.brave_upstream_url)
            .field("tavily_upstream_url", &self.tavily_upstream_url)
            .field("bind_addr", &self.bind_addr)
            .field("firecrawl_keys", &self.firecrawl_keys)
            .field("brave_keys", &self.brave_keys)
            .field("tavily_keys", &self.tavily_keys)
            .field("cache", &self.cache)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_values(&std::env::vars().collect())
    }

    pub fn from_env_values(vars: &BTreeMap<String, String>) -> Result<Self, ConfigError> {
        let token = vars
            .get("SESHAT_TOKEN")
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .ok_or(ConfigError::MissingRequired)?;
        let search_upstream = vars
            .get("SESHAT_SEARCH_UPSTREAM")
            .map(String::as_str)
            .unwrap_or("firecrawl")
            .parse()?;
        let firecrawl_upstream_url = parse_base_url(
            vars.get("FIRECRAWL_UPSTREAM_URL")
                .map(String::as_str)
                .unwrap_or(DEFAULT_FIRECRAWL_URL),
        )?;
        let brave_upstream_url = parse_base_url(
            vars.get("BRAVE_SEARCH_UPSTREAM_URL")
                .map(String::as_str)
                .unwrap_or(DEFAULT_BRAVE_URL),
        )?;
        let tavily_upstream_url = parse_base_url(
            vars.get("TAVILY_SEARCH_UPSTREAM_URL")
                .map(String::as_str)
                .unwrap_or(DEFAULT_TAVILY_URL),
        )?;
        let bind_addr = vars
            .get("SESHAT_BIND_ADDR")
            .map(String::as_str)
            .unwrap_or(DEFAULT_BIND_ADDR)
            .parse()
            .map_err(|_| ConfigError::InvalidBindAddress)?;
        let firecrawl_keys = KeyPool::from_file_or_env(
            vars.get("FIRECRAWL_API_KEYS_FILE").map(String::as_str),
            vars.get("FIRECRAWL_API_KEYS").map(String::as_str),
            "firecrawl",
        )?;
        let brave_keys = match search_upstream {
            SearchUpstream::Firecrawl => None,
            SearchUpstream::Brave => Some(KeyPool::from_file_or_env(
                vars.get("BRAVE_SEARCH_API_KEYS_FILE").map(String::as_str),
                vars.get("BRAVE_SEARCH_API_KEYS").map(String::as_str),
                "brave",
            )?),
            SearchUpstream::Tavily => None,
        };
        let tavily_keys = match search_upstream {
            SearchUpstream::Firecrawl | SearchUpstream::Brave => None,
            SearchUpstream::Tavily => Some(KeyPool::from_file_or_env(
                vars.get("TAVILY_SEARCH_API_KEYS_FILE").map(String::as_str),
                vars.get("TAVILY_SEARCH_API_KEYS").map(String::as_str),
                "tavily",
            )?),
        };
        let cache = parse_cache_config(vars)?;

        Ok(Self {
            token,
            search_upstream,
            firecrawl_upstream_url,
            brave_upstream_url,
            tavily_upstream_url,
            bind_addr,
            firecrawl_keys,
            brave_keys,
            tavily_keys,
            cache,
        })
    }

    pub fn search_upstream(&self) -> SearchUpstream {
        self.search_upstream
    }

    pub fn firecrawl_upstream_url(&self) -> &Url {
        &self.firecrawl_upstream_url
    }

    pub fn brave_upstream_url(&self) -> &Url {
        &self.brave_upstream_url
    }

    pub fn tavily_upstream_url(&self) -> &Url {
        &self.tavily_upstream_url
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    pub fn firecrawl_keys(&self) -> &KeyPool {
        &self.firecrawl_keys
    }

    pub fn brave_keys(&self) -> Option<&KeyPool> {
        self.brave_keys.as_ref()
    }

    pub fn tavily_keys(&self) -> Option<&KeyPool> {
        self.tavily_keys.as_ref()
    }

    pub fn cache(&self) -> Option<&CacheConfig> {
        self.cache.as_ref()
    }

    pub fn is_ready(&self) -> bool {
        !self.token.is_empty()
            && !self.firecrawl_keys.is_empty()
            && match self.search_upstream {
                SearchUpstream::Firecrawl => true,
                SearchUpstream::Brave => self
                    .brave_keys
                    .as_ref()
                    .is_some_and(|pool| !pool.is_empty()),
                SearchUpstream::Tavily => self
                    .tavily_keys
                    .as_ref()
                    .is_some_and(|pool| !pool.is_empty()),
            }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

fn parse_cache_config(vars: &BTreeMap<String, String>) -> Result<Option<CacheConfig>, ConfigError> {
    let enabled = vars
        .get("SESHAT_CACHE_ENABLED")
        .map(String::as_str)
        .unwrap_or("false")
        .parse::<bool>()
        .map_err(|_| ConfigError::Invalid)?;
    if !enabled {
        return Ok(None);
    }

    let endpoint = required_cache_value(vars, "SESHAT_CACHE_S3_ENDPOINT")?;
    let bucket = required_cache_value(vars, "SESHAT_CACHE_S3_BUCKET")?;
    let region = required_cache_value(vars, "SESHAT_CACHE_S3_REGION")?;
    let access_key_id = required_cache_value(vars, "SESHAT_CACHE_S3_ACCESS_KEY_ID")?;
    let secret_access_key = required_cache_value(vars, "SESHAT_CACHE_S3_SECRET_ACCESS_KEY")?;

    let endpoint = parse_base_url(endpoint)?;
    validate_bucket(bucket)?;
    let search_ttl = parse_cache_ttl(
        vars,
        "SESHAT_SEARCH_CACHE_TTL_SECS",
        DEFAULT_SEARCH_CACHE_TTL_SECS,
    )?;
    let scrape_ttl = parse_cache_ttl(
        vars,
        "SESHAT_SCRAPE_CACHE_TTL_SECS",
        DEFAULT_SCRAPE_CACHE_TTL_SECS,
    )?;

    Ok(Some(CacheConfig {
        endpoint,
        bucket: bucket.to_owned(),
        region: region.to_owned(),
        access_key_id: access_key_id.to_owned(),
        secret_access_key: secret_access_key.to_owned(),
        search_ttl,
        scrape_ttl,
    }))
}

fn required_cache_value<'a>(
    vars: &'a BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, ConfigError> {
    vars.get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(ConfigError::Invalid)
}

fn validate_bucket(bucket: &str) -> Result<(), ConfigError> {
    if !(3..=63).contains(&bucket.len()) {
        return Err(ConfigError::Invalid);
    }

    let bytes = bucket.as_bytes();
    if !bytes.iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'.' || *byte == b'-'
    }) || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || bytes.windows(2).any(|pair| {
            matches!(
                (pair[0], pair[1]),
                (b'.', b'.') | (b'.', b'-') | (b'-', b'.')
            )
        })
    {
        return Err(ConfigError::Invalid);
    }

    Ok(())
}

fn parse_cache_ttl(
    vars: &BTreeMap<String, String>,
    name: &str,
    default_secs: u64,
) -> Result<Duration, ConfigError> {
    let seconds = vars
        .get(name)
        .map(String::as_str)
        .map(|value| value.parse::<u64>().map_err(|_| ConfigError::Invalid))
        .transpose()?
        .unwrap_or(default_secs);
    if seconds == 0 {
        return Err(ConfigError::Invalid);
    }
    Ok(Duration::from_secs(seconds))
}

fn parse_base_url(value: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidUrl);
    }
    Ok(url)
}
