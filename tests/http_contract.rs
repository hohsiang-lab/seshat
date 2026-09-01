use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use seshat::{
    config::Config,
    routes::{AppState, MAX_REQUEST_BODY_BYTES, build_router},
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

#[derive(Clone, Default)]
struct MockState {
    calls: Arc<Mutex<usize>>,
}

async fn mock_upstream(State(state): State<MockState>) -> impl IntoResponse {
    *state.calls.lock().await += 1;
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "caller payload rejected"})),
    )
}

async fn spawn_mock() -> (String, MockState, JoinHandle<()>) {
    let state = MockState::default();
    let router = Router::new()
        .route("/v2/search", post(mock_upstream))
        .route("/v2/scrape", post(mock_upstream))
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
async fn health_and_readiness_do_not_call_upstream() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let client = reqwest::Client::new();

    let health = client
        .get(format!("{seshat}/healthz"))
        .send()
        .await
        .expect("health request");
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(
        health.json::<Value>().await.expect("health JSON"),
        json!({"status": "ok"})
    );

    let readiness = client
        .get(format!("{seshat}/readyz"))
        .send()
        .await
        .expect("readiness request");
    assert_eq!(readiness.status(), StatusCode::OK);
    assert_eq!(
        readiness.json::<Value>().await.expect("readiness JSON"),
        json!({"status": "ready"})
    );
    assert_eq!(*mock_state.calls.lock().await, 0);

    seshat_handle.abort();
    mock_handle.abort();
}

#[tokio::test]
async fn data_routes_require_seshat_bearer_auth() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let response = reqwest::Client::new()
        .post(format!("{seshat}/v2/search"))
        .json(&json!({"query": "hello"}))
        .send()
        .await
        .expect("unauthenticated request");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("error JSON");
    assert_eq!(body["success"], false);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_eq!(*mock_state.calls.lock().await, 0);

    let invalid = reqwest::Client::new()
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("wrong-token")
        .json(&json!({"query": "hello"}))
        .send()
        .await
        .expect("invalid authentication request");
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(*mock_state.calls.lock().await, 0);

    seshat_handle.abort();
    mock_handle.abort();
}

#[tokio::test]
async fn invalid_input_is_stable_and_does_not_rotate_or_call_upstream() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("auth")
        .json(&json!({"query": "", "limit": 0}))
        .send()
        .await
        .expect("invalid search request");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error JSON");
    assert_eq!(body["error"]["code"], "invalid_request");
    assert_eq!(*mock_state.calls.lock().await, 0);

    let unsafe_url = client
        .post(format!("{seshat}/v2/scrape"))
        .bearer_auth("auth")
        .json(&json!({"url": "http://localhost/", "formats": ["markdown"]}))
        .send()
        .await
        .expect("unsafe scrape request");
    assert_eq!(unsafe_url.status(), StatusCode::BAD_REQUEST);
    assert_eq!(*mock_state.calls.lock().await, 0);

    seshat_handle.abort();
    mock_handle.abort();
}

#[tokio::test]
async fn oversized_input_returns_json_413_without_upstream_call() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let oversized = format!("{{\"query\":\"{}\"}}", "a".repeat(MAX_REQUEST_BODY_BYTES));
    let response = reqwest::Client::new()
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("auth")
        .header("content-type", "application/json")
        .body(oversized)
        .send()
        .await
        .expect("oversized request");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("error JSON");
    assert_eq!(body["error"]["code"], "payload_too_large");
    assert_eq!(*mock_state.calls.lock().await, 0);

    seshat_handle.abort();
    mock_handle.abort();
}

#[tokio::test]
async fn upstream_400_is_returned_without_trying_the_next_key() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let response = reqwest::Client::new()
        .post(format!("{seshat}/v2/search"))
        .bearer_auth("auth")
        .json(&json!({"query": "hello"}))
        .send()
        .await
        .expect("caller error request");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error JSON");
    assert_eq!(body["error"]["code"], "upstream_rejected");
    assert_eq!(*mock_state.calls.lock().await, 1);

    seshat_handle.abort();
    mock_handle.abort();
}
