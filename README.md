# Seshat

Seshat is a small Rust gateway that exposes the Firecrawl v2 HTTP subset used by
Hermes. It keeps Hermes on the existing, unmodified Firecrawl provider:

```yaml
web:
  backend: firecrawl
```

Hermes points `FIRECRAWL_API_URL` at Seshat. Hermes' `FIRECRAWL_API_KEY` is the
Seshat bearer token; upstream Firecrawl, Brave, and Tavily credentials stay
inside Seshat and are never sent by Hermes.

## Features

- Firecrawl-compatible `/v2/search` and `/v2/scrape` endpoints, plus health and
  readiness probes.
- Search through Firecrawl (default), Brave, Tavily, or a combined Brave/Tavily
  key pool. Scraping always stays on Firecrawl.
- Provider key pools rotate through eligible keys sequentially and cool down
  retryable failures; combined search never fans out or merges results.
- Optional shared S3-compatible RustFS cache for successful search and scrape
  responses.

## Routing phases

- Phase 1 (`SESHAT_SEARCH_UPSTREAM=firecrawl`, the default): `/v2/search` and
  `/v2/scrape` both use the Firecrawl key pool.
- Phase 2 (`SESHAT_SEARCH_UPSTREAM=brave`): `/v2/search` uses Brave Search and
  `/v2/scrape` continues to use Firecrawl. The two pools never cross-fallback.
- Phase 3 (`SESHAT_SEARCH_UPSTREAM=tavily`): `/v2/search` uses Tavily and
  `/v2/scrape` continues to use Firecrawl. The pools never cross-fallback.
- Phase 4 (`SESHAT_SEARCH_UPSTREAM=brave,tavily` or `tavily,brave`): all Brave
  and Tavily keys form one provider-labelled pool. A normal `/v2/search`
  request selects exactly one eligible `(provider, key)` pair and sends one
  upstream request; retryable failures may advance through other eligible pairs
  sequentially. It never fans out or merges provider results.
Retryable transport errors, `401`, `403`, `408`, `425`, `429`, and `5xx`
advance to another eligible pair; each pair is attempted at most once per request.
Caller errors `400` and `422` return without rotation. Failed pairs enter a
bounded process-local cooldown.

## Configuration

Required for all phases:

```text
SESHAT_TOKEN
FIRECRAWL_API_KEYS_FILE=/run/secrets/firecrawl-keys
```

Optional non-secret settings:

```text
SESHAT_BIND_ADDR=0.0.0.0:8080
SESHAT_SEARCH_UPSTREAM=firecrawl
FIRECRAWL_UPSTREAM_URL=https://api.firecrawl.dev
BRAVE_SEARCH_UPSTREAM_URL=https://api.search.brave.com
TAVILY_SEARCH_UPSTREAM_URL=https://api.tavily.com
```

Phase 2 additionally requires:

```text
BRAVE_SEARCH_API_KEYS_FILE=/run/secrets/brave-keys
```

Phase 3 additionally requires:

```text
SESHAT_SEARCH_UPSTREAM=tavily
TAVILY_SEARCH_API_KEYS_FILE=/run/secrets/tavily-keys
```

Phase 4 additionally requires both search pools and accepts either provider order:

```text
SESHAT_SEARCH_UPSTREAM=tavily,brave
BRAVE_SEARCH_API_KEYS_FILE=/run/secrets/brave-keys
TAVILY_SEARCH_API_KEYS_FILE=/run/secrets/tavily-keys
```

Key files contain one key per line. Blank lines are ignored and duplicate keys
are removed while preserving order. File sources win over the local-only
newline-separated `FIRECRAWL_API_KEYS`, `BRAVE_SEARCH_API_KEYS`, and
`TAVILY_SEARCH_API_KEYS` fallbacks.
Never put credentials in this repository, request payloads, logs, image layers,
workflow configuration, or artifacts.

With Tavily selected, `/v2/search` sends a basic search request with
`max_results`, `include_answer=false`, and `include_raw_content=false`. Tavily
`content` becomes the Firecrawl-compatible `description`; complete page content
still comes from `/v2/scrape` through Firecrawl. Scraping is fixed to Firecrawl
in every phase and never uses the Tavily pool. A Tavily `429` advances to the
next eligible key and applies the normal bounded process-local cooldown;
`432`/`433` usage-limit responses return upstream unavailable without rotating
the key.

## RustFS response cache

The shared response cache is opt-in and disabled by default. These non-secret
settings are the current Monster deployment values and application defaults:

```text
SESHAT_CACHE_ENABLED=false
SESHAT_CACHE_S3_ENDPOINT=http://rustfs.rustfs.svc.cluster.local:9000
SESHAT_CACHE_S3_BUCKET=seshat-cache
SESHAT_CACHE_S3_REGION=us-east-1
SESHAT_SEARCH_CACHE_TTL_SECS=600
SESHAT_SCRAPE_CACHE_TTL_SECS=86400
```

