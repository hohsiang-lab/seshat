use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State, rejection::BytesRejection},
    http::{HeaderMap, header::AUTHORIZATION},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::cache::{CacheOperation, CacheStore, scrape_cache_key, search_cache_key};
use crate::config::{Config, SearchUpstream};
use crate::error::ApiError;
use crate::providers::brave::BraveProvider;
use crate::providers::firecrawl::FirecrawlProvider;
use crate::providers::tavily::TavilyProvider;
use crate::providers::{ScrapeInput, ScrapeResponse, SearchInput, SearchResponse};

pub const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const MAX_QUERY_BYTES: usize = 2_000;
const MAX_URL_BYTES: usize = 2_048;
const MAX_DOCUMENT_BYTES: usize = 3 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct AppState {
    token: Arc<str>,
    search_upstream: SearchUpstream,
    firecrawl: FirecrawlProvider,
    brave: Option<BraveProvider>,
    tavily: Option<TavilyProvider>,
    cache: Option<CacheStore>,
    ready: bool,
}

impl AppState {
    pub fn new(config: Config, client: reqwest::Client) -> Self {
        let cache = config.cache().map(CacheStore::new);
        let firecrawl = FirecrawlProvider::new(
            client.clone(),
            config.firecrawl_upstream_url().clone(),
            config.firecrawl_keys().clone(),
        );
        let tavily = config.tavily_keys().map(|pool| {
            TavilyProvider::new(
                client.clone(),
                config.tavily_upstream_url().clone(),
                pool.clone(),
            )
        });
        let brave = config.brave_keys().map(|pool| {
            BraveProvider::new(client, config.brave_upstream_url().clone(), pool.clone())
        });
        Self {
            token: Arc::from(config.token().to_owned()),
            search_upstream: config.search_upstream(),
            firecrawl,
            brave,
            tavily,
            cache,
            ready: config.is_ready(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v2/search", post(search))
        .route("/v2/scrape", post(scrape))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn readyz(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    if state.is_ready() {
        Ok(Json(HealthResponse { status: "ready" }))
    } else {
        Err(ApiError::NotReady)
    }
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<SearchResponse>, ApiError> {
    require_auth(&headers, &state)?;
    let body = body.map_err(|_| ApiError::PayloadTooLarge)?;
    let request: SearchRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::InvalidInput)?;
    let input = normalize_search(request)?;
    let provider = state.search_upstream.as_str();
    let cache_key = search_cache_key(&input, provider);
    if let Some(cache) = state.cache.as_ref()
        && let Some(response) = cache
            .get::<SearchResponse>(CacheOperation::Search, provider, &cache_key)
            .await
    {
        if response.success {
            return Ok(Json(response));
        }
        tracing::warn!(
            operation = "search",
            provider,
            cache_outcome = "invalid",
            "cache payload rejected"
        );
    }
    let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
        match state.search_upstream {
            SearchUpstream::Firecrawl => state.firecrawl.search(&input).await,
            SearchUpstream::Brave => {
                state
                    .brave
                    .as_ref()
                    .ok_or(ApiError::UpstreamUnavailable)?
                    .search(&input)
                    .await
            }
            SearchUpstream::Tavily => {
                state
                    .tavily
                    .as_ref()
                    .ok_or(ApiError::UpstreamUnavailable)?
                    .search(&input)
                    .await
            }
        }
    })
    .await
    .map_err(|_| ApiError::GatewayTimeout)??;
    if response.success
        && let Some(cache) = state.cache.as_ref()
    {
        cache
            .put(CacheOperation::Search, provider, &cache_key, &response)
            .await;
    }
    Ok(Json(response))
}

async fn scrape(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<ScrapeResponse>, ApiError> {
    require_auth(&headers, &state)?;
    let body = body.map_err(|_| ApiError::PayloadTooLarge)?;
    let request: ScrapeRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::InvalidInput)?;
    let input = normalize_scrape(request).await?;
    let provider = "firecrawl";
    let cache_key = scrape_cache_key(&input, provider);
    if let Some(cache) = state.cache.as_ref()
        && let Some(response) = cache
            .get::<ScrapeResponse>(CacheOperation::Scrape, provider, &cache_key)
            .await
    {
        if response.success && validate_scrape_response(&response).await.is_ok() {
            return Ok(Json(response));
        }
        tracing::warn!(
            operation = "scrape",
            provider,
            cache_outcome = "invalid",
            "cache payload rejected"
        );
    }
    let response = tokio::time::timeout(REQUEST_TIMEOUT, state.firecrawl.scrape(&input))
        .await
        .map_err(|_| ApiError::GatewayTimeout)??;
    validate_scrape_response(&response).await?;
    if response.success
        && let Some(cache) = state.cache.as_ref()
    {
        cache
            .put(CacheOperation::Scrape, provider, &cache_key, &response)
            .await;
    }
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default)]
    limit: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct ScrapeRequest {
    url: String,
    #[serde(default)]
    formats: Option<Vec<String>>,
}

fn require_auth(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    let expected = format!("Bearer {}", state.token);
    let provided = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if provided == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

fn normalize_search(request: SearchRequest) -> Result<SearchInput, ApiError> {
    let query = request.query.trim();
    if query.is_empty() || query.len() > MAX_QUERY_BYTES {
        return Err(ApiError::InvalidInput);
    }
    let limit = request.limit.unwrap_or(5);
    if !(1..=20).contains(&limit) {
        return Err(ApiError::InvalidInput);
    }
    Ok(SearchInput {
        query: query.to_owned(),
        limit,
    })
}

async fn normalize_scrape(request: ScrapeRequest) -> Result<ScrapeInput, ApiError> {
    if request.url.len() > MAX_URL_BYTES {
        return Err(ApiError::InvalidInput);
    }
    validate_target_url(&request.url)
        .await
        .map_err(|_| ApiError::InvalidInput)?;
    let formats = normalize_formats(request.formats)?;
    Ok(ScrapeInput {
        url: request.url,
        formats,
    })
}

fn normalize_formats(formats: Option<Vec<String>>) -> Result<Vec<String>, ApiError> {
    let formats = formats.unwrap_or_else(|| vec!["markdown".to_owned()]);
    if formats.is_empty() || formats.len() > 2 {
        return Err(ApiError::InvalidInput);
    }
    let mut normalized = Vec::with_capacity(formats.len());
    for format in formats {
        if !matches!(format.as_str(), "markdown" | "html") || normalized.contains(&format) {
            return Err(ApiError::InvalidInput);
        }
        normalized.push(format);
    }
    Ok(normalized)
}

async fn validate_scrape_response(response: &ScrapeResponse) -> Result<(), ApiError> {
    if response
        .data
        .markdown
        .as_ref()
        .is_some_and(|content| content.len() > MAX_DOCUMENT_BYTES)
        || response
            .data
            .html
            .as_ref()
            .is_some_and(|content| content.len() > MAX_DOCUMENT_BYTES)
    {
        return Err(ApiError::UpstreamMalformed);
    }
    if let Some(source_url) = response.data.metadata.source_url.as_deref() {
        validate_target_url(source_url)
            .await
            .map_err(|_| ApiError::UpstreamMalformed)?;
    }
    Ok(())
}

async fn validate_target_url(value: &str) -> Result<(), ()> {
    let url = Url::parse(value).map_err(|_| ())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || has_credential_query(&url)
    {
        return Err(());
    }
    let host = url.host_str().ok_or(())?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return Err(());
    }
    let port = url.port_or_known_default().ok_or(())?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| ())?;
    let mut found_address = false;
    for address in addresses {
        found_address = true;
        if is_forbidden_ip(address.ip()) {
            return Err(());
        }
    }
    found_address.then_some(()).ok_or(())
}

