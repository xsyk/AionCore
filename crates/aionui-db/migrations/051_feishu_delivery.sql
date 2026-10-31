-- Durable transport queues: receipts become done only after local acceptance or remote acknowledgement.
CREATE TABLE IF NOT EXISTS feishu_inbox (
 id TEXT PRIMARY KEY NOT NULL,
 binding_id TEXT NOT NULL REFERENCES feishu_conversation_bindings(id) ON DELETE CASCADE,
 content TEXT NOT NULL,
 attempt_epoch TEXT,
 done INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS feishu_outbox (
 id TEXT PRIMARY KEY NOT NULL,
 binding_id TEXT NOT NULL REFERENCES feishu_conversation_bindings(id) ON DELETE CASCADE,
 message_id TEXT NOT NULL,
 part INTEGER NOT NULL,
 content TEXT NOT NULL,
 platform_message_id TEXT,
 first_attempt_at INTEGER,
 delivery_state TEXT NOT NULL DEFAULT 'pending' CHECK(delivery_state IN ('pending','unknown')),
 UNIQUE(binding_id, message_id, part)
);

-- Snapshot all existing message IDs atomically with each new binding. Clock
-- timestamps are not a safe history boundary during concurrent writes/rebinds.
CREATE TABLE IF NOT EXISTS feishu_binding_baseline_messages (
 binding_id TEXT NOT NULL REFERENCES feishu_conversation_bindings(id) ON DELETE CASCADE,
 message_id TEXT NOT NULL,
 PRIMARY KEY(binding_id,message_id)
);
INSERT OR IGNORE INTO feishu_binding_baseline_messages(binding_id,message_id)
 SELECT b.id,m.id FROM feishu_conversation_bindings b JOIN messages m ON m.conversation_id=b.conversation_id;

-- Raw transport acceptance precedes the success acknowledgement. Kept outside
-- binding FKs because /bind itself creates the binding.
CREATE TABLE IF NOT EXISTS feishu_ingress_events (
 id TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS feishu_ingress_pending ON feishu_ingress_events(done);
CREATE INDEX IF NOT EXISTS feishu_outbox_pending ON feishu_outbox(delivery_state,platform_message_id);
CREATE INDEX IF NOT EXISTS feishu_inbox_pending ON feishu_inbox(done);
