# Seshat RustFS Response Cache Implementation Plan

> **For Hermes workers:** Use a fresh `delegate_task` per implementation task with strict TDD. After each task, verify its exact diff and listed checks before starting the next task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in, shared RustFS response cache to Seshat `/v2/search` and `/v2/scrape` without changing the existing Firecrawl-compatible API or validation boundaries.

**Architecture:** Add one concrete `CacheStore` backed by the AWS Rust S3 SDK and an optional `AppState.cache`. Routes authenticate and normalize requests first, derive an opaque SHA-256 key, read a fresh envelope from RustFS, and otherwise call the existing provider before best-effort storage. Freshness is calculated from the RustFS `Last-Modified` timestamp plus the current operation TTL; cache failures fail open.

**Tech Stack:** Rust 2024, Axum 0.8, Tokio, `aws-sdk-s3` with Rustls and path-style addressing, Serde JSON, `sha2`, `hex`, RustFS S3-compatible API, existing Firecrawl/Brave providers.

**Spec:** `docs/superpowers/specs/2026-09-01-seshat-response-cache-design.md`

## Global Constraints

- Cache is disabled by default with `SESHAT_CACHE_ENABLED=false`; disabling it must preserve the current direct-provider path.
- Search TTL defaults to `600` seconds and scrape TTL defaults to `86400` seconds; both are positive unsigned runtime values.
- Freshness is `Last-Modified + current operation TTL`; the cache envelope must not contain `expires_at`.
- Cache `GetObject` and `PutObject` failures fail open; expired content is never served as stale fallback.
- Cache I/O has a separate one-second timeout; the existing provider request timeout remains fifteen seconds.
- Concurrent misses may duplicate provider calls; the last validated write wins. Do not add a queue, database, distributed lock, invalidation API, or stale-while-revalidate path.
- Authentication and request validation happen before cache access. Cached scrape data must pass the existing response-size and source-URL validation before return.
- Cache only validated successful responses. Never cache authentication failures, caller errors, provider errors, malformed responses, arbitrary headers, or credentials.
- Cache keys use deterministic secret-free JSON and SHA-256 under `cache/v1`; raw queries, URLs, bearer tokens, upstream keys, and caller headers never become keys, metadata, logs, or metrics.
- RustFS is used only through its S3 data plane with runtime-injected endpoint, region, access key, and secret key; application code uses only `GetObject` and `PutObject`.
- When enabled, endpoint, bucket, region, and both credentials are required explicitly; only search/scrape TTLs have application defaults.
- Object reads are bounded by `MAX_UPSTREAM_BODY_BYTES`; no unbounded `ByteStream::collect()` is permitted.
- Extend existing Rust/unit and contract tests; do not add a committed test file for RustFS. Use a disposable RustFS bucket and counting mock upstream for the manual integration matrix.
- The RustFS bucket, dedicated identity, prefix policy, ExternalSecret wiring, and lifecycle rule belong to a separate `dev-infra` change and must not be added to this Seshat repository plan.

---

## File Map

### Modify

- `Cargo.toml` — add `hex`/`sha2` with key generation and `aws-sdk-s3` with the concrete store; keep additions scoped to the task that first uses each dependency.
- `Cargo.lock` — record only those direct dependency additions and their required transitive graph.
- `src/lib.rs` — expose the new `cache` module.
- `src/config.rs` — parse opt-in cache settings, TTLs, RustFS endpoint, bucket, region, and secret-backed credentials; keep secrets out of `Debug`.
- `src/routes.rs` — attach an optional `CacheStore` to `AppState` and add cache-aside behavior to search and scrape after existing validation.
- `tests/config_contract.rs` — cover disabled defaults, complete cache configuration, TTL parsing, and secret-safe diagnostics.
- `tests/http_contract.rs` — make the direct-provider baseline explicitly run with cache disabled.
- `README.md` — document cache settings, freshness, failure behavior, rollback, and the deployment boundary.

### Create

- `src/cache.rs` — own cache operation identifiers, canonical key generation, envelope validation, freshness calculation, S3 client construction, bounded reads, best-effort writes, and cache outcome logging. Keep this as one concrete module; do not introduce a trait or factory.

### Do not modify

