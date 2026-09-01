use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use url::Url;

use crate::error::ConfigError;
use crate::key_pool::KeyPool;

const DEFAULT_FIRECRAWL_URL: &str = "https://api.firecrawl.dev";
const DEFAULT_BRAVE_URL: &str = "https://api.search.brave.com";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchUpstream {
    Firecrawl,
    Brave,
}

impl FromStr for SearchUpstream {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "firecrawl" => Ok(Self::Firecrawl),
            "brave" => Ok(Self::Brave),
            _ => Err(ConfigError::Invalid),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    token: String,
    search_upstream: SearchUpstream,
    firecrawl_upstream_url: Url,
    brave_upstream_url: Url,
    bind_addr: SocketAddr,
    firecrawl_keys: KeyPool,
    brave_keys: Option<KeyPool>,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("token", &"[REDACTED]")
            .field("search_upstream", &self.search_upstream)
            .field("firecrawl_upstream_url", &self.firecrawl_upstream_url)
            .field("brave_upstream_url", &self.brave_upstream_url)
            .field("bind_addr", &self.bind_addr)
            .field("firecrawl_keys", &self.firecrawl_keys)
            .field("brave_keys", &self.brave_keys)
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
        };

        Ok(Self {
            token,
            search_upstream,
            firecrawl_upstream_url,
            brave_upstream_url,
            bind_addr,
            firecrawl_keys,
            brave_keys,
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

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    pub fn firecrawl_keys(&self) -> &KeyPool {
        &self.firecrawl_keys
    }

    pub fn brave_keys(&self) -> Option<&KeyPool> {
        self.brave_keys.as_ref()
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
            }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
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
