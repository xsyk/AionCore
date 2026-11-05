#![cfg(feature = "lark")]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use aionui_channel::gateway::{GatewayClient, GatewayConfig, GatewayOutboundRequest};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};

fn config(url: &str) -> GatewayConfig {
    GatewayConfig {
        url: url.into(),
        node_id: "node_test".into(),
        token: "private-node-test-token-with-32-characters".into(),
        app_id: "cli_test".into(),
        tenant_key: "tenant_test".into(),
    }
}

#[test]
fn gateway_credentials_require_tls_or_numeric_loopback() {
    for url in [
        "https://route.example.com/feishu",
        "http://127.0.0.1:25910",
        "http://[::1]:25910",
    ] {
        assert!(GatewayClient::new(config(url)).is_ok(), "valid endpoint rejected");
    }
    for url in [
        "http://192.168.0.220:25910",
        "http://localhost:25910",
        "https://user:password@example.com",
        "https://example.com/?token=x",
        "https://example.com/#x",
    ] {
        assert!(GatewayClient::new(config(url)).is_err(), "unsafe endpoint accepted");
    }
    assert!(!format!("{:?}", config("https://example.com")).contains("private-node"));
}

async fn server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, handle)
}

#[tokio::test]
async fn queued_receipt_is_polled_until_platform_delivery() {
    async fn enqueue(headers: HeaderMap, Json(body): Json<serde_json::Value>) -> (StatusCode, Json<serde_json::Value>) {
        assert_eq!(
            headers["authorization"],
            "Bearer private-node-test-token-with-32-characters"
        );
        assert_eq!(body["route_id"], "r_test");
        assert_eq!(body["generation"], "g_test");
        (
            StatusCode::ACCEPTED,
            Json(
                serde_json::json!({"outbound":{"id":"o_test","state":"pending","node_id":"node_test","route_id":"r_test","generation":"g_test","idempotency_key":"reply-one"}}),
            ),
        )
    }
    async fn receipt(State(count): State<Arc<AtomicUsize>>) -> Json<serde_json::Value> {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Json(
                serde_json::json!({"outbound":{"id":"o_test","state":"sending","node_id":"node_test","route_id":"r_test","generation":"g_test","idempotency_key":"reply-one"}}),
            )
        } else {
            Json(
                serde_json::json!({"outbound":{"id":"o_test","state":"delivered","platform_message_id":"om_real","node_id":"node_test","route_id":"r_test","generation":"g_test","idempotency_key":"reply-one"}}),
            )
        }
    }
    let count = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/prefix/api/node/outbound", post(enqueue))
        .route("/prefix/api/node/outbound/o_test", get(receipt))
        .with_state(count.clone());
    let (url, task) = server(router).await;
    let client = GatewayClient::new(config(&format!("{url}/prefix"))).unwrap();
    let request = GatewayOutboundRequest {
        idempotency_key: "reply-one".into(),
        route_id: "r_test".into(),
        generation: "g_test".into(),
        kind: "send".into(),
        text: Some("test".into()),
        ..Default::default()
    };
    assert_eq!(client.send_and_wait(&request).await.unwrap(), "om_real");
    assert!(count.load(Ordering::SeqCst) >= 2);
    task.abort();
}

#[tokio::test]
async fn redirect_and_remote_error_never_forward_or_expose_credentials() {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let (other, other_task) = server(Router::new().route(
        "/capture",
        post(move || {
            let count = captured.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                "unexpected"
            }
        }),
    ))
    .await;
    let destination = format!("{other}/capture");
    let router = Router::new().route(
        "/api/node/intents",
        post(move || {
            let location = destination.clone();
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [("location", location)],
                    "private-node-test-token-with-32-characters prompt-content",
                )
            }
        }),
    );
    let (url, task) = server(router).await;
    let error = GatewayClient::new(config(&url))
        .unwrap()
        .create_intent("conversation", "union")
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(!message.contains("private-node") && !message.contains("prompt-content"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
    other_task.abort();
}

#[tokio::test]
async fn mismatched_receipt_cannot_complete_another_nodes_or_generations_message() {
    for field in ["node_id", "route_id", "generation", "idempotency_key"] {
        let mut value = serde_json::json!({"outbound":{"id":"o_test","state":"delivered","platform_message_id":"om_wrong","node_id":"node_test","route_id":"r_test","generation":"g_test","idempotency_key":"reply-one"}});
        value["outbound"][field] = "another".into();
        let (url, task) = server(Router::new().route(
            "/api/node/outbound",
            post(move || {
                let value = value.clone();
                async move { (StatusCode::ACCEPTED, Json(value)) }
            }),
        ))
        .await;
        let client = GatewayClient::new(config(&url)).unwrap();
        let request = GatewayOutboundRequest {
            idempotency_key: "reply-one".into(),
            route_id: "r_test".into(),
            generation: "g_test".into(),
            kind: "send".into(),
            text: Some("test".into()),
            ..Default::default()
        };
        let error = client.send_and_wait(&request).await.unwrap_err();
        assert!(error.to_string().contains("GATEWAY_RECEIPT_MISMATCH"));
        task.abort();
    }
}

#[tokio::test]
async fn receipt_identity_is_rechecked_after_queue_acceptance() {
    let initial = serde_json::json!({"outbound":{"id":"o_test","state":"pending","node_id":"node_test","route_id":"r_test","generation":"g_test","idempotency_key":"reply-one"}});
    let changed = serde_json::json!({"outbound":{"id":"o_test","state":"delivered","platform_message_id":"om_wrong","node_id":"node_test","route_id":"r_test","generation":"g_another","idempotency_key":"reply-one"}});
    let router = Router::new()
        .route(
            "/api/node/outbound",
            post(move || {
                let value = initial.clone();
                async move { (StatusCode::ACCEPTED, Json(value)) }
            }),
        )
        .route(
            "/api/node/outbound/o_test",
            get(move || {
                let value = changed.clone();
                async move { Json(value) }
            }),
        );
    let (url, task) = server(router).await;
    let request = GatewayOutboundRequest {
        idempotency_key: "reply-one".into(),
        route_id: "r_test".into(),
        generation: "g_test".into(),
        kind: "send".into(),
        text: Some("test".into()),
        ..Default::default()
    };
    let error = GatewayClient::new(config(&url))
        .unwrap()
        .send_and_wait(&request)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("GATEWAY_RECEIPT_MISMATCH"));
    task.abort();
}