- `src/providers/firecrawl.rs` and `src/providers/brave.rs` — provider routing and key-pool behavior remain unchanged.
- `src/error.rs` — cache outcomes never become new public API errors; reuse the existing provider errors for live calls.
- `src/main.rs` — `AppState::new` remains synchronous by constructing the S3 client from the generated SDK builder without async config loading.
- `.github/workflows/ci.yml` and `Dockerfile` — existing checks and non-root image flow already cover the new Cargo dependency graph.
- Hermes configuration, Firecrawl-compatible route schemas, and any `dev-infra` checkout.

---

## Task 1: Add validated opt-in cache configuration

**Files:**

- Modify: `src/config.rs`
- Modify: `tests/config_contract.rs`

**Interfaces:**

- Consumes: existing `BTreeMap<String, String>` input used by `Config::from_env_values`.
- Produces: `pub struct CacheConfig`, `Config::cache() -> Option<&CacheConfig>`, `CacheConfig::endpoint() -> &Url`, `CacheConfig::bucket() -> &str`, `CacheConfig::region() -> &str`, `CacheConfig::search_ttl() -> Duration`, `CacheConfig::scrape_ttl() -> Duration`, and crate-visible credential accessors used only by `CacheStore`.

- [ ] **Step 1: Write the failing configuration contract tests**

Append tests to the existing `tests/config_contract.rs` using the file's current `vars` helper. The credential strings below are synthetic test sentinels, not deployment values.

```rust
use std::time::Duration;

#[test]
fn cache_is_disabled_without_storage_settings() {
    let config = Config::from_env_values(&vars(&[
        ("SESHAT_TOKEN", "auth"),
        ("FIRECRAWL_API_KEYS", "alpha"),
    ]))
    .expect("cache-disabled config should load");

    assert!(config.cache().is_none());
}

#[test]
fn enabled_cache_loads_endpoint_bucket_region_credentials_and_ttls() {
    let config = Config::from_env_values(&vars(&[
        ("SESHAT_TOKEN", "auth"),
        ("FIRECRAWL_API_KEYS", "alpha"),
        ("SESHAT_CACHE_ENABLED", "true"),
        ("SESHAT_CACHE_S3_ENDPOINT", "http://rustfs.test:9000"),
        ("SESHAT_CACHE_S3_BUCKET", "seshat-cache"),
        ("SESHAT_CACHE_S3_REGION", "us-east-1"),
        ("SESHAT_CACHE_S3_ACCESS_KEY_ID", "synthetic-access"),
        ("SESHAT_CACHE_S3_SECRET_ACCESS_KEY", "synthetic-secret"),
        ("SESHAT_SEARCH_CACHE_TTL_SECS", "7"),
        ("SESHAT_SCRAPE_CACHE_TTL_SECS", "11"),
    ]))
    .expect("complete cache config should load");

    let cache = config.cache().expect("cache should be enabled");
    assert_eq!(cache.endpoint().as_str(), "http://rustfs.test:9000/");
    assert_eq!(cache.bucket(), "seshat-cache");
    assert_eq!(cache.region(), "us-east-1");
    assert_eq!(cache.search_ttl(), Duration::from_secs(7));
    assert_eq!(cache.scrape_ttl(), Duration::from_secs(11));
    let debug = format!("{config:?}");
    assert!(!debug.contains("synthetic-access"));
    assert!(!debug.contains("synthetic-secret"));
}

#[test]
fn invalid_cache_values_fail_without_echoing_secret_or_raw_value() {
    for ttl in ["0", "not-a-number"] {
        let error = Config::from_env_values(&vars(&[
            ("SESHAT_TOKEN", "auth"),
            ("FIRECRAWL_API_KEYS", "alpha"),
            ("SESHAT_CACHE_ENABLED", "true"),
            ("SESHAT_CACHE_S3_ENDPOINT", "http://rustfs.test:9000"),
            ("SESHAT_CACHE_S3_BUCKET", "seshat-cache"),
            ("SESHAT_CACHE_S3_REGION", "us-east-1"),
            ("SESHAT_CACHE_S3_ACCESS_KEY_ID", "synthetic-access"),
            ("SESHAT_CACHE_S3_SECRET_ACCESS_KEY", "synthetic-secret"),
            ("SESHAT_SEARCH_CACHE_TTL_SECS", ttl),
        ]))
        .expect_err("invalid TTL must fail");

        assert_eq!(error.code(), "invalid_configuration");
        assert!(!error.to_string().contains(ttl));
        assert!(!error.to_string().contains("synthetic-secret"));
    }
}
```

