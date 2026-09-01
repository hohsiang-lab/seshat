use seshat::config::{Config, SearchUpstream};
use std::collections::BTreeMap;

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
    ]))
    .expect("custom upstream URLs should parse");

    assert_eq!(
        config.firecrawl_upstream_url().as_str(),
        "http://firecrawl.test/"
    );
    assert_eq!(config.brave_upstream_url().as_str(), "http://brave.test/");
    assert!(!format!("{config:?}").contains("auth"));
    assert!(!format!("{config:?}").contains("alpha"));
}
