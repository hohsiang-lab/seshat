pub mod brave;
pub mod firecrawl;
pub mod tavily;

use std::collections::BTreeMap;
use std::time::Instant;

use reqwest::Response;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use url::Url;

use crate::error::ApiError;
use crate::key_pool::{FailureClass, KeyCandidate, KeyPool};

pub const MAX_UPSTREAM_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SearchInput {
    pub query: String,
    pub limit: u16,
}

#[derive(Clone, Debug)]
pub struct ScrapeInput {
    pub url: String,
    pub formats: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchResponse {
    pub success: bool,
    pub data: SearchData,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchData {
    #[serde(default)]
    pub web: Vec<WebResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WebResult {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScrapeResponse {
    pub success: bool,
    pub data: ScrapeData,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScrapeData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(default)]
    pub metadata: ScrapeMetadata,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ScrapeMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "sourceURL", default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub(crate) fn endpoint(base: &Url, path: &str) -> Url {
    let mut endpoint = base.clone();
    let base_path = base.path().trim_end_matches('/');
    let path = path.trim_start_matches('/');
    let joined = if base_path.is_empty() {
        format!("/{path}")
    } else {
        format!("{base_path}/{path}")
    };
    endpoint.set_path(&joined);
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    endpoint
}

pub(crate) async fn read_limited_body(response: Response) -> Result<Vec<u8>, ApiError> {
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_UPSTREAM_BODY_BYTES)
    {
        return Err(ApiError::UpstreamMalformed);
    }

    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ApiError::UpstreamUnavailable)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_UPSTREAM_BODY_BYTES {
            return Err(ApiError::UpstreamMalformed);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) fn decode_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|_| ApiError::UpstreamMalformed)
}

pub(crate) fn classify_request_error(error: &reqwest::Error) -> FailureClass {
    if error.is_timeout() {
        FailureClass::Timeout
    } else {
        FailureClass::Connection
    }
}

pub(crate) fn log_success(
    pool: &KeyPool,
    candidate: &KeyCandidate,
    attempt: usize,
    started: Instant,
) {
    tracing::info!(
        provider = pool.provider_name(),
        key_slot = candidate.slot,
        status_class = "2xx",
        attempt_count = attempt,
        latency_ms = started.elapsed().as_millis() as u64,
        "upstream request succeeded"
    );
}

pub(crate) fn log_failure(
    pool: &KeyPool,
    candidate: &KeyCandidate,
    attempt: usize,
    failure: FailureClass,
    started: Instant,
) {
    tracing::warn!(
        provider = pool.provider_name(),
        key_slot = candidate.slot,
        status_class = failure.status_class(),
        attempt_count = attempt,
        latency_ms = started.elapsed().as_millis() as u64,
        "upstream request failed; trying next eligible key"
    );
}