Run:

```bash
cargo test --test config_contract cache -- --nocapture
```

Expected: FAIL because `CacheConfig`, `Config::cache`, and the cache fields do not exist yet.

Extend the focused tests with one default-TTL case (enabled cache without TTL variables yields `600` and `86400` seconds) and table-driven invalid cases for the boolean flag, each required RustFS field, endpoint/bucket validation, zero/non-numeric/overflowing search and scrape TTLs. Assert the stable configuration code and that no supplied secret or raw invalid value is echoed.

- [ ] **Step 2: Implement `CacheConfig` and parsing**

In `src/config.rs`:

1. Add `use std::time::Duration`.
2. Add constants `DEFAULT_SEARCH_CACHE_TTL_SECS: u64 = 600` and `DEFAULT_SCRAPE_CACHE_TTL_SECS: u64 = 86_400`.
3. Add a public `CacheConfig` with private fields for `Url`, bucket, region, access key ID, secret access key, and the two `Duration` values.
4. Implement a manual `Debug` implementation that prints endpoint, bucket, region, and TTLs, plus the literal marker `[REDACTED]` for credentials. Never print either credential value.
5. Add `cache: Option<CacheConfig>` to `Config`, include only the redacted `CacheConfig` in `Config` debug output, and add `pub fn cache(&self) -> Option<&CacheConfig>`.
6. Add non-secret public accessors and crate-visible credential accessors. Credential accessors must return the original strings without trimming or logging.
7. Add `parse_cache_config(vars)` called from `Config::from_env_values` after the existing upstream settings are parsed:
   - missing `SESHAT_CACHE_ENABLED` means `false`;
   - parse the flag with `str::parse::<bool>()`, returning `ConfigError::Invalid` for any other value;
   - return `None` when false without requiring any RustFS fields;
   - when true, require non-empty endpoint, bucket, region, access key ID, and secret access key;
   - reuse the existing safe URL parser for an `http` or `https` endpoint with no userinfo, query, or fragment;
   - validate bucket length `3..=63`, lowercase ASCII letters/digits/dot/hyphen only, alphanumeric first and last characters, and no adjacent dots or dot-hyphen pair;
   - parse search and scrape TTLs as `u64`, use defaults when absent, and reject zero, non-numeric, or overflowing values with `ConfigError::Invalid`;
   - do not include raw environment values in any error.
8. Add `SearchUpstream::as_str() -> &'static str` returning exactly `"firecrawl"` or `"brave"`; this is the provider identifier used by the key and envelope.
9. Leave `Config::is_ready()` based on token and provider pools only. RustFS availability must not make `/readyz` fail.

Use the existing `ConfigError::Invalid` variant so no public error schema changes.

- [ ] **Step 3: Run the focused configuration tests**

```bash
cargo fmt --check
cargo test --test config_contract cache -- --nocapture
cargo test --test config_contract
```

Expected: PASS, with no synthetic credential sentinel present in formatted configuration diagnostics.

- [ ] **Step 4: Commit the configuration slice**

```bash
git add src/config.rs tests/config_contract.rs
git commit -m "feat: add opt-in response cache configuration"
```

---

## Task 2: Implement deterministic keys, envelopes, and freshness logic

**Files:**

