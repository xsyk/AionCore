#![cfg(feature = "lark")]

//! Responses captured from the actual Go offline HTTP server, not a Rust DTO.
//! Regenerate with ai/docs/superpowers/sdd/1.0.3/capture-gateway-contract.py.

use std::collections::HashMap;

use aionui_channel::gateway::{GatewayClient, GatewayConfig, GatewayDelivery};
use axum::{
    Router,
    extract::Query,
    http::{HeaderMap, header::CONTENT_TYPE},
    routing::get,
};
use serde_json::Value;

const CAPTURE: &str = include_str!("fixtures/feishu_gateway_control.json");
const NODE_TOKEN: &str = "fixture-node-token-more-than-32-characters";

async fn captured_deliveries(name: &str) -> (Vec<GatewayDelivery>, Value) {
    let capture: Value = serde_json::from_str(CAPTURE).unwrap();
    assert_eq!(capture["source"]["mode"], "offline");
    let response = capture["responses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|response| response["name"] == name)
        .unwrap();
    assert_eq!(response["status"], 200);
    let body = response["body"].as_str().unwrap().to_owned();
    let expected: Value = serde_json::from_str(&body).unwrap();
    let route = &expected["deliveries"][0]["route"];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = GatewayConfig {
        url: format!("http://{}", listener.local_addr().unwrap()),
        node_id: route["node_id"].as_str().unwrap().into(),
        token: NODE_TOKEN.into(),
        app_id: route["app_id"].as_str().unwrap().into(),
        tenant_key: route["tenant_key"].as_str().unwrap().into(),
        identity: None,
    };
    let router = Router::new().route(
        "/api/node/deliveries",
        get(
            move |headers: HeaderMap, Query(query): Query<HashMap<String, String>>| {
                let body = body.clone();
                async move {
                    assert_eq!(headers["authorization"], format!("Bearer {NODE_TOKEN}"));
                    assert_eq!(query.get("limit").map(String::as_str), Some("8"));
                    ([(CONTENT_TYPE, "application/json")], body)
                }
            },
        ),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let result = GatewayClient::new(config).unwrap().deliveries().await;
    task.abort();
    (
        result.unwrap_or_else(|error| panic!("actual Go {name} response was rejected: {error}")),
        expected,
    )
}

#[tokio::test]
async fn go_control_deliveries_without_expiry_are_accepted() {
    for (name, kind, status) in [
        ("prepare", "binding.prepare", "preparing"),
        ("revoke", "binding.revoke", "revoking"),
    ] {
        let (deliveries, response) = captured_deliveries(name).await;
        let raw = &response["deliveries"][0];
        assert!(raw.get("expires_at").is_none(), "Go omits a zero control TTL");
        assert_eq!(deliveries.len(), 1);
        let delivery = &deliveries[0];
        assert_eq!(delivery.kind, kind);
        assert_eq!(delivery.expires_at, 0);
        assert_eq!(delivery.id, raw["id"].as_str().unwrap());
        assert_eq!(delivery.created_at, raw["created_at"].as_i64().unwrap());
        assert_eq!(delivery.route.status, status);
        assert_eq!(delivery.route.id, raw["route"]["id"].as_str().unwrap());
        assert_eq!(delivery.route.generation, raw["route"]["generation"].as_str().unwrap());
        assert_eq!(delivery.route.conversation_id, "capture-conversation");
        assert_eq!(delivery.route.owner_union_id, "capture-owner-union");
        if name == "prepare" {
            assert!(raw["route"].get("prepared_at").is_none());
            assert!(delivery.route.prepared_at.is_none());
            assert_eq!(delivery.event.as_ref().unwrap().id, "capture-bind-event");
        } else {
            assert!(raw.get("event").is_none());
            assert!(delivery.event.is_none());
            assert!(delivery.route.prepared_at.unwrap() > 0);
        }
    }
}

#[tokio::test]
async fn go_text_answer_delivery_has_no_original_card_message_id() {
    let (deliveries, response) = captured_deliveries("answer").await;
    assert_eq!(deliveries.len(), 1);
    let delivery = &deliveries[0];
    assert_eq!(delivery.kind, "interaction");
    assert!(delivery.expires_at > delivery.created_at);
    assert_eq!(delivery.route.status, "active");
    let event = delivery.event.as_ref().unwrap();
    assert_eq!(event.id, "capture-answer-event");
    assert_eq!(event.action, "answer");
    assert_eq!(event.interaction_id, "capture-interaction");
    assert_eq!(event.text, "/answer capture-interaction A");
    assert_eq!(event.actor_type, "user");
    assert_eq!(event.actor_open_id, "capture-owner-open");
    assert!(response["deliveries"][0]["event"].get("platform_message_id").is_none());
    assert!(event.platform_message_id.is_empty());
}
