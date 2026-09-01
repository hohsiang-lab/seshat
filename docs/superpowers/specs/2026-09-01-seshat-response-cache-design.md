# Seshat RustFS durable response cache design

- **Status:** Approved design
- **Repository:** `hohsiang-lab/seshat`
- **Scope:** Seshat `/v2/search` and `/v2/scrape`
- **Date:** 2026-09-01

## Decision summary

Seshat will use RustFS as a shared, durable **response cache** through its
S3-compatible API. The cache is cache-aside and opt-in. A fresh object is
returned without calling the provider; a miss or expired object calls the
configured provider and best-effort writes the validated response back to
RustFS.

The cache policy is:

- search TTL: `600` seconds by default;
- scrape TTL: `86400` seconds by default;
- both TTLs are runtime-configurable;
- RustFS read/write failures fail open and do not change the caller response;
- expired entries are never served as stale fallback;
- concurrent misses may call the provider more than once; the last valid write
  wins.

For this Seshat-specific cache, freshness is determined from the S3 object
`Last-Modified` value plus the current operation TTL. The object envelope still
contains version and response-type metadata, but does not duplicate an
`expires_at` field. This keeps the MVP small and makes a TTL configuration
change apply immediately to existing objects.

## Existing constraints

Seshat currently has:

- `POST /v2/search` and `POST /v2/scrape` routes in `src/routes.rs`;
- normalized `SearchInput` and `ScrapeInput` values in `src/providers/mod.rs`;
- Firecrawl and optional Brave providers with bounded retry and key cooldown;
- a 15-second route timeout, upstream body limit, and scrape response-size
  validation;
- bearer authentication and SSRF-related URL validation before provider calls;
- no cache, database, object-storage client, or shared request state;
- a clean `main` branch with no existing cache contract.

The current Firecrawl-compatible HTTP contract remains unchanged. Hermes does
not need a new provider or configuration change.

## Goals

1. Avoid repeated upstream calls for equivalent normalized search and scrape
   requests.
2. Share cache entries across Seshat replicas and process restarts.
3. Preserve the existing provider, authentication, timeout, response validation,
   and secret-redaction boundaries.
4. Keep RustFS unavailable from becoming a request outage.
5. Keep the cache key opaque and deterministic.
6. Let RustFS lifecycle rules reclaim old cache objects without using lifecycle
   timing as response freshness logic.

## Non-goals

- historical archive or replay storage;
- stale-if-error responses;
- a cache administration or invalidation HTTP API;
- Redis, a database, a queue, or a distributed lock;
- per-user cache isolation, because the current Seshat response contract has no
  user-specific request fields;
- changes to Hermes or the Firecrawl-compatible route schema;
- using RustFS administrative APIs or anonymous objects.

## Architecture

Add one concrete `CacheStore` component and keep storage operations outside the
HTTP handlers' implementation details. No trait, factory, queue, or second
storage abstraction is needed for the first implementation.

```text
AppState
  ├─ FirecrawlProvider
  ├─ BraveProvider
  └─ cache: Option<CacheStore>

route
  ├─ authenticate
  ├─ parse and normalize
  ├─ validate request boundary
  ├─ derive opaque cache key
  ├─ CacheStore.get
  │    ├─ fresh and valid: return cached response
  │    └─ missing, expired, invalid, or unavailable: continue
  ├─ call selected provider
  ├─ validate successful provider response
  ├─ CacheStore.put (bounded and best effort)
  └─ return provider response
```

`CacheStore` uses the AWS Rust S3 SDK with an explicitly configured RustFS
endpoint, region, credentials, and path-style addressing. RustFS is an
S3-compatible backend; Seshat does not depend on a RustFS-specific client.

The cache is optional in `AppState`. With cache disabled, the route follows the
current provider path. With cache enabled but RustFS temporarily unavailable,
`readyz` remains ready and the route falls through to the provider.

## Request data flow

