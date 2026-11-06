//! Composition adapter: Auth owns identity admission, Channel owns node transport.
use super::state::RouterBuildError;
use crate::services::AppServices;
use aionui_api_types::{
    FeishuGatewayIdentityBeginRequest, FeishuGatewayIdentityExchange, FeishuGatewayIdentityExchangeRequest,
    FeishuGatewayIdentityTransaction,
};
use aionui_auth::feishu::{
    FeishuLoginError,
    gateway_identity::{FeishuGatewayIdentityBackend, GatewayIdentityTrust},
    gateway_login::GatewayIdentityLogin,
};
use aionui_channel::gateway::GatewayClient;
use std::sync::Arc;

struct Backend(Arc<GatewayClient>);

#[async_trait::async_trait]
impl FeishuGatewayIdentityBackend for Backend {
    async fn begin(
        &self,
        request: FeishuGatewayIdentityBeginRequest,
    ) -> Result<FeishuGatewayIdentityTransaction, FeishuLoginError> {
        self.0
            .begin_identity(request)
            .await
            .map_err(|_| upstream("FEISHU_GATEWAY_BEGIN_FAILED"))
    }
    async fn exchange(
        &self,
        request: FeishuGatewayIdentityExchangeRequest,
    ) -> Result<FeishuGatewayIdentityExchange, FeishuLoginError> {
        self.0
            .exchange_identity(request)
            .await
            .map_err(|_| upstream("FEISHU_GATEWAY_EXCHANGE_FAILED"))
    }
}

fn upstream(code: &'static str) -> FeishuLoginError {
    tracing::warn!(code, "feishu: identity gateway request failed");
    FeishuLoginError::Upstream(code.into())
}

pub(super) fn build(
    services: &AppServices,
    gateway: Option<&Arc<GatewayClient>>,
) -> Result<Option<Arc<GatewayIdentityLogin>>, RouterBuildError> {
    let Some(gateway) = gateway else {
        return Ok(None);
    };
    let Some(identity) = gateway.identity_config() else {
        tracing::info!(
            code = "FEISHU_GATEWAY_LOGIN_LEGACY_MODE",
            "feishu: node identity trust is not configured; login retains direct OAuth/H5 mode"
        );
        return Ok(None);
    };
    let trust = GatewayIdentityTrust {
        node_id: gateway.node_id().into(),
        app_id: gateway.app_id().into(),
        tenant_key: gateway.tenant_key().into(),
        verification_key: identity.verification_key.clone(),
        key_id: identity.key_id.clone(),
    };
    let service = GatewayIdentityLogin::new(
        trust,
        Arc::new(Backend(gateway.clone())),
        Arc::new(aionui_db::SqliteFeishuIdentityTransactionRepository::new(
            services.database.pool().clone(),
        )),
    )
    .map_err(|error| {
        RouterBuildError::new(
            "router.auth.feishu_gateway",
            "invalid pinned Feishu gateway identity configuration",
        )
        .with_source(error)
    })?;
    tracing::info!(
        code = "FEISHU_GATEWAY_IDENTITY_ENABLED",
        "feishu: OAuth/H5 use the pinned identity gateway"
    );
    Ok(Some(Arc::new(service)))
}
