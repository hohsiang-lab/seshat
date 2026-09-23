use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use seshat::{
    config::Config,
    routes::{AppState, build_router},
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

#[derive(Clone, Default)]
struct MockState {
    brave_requests: Arc<Mutex<Vec<BraveRequest>>>,
    firecrawl_search_calls: Arc<Mutex<usize>>,
    scrape_requests: Arc<Mutex<Vec<ScrapeRequest>>>,
    brave_fails: bool,
}

#[derive(Clone, Debug)]
struct BraveRequest {
    path: String,
    query: String,
    token: Option<String>,
}

#[derive(Clone, Debug)]
struct ScrapeRequest {
    authorization: Option<String>,
    body: Value,
}

async fn mock_brave(
    State(state): State<MockState>,
    uri: Uri,
    headers: HeaderMap,
) -> impl IntoResponse {
    state.brave_requests.lock().await.push(BraveRequest {
        path: uri.path().to_owned(),
        query: uri.query().unwrap_or_default().to_owned(),
        token: headers
            .get("x-subscription-token")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    });
    if state.brave_fails {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "brave unavailable"})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "web": {"results": [{"title": "Brave result", "url": "https://example.org", "description": "A result"}]}
        })),
    )
}

async fn mock_firecrawl_search(State(state): State<MockState>) -> impl IntoResponse {
    *state.firecrawl_search_calls.lock().await += 1;
    (
        StatusCode::OK,
        Json(json!({"success": true, "data": {"web": []}})),
    )
}

async fn mock_scrape(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    state.scrape_requests.lock().await.push(ScrapeRequest {
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body,
    });
    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "data": {"markdown": "# Firecrawl", "metadata": {"sourceURL": "https://example.com"}}
        })),
    )
}

async fn spawn_mock(fails: bool) -> (String, MockState, JoinHandle<()>) {
    let state = MockState {
        brave_fails: fails,
        ..MockState::default()
    };
    let router = Router::new()
        .route("/res/v1/web/search", get(mock_brave))
        .route("/v2/search", post(mock_firecrawl_search))
        .route("/v2/scrape", post(mock_scrape))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
    let address = listener.local_addr().expect("mock address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("mock server");
    });
    (format!("http://{address}"), state, handle)
}

async fn spawn_seshat(config: Config) -> (String, JoinHandle<()>) {
    let app = build_router(AppState::new(
        config,
        reqwest::Client::builder()
            .build()
            .expect("client should build"),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("seshat bind");
    let address = listener.local_addr().expect("seshat address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("seshat server");
    });
    (format!("http://{address}"), handle)
}

fn config(upstream: &str, brave_fails: bool) -> Config {
    Config::from_env_values(&BTreeMap::from([
        ("SESHAT_TOKEN".to_owned(), "auth".to_owned()),
        ("SESHAT_SEARCH_UPSTREAM".to_owned(), "brave".to_owned()),
        ("FIRECRAWL_API_KEYS".to_owned(), "firecrawl-a".to_owned()),
        (
            "BRAVE_SEARCH_API_KEYS".to_owned(),
            "brave-a\nbrave-b".to_owned(),
        ),
        ("FIRECRAWL_UPSTREAM_URL".to_owned(), upstream.to_owned()),
        ("BRAVE_SEARCH_UPSTREAM_URL".to_owned(), upstream.to_owned()),
        (
            "BRAVE_FAILS".to_owned(),
            if brave_fails { "true" } else { "false" }.to_owned(),
        ),
    ]))
    .expect("phase two test config should load")
}

#[tokio::test]
async fn brave_search_maps_to_firecrawl_shape_with_exact_auth_and_query() {
    let (upstream, mock_state, mock_handle) = spawn_mock(false).await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream, false)).await;
    let response = reqwest::Client::new()
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("auth")
        .json(&json!({"query": "rust async", "limit": 3, "origin": "sdk"}))
        .send()
        .await
        .expect("search request");

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("response JSON");
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["web"][0]["title"], "Brave result");

    let requests = mock_state.brave_requests.lock().await.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/res/v1/web/search");
    assert_eq!(requests[0].query, "q=rust+async&count=3");
    assert_eq!(requests[0].token.as_deref(), Some("brave-a"));
    assert_eq!(*mock_state.firecrawl_search_calls.lock().await, 0);

    seshat_handle.abort();
    mock_handle.abort();
}

#[tokio::test]
async fn brave_failure_tries_only_brave_keys_and_scrape_keeps_firecrawl_pool() {
    let (upstream, mock_state, mock_handle) = spawn_mock(true).await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream, true)).await;
    let client = reqwest::Client::new();
    let search = client
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("auth")
        .json(&json!({"query": "hello"}))
        .send()
        .await
        .expect("search request");

    assert_eq!(search.status(), StatusCode::BAD_GATEWAY);
    let error_body: Value = search.json().await.expect("error JSON");
    assert_eq!(error_body["success"], false);
    assert_eq!(error_body["error"]["code"], "upstream_exhausted");
    assert_eq!(error_body["error"]["provider"], "brave");
    assert_eq!(error_body["error"]["failure_class"], "5xx");
    assert_eq!(mock_state.brave_requests.lock().await.len(), 2);
    assert_eq!(*mock_state.firecrawl_search_calls.lock().await, 0);

    let scrape = client
        .post(format!("{seshat}/v2/scrape"))
        .bearer_auth("auth")
        .json(&json!({"url": "https://example.com", "formats": ["markdown"]}))
        .send()
        .await
        .expect("scrape request");
    assert_eq!(scrape.status(), StatusCode::OK);
    let scrape_requests = mock_state.scrape_requests.lock().await.clone();
    assert_eq!(scrape_requests.len(), 1);
    assert_eq!(
        scrape_requests[0].authorization.as_deref(),
        Some("Bearer firecrawl-a")
    );
    assert_eq!(
        scrape_requests[0].body,
        json!({"url": "https://example.com", "formats": ["markdown"]})
    );

    seshat_handle.abort();
    mock_handle.abort();
}