- Create: `src/cache.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**

- Consumes: `SearchInput`, `ScrapeInput`, `SearchResponse`, `ScrapeResponse`, and provider identifiers from `SearchUpstream::as_str()`.
- Produces: crate-visible `CacheOperation`, `search_cache_key`, `scrape_cache_key`, `encode_envelope`, `decode_envelope`, and `cache_freshness` for the S3 implementation and routes; cache lookup uses standard `Option<T>`.

- [ ] **Step 1: Add hashing dependencies and failing pure unit tests**

Add only `hex = "0.4"` and `sha2 = "0.11"` to `Cargo.toml`, add `pub mod cache;` to `src/lib.rs`, resolve `Cargo.lock`, then create `src/cache.rs` with a test module. Run the test target and confirm it fails because the referenced cache functions are not implemented yet; do not accept a “0 tests” result. The tests must not construct an S3 client or make a network call.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ScrapeInput, SearchInput, SearchResponse};
    use std::time::{Duration, SystemTime};

    #[test]
    fn search_key_is_deterministic_opaque_and_provider_scoped() {
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
        assert!(first.starts_with("cache/v1/search/firecrawl/"));
        assert!(first.ends_with(".json"));
        assert_eq!(first.rsplit('/').next().expect("digest").len(), 69);
        assert!(!first.contains("rust cache"));
    }

    #[test]
    fn scrape_key_treats_formats_as_an_order_independent_set() {
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
        assert_ne!(
            scrape_cache_key(&markdown_html, "firecrawl"),
            scrape_cache_key(&markdown_html, "brave")
        );
    }

    #[test]
    fn envelope_round_trip_requires_version_operation_and_provider() {
        let response = SearchResponse {
            success: true,
            data: crate::providers::SearchData { web: Vec::new() },
        };
        let bytes = encode_envelope(CacheOperation::Search, "firecrawl", &response)
            .expect("envelope should serialize");
        let decoded: SearchResponse = decode_envelope(
            &bytes,
            CacheOperation::Search,
            "firecrawl",
        )
        .expect("matching envelope should decode");
        assert!(decoded.success);

        let mut wrong_version: serde_json::Value =
            serde_json::from_slice(&bytes).expect("envelope JSON");
        wrong_version["envelope_version"] = serde_json::json!(2);
        let wrong_version = serde_json::to_vec(&wrong_version).expect("wrong version JSON");
        assert!(decode_envelope::<SearchResponse>(
            &wrong_version,
            CacheOperation::Search,
            "firecrawl",
        )
        .is_err());
    }

    #[test]
    fn freshness_rejects_future_and_expired_objects() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let recent = SystemTime::UNIX_EPOCH + Duration::from_secs(95);
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(90);
        let future = SystemTime::UNIX_EPOCH + Duration::from_secs(101);

        assert_eq!(cache_freshness(recent, now, Duration::from_secs(10)), Freshness::Fresh);
        assert_eq!(cache_freshness(old, now, Duration::from_secs(10)), Freshness::Expired);
        assert_eq!(cache_freshness(future, now, Duration::from_secs(10)), Freshness::Invalid);
    }
}
```

Run:

```bash
cargo test cache::tests -- --nocapture
```

Expected: FAIL because the module and pure functions do not exist.

Before implementing, extend the envelope test with malformed JSON and independent wrong-operation and wrong-provider mutations; each must fail closed. Keep the test pure and network-free.

- [ ] **Step 2: Implement canonical key functions**

In `src/cache.rs`, define:

```rust
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

```

Use fixed-order Serde structs for canonical input. Search fields are `cache_version`, `operation`, `provider`, `query`, and `limit`. Scrape fields are `cache_version`, `operation`, `provider`, `url`, and `formats`. Serialize with `serde_json::to_vec`, hash with `sha2::Sha256`, hex encode with `hex::encode`, and return exactly:

```text
cache/v1/search/ + provider identifier + "/" + 64-character lowercase hexadecimal digest + ".json"
cache/v1/scrape/ + provider identifier + "/" + 64-character lowercase hexadecimal digest + ".json"
```

The provider path segment is selected only from the internal `firecrawl`/`brave` identifiers. In `scrape_cache_key`, clone and `sort_unstable()` the already validated format vector before serialization so input order cannot create duplicate semantic entries. Do not put raw query or URL text in the returned key.

- [ ] **Step 3: Implement envelope and freshness helpers**

Define a generic envelope with no timestamp fields:

```rust
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
```

`decode_envelope` must deserialize the expected generic type, compare `envelope_version`, `operation`, and `provider` exactly, and return an error for any mismatch or malformed JSON. It must not accept unknown operation/provider values merely because the payload shape is valid.

Implement `cache_freshness(modified, now, ttl)` with `SystemTime::duration_since`: a negative duration is `Invalid`, an age greater than or equal to the current TTL is `Expired`, and an age less than the TTL is `Fresh`. The runtime path will pass the operation's current configured TTL, not a timestamp read from the envelope.

- [ ] **Step 4: Run the pure cache tests and formatting**

```bash
cargo fmt --check
cargo test cache::tests -- --nocapture
```

Expected: PASS, including deterministic provider-aware keys, order-independent scrape formats, envelope rejection, and future/expired timestamp handling.

