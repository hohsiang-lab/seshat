use std::time::Instant;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use url::Url;

use super::{
    SearchData, SearchInput, SearchResponse, WebResult, classify_request_error, decode_json,
    endpoint, log_failure, log_success, read_limited_body,
};
use crate::error::ApiError;
use crate::key_pool::{FailureClass, KeyPool};

#[derive(Clone)]
pub struct TavilyProvider {
    client: Client,
    base_url: Url,
    pool: KeyPool,
}

impl TavilyProvider {
    pub fn new(client: Client, base_url: Url, pool: KeyPool) -> Self {
        Self {
            client,
            base_url,
            pool,
        }
    }

    pub async fn search(&self, input: &SearchInput) -> Result<SearchResponse, ApiError> {
        let url = endpoint(&self.base_url, "search");
        let candidates = self.pool.candidates();
        if candidates.is_empty() {
            return Err(ApiError::NoEligibleKey { provider: "tavily" });
        }
        let payload = SearchPayload {
            query: &input.query,
            search_depth: "basic",
            max_results: input.limit,
            include_answer: false,
            include_raw_content: false,
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
                let status_code = status.as_u16();
                if matches!(status_code, 432 | 433) {
                    return Err(ApiError::UpstreamUnavailable);
                }

                let failure = FailureClass::from_status(status_code);
                if failure.is_retryable() {
                    last_failure_class = failure.status_class();
                    self.pool.mark_failure(candidate.slot, failure);
                    log_failure(&self.pool, candidate, attempt, failure, started);
                    continue;
                }
                if matches!(status_code, 400 | 422) {
                    return Err(ApiError::UpstreamCallerError(status_code));
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
            let result: TavilyResponse = decode_json(&body)?;
            self.pool.mark_success(candidate.slot);
            log_success(&self.pool, candidate, attempt, started);
            return Ok(result.into_search_response());
        }

        Err(ApiError::UpstreamExhausted {
            provider: "tavily",
            failure_class: last_failure_class,
        })
    }
}

#[derive(Serialize)]
struct SearchPayload<'a> {
    query: &'a str,
    search_depth: &'static str,
    max_results: u16,
    include_answer: bool,
    include_raw_content: bool,
}

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: Option<String>,
}

impl TavilyResponse {
    fn into_search_response(self) -> SearchResponse {
        SearchResponse {
            success: true,
            data: SearchData {
                web: self
                    .results
                    .into_iter()
                    .filter(|result| !result.url.is_empty())
                    .map(|result| WebResult {
                        url: result.url,
                        title: result.title,
                        description: result.content.unwrap_or_default(),
                    })
                    .collect(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SearchPayload, TavilyResponse};
    use serde_json::json;

    #[test]
    fn serializes_approved_search_payload() {
        let payload = SearchPayload {
            query: "rust async",
            search_depth: "basic",
            max_results: 7,
            include_answer: false,
            include_raw_content: false,
        };

        assert_eq!(
            serde_json::to_value(payload).expect("payload should serialize"),
            json!({
                "query": "rust async",
                "search_depth": "basic",
                "max_results": 7,
                "include_answer": false,
                "include_raw_content": false,
            })
        );
    }

    #[test]
    fn maps_tavily_results_to_search_response() {
        let response: TavilyResponse = serde_json::from_value(json!({
            "answer": "ignored",
            "results": [
                {
                    "url": "https://example.com/one",
                    "title": "One",
                    "content": "First result",
                    "score": 0.9,
                    "raw_content": "ignored"
                },
                {
                    "url": "",
                    "title": "Empty URL",
                    "content": "Filtered"
                },
                {
                    "url": "https://example.com/two",
                    "title": "Two",
                    "images": ["ignored"]
                }
            ],
            "usage": {"credits": 1}
        }))
        .expect("Tavily response should deserialize");

        assert_eq!(
            serde_json::to_value(response.into_search_response())
                .expect("search response should serialize"),
            json!({
                "success": true,
                "data": {
                    "web": [
                        {
                            "url": "https://example.com/one",
                            "title": "One",
                            "description": "First result"
                        },
                        {
                            "url": "https://example.com/two",
                            "title": "Two",
                            "description": ""
                        }
                    ]
                }
            })
        );
    }
}
