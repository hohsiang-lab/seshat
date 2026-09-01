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
SESHAT_TOKEN=<injected-out-of-band>
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

## HTTP contract

- `GET /healthz` — liveness; no upstream call.
- `GET /readyz` — checks loaded required pools; no upstream call.
- `POST /v2/search` — Firecrawl-compatible `query` and bounded `limit`.
- `POST /v2/scrape` — Firecrawl-compatible `url` and `formats` limited to
  `markdown` and `html`.

The two data routes require `Authorization: Bearer <Seshat token>`. The SDK's
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

The Dockerfile builds a non-root runtime image. Pull requests run checks and a
non-publishing build. The workflow's `main` publish path is configured for the
provisional package `ghcr.io/hohsiang-lab/seshat`; confirm the GitHub owner and
package name before creating a remote repository or publishing. Published tags
are commit SHA tags only, and the workflow records the resulting digest.

Kubernetes, Argo CD, GitOps, and cluster deployment are intentionally outside
this repository's initial implementation.