- [ ] **Step 5: Commit the pure cache slice**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/cache.rs
git commit -m "feat: add response cache keys and envelopes"
```

---

## Task 3: Add the bounded RustFS S3 `CacheStore`

**Files:**

- Modify: `src/cache.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**

- Consumes: `CacheConfig`, `CacheOperation`, `encode_envelope`, `decode_envelope`, and `cache_freshness`.
- Produces: `CacheStore::new(&CacheConfig) -> CacheStore`, `CacheStore::get<T>(&self, CacheOperation, &str, &str) -> Option<T>`, and `CacheStore::put<T>(&self, CacheOperation, &str, &str, &T)`.

- [ ] **Step 1: Add the S3 SDK and construct the client with explicit RustFS settings**

Add `aws-sdk-s3 = { version = "1", features = ["behavior-version-latest"] }` and resolve `Cargo.lock`. Do not add `aws-config`; the generated S3 config builder is constructed synchronously with explicit credentials.

Add these fields to `CacheStore`:

```rust
#[derive(Clone)]
pub(crate) struct CacheStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    search_ttl: std::time::Duration,
    scrape_ttl: std::time::Duration,
}
```

Use the generated SDK builder so `CacheStore::new` remains synchronous and does not load ambient AWS configuration:

```rust
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
    .retry_config(
        aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(1),
    )
    .build();
let client = aws_sdk_s3::Client::from_conf(s3_config);
```

Set `CACHE_IO_TIMEOUT` to `Duration::from_secs(1)` and `MAX_CACHE_OBJECT_BYTES` to `MAX_UPSTREAM_BODY_BYTES`. Do not use `HeadObject`, `ListObjects`, `DeleteObject`, bucket creation, or administrative RustFS APIs.

- [ ] **Step 2: Implement bounded cache reads**

Implement:

```rust
pub(crate) async fn get<T>(
    &self,
    operation: CacheOperation,
    provider: &str,
    key: &str,
) -> Option<T>
where
    T: serde::de::DeserializeOwned,
```

Wrap the entire `GetObject` request and body stream in `tokio::time::timeout(CACHE_IO_TIMEOUT, ...)`. The read sequence is:

1. call `self.client.get_object().bucket(&self.bucket).key(key).send()`;
2. classify `NoSuchKey` or HTTP 404 as a missing result; classify all other SDK, timeout, and transport failures as unavailable without logging the raw SDK error;
3. require `output.last_modified`; convert the SDK `DateTime` with `SystemTime::try_from`; missing or conversion failure is `Invalid`;
4. compare `Last-Modified` to `SystemTime::now()` using `cache_freshness` and the current TTL for `operation`; future is `Invalid`, age at/above TTL is `Expired`; return before consuming the body for an expired object;
5. reject negative or over-limit `content_length` values;
6. consume `output.body` with the SDK's bounded `try_next()` loop, checking `body.len().saturating_add(chunk.len())` before every append; stop with `Invalid` when the cap would be exceeded and `Unavailable` on stream failure;
7. call `decode_envelope` with the expected operation and provider; malformed, wrong-version, wrong-operation, wrong-provider, or wrong-payload JSON is `Invalid`;
8. return `Some(payload)` only after all checks pass; return `None` for every miss, invalid object, or unavailable cache outcome.

The cache module must never log the object key, body, URL, query, credential, authorization header, or SDK error text.

- [ ] **Step 3: Implement best-effort cache writes**

Implement:

```rust
pub(crate) async fn put<T>(
    &self,
    operation: CacheOperation,
    provider: &str,
    key: &str,
    payload: &T,
)
where
    T: serde::Serialize,
```

Serialize with `encode_envelope`. If serialization fails or the encoded body exceeds `MAX_CACHE_OBJECT_BYTES`, emit a sanitized warning and return. Otherwise wrap this request in the same one-second timeout:

```rust
self.client
    .put_object()
    .bucket(&self.bucket)
    .key(key)
    .content_type("application/json")
    .body(aws_sdk_s3::primitives::ByteStream::from(body))
    .send()
    .await;
```

Do not set `Cache-Control` as a freshness mechanism and do not add an `expires_at` envelope field. Ignore all PUT failures for the caller response, but emit a warning with only operation, provider, `cache_outcome`, object-size class, and latency.

