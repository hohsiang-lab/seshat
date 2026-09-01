"""Smoke-test the unmodified Hermes Firecrawl provider against Seshat.

Set SESHAT_SMOKE_URL and configure PYTHONPATH to the Hermes installation before
running this module. The test intentionally does not contain or print any
credential values.
"""

import asyncio
import os

import pytest


pytestmark = pytest.mark.skipif(
    not os.getenv("SESHAT_SMOKE_URL"),
    reason="set SESHAT_SMOKE_URL for the local Hermes provider smoke test",
)


def _provider():
    expected_url = os.getenv("SESHAT_SMOKE_URL", "").rstrip("/")
    configured_url = os.getenv("FIRECRAWL_API_URL", "").rstrip("/")
    assert configured_url == expected_url

    from plugins.web.firecrawl.provider import FirecrawlWebSearchProvider

    return FirecrawlWebSearchProvider()


def test_existing_hermes_provider_searches_through_seshat():
    provider = _provider()
    assert provider.name == "firecrawl"
    assert provider.supports_search()
    result = provider.search("seshat compatibility smoke", limit=2)
    assert result["success"] is True
    assert result["data"]["web"]


def test_existing_hermes_provider_extracts_through_seshat():
    provider = _provider()
    target = os.getenv("SESHAT_SMOKE_TARGET_URL", "https://example.com")
    result = asyncio.run(provider.extract([target], format="markdown"))
    assert len(result) == 1
    assert "error" not in result[0]
    assert result[0]["content"]
