use std::time::Instant;

use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::{
    SearchData, SearchInput, SearchResponse, WebResult, classify_request_error, decode_json,
    endpoint, log_failure, log_success,
};
use crate::error::ApiError;
use crate::key_pool::KeyPool;

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
        let candidates = self.pool.candidates();
        if candidates.is_empty() {
            return Err(ApiError::NoEligibleKey { provider: "brave" });
        }
        let mut last_failure_class = "unknown";

        for (index, candidate) in candidates.iter().enumerate() {
            let attempt = index + 1;
            let started = Instant::now();
            match self.search_with_key(input, candidate.secret()).await {
                Ok(response) => {
                    self.pool.mark_success(candidate.slot);
                    log_success(&self.pool, candidate, attempt, started);
                    return Ok(response);
                }
                Err(super::SearchAttemptError::Retryable(failure)) => {
                    last_failure_class = failure.status_class();
                    self.pool.mark_failure(candidate.slot, failure);
                    log_failure(&self.pool, candidate, attempt, failure, started);
                }
                Err(super::SearchAttemptError::Api(error)) => return Err(error),
            }
        }

        Err(ApiError::UpstreamExhausted {
            provider: "brave",
            failure_class: last_failure_class,
        })
    }

    pub(crate) async fn search_with_key(
        &self,
        input: &SearchInput,
        secret: &str,
    ) -> Result<SearchResponse, super::SearchAttemptError> {
        let mut url = endpoint(&self.base_url, "res/v1/web/search");
        url.query_pairs_mut()
            .append_pair("q", &input.query)
            .append_pair("count", &input.limit.to_string());
        let response = self
            .client
            .get(url)
            .header("X-Subscription-Token", secret)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|error| {
                super::SearchAttemptError::Retryable(classify_request_error(&error))
            })?;
        super::classify_response_status(response.status().as_u16(), false)?;
        let body = super::read_attempt_body(response).await?;
        let result: BraveResponse = decode_json(&body)?;
        Ok(result.into_search_response())
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