- [ ] **Step 4: Add sanitized cache outcome logging**

Add one internal logging helper used by both GET and PUT. Allowed fields are:

```text
operation: search|scrape
provider: firecrawl|brave
cache_outcome: hit|missing|expired|invalid|unavailable|stored|write_failed
object_size_class: empty|small|large|oversized|unknown
latency_ms: integer duration
```

Use `tracing::debug!` for ordinary miss/hit/stored outcomes and `tracing::warn!` for unavailable/invalid/write-failed outcomes. Never include the key or serialized response.

- [ ] **Step 5: Compile and run the focused checks**

```bash
cargo fmt --check
cargo check --all-targets
cargo test cache::tests -- --nocapture
```

Expected: PASS. Inspect source and `cargo tree` to confirm `aws-sdk-s3` is the only new storage dependency and that no cache implementation uses unbounded body collection.

- [ ] **Step 6: Commit the S3 cache slice**

```bash
git add src/cache.rs Cargo.toml Cargo.lock
git commit -m "feat: add RustFS-backed response cache store"
```

---

## Task 4: Wire cache-aside behavior into `AppState`, search, and scrape

**Files:**

- Modify: `src/routes.rs`
- Modify: `tests/http_contract.rs`

**Interfaces:**

- Consumes: `CacheStore`, `CacheOperation`, `search_cache_key`, `scrape_cache_key`, `SearchUpstream::as_str`, and the existing provider/response validators.
- Produces: `AppState { cache: Option<CacheStore> }` while keeping the public HTTP routes and response JSON unchanged.

- [ ] **Step 1: Make the existing HTTP baseline explicit about disabled cache**

In the existing `tests/http_contract.rs::config` helper, add:

```rust
("SESHAT_CACHE_ENABLED".to_owned(), "false".to_owned()),
```

Keep all existing health, readiness, auth, invalid-input, payload-limit, and upstream caller-error assertions. The test suite must continue to prove that cache-disabled requests perform exactly the current upstream calls.

- [ ] **Step 2: Add optional cache state without changing readiness**

Import the cache module in `src/routes.rs`, add `cache: Option<CacheStore>` to `AppState`, and construct it in `AppState::new`:

```rust
let cache = config.cache().map(CacheStore::new);
Self {
    token: Arc::from(config.token().to_owned()),
    search_upstream: config.search_upstream(),
    firecrawl,
    brave,
    cache,
    ready: config.is_ready(),
}
```

Do not make `AppState::new` async. Do not probe RustFS during construction or readiness. With cache disabled, `cache` is `None` and no S3 request can occur.

- [ ] **Step 3: Insert the search cache lookup after auth and normalization**

In `search`, retain the current order through `require_auth`, body decode, and `normalize_search`. Then use the selected provider identifier and key:

```rust
let provider = state.search_upstream.as_str();
let cache_key = search_cache_key(&input, provider);
if let Some(cache) = state.cache.as_ref() {
    if let Some(response) = cache
        .get::<SearchResponse>(CacheOperation::Search, provider, &cache_key)
        .await
    {
        if response.success {
            return Ok(Json(response));
        }
        tracing::warn!(operation = "search", provider, cache_outcome = "invalid", "cache payload rejected");
    }
}
```

Then execute the existing fifteen-second provider timeout unchanged. After the provider returns a successful `SearchResponse`, call `cache.put(CacheOperation::Search, provider, &cache_key, &response).await` if enabled, and return the provider response regardless of PUT result. Do not put the cache lookup inside the provider key-pool retry loop.

- [ ] **Step 4: Insert the scrape cache lookup after all existing URL validation**

In `scrape`, retain auth, JSON parsing, `normalize_scrape`, DNS/private-address checks, credential-query checks, and format checks before deriving the key:

```rust
let provider = "firecrawl";
let cache_key = scrape_cache_key(&input, provider);
if let Some(cache) = state.cache.as_ref() {
    if let Some(response) = cache
        .get::<ScrapeResponse>(CacheOperation::Scrape, provider, &cache_key)
        .await
    {
        if response.success && validate_scrape_response(&response).await.is_ok() {
            return Ok(Json(response));
        }
        tracing::warn!(operation = "scrape", provider, cache_outcome = "invalid", "cache payload rejected");
    }
}
```

