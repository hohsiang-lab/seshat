use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::post,
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
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    path: String,
    authorization: Option<String>,
    body: Value,
}

async fn mock_search(
    State(state): State<MockState>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    record(&state, uri, headers, body).await;
    let authorization = state
        .requests
        .lock()
        .await
        .last()
        .and_then(|request| request.authorization.clone());
    if authorization.as_deref() == Some("Bearer alpha") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "temporary upstream failure"})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "data": {"web": [{"url": "https://example.com", "title": "Example", "description": "A page"}]}
        })),
    )
}

async fn mock_scrape(
    State(state): State<MockState>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    record(&state, uri, headers, body).await;
    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "data": {
                "markdown": "# Example",
                "html": "<h1>Example</h1>",
                "metadata": {"title": "Example", "sourceURL": "https://example.com"}
            }
        })),
    )
}

async fn record(state: &MockState, uri: Uri, headers: HeaderMap, body: Value) {
    state.requests.lock().await.push(RecordedRequest {
        path: uri.path().to_owned(),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body,
    });
}

async fn spawn_mock() -> (String, MockState, JoinHandle<()>) {
    let state = MockState::default();
    let router = Router::new()
        .route("/v2/search", post(mock_search))
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
    let router = build_router(AppState::new(
        config,
        reqwest::Client::builder()
            .build()
            .expect("client should build"),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("seshat bind");
    let address = listener.local_addr().expect("seshat address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("seshat server");
    });
    (format!("http://{address}"), handle)
}

fn config(upstream: &str) -> Config {
    Config::from_env_values(&BTreeMap::from([
        ("SESHAT_TOKEN".to_owned(), "auth".to_owned()),
        ("FIRECRAWL_API_KEYS".to_owned(), "alpha\nbeta".to_owned()),
        ("FIRECRAWL_UPSTREAM_URL".to_owned(), upstream.to_owned()),
    ]))
    .expect("test config should load")
}

#[tokio::test]
async fn search_uses_firecrawl_v2_and_falls_back_to_next_key() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let response = reqwest::Client::new()
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("auth")
        .json(&json!({"query": "hello", "limit": 2, "origin": "sdk"}))
        .send()
        .await
        .expect("search request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("response JSON");
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["web"][0]["title"], "Example");

    let requests = mock_state.requests.lock().await.clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/v2/search");
    assert_eq!(requests[1].path, "/v2/search");
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer alpha"));
    assert_eq!(requests[1].authorization.as_deref(), Some("Bearer beta"));
    assert_eq!(requests[1].body, json!({"query": "hello", "limit": 2}));

    seshat_handle.abort();
    mock_handle.abort();
}

#[tokio::test]
async fn scrape_uses_firecrawl_v2_and_returns_document_fields() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let response = reqwest::Client::new()
        .post(format!("{seshat}/v2/scrape"))
        .bearer_auth("auth")
        .json(&json!({"url": "https://example.com", "formats": ["markdown"], "origin": "sdk"}))
        .send()
        .await
        .expect("scrape request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("response JSON");
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["markdown"], "# Example");
    assert_eq!(body["data"]["metadata"]["sourceURL"], "https://example.com");

    let requests = mock_state.requests.lock().await.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v2/scrape");
    assert_eq!(
        requests[0].body,
        json!({"url": "https://example.com", "formats": ["markdown"]})
    );

    seshat_handle.abort();
    mock_handle.abort();
}