### Search

1. Require the existing Seshat bearer token.
2. Parse and normalize `query` and `limit` using the existing limits.
3. Derive a key from the normalized input and selected search provider.
4. Call `GetObject` for the key.
5. Inspect `Last-Modified` before consuming the object body:
   - missing or future timestamp: treat as a miss;
   - age below the configured search TTL: read and validate the envelope;
   - age at or above the TTL: treat as expired.
6. A valid fresh envelope returns its `SearchResponse` without an upstream call.
7. For every other cache result, call the existing Firecrawl or Brave provider
   inside the existing request timeout.
8. After a successful validated response, best-effort `PutObject` and return the
   response regardless of PUT success.

### Scrape

1. Require the existing Seshat bearer token.
2. Parse and run the existing URL, DNS, private-address, credential-query, and
   format validation.
3. Derive a key from the normalized URL, canonical format set, and Firecrawl
   provider identifier.
4. Call `GetObject` and apply the scrape TTL to `Last-Modified`.
5. A fresh object is deserialized and checked with the same content-size and
   source-URL validation used for a live Firecrawl response.
6. For a miss, expired object, invalid object, or cache error, call Firecrawl.
7. Validate the provider response, best-effort `PutObject`, and return it.

Request authentication and validation always happen before cache access. A
cached scrape response cannot bypass SSRF protections.

## Canonical key

Keys use a fixed `cache/v1` prefix and SHA-256 over deterministic, secret-free
JSON. The raw query and URL are never used as the object key.

Search input includes:

```json
{"cache_version":1,"operation":"search","provider":"firecrawl","query":"rust cache","limit":5}
```

Scrape input includes:

```json
{"cache_version":1,"operation":"scrape","provider":"firecrawl","url":"https://example.com/page","formats":["html","markdown"]}
```

The resulting object paths are:

```text
cache/v1/search/<provider>/<sha256>.json
cache/v1/scrape/<provider>/<sha256>.json
```

The provider is included both in the canonical input and the path. This
prevents a Firecrawl response from being reused after switching search routing
to Brave. The supported scrape formats are a semantic set, so they are sorted
after validation before key generation. All fields that can change the result
must be included in the canonical input.

The bearer token, upstream API keys, arbitrary caller headers, and credential-
bearing URLs never enter the key, envelope metadata, logs, or metrics.

## Object envelope

The object body is JSON with a small versioned envelope:

```json
{
  "envelope_version": 1,
  "operation": "scrape",
  "provider": "firecrawl",
  "payload": {
    "success": true,
    "data": {}
  }
}
```

The envelope does not contain `expires_at`. Freshness is the age of the
RustFS object compared with the current operation TTL. The envelope exists to
validate that the object belongs to the expected cache contract and to reject
objects written by a different version or operation.

On a cache hit, Seshat validates:

- `envelope_version`;
- expected operation and provider;
- JSON decoding and response shape;
- the existing response-size limits;
- the existing scrape source-URL and content validation where applicable.

An invalid or oversized object is a miss. Seshat does not return untrusted
stored data merely because its timestamp is recent.

## Freshness and TTL

Configuration names and defaults:

```text
SESHAT_CACHE_ENABLED=false
SESHAT_CACHE_S3_ENDPOINT=http://rustfs.rustfs.svc.cluster.local:9000
SESHAT_CACHE_S3_BUCKET=seshat-cache
SESHAT_CACHE_S3_REGION=us-east-1
SESHAT_CACHE_S3_ACCESS_KEY_ID  # sourced from the deployment secret store
SESHAT_CACHE_S3_SECRET_ACCESS_KEY  # sourced from the deployment secret store
SESHAT_SEARCH_CACHE_TTL_SECS=600
SESHAT_SCRAPE_CACHE_TTL_SECS=86400
```

The endpoint and bucket are non-secret runtime configuration. Access and
secret keys are injected from the deployment secret store. TTL values must be
positive unsigned seconds; zero, non-numeric, and overflowing values are
invalid configuration.