Call the existing Firecrawl provider timeout on every miss/expired/invalid/unavailable outcome. Only after `validate_scrape_response(&response).await?` succeeds may the route call `cache.put(CacheOperation::Scrape, provider, &cache_key, &response).await`. Never serve an expired object when Firecrawl fails; propagate the existing provider error.

- [ ] **Step 5: Preserve format and response semantics**

Do not change the accepted `markdown`/`html` set, response fields, provider routing, or SSRF checks. `scrape_cache_key` canonicalizes format order only for the key; the provider receives the same normalized `ScrapeInput` semantics as before. Do not cache `/healthz`, `/readyz`, unauthorized requests, invalid requests, or provider failures.

- [ ] **Step 6: Run the route and regression tests**

```bash
cargo fmt --check
cargo test --all-targets
python -m pytest tests -q
```

Expected: PASS. Existing contract tests must still show no upstream call for auth/validation failures and the same direct-provider behavior with cache explicitly disabled.

- [ ] **Step 7: Commit route integration**

```bash
git add src/routes.rs tests/http_contract.rs
git commit -m "feat: add cache-aside flow to search and scrape"
```

---

## Task 5: Document runtime configuration and the deployment handoff

**Files:**

- Modify: `README.md`

**Interfaces:**

- Consumes: the environment names and semantics implemented in Tasks 1–4.
- Produces: an operator-readable Seshat runtime contract without credentials or deployment manifests.

- [ ] **Step 1: Add a cache configuration section**

Document the following non-secret settings and defaults exactly:

```text
SESHAT_CACHE_ENABLED=false
SESHAT_CACHE_S3_ENDPOINT=http://rustfs.rustfs.svc.cluster.local:9000
SESHAT_CACHE_S3_BUCKET=seshat-cache
SESHAT_CACHE_S3_REGION=us-east-1
SESHAT_SEARCH_CACHE_TTL_SECS=600
SESHAT_SCRAPE_CACHE_TTL_SECS=86400
```

List `SESHAT_CACHE_S3_ACCESS_KEY_ID` and `SESHAT_CACHE_S3_SECRET_ACCESS_KEY` by name as secret-manager-injected values, without sample values. State that enabled mode requires all RustFS settings and that TTLs must be positive unsigned seconds.
Clarify that the endpoint, bucket, and region shown above are the current Monster deployment values, not parser fallbacks; enabled mode requires them explicitly, while only the two TTLs default in application code.

- [ ] **Step 2: Document behavior and rollback**

State that:

- a fresh equivalent search or scrape response avoids the provider call;
- freshness is `Last-Modified` plus the current per-operation TTL;
- the envelope is versioned but has no `expires_at`;
- RustFS read/write failure falls through or warns without changing a successful provider response;
- expired content is not a stale fallback;
- duplicate concurrent misses are allowed;
- the cache key is opaque under `cache/v1` and provider-aware;
- lifecycle is storage cleanup only; if an operation TTL is configured beyond seven days, update the lifecycle retention in the same deployment change;
- disabling `SESHAT_CACHE_ENABLED` returns Seshat to the current direct-provider path and leaves old objects for lifecycle cleanup.

- [ ] **Step 3: Record the external deployment boundary**

Add a short operator note that this repository does not create buckets or credentials. A separate `dev-infra` change must provision:

- a dedicated `seshat-cache` bucket;
- a Seshat-specific non-root RustFS identity, separate from RustFS root credentials and OpenViking credentials;
- `GetObject` and `PutObject` only under `cache/v1/`;
- secret-backed injection of the two cache credential environment variables;
- a seven-day lifecycle rule for `cache/v1/`;
- an unversioned bucket, or matching noncurrent-version expiration when bucket versioning is enabled;
- the current Monster endpoint `http://rustfs.rustfs.svc.cluster.local:9000`, region `us-east-1`, and path-style addressing.

Explicitly state that cache remains disabled until the separate deployment change has passed bucket, policy, secret metadata, and lifecycle readback. Do not document Vault property names until the approved deployment contract supplies them.

- [ ] **Step 4: Check documentation and commit it**

```bash
git diff --check
git add README.md
git commit -m "docs: document RustFS response cache runtime contract"
```

The README review must contain no credential values, bearer headers, access-key values, secret-key values, or unfinished markers.

---

