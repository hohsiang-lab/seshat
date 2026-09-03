use sha2::{Digest, Sha256};
use std::time::{Duration, Instant, SystemTime};

use crate::config::CacheConfig;
use crate::providers::{MAX_UPSTREAM_BODY_BYTES, ScrapeInput, SearchInput};

pub(crate) const CACHE_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheOperation {
    Search,
    Scrape,
}

impl CacheOperation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Scrape => "scrape",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Freshness {
    Fresh,
    Expired,
    Invalid,
}

#[derive(serde::Serialize)]
struct SearchKey<'a> {
    cache_version: u8,
    operation: &'static str,
    provider: &'a str,
    query: &'a str,
    limit: u16,
}

#[derive(serde::Serialize)]
struct ScrapeKey<'a> {
    cache_version: u8,
    operation: &'static str,
    provider: &'a str,
    url: &'a str,
    formats: Vec<String>,
}

fn cache_key<T: serde::Serialize>(prefix: &str, provider: &str, input: &T) -> String {
    let bytes = serde_json::to_vec(input).expect("cache key should serialize");
    format!(
        "{prefix}/{provider}/{}.json",
        hex::encode(Sha256::digest(bytes))
    )
}

pub(crate) fn search_cache_key(input: &SearchInput, provider: &str) -> String {
    cache_key(
        "cache/v1/search",
        provider,
        &SearchKey {
            cache_version: CACHE_VERSION,
            operation: CacheOperation::Search.as_str(),
            provider,
            query: &input.query,
            limit: input.limit,
        },
    )
}

pub(crate) fn scrape_cache_key(input: &ScrapeInput, provider: &str) -> String {
    let mut formats = input.formats.clone();
    formats.sort_unstable();
    cache_key(
        "cache/v1/scrape",
        provider,
        &ScrapeKey {
            cache_version: CACHE_VERSION,
            operation: CacheOperation::Scrape.as_str(),
            provider,
            url: &input.url,
            formats,
        },
    )
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct CacheEnvelope<T> {
    envelope_version: u8,
    operation: String,
    provider: String,
    payload: T,
}

pub(crate) fn encode_envelope<T: serde::Serialize>(
    operation: CacheOperation,
    provider: &str,
    payload: &T,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&CacheEnvelope {
        envelope_version: CACHE_VERSION,
        operation: operation.as_str().to_owned(),
        provider: provider.to_owned(),
        payload,
    })
}

pub(crate) fn decode_envelope<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    operation: CacheOperation,
    provider: &str,
) -> Result<T, serde_json::Error> {
    let envelope: CacheEnvelope<T> = serde_json::from_slice(bytes)?;
    if envelope.envelope_version != CACHE_VERSION
        || envelope.operation != operation.as_str()
        || envelope.provider != provider
    {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "invalid cache envelope",
        ));
    }
    Ok(envelope.payload)
}

pub(crate) fn cache_freshness(modified: SystemTime, now: SystemTime, ttl: Duration) -> Freshness {
    let age = match now.duration_since(modified) {
        Ok(age) => age,
        Err(_) => return Freshness::Invalid,
    };
    if age >= ttl {
        Freshness::Expired
    } else {
        Freshness::Fresh
    }
}

const CACHE_IO_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CACHE_OBJECT_BYTES: usize = MAX_UPSTREAM_BODY_BYTES;
const SMALL_CACHE_OBJECT_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct CacheStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    search_ttl: Duration,
    scrape_ttl: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheOutcome {
    Hit,
    Missing,
    Expired,
    Invalid,
    Unavailable,
    Stored,
    WriteFailed,
}

impl CacheOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Missing => "missing",
            Self::Expired => "expired",
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
            Self::Stored => "stored",
            Self::WriteFailed => "write_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjectSizeClass {
    Empty,
    Small,
    Large,
    Oversized,
    Unknown,
}

impl ObjectSizeClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Small => "small",
            Self::Large => "large",
            Self::Oversized => "oversized",
            Self::Unknown => "unknown",
        }
    }
}

fn object_size_class(size: Option<usize>) -> ObjectSizeClass {
    match size {
        None => ObjectSizeClass::Unknown,
        Some(0) => ObjectSizeClass::Empty,
        Some(size) if size > MAX_CACHE_OBJECT_BYTES => ObjectSizeClass::Oversized,
        Some(size) if size <= SMALL_CACHE_OBJECT_BYTES => ObjectSizeClass::Small,
        Some(_) => ObjectSizeClass::Large,
    }
}

