-- A node credential belongs to one gateway application and enterprise. Tokens
-- remain in a private deployment file, never in this identity projection.
CREATE TABLE IF NOT EXISTS feishu_gateway_identity (
 id INTEGER PRIMARY KEY CHECK (id=1),
 node_id TEXT NOT NULL,
 app_id TEXT NOT NULL,
 tenant_key TEXT NOT NULL
);

-- Prepared routes deliberately do not create a legacy binding. Only an active
-- projection may enter the existing conversation inbox/outbox execution path.
-- Keep these rows independent of conversation deletion so revocation can still
-- fence an accepted generation and retain its audit identity.
CREATE TABLE IF NOT EXISTS feishu_gateway_routes (
 route_id TEXT PRIMARY KEY NOT NULL,
 generation TEXT NOT NULL UNIQUE,
 node_id TEXT NOT NULL,
 conversation_id TEXT NOT NULL,
 owner_user_id TEXT NOT NULL,
 owner_union_id TEXT NOT NULL,
 app_id TEXT NOT NULL,
 tenant_key TEXT NOT NULL,
 chat_id TEXT NOT NULL,
 chat_type TEXT NOT NULL CHECK (chat_type IN ('p2p','group')),
 status TEXT NOT NULL CHECK (status IN ('prepared','active','fenced')),
 prepared_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS feishu_gateway_active_conversation
 ON feishu_gateway_routes(conversation_id) WHERE status='active';
CREATE UNIQUE INDEX IF NOT EXISTS feishu_gateway_active_chat
 ON feishu_gateway_routes(app_id,tenant_key,chat_id) WHERE status='active';

CREATE TABLE IF NOT EXISTS feishu_gateway_baseline_messages (
 route_id TEXT NOT NULL REFERENCES feishu_gateway_routes(route_id) ON DELETE CASCADE,
 message_id TEXT NOT NULL,
 PRIMARY KEY(route_id,message_id)
);

-- The gateway delivery receipt outlives local bindings. Acknowledgement and
-- application handling are separate; a previous process's uncertain attempt
-- must not become an automatic second execution.
CREATE TABLE IF NOT EXISTS feishu_gateway_deliveries (
 id TEXT PRIMARY KEY NOT NULL,
 route_id TEXT NOT NULL,
 generation TEXT NOT NULL,
 kind TEXT NOT NULL CHECK (kind IN ('binding.prepare','binding.revoke','message','interaction')),
 payload TEXT NOT NULL,
 attempt_epoch TEXT,
 done INTEGER NOT NULL DEFAULT 0 CHECK (done IN (0,1)),
 outcome TEXT
);
CREATE INDEX IF NOT EXISTS feishu_gateway_delivery_pending ON feishu_gateway_deliveries(done);

CREATE TABLE IF NOT EXISTS feishu_gateway_interactions (
 token TEXT PRIMARY KEY NOT NULL,
 binding_id TEXT NOT NULL REFERENCES feishu_conversation_bindings(id) ON DELETE CASCADE,
 payload TEXT NOT NULL,
 expires_at INTEGER NOT NULL
);