fn has_credential_query(url: &Url) -> bool {
    const CREDENTIAL_NAMES: &[&str] = &[
        "password",
        "passwd",
        "pass",
        "token",
        "api_key",
        "apikey",
        "key",
        "secret",
        "access_token",
        "authorization",
    ];
    url.query_pairs().any(|(name, _)| {
        CREDENTIAL_NAMES
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
    })
}

fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => is_forbidden_ipv4(address),
        IpAddr::V6(address) => is_forbidden_ipv6(address),
    }
}

fn is_forbidden_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || address == Ipv4Addr::new(169, 254, 169, 254)
        || address == Ipv4Addr::new(100, 100, 100, 200)
}

fn is_forbidden_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let first = segments[0];
    let mapped_ipv4 = (segments[..6] == [0, 0, 0, 0, 0, 0xffff])
        .then(|| Ipv4Addr::from(((segments[6] as u32) << 16) | segments[7] as u32));
    mapped_ipv4.is_some_and(is_forbidden_ipv4)
        || address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::{AppState, has_credential_query, is_forbidden_ip};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use url::Url;

    #[test]
    fn enabled_cache_is_attached_without_affecting_readiness() {
        let config = crate::config::Config::from_env_values(&std::collections::BTreeMap::from([
            ("SESHAT_TOKEN".to_owned(), "auth".to_owned()),
            ("FIRECRAWL_API_KEYS".to_owned(), "alpha".to_owned()),
            ("SESHAT_CACHE_ENABLED".to_owned(), "true".to_owned()),
            (
                "SESHAT_CACHE_S3_ENDPOINT".to_owned(),
                "http://127.0.0.1:1".to_owned(),
            ),
            (
                "SESHAT_CACHE_S3_BUCKET".to_owned(),
                "seshat-cache".to_owned(),
            ),
            ("SESHAT_CACHE_S3_REGION".to_owned(), "us-east-1".to_owned()),
            (
                "SESHAT_CACHE_S3_ACCESS_KEY_ID".to_owned(),
                "access".to_owned(),
            ),
            (
                "SESHAT_CACHE_S3_SECRET_ACCESS_KEY".to_owned(),
                "secret".to_owned(),
            ),
        ]))
        .expect("enabled cache config should load");

        let state = AppState::new(config, reqwest::Client::new());
        assert!(state.cache.is_some());
        assert!(state.is_ready());
    }

    #[test]
    fn tavily_selection_constructs_tavily_without_brave() {
        let config = crate::config::Config::from_env_values(&std::collections::BTreeMap::from([
            ("SESHAT_TOKEN".to_owned(), "auth".to_owned()),
            ("SESHAT_SEARCH_UPSTREAM".to_owned(), "tavily".to_owned()),
            ("FIRECRAWL_API_KEYS".to_owned(), "firecrawl".to_owned()),
            ("TAVILY_SEARCH_API_KEYS".to_owned(), "tavily".to_owned()),
        ]))
        .expect("Tavily config should load");

        let state = AppState::new(config, reqwest::Client::new());
        assert!(state.tavily.is_some());
        assert!(state.brave.is_none());
    }

    #[test]
    fn rejects_private_loopback_metadata_and_mapped_ipv4_addresses() {
        assert!(is_forbidden_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_forbidden_ip(IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_forbidden_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_forbidden_ip(
            "::ffff:127.0.0.1".parse().expect("mapped IPv4")
        ));
        assert!(!is_forbidden_ip(
            "2001:db8::1".parse().expect("documentation IPv6")
        ));
    }

    #[test]
    fn rejects_credential_query_parameters() {
        let credential_url = Url::parse("https://example.com/?api_key=redacted").expect("URL");
        let normal_url = Url::parse("https://example.com/?page=1").expect("URL");
        assert!(has_credential_query(&credential_url));
        assert!(!has_credential_query(&normal_url));
    }
}
