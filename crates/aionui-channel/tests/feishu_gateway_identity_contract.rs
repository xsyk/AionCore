//! Actual Go HTTP captures exercise the node client's identity DTO envelopes.
use aionui_api_types::{FeishuGatewayIdentityBeginRequest, FeishuGatewayIdentityExchangeRequest, FeishuIdentityKind};
use aionui_channel::gateway::{GatewayClient, GatewayConfig};
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode, header},
    routing::post,
};
use serde_json::Value;
use std::sync::Arc;

const TOKEN: &str = "gateway-identity-contract-test-token-at-least-32-bytes";

#[tokio::test]
async fn actual_go_identity_http_envelopes_work_for_oauth_and_h5() {
    let capture: Value = serde_json::from_str(include_str!(
        "../../aionui-auth/tests/fixtures/feishu_gateway_identity.json"
    ))
    .unwrap();
    let capture = Arc::new(capture);
    let begin_capture = capture.clone();
    let exchange_capture = capture.clone();
    let router = Router::new()
        .route(
            "/api/node/identity/transactions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let capture = begin_capture.clone();
                async move {
                    assert_eq!(headers[header::AUTHORIZATION], format!("Bearer {TOKEN}"));
                    let kind = body["kind"].as_str().unwrap();
                    let fixture = &capture["cases"][kind]["begin"];
                    let expected: Value = serde_json::from_str(fixture["request_raw_body"].as_str().unwrap()).unwrap();
                    assert_eq!(body, expected);
                    (
                        StatusCode::CREATED,
                        [(header::CONTENT_TYPE, "application/json")],
                        fixture["raw_body"].as_str().unwrap().to_owned(),
                    )
                }
            }),
        )
        .route(
            "/api/node/identity/exchange",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let capture = exchange_capture.clone();
                async move {
                    assert_eq!(headers[header::AUTHORIZATION], format!("Bearer {TOKEN}"));
                    let fixture = ["oauth", "h5"]
                        .into_iter()
                        .map(|kind| &capture["cases"][kind])
                        .find(|case| case["transaction_id"] == body["transaction_id"])
                        .unwrap();
                    let expected: Value =
                        serde_json::from_str(fixture["exchange"]["request_raw_body"].as_str().unwrap()).unwrap();
                    assert_eq!(body, expected);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        fixture["exchange"]["raw_body"].as_str().unwrap().to_owned(),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = GatewayClient::new(GatewayConfig {
        url: format!("http://{}", listener.local_addr().unwrap()),
        node_id: capture["node"]["id"].as_str().unwrap().into(),
        token: TOKEN.into(),
        app_id: capture["app_id"].as_str().unwrap().into(),
        tenant_key: capture["tenant_key"].as_str().unwrap().into(),
        identity: None,
    })
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    for kind in [FeishuIdentityKind::Oauth, FeishuIdentityKind::H5] {
        let fixture = &capture["cases"][kind.as_str()];
        let begin: FeishuGatewayIdentityBeginRequest =
            serde_json::from_str(fixture["begin"]["request_raw_body"].as_str().unwrap()).unwrap();
        let transaction = client.begin_identity(begin).await.unwrap();
        assert_eq!(transaction.transaction_id, fixture["transaction_id"].as_str().unwrap());
        assert_eq!(transaction.expires_at, capture["now_ms"].as_i64().unwrap() + 600_000);
        let exchange: FeishuGatewayIdentityExchangeRequest =
            serde_json::from_str(fixture["exchange"]["request_raw_body"].as_str().unwrap()).unwrap();
        let result = client.exchange_identity(exchange).await.unwrap();
        assert_eq!(result.identity.union_id, "union_contract_test_only");
        assert_eq!(result.expires_at % 1000, 789);
        assert_eq!(result.assertion.split('.').count(), 3);
    }
    server.abort();
    let _ = server.await;
}
