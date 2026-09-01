use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::json;
use seshat::{
    config::Config,
    routes::{AppState, build_router},
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

#[derive(Clone, Default)]
struct MockState {
    brave_tokens: Arc<Mutex<Vec<String>>>,
    firecrawl_search_calls: Arc<Mutex<usize>>,
    firecrawl_scrape_calls: Arc<Mutex<usize>>,
}

async fn brave_failure(
    State(state): State<MockState>,
    uri: Uri,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = headers
        .get("x-subscription-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert_eq!(uri.path(), "/res/v1/web/search");
    state.brave_tokens.lock().await.push(token.to_owned());
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "temporary"})),
    )
}

async fn firecrawl_search(State(state): State<MockState>) -> impl IntoResponse {
    *state.firecrawl_search_calls.lock().await += 1;
    (
        StatusCode::OK,
        Json(json!({"success": true, "data": {"web": []}})),
    )
}

async fn firecrawl_scrape(State(state): State<MockState>) -> impl IntoResponse {
    *state.firecrawl_scrape_calls.lock().await += 1;
    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "data": {"markdown": "# Example", "metadata": {"sourceURL": "https://example.com"}}
        })),
    )
}

async fn spawn_mock() -> (String, MockState, JoinHandle<()>) {
    let state = MockState::default();
    let router = Router::new()
        .route("/res/v1/web/search", get(brave_failure))
        .route("/v2/search", post(firecrawl_search))
        .route("/v2/scrape", post(firecrawl_scrape))
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

fn phase_two_config(upstream: &str) -> Config {
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
    ]))
    .expect("phase two config should load")
}

#[tokio::test]
async fn phase_two_keeps_pools_independent_and_invalid_input_does_not_rotate() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(phase_two_config(&upstream)).await;
    let client = reqwest::Client::new();

    let invalid = client
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("auth")
        .json(&json!({"query": "", "limit": 0}))
        .send()
        .await
        .expect("invalid request");
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(mock_state.brave_tokens.lock().await.is_empty());

    let search = client
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("auth")
        .json(&json!({"query": "hello"}))
        .send()
        .await
        .expect("Brave request");
    assert_eq!(search.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        *mock_state.brave_tokens.lock().await,
        vec!["brave-a".to_owned(), "brave-b".to_owned()]
    );
    assert_eq!(*mock_state.firecrawl_search_calls.lock().await, 0);

    let scrape = client
        .post(format!("{seshat}/v2/scrape"))
        .bearer_auth("auth")
        .json(&json!({"url": "https://example.com", "formats": ["markdown"]}))
        .send()
        .await
        .expect("Firecrawl request");
    assert_eq!(scrape.status(), StatusCode::OK);
    assert_eq!(*mock_state.firecrawl_scrape_calls.lock().await, 1);
    assert_eq!(*mock_state.firecrawl_search_calls.lock().await, 0);

    seshat_handle.abort();
    mock_handle.abort();
}