For each `GetObject`, Seshat uses the standard S3 `Last-Modified` value from
the response. A missing or future timestamp is a miss. Otherwise, an object is
fresh when its age is less than the operation TTL. RustFS object timestamps are
appropriate here because the cache bucket is dedicated to Seshat and writes
are restricted to the Seshat cache identity. The envelope remains responsible
for schema/type validation, not freshness.

`Cache-Control` is not used for freshness. It is HTTP cache metadata for clients
and intermediaries; it does not cause Seshat or RustFS to delete or reject the
object. RustFS lifecycle expiration is also not used for freshness.

## RustFS integration and permissions

The cache client uses only the S3 data-plane operations required by the route:

- `GetObject` for lookup and freshness metadata;
- `PutObject` for successful validated provider responses.

`HeadObject` is not required because `GetObject` supplies `Last-Modified` before
the response body needs to be consumed. `ListObject`, `DeleteObject`, bucket
administration, and anonymous access are not part of the application path.

The deployment must provision:

1. a dedicated `seshat-cache` bucket or an equivalent dedicated bucket;
2. a Seshat-specific RustFS service identity, separate from OpenViking and root
   credentials;
3. a policy allowing only `GetObject` and `PutObject` under `cache/v1/`;
4. secret-backed injection of the identity into the Seshat workload;
5. a lifecycle rule for the `cache/v1/` prefix.

The current Monster RustFS endpoint and path-style configuration are compatible
with this client model. Bucket and credential provisioning belongs to the
GitOps/deployment change, not to `routes.rs` or runtime bucket auto-creation.

## Error behavior

Cache errors are internal cache outcomes, not new API errors.

| Condition | Internal result | Caller behavior |
|---|---|---|
| `GetObject` not found | miss | call provider |
| object TTL expired | expired miss | call provider |
| missing/future `Last-Modified` | miss | call provider |
| malformed or wrong-version envelope | invalid miss | call provider |
| cache object exceeds limit | invalid miss | call provider |
| RustFS timeout/connection error | unavailable | call provider |
| RustFS permission or 5xx error | unavailable | call provider |
| provider caller/auth/error response | provider error | preserve existing API error |
| provider malformed response | provider error | preserve existing API error |
| `PutObject` failure after provider success | write warning | return provider success |

There is no stale-if-error path. If the object is expired and the provider
fails, Seshat returns the provider failure rather than serving stale content.
No authentication failure, caller error, provider failure, malformed response,
or unvalidated response is written to the cache.

Cache reads and writes have a separate one-second operation timeout so a
backend outage cannot consume the full 15-second gateway timeout. A cache
failure is logged with operation, provider, outcome, object-size class, and
latency only. Raw query, URL, object body, credentials, and authorization
headers are never logged.

## Lifecycle cleanup

Logical TTL and physical deletion are separate:

```text
Last-Modified + current TTL → whether Seshat serves the object
RustFS lifecycle rule      → when RustFS eventually removes the object
```

RustFS lifecycle cleanup is asynchronous. The initial deployment should apply
a seven-day expiration rule to `cache/v1/`, which is a storage-retention
ceiling rather than a freshness guarantee. If an operator later configures a
TTL longer than seven days, the lifecycle rule must be increased in the same
deployment change; early physical deletion remains safe because it only causes
a cache miss.

The application does not delete each expired object during a request. This
avoids an additional permission, race, and latency path. If bucket versioning
is enabled, noncurrent-version expiration must also be configured; otherwise
the cache bucket should remain unversioned because historical versions are not
needed for response caching.

## Concurrency and invalidation

The MVP intentionally accepts duplicate upstream calls when multiple requests
miss the same key concurrently. A successful validated response can overwrite
the same object, and the last valid write wins. No distributed lock is needed
for correctness.

