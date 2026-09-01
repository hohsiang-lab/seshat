use std::time::Instant;

use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::{
    SearchData, SearchInput, SearchResponse, WebResult, classify_request_error, decode_json,
    endpoint, log_failure, log_success, read_limited_body,
};
use crate::error::ApiError;
use crate::key_pool::{FailureClass, KeyPool};

#[derive(Clone)]
pub struct BraveProvider {
    client: Client,
    base_url: Url,
    pool: KeyPool,
}

impl BraveProvider {
    pub fn new(client: Client, base_url: Url, pool: KeyPool) -> Self {
        Self {
            client,
            base_url,
            pool,
        }
    }

    pub async fn search(&self, input: &SearchInput) -> Result<SearchResponse, ApiError> {
        let mut url = endpoint(&self.base_url, "res/v1/web/search");
        url.query_pairs_mut()
            .append_pair("q", &input.query)
            .append_pair("count", &input.limit.to_string());
        let candidates = self.pool.candidates();
        if candidates.is_empty() {
            return Err(ApiError::NoEligibleKey { provider: "brave" });
        }
        let mut last_failure_class = "unknown";

        for (index, candidate) in candidates.iter().enumerate() {
            let attempt = index + 1;
            let started = Instant::now();
            let response = self
                .client
                .get(url.clone())
                .header("X-Subscription-Token", candidate.secret())
                .header("Accept", "application/json")
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    let failure = classify_request_error(&error);
                    last_failure_class = failure.status_class();
                    self.pool.mark_failure(candidate.slot, failure);
                    log_failure(&self.pool, candidate, attempt, failure, started);
                    continue;
                }
            };

            let status = response.status();
            if !status.is_success() {
                let failure = FailureClass::from_status(status.as_u16());
                if failure.is_retryable() {
                    last_failure_class = failure.status_class();
                    self.pool.mark_failure(candidate.slot, failure);
                    log_failure(&self.pool, candidate, attempt, failure, started);
                    continue;
                }
                if matches!(status.as_u16(), 400 | 422) {
                    return Err(ApiError::UpstreamCallerError(status.as_u16()));
                }
                return Err(ApiError::UpstreamUnavailable);
            }

            let body = match read_limited_body(response).await {
                Ok(body) => body,
                Err(ApiError::UpstreamUnavailable) => {
                    let failure = FailureClass::Connection;
                    last_failure_class = failure.status_class();
                    self.pool.mark_failure(candidate.slot, failure);
                    log_failure(&self.pool, candidate, attempt, failure, started);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let result: BraveResponse = decode_json(&body)?;
            self.pool.mark_success(candidate.slot);
            log_success(&self.pool, candidate, attempt, started);
            return Ok(result.into_search_response());
        }

        Err(ApiError::UpstreamExhausted {
            provider: "brave",
            failure_class: last_failure_class,
        })
    }
}

#[derive(Debug, Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: BraveWeb,
}

#[derive(Debug, Default, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

impl BraveResponse {
    fn into_search_response(self) -> SearchResponse {
        SearchResponse {
            success: true,
            data: SearchData {
                web: self
                    .web
                    .results
                    .into_iter()
                    .filter(|result| !result.url.is_empty())
                    .map(|result| WebResult {
                        url: result.url,
                        title: result.title,
                        description: result.description,
                    })
                    .collect(),
            },
        }
    }
}
