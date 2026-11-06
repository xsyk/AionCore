use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{GatewayConfig, GatewayDelivery, GatewayIntent, GatewayOutbound, GatewayOutboundRequest, GatewayRoute};
use crate::error::ChannelError;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Bounded node API client. Upstream payloads and credentials are never errors.
#[derive(Clone)]
pub struct GatewayClient {
    config: GatewayConfig,
    http: reqwest::Client,
    base: String,
}

impl GatewayClient {
    pub fn new(config: GatewayConfig) -> Result<Self, ChannelError> {
        let url = config.validate()?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(|_| failed("GATEWAY_CLIENT_INIT"))?;
        Ok(Self {
            base: url.as_str().trim_end_matches('/').to_owned(),
            config,
            http,
        })
    }

    pub fn node_id(&self) -> &str {
        &self.config.node_id
    }
    pub fn app_id(&self) -> &str {
        &self.config.app_id
    }
    pub fn tenant_key(&self) -> &str {
        &self.config.tenant_key
    }
    pub fn identity_config(&self) -> Option<&super::GatewayIdentityConfig> {
        self.config.identity.as_ref()
    }

    pub async fn begin_identity(
        &self,
        request: aionui_api_types::FeishuGatewayIdentityBeginRequest,
    ) -> Result<aionui_api_types::FeishuGatewayIdentityTransaction, ChannelError> {
        let response: aionui_api_types::FeishuGatewayIdentityBeginResponse = self
            .request(
                Method::POST,
                "identity/transactions",
                &[],
                Some(serde_json::to_value(request).map_err(|_| failed("GATEWAY_IDENTITY_REQUEST_INVALID"))?),
                StatusCode::CREATED,
                false,
            )
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        Ok(response.transaction)
    }

