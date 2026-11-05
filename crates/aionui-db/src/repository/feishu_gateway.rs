use crate::DbError;
use crate::models::{FeishuBindingRow, FeishuGatewayDeliveryRow, FeishuGatewayInteractionRow, FeishuGatewayRouteRow};

#[async_trait::async_trait]
pub trait IFeishuGatewayRepository: Send + Sync {
    async fn ensure_identity(&self, node_id: &str, app_id: &str, tenant_key: &str) -> Result<(), DbError>;
    async fn prepare_route(&self, route: &FeishuGatewayRouteRow) -> Result<(), DbError>;
    async fn activate_route(&self, route_id: &str, generation: &str) -> Result<FeishuBindingRow, DbError>;
    async fn fence_route(&self, route_id: &str, generation: &str) -> Result<(), DbError>;
    async fn route_by_generation(&self, generation: &str) -> Result<Option<FeishuGatewayRouteRow>, DbError>;
    async fn list_routes(&self) -> Result<Vec<FeishuGatewayRouteRow>, DbError>;
    async fn enqueue_delivery(
        &self,
        id: &str,
        route_id: &str,
        generation: &str,
        kind: &str,
        payload: &str,
    ) -> Result<(), DbError>;
    async fn pending_deliveries(&self) -> Result<Vec<FeishuGatewayDeliveryRow>, DbError>;
    /// False means the receipt is already complete or was attempted by another
    /// process epoch; neither case grants permission to execute it again.
    async fn start_delivery(&self, id: &str, epoch: &str) -> Result<bool, DbError>;
    async fn complete_delivery(&self, id: &str, outcome: &str) -> Result<(), DbError>;
    async fn save_interaction(
        &self,
        token: &str,
        binding_id: &str,
        payload: &str,
        expires_at: i64,
    ) -> Result<(), DbError>;
    async fn list_interactions(&self, now: i64) -> Result<Vec<FeishuGatewayInteractionRow>, DbError>;
    async fn delete_interaction(&self, token: &str) -> Result<(), DbError>;
}