fn content_length_size_class(content_length: Option<i64>) -> ObjectSizeClass {
    match content_length {
        None => ObjectSizeClass::Unknown,
        Some(length) if length < 0 => ObjectSizeClass::Oversized,
        Some(length) if length as u64 > MAX_CACHE_OBJECT_BYTES as u64 => ObjectSizeClass::Oversized,
        Some(length) => object_size_class(Some(length as usize)),
    }
}

fn sanitized_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "firecrawl" => Some("firecrawl"),
        "brave" => Some("brave"),
        "tavily" => Some("tavily"),
        _ => None,
    }
}

fn log_cache_outcome(
    operation: CacheOperation,
    provider: &str,
    outcome: CacheOutcome,
    object_size: ObjectSizeClass,
    started: Instant,
) {
    let Some(provider) = sanitized_provider(provider) else {
        return;
    };
    let operation = operation.as_str();
    let cache_outcome = outcome.as_str();
    let object_size_class = object_size.as_str();
    let latency_ms = started.elapsed().as_millis() as u64;

    match outcome {
        CacheOutcome::Invalid | CacheOutcome::Unavailable | CacheOutcome::WriteFailed => {
            tracing::warn!(
                operation,
                provider,
                cache_outcome,
                object_size_class,
                latency_ms,
                "cache operation failed"
            );
        }
        CacheOutcome::Hit
        | CacheOutcome::Missing
        | CacheOutcome::Expired
        | CacheOutcome::Stored => {
            tracing::debug!(
                operation,
                provider,
                cache_outcome,
                object_size_class,
                latency_ms,
                "cache operation completed"
            );
        }
    }
}

impl CacheStore {
    pub(crate) fn new(config: &CacheConfig) -> Self {
        let credentials = aws_sdk_s3::config::Credentials::new(
            config.access_key_id().to_owned(),
            config.secret_access_key().to_owned(),
            None,
            None,
            "seshat-cache",
        );
        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_s3::config::Region::new(config.region().to_owned()))
            .endpoint_url(config.endpoint().as_str())
            .credentials_provider(credentials)
            .force_path_style(true)
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(1))
            .build();

        Self {
            client: aws_sdk_s3::Client::from_conf(s3_config),
            bucket: config.bucket().to_owned(),
            search_ttl: config.search_ttl(),
            scrape_ttl: config.scrape_ttl(),
        }
    }

    pub(crate) async fn get<T>(
        &self,
        operation: CacheOperation,
        provider: &str,
        key: &str,
    ) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let started = Instant::now();
        if sanitized_provider(provider).is_none() {
            log_cache_outcome(
                operation,
                provider,
                CacheOutcome::Invalid,
                ObjectSizeClass::Unknown,
                started,
            );
            return None;
        }

        match tokio::time::timeout(CACHE_IO_TIMEOUT, self.read(operation, provider, key)).await {
            Ok(Ok((payload, object_size))) => {
                log_cache_outcome(operation, provider, CacheOutcome::Hit, object_size, started);
                Some(payload)
            }
            Ok(Err((outcome, object_size))) => {
                log_cache_outcome(operation, provider, outcome, object_size, started);
                None
            }
            Err(_) => {
                log_cache_outcome(
                    operation,
                    provider,
                    CacheOutcome::Unavailable,
                    ObjectSizeClass::Unknown,
                    started,
                );
                None
            }
        }
    }

    async fn read<T>(
        &self,
        operation: CacheOperation,
        provider: &str,
        key: &str,
    ) -> Result<(T, ObjectSizeClass), (CacheOutcome, ObjectSizeClass)>
    where
        T: serde::de::DeserializeOwned,
    {
        let output = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(output) => output,
            Err(error) => {
                let outcome = if is_missing_get_error(&error) {
                    CacheOutcome::Missing
                } else {
                    CacheOutcome::Unavailable
                };
                return Err((outcome, ObjectSizeClass::Unknown));
            }
        };

        let content_length = output.content_length;
        let advertised_size = content_length_size_class(content_length);
        let modified = match output.last_modified.as_ref() {
            Some(value) => match SystemTime::try_from(*value) {
                Ok(value) => value,
                Err(_) => return Err((CacheOutcome::Invalid, advertised_size)),
            },
            None => return Err((CacheOutcome::Invalid, advertised_size)),
        };

        match cache_freshness(modified, SystemTime::now(), self.ttl(operation)) {
            Freshness::Fresh => {}
            Freshness::Expired => return Err((CacheOutcome::Expired, advertised_size)),
            Freshness::Invalid => return Err((CacheOutcome::Invalid, advertised_size)),
        }

        if content_length
            .is_some_and(|length| length < 0 || length as u64 > MAX_CACHE_OBJECT_BYTES as u64)
        {
            return Err((CacheOutcome::Invalid, ObjectSizeClass::Oversized));
        }

        let capacity = content_length
            .map(|length| length as usize)
            .unwrap_or_default();
        let mut body = Vec::with_capacity(capacity);
        let mut stream = output.body;
        while let Some(chunk) = stream.try_next().await.map_err(|_| {
            (
                CacheOutcome::Unavailable,
                object_size_class(Some(body.len())),
            )
        })? {
            let next_len = body.len().saturating_add(chunk.len());
            if next_len > MAX_CACHE_OBJECT_BYTES {
                return Err((CacheOutcome::Invalid, ObjectSizeClass::Oversized));
            }
            body.extend_from_slice(&chunk);
        }

        let object_size = object_size_class(Some(body.len()));
        let payload = decode_envelope(&body, operation, provider)
            .map_err(|_| (CacheOutcome::Invalid, object_size))?;
        Ok((payload, object_size))
    }

    pub(crate) async fn put<T>(
        &self,
        operation: CacheOperation,
        provider: &str,
        key: &str,
        payload: &T,
    ) where
        T: serde::Serialize,
    {
        let started = Instant::now();
        if sanitized_provider(provider).is_none() {
            log_cache_outcome(
                operation,
                provider,
                CacheOutcome::WriteFailed,
                ObjectSizeClass::Unknown,
                started,
            );
            return;
        }

        let body = match encode_envelope(operation, provider, payload) {
            Ok(body) => body,
            Err(_) => {
                log_cache_outcome(
                    operation,
                    provider,
                    CacheOutcome::WriteFailed,
                    ObjectSizeClass::Unknown,
                    started,
                );
                return;
            }
        };
        let object_size = object_size_class(Some(body.len()));
        if body.len() > MAX_CACHE_OBJECT_BYTES {
            log_cache_outcome(
                operation,
                provider,
                CacheOutcome::WriteFailed,
                ObjectSizeClass::Oversized,
                started,
            );
            return;
        }

        let result = tokio::time::timeout(
            CACHE_IO_TIMEOUT,
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .content_type("application/json")
                .body(aws_sdk_s3::primitives::ByteStream::from(body))
                .send(),
        )
        .await;
        let outcome = if matches!(result, Ok(Ok(_))) {
            CacheOutcome::Stored
        } else {
            CacheOutcome::WriteFailed
        };
        log_cache_outcome(operation, provider, outcome, object_size, started);
    }

    fn ttl(&self, operation: CacheOperation) -> Duration {
        match operation {
            CacheOperation::Search => self.search_ttl,
            CacheOperation::Scrape => self.scrape_ttl,
        }
    }
}

