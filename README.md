# Seshat

Seshat is a small Rust gateway that exposes the Firecrawl v2 HTTP subset used by
Hermes. It keeps Hermes on the existing, unmodified Firecrawl provider:

```yaml
web:
  backend: firecrawl
```

Hermes points `FIRECRAWL_API_URL` at Seshat. Hermes' `FIRECRAWL_API_KEY` is the
Seshat bearer token; upstream Firecrawl and Brave credentials stay inside
Seshat and are never sent by Hermes.

## Routing phases

- Phase 1 (`SESHAT_SEARCH_UPSTREAM=firecrawl`, the default): `/v2/search` and
  `/v2/scrape` both use the Firecrawl key pool.
- Phase 2 (`SESHAT_SEARCH_UPSTREAM=brave`): `/v2/search` uses Brave Search and
  `/v2/scrape` continues to use Firecrawl. The two pools never cross-fallback.

Every request selects an eligible key round-robin. Retryable transport errors,
`401`, `403`, `408`, `425`, `429`, and `5xx` advance to another key at most
once per request. Failed keys enter a bounded process-local cooldown. `400`
and `422` are returned without rotation.

## Configuration

Required for both phases:

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
```

Phase 2 additionally requires:

```text
BRAVE_SEARCH_API_KEYS_FILE=/run/secrets/brave-keys
```

Key files contain one key per line. Blank lines are ignored and duplicate keys
are removed while preserving order. File sources win over the local-only
newline-separated `FIRECRAWL_API_KEYS` and `BRAVE_SEARCH_API_KEYS` fallbacks.
Never put credentials in this repository, request payloads, logs, image layers,
workflow configuration, or artifacts.

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
- `POST /v2/search` — Firecrawl-compatible `query` and bounded `limit`.
- `POST /v2/scrape` — Firecrawl-compatible `url` and `formats` limited to
  `markdown` and `html`.

The two data routes require configured bearer authentication. The SDK's
`origin` field is accepted and ignored. Caller headers, actions, proxy
settings, and arbitrary provider options are not forwarded.

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
`ghcr.io/hohsiang-lab/seshat:latest`; the workflow records the resulting
digest for reproducible pinning.

Kubernetes, Argo CD, GitOps, and cluster deployment are intentionally outside
this repository's initial implementation.
