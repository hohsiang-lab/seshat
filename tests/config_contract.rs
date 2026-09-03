use seshat::config::{Config, SearchUpstream};
use std::collections::BTreeMap;
use std::time::Duration;

fn vars(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn phase_one_defaults_to_firecrawl_and_loads_firecrawl_pool() {
    let config = Config::from_env_values(&vars(&[
        ("SESHAT_TOKEN", "auth"),
        ("FIRECRAWL_API_KEYS", "alpha\n beta\nalpha"),
    ]))
    .expect("phase one config should load");

    assert_eq!(config.search_upstream(), SearchUpstream::Firecrawl);
    assert!(config.brave_keys().is_none());
    assert!(config.is_ready());
}

#[test]
fn phase_two_requires_a_brave_pool() {
    let error = Config::from_env_values(&vars(&[
        ("SESHAT_TOKEN", "auth"),
        ("SESHAT_SEARCH_UPSTREAM", "brave"),
        ("FIRECRAWL_API_KEYS", "alpha"),
    ]))
    .expect_err("phase two without Brave keys must fail");

    assert_eq!(error.code(), "missing_key_pool");
    assert!(!error.to_string().contains("alpha"));
}

#[test]
fn phase_three_selects_tavily_and_loads_its_pool() {
    let config = Config::from_env_values(&vars(&[
        ("SESHAT_TOKEN", "auth"),
        ("SESHAT_SEARCH_UPSTREAM", "tavily"),
        ("FIRECRAWL_API_KEYS", "firecrawl-key"),
        ("TAVILY_SEARCH_API_KEYS", "tavily-alpha\ntavily-beta"),
    ]))
    .expect("phase three config should load");

    assert_eq!(config.search_upstream(), SearchUpstream::Tavily);
    assert_eq!(
        config.tavily_upstream_url().as_str(),
        "https://api.tavily.com/"
    );
    let tavily_keys = config.tavily_keys().expect("Tavily keys should load");
    assert_eq!(tavily_keys.provider_name(), "tavily");
    assert_eq!(tavily_keys.len(), 2);
    assert!(config.brave_keys().is_none());
    assert!(config.is_ready());
}

#[test]
fn invalid_selector_fails_without_echoing_configuration_value() {
    let error = Config::from_env_values(&vars(&[
        ("SESHAT_TOKEN", "auth"),
        ("SESHAT_SEARCH_UPSTREAM", "not-a-provider"),
        ("FIRECRAWL_API_KEYS", "alpha"),
    ]))
    .expect_err("unknown selector must fail");

    assert_eq!(error.code(), "invalid_configuration");
    assert!(!error.to_string().contains("not-a-provider"));
}

#[test]
fn upstream_urls_are_configurable_without_exposing_secrets() {
    let config = Config::from_env_values(&vars(&[
        ("SESHAT_TOKEN", "auth"),
        ("FIRECRAWL_API_KEYS", "alpha"),
        ("FIRECRAWL_UPSTREAM_URL", "http://firecrawl.test"),
        ("BRAVE_SEARCH_UPSTREAM_URL", "http://brave.test"),
        ("TAVILY_SEARCH_UPSTREAM_URL", "http://tavily.test"),
    ]))
    .expect("custom upstream URLs should parse");

    assert_eq!(
        config.firecrawl_upstream_url().as_str(),
        "http://firecrawl.test/"
    );
    assert_eq!(config.brave_upstream_url().as_str(), "http://brave.test/");
    assert_eq!(config.tavily_upstream_url().as_str(), "http://tavily.test/");
    assert!(!format!("{config:?}").contains("auth"));
    assert!(!format!("{config:?}").contains("alpha"));
}

fn enabled_cache_vars(overrides: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut values = vars(&[
        ("SESHAT_TOKEN", "auth"),
        ("FIRECRAWL_API_KEYS", "alpha"),
        ("SESHAT_CACHE_ENABLED", "true"),
        ("SESHAT_CACHE_S3_ENDPOINT", "http://rustfs.test:9000"),
        ("SESHAT_CACHE_S3_BUCKET", "seshat-cache"),
        ("SESHAT_CACHE_S3_REGION", "us-east-1"),
        ("SESHAT_CACHE_S3_ACCESS_KEY_ID", "synthetic-access"),
        ("SESHAT_CACHE_S3_SECRET_ACCESS_KEY", "synthetic-secret"),
    ]);
    for (name, value) in overrides {
        values.insert((*name).to_owned(), (*value).to_owned());
    }
    values
}

fn assert_invalid_cache_value(name: &str, value: &str) {
    let error = Config::from_env_values(&enabled_cache_vars(&[(name, value)]))
        .expect_err("invalid cache value must fail");

    assert_eq!(error.code(), "invalid_configuration");
    if !value.is_empty() {
        assert!(!error.to_string().contains(value));
    }
    assert!(!error.to_string().contains("synthetic-access"));
    assert!(!error.to_string().contains("synthetic-secret"));
}

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
    let config = Config::from_env_values(&enabled_cache_vars(&[
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
    assert!(config.is_ready());
    let debug = format!("{cache:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("synthetic-access"));
    assert!(!debug.contains("synthetic-secret"));
}

#[test]
fn enabled_cache_uses_default_ttls() {
    let config =
        Config::from_env_values(&enabled_cache_vars(&[])).expect("cache defaults should load");
    let cache = config.cache().expect("cache should be enabled");

    assert_eq!(cache.search_ttl(), Duration::from_secs(600));
    assert_eq!(cache.scrape_ttl(), Duration::from_secs(86_400));
}

#[test]
fn invalid_cache_enabled_flags_fail_without_echoing_values() {
    for flag in ["yes", "1", "TRUE"] {
        assert_invalid_cache_value("SESHAT_CACHE_ENABLED", flag);
    }
}

#[test]
fn enabled_cache_requires_each_storage_field() {
    for field in [
        "SESHAT_CACHE_S3_ENDPOINT",
        "SESHAT_CACHE_S3_BUCKET",
        "SESHAT_CACHE_S3_REGION",
        "SESHAT_CACHE_S3_ACCESS_KEY_ID",
        "SESHAT_CACHE_S3_SECRET_ACCESS_KEY",
    ] {
        let mut values = enabled_cache_vars(&[]);
        values.remove(field);
        let error = Config::from_env_values(&values).expect_err("missing cache field must fail");

        assert_eq!(error.code(), "invalid_configuration");
        assert!(!error.to_string().contains("synthetic-access"));
        assert!(!error.to_string().contains("synthetic-secret"));
    }
}

#[test]
fn enabled_cache_rejects_empty_storage_fields() {
    for field in [
        "SESHAT_CACHE_S3_ENDPOINT",
        "SESHAT_CACHE_S3_BUCKET",
        "SESHAT_CACHE_S3_REGION",
        "SESHAT_CACHE_S3_ACCESS_KEY_ID",
        "SESHAT_CACHE_S3_SECRET_ACCESS_KEY",
    ] {
        assert_invalid_cache_value(field, "");
    }
}

#[test]
fn enabled_cache_rejects_unsafe_endpoints() {
    for endpoint in [
        "ftp://rustfs.test:9000",
        "http://user:password@rustfs.test:9000",
        "http://rustfs.test:9000?prefix=cache",
        "http://rustfs.test:9000/#cache",
        "rustfs.test:9000",
    ] {
        assert_invalid_cache_value("SESHAT_CACHE_S3_ENDPOINT", endpoint);
    }
}

#[test]
fn enabled_cache_rejects_invalid_bucket_names() {
    let mut buckets = vec![
        "ab".to_owned(),
        "A-bucket".to_owned(),
        "bucket_name".to_owned(),
        "-bucket".to_owned(),
        "bucket-".to_owned(),
        ".bucket".to_owned(),
        "bucket.".to_owned(),
        "bucket..cache".to_owned(),
        "bucket.-cache".to_owned(),
        "bucket-.cache".to_owned(),
    ];
    buckets.push("a".repeat(64));

    for bucket in buckets {
        assert_invalid_cache_value("SESHAT_CACHE_S3_BUCKET", &bucket);
    }
}

#[test]
fn enabled_cache_rejects_invalid_ttls() {
    for field in [
        "SESHAT_SEARCH_CACHE_TTL_SECS",
        "SESHAT_SCRAPE_CACHE_TTL_SECS",
    ] {
        for ttl in ["0", "not-a-number", "18446744073709551616"] {
            assert_invalid_cache_value(field, ttl);
        }
    }
}

#[test]
fn search_upstream_identifiers_are_stable() {
    assert_eq!(SearchUpstream::Firecrawl.as_str(), "firecrawl");
    assert_eq!(SearchUpstream::Brave.as_str(), "brave");
    assert_eq!(SearchUpstream::Tavily.as_str(), "tavily");
}