The endpoint, bucket, and region above are current Monster deployment values,
not application parser fallbacks. Enabled mode requires every RustFS setting
explicitly, including the two secret-backed credential variables below; only
the two TTLs default in application code. TTLs are positive unsigned seconds.

The following variables are injected by the secret manager; their values must
not appear in this repository:

- `SESHAT_CACHE_S3_ACCESS_KEY_ID`
- `SESHAT_CACHE_S3_SECRET_ACCESS_KEY`

A fresh equivalent search or scrape response avoids the provider call.
Freshness is `Last-Modified` plus the current operation TTL. The response
envelope is versioned and has no `expires_at` field. RustFS read and write
failures fail open: reads fall through to the provider, while a write failure
warns without changing a successful provider response. Expired entries never
serve stale content, including when the provider fails. Duplicate concurrent
misses are allowed.

Cache keys are opaque and provider-aware under `cache/v1/`; raw queries, URLs,
credentials, and authorization data are not cache keys. Lifecycle is storage
cleanup only, not freshness. The deployment lifecycle retention is seven days;
if an operation TTL is configured beyond seven days, update that lifecycle
retention in the same deployment change.

To disable or roll back the cache, set `SESHAT_CACHE_ENABLED=false`. Seshat
then uses the direct-provider path again and leaves old cache objects for
lifecycle cleanup.

### Deployment handoff

This repository does not create buckets or credentials. No manifests or
deployment changes belong here. A separate `dev-infra` change must provision:

- a dedicated `seshat-cache` bucket;
- a Seshat-specific non-root RustFS identity, separate from RustFS root and
  OpenViking credentials;
- `GetObject` and `PutObject` only under `cache/v1/`;
- secret-backed environment injection for the two cache credential variables;
- a seven-day lifecycle rule for `cache/v1/`;
- an unversioned bucket, or matching noncurrent-version expiration when
  versioning is enabled;
- the current Monster endpoint
  `http://rustfs.rustfs.svc.cluster.local:9000`, region `us-east-1`, and
  path-style addressing.

Keep the cache disabled until bucket, policy, secret metadata, and lifecycle
readback all pass. Vault property names are intentionally omitted until the
approved deployment contract supplies them.

## HTTP contract

- `GET /healthz` — liveness; no upstream call.
- `GET /readyz` — checks loaded required pools; no upstream call.
- `POST /v2/search` — non-empty `query` (up to 2,000 bytes); optional `limit`
  defaults to `5` and must be between `1` and `20`.
- `POST /v2/scrape` — `url` and optional `formats`; formats default to
  `["markdown"]` and accept `markdown` and/or `html` (each at most once).

The two data routes require configured bearer authentication. The SDK's
`origin` field is accepted and ignored. Caller headers, actions, proxy
settings, and arbitrary provider options are not forwarded.

Example requests (assuming the default local bind address and an injected token):

```bash
curl -sS http://127.0.0.1:8080/v2/search \
  -H "Authorization: Bearer ${SESHAT_TOKEN}" \
  -H 'Content-Type: application/json' \
  -d '{"query":"Rust async","limit":5}'

curl -sS http://127.0.0.1:8080/v2/scrape \
  -H "Authorization: Bearer ${SESHAT_TOKEN}" \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","formats":["markdown"]}'
```

Search results are returned under `data.web[]` with `url`, `title`, and
`description`; scrape output is returned under `data` with selected document
formats and metadata.

Seshat enforces URL scheme, userinfo, credential-query, DNS-resolved private /
loopback / link-local / metadata destination, body-size, content-size, and
upstream timeout boundaries. Hosted Firecrawl still controls its own fetcher;
Seshat does not claim to control hosted redirect, DNS-rebinding, MIME, or
outbound-network behavior.

## Local verification

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
python -m pytest tests -q
```

The Hermes provider smoke tests are skipped unless `SESHAT_SMOKE_URL` is set.
To run them against a local Seshat instance, inject the Seshat token and key
files out-of-band, then set `FIRECRAWL_API_URL` to Seshat and
`FIRECRAWL_API_KEY` to that injected Seshat token. Use the Hermes installation
on `PYTHONPATH`; the provider source itself is not modified.

No mock test or readiness result is an authenticated production Firecrawl or
Brave result. Production-provider verification requires an explicit live probe
with externally supplied credentials.

## Container

The Dockerfile builds a non-root runtime image. Pull requests run checks only.
Pushes to `main` build `linux/amd64` and `linux/arm64` on native runners in
parallel, then merge the platform digests into
`devhohing/seshat:latest`; the workflow records the resulting
digest for reproducible pinning.

Kubernetes, Argo CD, GitOps, and cluster deployment are intentionally outside
this repository's initial implementation.