## Task 6: Run repository checks and the disposable RustFS smoke matrix

**Files:** none committed by this task; use only disposable local processes/files and an explicitly approved disposable RustFS bucket.

**Boundary:** This is verification, not deployment. Keep credentials in injected process environment/memory; never put them in command arguments, fixtures, repository files, logs, or reports. If the approved RustFS endpoint or secret injection is unavailable, record those RustFS cases as `UNVERIFIED` rather than substituting a guessed pass.

- [ ] **Step 1: Run repository-native gates from a clean worktree**

```bash
git status --short --branch
git diff --check
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
python -m pytest tests -q
docker build --pull=false --tag seshat:cache-check .
```

Confirm the image uses the existing non-root release flow and that no unrelated files are dirty.

- [ ] **Step 2: Execute the required cache matrix**

Run a loopback-only counting Firecrawl-compatible mock outside the repository and a private disposable RustFS bucket through the approved admin path. Read back the lifecycle configuration before cleanup. Verify, as separate evidence:

1. disabled-cache direct-provider baseline;
2. authenticated search/scrape miss → one provider call → one object, then fresh hit → no additional call;
3. query, limit, URL, format-set, provider, and cache-version key separation;
4. reverse-order scrape formats, process restart, and second-instance sharing;
5. one-second current-TTL expiry and changed-TTL readback without rewriting;
6. missing/future `Last-Modified`, malformed/wrong-version/wrong-operation/wrong-provider/oversized objects → provider fallback;
7. RustFS read/write failure, provider caller/error/timeout/malformed responses, and expired-object-plus-provider-failure → existing API behavior with no stale fallback or error write;
8. auth/SSRF/response validation before cache access and sanitized logs without secrets, raw requests, object keys, bodies, or SDK error text;
9. path-style, SigV4, bounded `GetObject`, prefix-only runtime permission, and enabled seven-day `cache/v1/` lifecycle readback.

Use only the existing successful compatibility fixtures for the mock response. Stop processes and remove only the disposable bucket/prefix after evidence capture; lifecycle deletion is asynchronous.

- [ ] **Step 3: Record separate evidence**

Record the exact commit and clean status, each local gate result, cache-disabled regression, cache behavior matrix, RustFS data-plane/permission/lifecycle readbacks, and every unavailable live gate as `UNVERIFIED`. No deployment rollout is part of this plan.

---

## Separate `dev-infra` follow-up boundary

The approved spec includes deployment provisioning, but the current Seshat repository has no Kubernetes manifests and the exact Vault property contract is not approved. After this Seshat plan is accepted, create a separate plan from fresh `dev-infra/main` for the RustFS bucket/identity/policy/lifecycle and Seshat workload wiring. That plan must first re-read the live Monster RustFS Service, current ExternalSecret conventions, the approved Vault metadata-only property names, and the target Seshat deployment chart path. It must not reuse RustFS root credentials, print secret data, or enable `SESHAT_CACHE_ENABLED` before exact secret and policy readback.

## Rollback

Set `SESHAT_CACHE_ENABLED=false` and remove cache runtime settings from the workload. The route returns to the direct provider path; no cache object deletion is required because lifecycle cleanup can reclaim the `cache/v1/` prefix.

## Plan self-review

- **Spec coverage:** Tasks 1–2 cover configuration, deterministic keys, envelope validation, and current-TTL freshness. Task 3 covers bounded RustFS data-plane access, timeouts, fail-open writes, and sanitized logs. Task 4 covers both routes, auth/validation ordering, response validation, and no stale fallback. Task 5 covers runtime documentation and rollback. Task 6 covers the required local, RustFS, mock-provider, expiry, failure, restart, lifecycle, and secret-safety checks. The separate deployment boundary covers the bucket, dedicated identity, prefix policy, ExternalSecret, and lifecycle handoff without crossing repositories.
- **Completeness scan:** no incomplete implementation marker or unfinished configuration value is used in the plan. Runtime secrets are explicitly supplied out-of-band and never represented as literals.
- **Type consistency:** `CacheOperation`, `CacheStore::get`, `CacheStore::put`, `search_cache_key`, `scrape_cache_key`, and `CacheConfig` are defined before later tasks consume them; lookup uses standard `Option<T>`. `SearchResponse` and `ScrapeResponse` retain their existing provider module types.
