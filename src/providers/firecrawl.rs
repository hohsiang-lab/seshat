use std::time::Instant;

use reqwest::Client;
use serde::Serialize;
use url::Url;

use super::{
    ScrapeInput, ScrapeResponse, SearchInput, SearchResponse, classify_request_error, decode_json,
    endpoint, log_failure, log_success, read_limited_body,
};
use crate::error::ApiError;
use crate::key_pool::{FailureClass, KeyPool};

#[derive(Clone)]
pub struct FirecrawlProvider {
    client: Client,
    base_url: Url,
    pool: KeyPool,
}

impl FirecrawlProvider {
    pub fn new(client: Client, base_url: Url, pool: KeyPool) -> Self {
        Self {
            client,
            base_url,
            pool,
        }
    }

    pub async fn search(&self, input: &SearchInput) -> Result<SearchResponse, ApiError> {
        let url = endpoint(&self.base_url, "v2/search");
        let candidates = self.pool.candidates();
        if candidates.is_empty() {
            return Err(ApiError::NoEligibleKey {
                provider: "firecrawl",
            });
        }
        let payload = SearchPayload {
            query: &input.query,
            limit: input.limit,
        };
        let mut last_failure_class = "unknown";

        for (index, candidate) in candidates.iter().enumerate() {
            let attempt = index + 1;
            let started = Instant::now();
            let response = self
                .client
                .post(url.clone())
                .bearer_auth(candidate.secret())
                .json(&payload)
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
            let result: SearchResponse = decode_json(&body)?;
            if !result.success {
                return Err(ApiError::UpstreamMalformed);
            }
            self.pool.mark_success(candidate.slot);
            log_success(&self.pool, candidate, attempt, started);
            return Ok(result);
        }

        Err(ApiError::UpstreamExhausted {
            provider: "firecrawl",
            failure_class: last_failure_class,
        })
    }

    pub async fn scrape(&self, input: &ScrapeInput) -> Result<ScrapeResponse, ApiError> {
        let url = endpoint(&self.base_url, "v2/scrape");
        let candidates = self.pool.candidates();
        if candidates.is_empty() {
            return Err(ApiError::NoEligibleKey {
                provider: "firecrawl",
            });
        }
        let payload = ScrapePayload {
            url: &input.url,
            formats: &input.formats,
        };
        let mut last_failure_class = "unknown";

        for (index, candidate) in candidates.iter().enumerate() {
            let attempt = index + 1;
            let started = Instant::now();
            let response = self
                .client
                .post(url.clone())
                .bearer_auth(candidate.secret())
                .json(&payload)
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
            let result: ScrapeResponse = decode_json(&body)?;
            if !result.success {
                return Err(ApiError::UpstreamMalformed);
            }
            self.pool.mark_success(candidate.slot);
            log_success(&self.pool, candidate, attempt, started);
            return Ok(result);
        }

        Err(ApiError::UpstreamExhausted {
            provider: "firecrawl",
            failure_class: last_failure_class,
        })
    }
}

#[derive(Serialize)]
struct SearchPayload<'a> {
    query: &'a str,
    limit: u16,
}

#[derive(Serialize)]
struct ScrapePayload<'a> {
    url: &'a str,
    formats: &'a [String],
}