    pub async fn exchange_identity(
        &self,
        request: aionui_api_types::FeishuGatewayIdentityExchangeRequest,
    ) -> Result<aionui_api_types::FeishuGatewayIdentityExchange, ChannelError> {
        let response: aionui_api_types::FeishuGatewayIdentityExchangeResponse = self
            .request(
                Method::POST,
                "identity/exchange",
                &[],
                Some(serde_json::to_value(request).map_err(|_| failed("GATEWAY_IDENTITY_REQUEST_INVALID"))?),
                StatusCode::OK,
                false,
            )
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        Ok(response.identity)
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<serde_json::Value>,
        status: StatusCode,
        missing: bool,
    ) -> Result<Option<T>, ChannelError> {
        let mut request = self
            .http
            .request(method, format!("{}/api/node/{path}", self.base))
            .bearer_auth(&self.config.token)
            .query(query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.map_err(|_| failed("GATEWAY_TRANSPORT"))?;
        if missing && response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != status {
            return Err(failed(match response.status() {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "GATEWAY_AUTH_REJECTED",
                StatusCode::CONFLICT => "GATEWAY_STATE_CONFLICT",
                StatusCode::GONE => "GATEWAY_EXPIRED",
                _ => "GATEWAY_RESPONSE_REJECTED",
            }));
        }
        if response.content_length().is_some_and(|n| n > MAX_RESPONSE_BYTES as u64) {
            return Err(failed("GATEWAY_RESPONSE_TOO_LARGE"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| failed("GATEWAY_RESPONSE_READ"))? {
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                return Err(failed("GATEWAY_RESPONSE_TOO_LARGE"));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| failed("GATEWAY_RESPONSE_INVALID"))
    }

    pub async fn create_intent(
        &self,
        conversation_id: &str,
        owner_union_id: &str,
    ) -> Result<GatewayIntent, ChannelError> {
        #[derive(Deserialize)]
        struct Response {
            intent: GatewayIntent,
        }
        let response: Response = self
            .request(
                Method::POST,
                "intents",
                &[],
                Some(serde_json::json!({"conversation_id":conversation_id,"owner_union_id":owner_union_id})),
                StatusCode::CREATED,
                false,
            )
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        if response.intent.node_id != self.node_id()
            || response.intent.conversation_id != conversation_id
            || response.intent.owner_union_id != owner_union_id
            || response.intent.code.is_empty()
        {
            return Err(failed("GATEWAY_INTENT_MISMATCH"));
        }
        Ok(response.intent)
    }

    pub async fn route(&self, conversation_id: &str) -> Result<Option<GatewayRoute>, ChannelError> {
        #[derive(Deserialize)]
        struct Response {
            route: GatewayRoute,
        }
        let response: Option<Response> = self
            .request(
                Method::GET,
                "route",
                &[("conversation_id", conversation_id)],
                None,
                StatusCode::OK,
                true,
            )
            .await?;
        match response {
            None => Ok(None),
            Some(response) => {
                self.verify_route(&response.route)?;
                if response.route.conversation_id != conversation_id {
                    return Err(failed("GATEWAY_ROUTE_MISMATCH"));
                }
                Ok(Some(response.route))
            }
        }
    }

    pub async fn deliveries(&self) -> Result<Vec<GatewayDelivery>, ChannelError> {
        #[derive(Deserialize)]
        struct Response {
            deliveries: Vec<GatewayDelivery>,
        }
        let response: Response = self
            .request(
                Method::GET,
                "deliveries",
                &[("limit", "8")],
                None,
                StatusCode::OK,
                false,
            )
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        if response.deliveries.len() > 8 {
            return Err(failed("GATEWAY_RESPONSE_TOO_LARGE"));
        }
        for d in &response.deliveries {
            validate_id(&d.id)?;
            self.verify_route(&d.route)?;
        }
        Ok(response.deliveries)
    }

    pub async fn ack(&self, delivery_id: &str, status: &str) -> Result<(), ChannelError> {
        validate_id(delivery_id)?;
        if !matches!(status, "accepted" | "prepared" | "revoked") {
            return Err(failed("GATEWAY_ACK_INVALID"));
        }
        #[derive(Deserialize)]
        struct Response {
            acknowledged: bool,
        }
        let response: Response = self
            .request(
                Method::POST,
                &format!("deliveries/{delivery_id}/ack"),
                &[],
                Some(serde_json::json!({"status":status})),
                StatusCode::OK,
                false,
            )
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        if !response.acknowledged {
            return Err(failed("GATEWAY_ACK_REJECTED"));
        }
        Ok(())
    }

    pub async fn revoke(&self, route_id: &str) -> Result<GatewayRoute, ChannelError> {
        validate_id(route_id)?;
        #[derive(Deserialize)]
        struct Response {
            route: GatewayRoute,
        }
        let response: Response = self
            .request(
                Method::POST,
                &format!("routes/{route_id}/revoke"),
                &[],
                None,
                StatusCode::OK,
                false,
            )
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        self.verify_route(&response.route)?;
        if response.route.id != route_id {
            return Err(failed("GATEWAY_ROUTE_MISMATCH"));
        }
        Ok(response.route)
    }

    /// Reject preparation before acknowledging it; this cannot cancel an
    /// active generation or bypass the previous node's revocation fence.
    pub async fn reject(&self, route_id: &str) -> Result<GatewayRoute, ChannelError> {
        validate_id(route_id)?;
        #[derive(Deserialize)]
        struct Response {
            route: GatewayRoute,
        }
        let response: Response = self
            .request(
                Method::POST,
                &format!("routes/{route_id}/reject"),
                &[],
                None,
                StatusCode::OK,
                false,
            )
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        self.verify_route(&response.route)?;
        if response.route.id != route_id || response.route.status != "revoked" {
            return Err(failed("GATEWAY_ROUTE_MISMATCH"));
        }
        Ok(response.route)
    }

    pub async fn submit(&self, request: &GatewayOutboundRequest) -> Result<GatewayOutbound, ChannelError> {
        validate_id(&request.route_id)?;
        validate_id(&request.generation)?;
        #[derive(Deserialize)]
        struct Response {
            outbound: GatewayOutbound,
        }
        let body = serde_json::to_value(request).map_err(|_| failed("GATEWAY_REQUEST_INVALID"))?;
        let response: Response = self
            .request(Method::POST, "outbound", &[], Some(body), StatusCode::ACCEPTED, false)
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        validate_id(&response.outbound.id)?;
        self.verify_receipt(&response.outbound, request)?;
        Ok(response.outbound)
    }

    pub async fn outbound(&self, id: &str) -> Result<GatewayOutbound, ChannelError> {
        validate_id(id)?;
        #[derive(Deserialize)]
        struct Response {
            outbound: GatewayOutbound,
        }
        let response: Response = self
            .request(Method::GET, &format!("outbound/{id}"), &[], None, StatusCode::OK, false)
            .await?
            .ok_or_else(|| failed("GATEWAY_RESPONSE_INVALID"))?;
        if response.outbound.id != id || response.outbound.node_id != self.node_id() {
            return Err(failed("GATEWAY_RECEIPT_MISMATCH"));
        }
        Ok(response.outbound)
    }

    pub async fn send_and_wait(&self, request: &GatewayOutboundRequest) -> Result<String, ChannelError> {
        let wait = async {
            let mut receipt = self.submit(request).await?;
            loop {
                self.verify_receipt(&receipt, request)?;
                match receipt.state.as_str() {
                    "delivered" => {
                        return receipt
                            .platform_message_id
                            .filter(|id| !id.is_empty())
                            .ok_or_else(|| failed("GATEWAY_RECEIPT_INVALID"));
                    }
                    "unknown" => return Err(failed("GATEWAY_OUTCOME_UNKNOWN")),
                    "canceled" => return Err(failed("GATEWAY_OUTBOUND_CANCELED")),
                    "pending" | "sending" => {}
                    _ => return Err(failed("GATEWAY_RECEIPT_INVALID")),
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                receipt = self.outbound(&receipt.id).await?;
            }
        };
        tokio::time::timeout(Duration::from_secs(12), wait)
            .await
            .map_err(|_| failed("GATEWAY_RECEIPT_PENDING"))?
    }

    fn verify_route(&self, route: &GatewayRoute) -> Result<(), ChannelError> {
        validate_id(&route.id)?;
        validate_id(&route.generation)?;
        if route.node_id != self.node_id() || route.app_id != self.app_id() || route.tenant_key != self.tenant_key() {
            return Err(failed("GATEWAY_ROUTE_MISMATCH"));
        }
        Ok(())
    }

    fn verify_receipt(&self, receipt: &GatewayOutbound, request: &GatewayOutboundRequest) -> Result<(), ChannelError> {
        if receipt.node_id != self.node_id()
            || receipt.route_id != request.route_id
            || receipt.generation != request.generation
            || receipt.idempotency_key != request.idempotency_key
        {
            return Err(failed("GATEWAY_RECEIPT_MISMATCH"));
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), ChannelError> {
    if id.is_empty()
        || id.len() > 512
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(failed("GATEWAY_IDENTIFIER_INVALID"));
    }
    Ok(())
}

fn failed(code: &'static str) -> ChannelError {
    ChannelError::ConnectionFailed(code.into())
}