There is no invalidation API. Versioned key prefixes provide coarse invalidation
when cache semantics change. Changing `cache/v1` to a new version is a reviewed
code/configuration change; old objects are then reclaimed by lifecycle.

## Verification

The repository's existing formatting, lint, Rust test, Python compatibility,
and container checks remain the baseline. Per the repository workflow
preference, cache behavior is verified with a disposable RustFS/cache bucket
and a counting mock upstream through manual CLI smoke checks rather than adding
new test files.

Required checks:

1. With cache disabled, existing route behavior and provider call counts remain
   unchanged.
2. First authenticated search and scrape requests miss, call the provider once,
   and write one object.
3. Repeating the same normalized request within TTL returns the same response
   without another provider call.
4. Changing query, limit, URL, formats, provider, or cache version produces a
   different key.
5. With a disposable one-second TTL, an object is a hit before expiry and a miss
   after expiry; expiry does not require immediate RustFS deletion.
6. A missing, future-timestamp, malformed, wrong-version, or oversized object
   falls through to the provider.
7. RustFS read failure and write failure both fail open.
8. Provider errors and malformed responses are not written to RustFS.
9. A Seshat restart and a second Seshat instance can read an object written by
   the first instance.
10. Cache hits still perform the existing request authentication and scrape URL
    validation.
11. Sanitized logs contain cache outcome and timing but no secret, raw request,
    or cached body.
12. RustFS lifecycle readback with a disposable prefix confirms eventual
    background deletion; verification does not assume an exact deletion time.

The final implementation verification must include at least:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
python -m pytest tests -q
```

RustFS operations must be verified separately against the target RustFS release
for path-style addressing, SigV4 credentials, `GetObject`, `PutObject`,
`Last-Modified`, prefix permissions, and lifecycle behavior. Secret values must
not appear in command arguments, shell history, output, fixtures, or reports.

## Implementation sequence

1. Add validated cache configuration and TTL defaults while preserving the
   current behavior when caching is disabled.
2. Add the concrete S3-backed `CacheStore` with bounded object reads/writes and
   fail-open error handling.
3. Add deterministic key generation and envelope validation.
4. Integrate cache-aside flow into `search` and `scrape` after existing request
   validation and before provider calls.
5. Add sanitized cache outcome logging and document runtime configuration.
6. Add the separate deployment change for the RustFS bucket, dedicated
   credential, prefix policy, ExternalSecret wiring, and lifecycle rule.
7. Run the existing repository checks and the disposable RustFS/mock-provider
   CLI verification matrix.

## Rollback

Disable `SESHAT_CACHE_ENABLED` and remove the cache runtime configuration from
the workload. Seshat then returns to the current direct-provider path without
requiring cache data deletion or a route contract change. Cache objects can be
left for the lifecycle rule to reclaim.

## Acceptance criteria

The design is complete when:

- fresh equivalent requests avoid upstream calls;
- expired or unavailable cache entries fall through without changing API
  availability;
- cache keys are deterministic, opaque, provider-aware, and secret-free;
- cached responses are independently validated before returning;
- the existing Firecrawl-compatible API and SSRF/auth boundaries remain intact;
- RustFS is consumed only through the S3 data plane with a dedicated identity;
- lifecycle cleanup is configured as asynchronous storage cleanup;
- existing checks and the manual RustFS smoke matrix pass;
- disabling the cache provides a clean rollback path.

## References

- RustFS S3 protocol: <https://docs.rustfs.com/administration/protocols/s3/>
- RustFS lifecycle management: <https://docs.rustfs.com/en/administration/data/lifecycle-management>
- AWS S3 object metadata: <https://docs.aws.amazon.com/AmazonS3/latest/userguide/UsingMetadata.html>
- AWS S3 lifecycle expiration: <https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-expire-general-considerations.html>
- AWS SDK for Rust S3: <https://docs.rs/aws-sdk-s3/latest/aws_sdk_s3/>