fn is_missing_get_error(
    error: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
) -> bool {
    error
        .as_service_error()
        .is_some_and(|error| error.is_no_such_key())
        || error
            .raw_response()
            .is_some_and(|response| response.status().as_u16() == 404)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ScrapeInput, SearchData, SearchInput, SearchResponse};
    use std::time::{Duration, SystemTime};

    #[test]
    fn search_key_is_deterministic_opaque_provider_scoped_and_canonical() {
        let input = SearchInput {
            query: "rust cache".to_owned(),
            limit: 5,
        };
        let first = search_cache_key(&input, "firecrawl");
        let second = search_cache_key(&input, "firecrawl");
        let brave = search_cache_key(&input, "brave");
        let other_limit = search_cache_key(
            &SearchInput {
                query: input.query.clone(),
                limit: 6,
            },
            "firecrawl",
        );

        assert_eq!(first, second);
        assert_ne!(first, brave);
        assert_ne!(first, other_limit);
        assert_eq!(
            first,
            "cache/v1/search/firecrawl/3674642922bfd9db9bf84514051c69ee92f5ce3b5fd2d63a7939e2819c719bdf.json"
        );
        assert!(first.ends_with(".json"));
        assert_eq!(first.rsplit('/').next().expect("digest").len(), 69);
        assert!(!first.contains("rust cache"));
    }

    #[test]
    fn scrape_key_treats_formats_as_an_order_independent_set_without_mutating_input() {
        let markdown_html = ScrapeInput {
            url: "https://example.com/page".to_owned(),
            formats: vec!["markdown".to_owned(), "html".to_owned()],
        };
        let html_markdown = ScrapeInput {
            url: markdown_html.url.clone(),
            formats: vec!["html".to_owned(), "markdown".to_owned()],
        };

        assert_eq!(
            scrape_cache_key(&markdown_html, "firecrawl"),
            scrape_cache_key(&html_markdown, "firecrawl")
        );
        assert_eq!(
            scrape_cache_key(&markdown_html, "firecrawl"),
            "cache/v1/scrape/firecrawl/1fffcf9af0263af6382b64c4546ccd8134160d10142ec0522eab3630ec1f9713.json"
        );
        assert_ne!(
            scrape_cache_key(&markdown_html, "firecrawl"),
            scrape_cache_key(&markdown_html, "brave")
        );
        assert_eq!(markdown_html.formats, ["markdown", "html"]);
        assert!(!scrape_cache_key(&markdown_html, "firecrawl").contains("example.com"));
    }

    #[test]
    fn envelope_round_trip_requires_version_operation_provider_and_valid_payload() {
        let response = SearchResponse {
            success: true,
            data: SearchData { web: Vec::new() },
        };
        let bytes = encode_envelope(CacheOperation::Search, "firecrawl", &response)
            .expect("envelope should serialize");
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).expect("envelope JSON");
        assert_eq!(envelope.as_object().expect("object").len(), 4);
        assert!(envelope.get("expires_at").is_none());

        let decoded: SearchResponse = decode_envelope(&bytes, CacheOperation::Search, "firecrawl")
            .expect("matching envelope should decode");
        assert!(decoded.success);

        let mut wrong_version = envelope.clone();
        wrong_version["envelope_version"] = serde_json::json!(2);
        let wrong_version = serde_json::to_vec(&wrong_version).expect("wrong version JSON");
        assert!(
            decode_envelope::<SearchResponse>(&wrong_version, CacheOperation::Search, "firecrawl",)
                .is_err()
        );

        let mut wrong_operation = envelope.clone();
        wrong_operation["operation"] = serde_json::json!("scrape");
        let wrong_operation = serde_json::to_vec(&wrong_operation).expect("wrong operation JSON");
        assert!(
            decode_envelope::<SearchResponse>(
                &wrong_operation,
                CacheOperation::Search,
                "firecrawl",
            )
            .is_err()
        );

        let mut wrong_provider = envelope.clone();
        wrong_provider["provider"] = serde_json::json!("brave");
        let wrong_provider = serde_json::to_vec(&wrong_provider).expect("wrong provider JSON");
        assert!(decode_envelope::<SearchResponse>(
            &wrong_provider,
            CacheOperation::Search,
            "firecrawl",
        )
        .is_err());

        let mut invalid_payload = envelope;
        invalid_payload["payload"] = serde_json::json!({"success": true});
        let invalid_payload = serde_json::to_vec(&invalid_payload).expect("invalid payload JSON");
        assert!(
            decode_envelope::<SearchResponse>(
                &invalid_payload,
                CacheOperation::Search,
                "firecrawl",
            )
            .is_err()
        );
        assert!(
            decode_envelope::<SearchResponse>(
                b"not valid JSON",
                CacheOperation::Search,
                "firecrawl",
            )
            .is_err()
        );
    }

    #[test]
    fn freshness_rejects_future_and_expires_at_ttl_boundary() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let recent = SystemTime::UNIX_EPOCH + Duration::from_secs(95);
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(90);
        let future = SystemTime::UNIX_EPOCH + Duration::from_secs(101);

        assert_eq!(
            cache_freshness(recent, now, Duration::from_secs(10)),
            Freshness::Fresh
        );
        assert_eq!(
            cache_freshness(old, now, Duration::from_secs(10)),
            Freshness::Expired
        );
        assert_eq!(
            cache_freshness(future, now, Duration::from_secs(10)),
            Freshness::Invalid
        );
    }

    #[test]
    fn object_size_classification_is_bounded_and_sanitized() {
        assert_eq!(object_size_class(None), ObjectSizeClass::Unknown);
        assert_eq!(object_size_class(Some(0)), ObjectSizeClass::Empty);
        assert_eq!(
            object_size_class(Some(MAX_CACHE_OBJECT_BYTES)),
            ObjectSizeClass::Large
        );
        assert_eq!(
            object_size_class(Some(MAX_CACHE_OBJECT_BYTES + 1)),
            ObjectSizeClass::Oversized
        );
    }

    #[test]
    fn tavily_is_allowed_by_cache_provider_allowlist() {
        assert_eq!(sanitized_provider("tavily"), Some("tavily"));
    }
}
