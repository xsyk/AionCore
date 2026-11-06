-- Browser-bound gateway login transactions are independent of local accounts.
-- Only hashes are stored; raw values stay in the OAuth HttpOnly cookie or the
-- short-lived H5 request memory. A consumed transaction cannot issue a second
-- local session.
CREATE TABLE IF NOT EXISTS feishu_identity_transactions (
 transaction_id TEXT PRIMARY KEY NOT NULL CHECK(length(transaction_id)=43),
 node_id TEXT NOT NULL CHECK(length(node_id)>0),
 kind TEXT NOT NULL CHECK(kind IN ('oauth','h5')),
 nonce_hash TEXT NOT NULL CHECK(length(nonce_hash)=43),
 browser_binding_hash TEXT NOT NULL CHECK(length(browser_binding_hash)=43),
 redirect_uri TEXT NOT NULL CHECK(length(redirect_uri)>0),
 expires_at INTEGER NOT NULL CHECK(expires_at>0),
 consumed_at INTEGER CHECK(consumed_at IS NULL OR consumed_at>0)
);
CREATE INDEX IF NOT EXISTS feishu_identity_transaction_expiry
 ON feishu_identity_transactions(expires_at);
