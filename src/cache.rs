use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};

use crate::providers::{ScrapeInput, SearchInput};

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
}
