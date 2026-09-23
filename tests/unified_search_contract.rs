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
    tavily_requests: Arc<Mutex<Vec<TavilyRequest>>>,
    brave_failure_token: Arc<Mutex<Option<String>>>,
}

#[derive(Clone, Debug)]
struct BraveRequest {
    query: String,
    count: String,
    token: String,
}

#[derive(Clone, Debug)]
struct TavilyRequest {
    authorization: String,
    body: Value,
}

async fn mock_brave(
    State(state): State<MockState>,
    uri: Uri,
    headers: HeaderMap,
) -> impl IntoResponse {
    let query = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find(|(name, _)| name == "q")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let count = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find(|(name, _)| name == "count")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let token = headers
        .get("x-subscription-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    assert_eq!(uri.path(), "/res/v1/web/search");
    let should_fail = state
        .brave_failure_token
        .lock()
        .await
        .as_deref()
        .is_some_and(|failure_token| failure_token == token);
    state.brave_requests.lock().await.push(BraveRequest {
        query,
        count,
        token,
    });
    if should_fail {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    (
        StatusCode::OK,
        Json(json!({
            "web": {"results": [{"title": "Brave result", "url": "https://brave.example/result", "description": "Brave"}]}
        })),
    )
}

async fn mock_tavily(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    state.tavily_requests.lock().await.push(TavilyRequest {
        authorization,
        body,
    });
    (
        StatusCode::OK,
        Json(json!({
            "results": [{"title": "Tavily result", "url": "https://tavily.example/result", "content": "Tavily"}]
        })),
    )
}

async fn spawn_mock() -> (String, MockState, JoinHandle<()>) {
    let state = MockState::default();
    let router = Router::new()
        .route("/res/v1/web/search", get(mock_brave))
        .route("/search", post(mock_tavily))
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

fn config(upstream: &str) -> Config {
    Config::from_env_values(&BTreeMap::from([
        ("SESHAT_TOKEN".to_owned(), "auth".to_owned()),
        (
            "SESHAT_SEARCH_UPSTREAM".to_owned(),
            "tavily,brave".to_owned(),
        ),
        ("FIRECRAWL_API_KEYS".to_owned(), "firecrawl".to_owned()),
        (
            "BRAVE_SEARCH_API_KEYS".to_owned(),
            "brave-a\nbrave-b".to_owned(),
        ),
        (
            "TAVILY_SEARCH_API_KEYS".to_owned(),
            "tavily-a\ntavily-b".to_owned(),
        ),
        ("FIRECRAWL_UPSTREAM_URL".to_owned(), upstream.to_owned()),
        ("BRAVE_SEARCH_UPSTREAM_URL".to_owned(), upstream.to_owned()),
        ("TAVILY_SEARCH_UPSTREAM_URL".to_owned(), upstream.to_owned()),
    ]))
    .expect("combined config should load")
}

#[tokio::test]
async fn combined_search_selects_one_provider_key_per_request_with_native_transport() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let client = reqwest::Client::new();

    for _ in 0..3 {
        let response = client
            .post(format!("{seshat}/v2/search"))
            .bearer_auth("auth")
            .json(&json!({"query": "rust async", "limit": 3}))
            .send()
            .await
            .expect("combined search request");
        assert_eq!(response.status(), StatusCode::OK);
    }

    let brave_requests = mock_state.brave_requests.lock().await.clone();
    assert_eq!(brave_requests.len(), 2);
    assert_eq!(
        brave_requests
            .iter()
            .map(|request| request.token.as_str())
            .collect::<Vec<_>>(),
        vec!["brave-a", "brave-b"]
    );
    assert!(
        brave_requests
            .iter()
            .all(|request| request.query == "rust async" && request.count == "3")
    );

    let tavily_requests = mock_state.tavily_requests.lock().await.clone();
    assert_eq!(tavily_requests.len(), 1);
    assert_eq!(tavily_requests[0].authorization, "Bearer tavily-a");
    assert_eq!(
        tavily_requests[0].body,
        json!({
            "query": "rust async",
            "search_depth": "basic",
            "max_results": 3,
            "include_answer": false,
            "include_raw_content": false
        })
    );
    assert_eq!(brave_requests.len() + tavily_requests.len(), 3);

    seshat_handle.abort();
    mock_handle.abort();
}

#[tokio::test]
async fn combined_search_cools_only_the_failed_provider_key() {
    let (upstream, mock_state, mock_handle) = spawn_mock().await;
    *mock_state.brave_failure_token.lock().await = Some("brave-a".to_owned());
    let (seshat, seshat_handle) = spawn_seshat(config(&upstream)).await;
    let client = reqwest::Client::new();

    for _ in 0..2 {
        let response = client
            .post(format!("{seshat}/v2/search"))
            .bearer_auth("auth")
            .json(&json!({"query": "cooldown", "limit": 1}))
            .send()
            .await
            .expect("combined search request");
        assert_eq!(response.status(), StatusCode::OK);
    }

    let brave_requests = mock_state.brave_requests.lock().await.clone();
    assert_eq!(
        brave_requests
            .iter()
            .map(|request| request.token.as_str())
            .collect::<Vec<_>>(),
        vec!["brave-a", "brave-b", "brave-b"]
    );
    assert!(mock_state.tavily_requests.lock().await.is_empty());

    seshat_handle.abort();
    mock_handle.abort();
}
