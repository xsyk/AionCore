CREATE TABLE IF NOT EXISTS feishu_conversation_bindings (
    id TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL UNIQUE REFERENCES conversations(id) ON DELETE CASCADE,
    owner_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    connector_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app_id TEXT NOT NULL,
    tenant_key TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    chat_type TEXT NOT NULL CHECK (chat_type IN ('p2p', 'group')),
    bound_at INTEGER NOT NULL,
    UNIQUE (app_id, tenant_key, chat_id)
);

CREATE TABLE IF NOT EXISTS feishu_conversation_binding_codes (
    code TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL UNIQUE REFERENCES conversations(id) ON DELETE CASCADE,
    owner_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS feishu_message_receipts (
    app_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (app_id, message_id)
);
