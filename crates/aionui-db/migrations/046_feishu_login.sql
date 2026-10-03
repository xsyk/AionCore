-- Feishu (Lark) OAuth web login.
--
-- Single-row configuration edited by the super admin. The app secret is stored
-- AES-256-GCM encrypted (same key as other at-rest credentials).
CREATE TABLE IF NOT EXISTS feishu_login_config (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    enabled         INTEGER NOT NULL DEFAULT 0,
    app_id          TEXT    NOT NULL DEFAULT '',
    app_secret_enc  TEXT,
    tenant_key      TEXT,
    public_base_url TEXT    NOT NULL DEFAULT '',
    api_base        TEXT,
    accounts_base   TEXT,
    updated_at      INTEGER NOT NULL
);

-- Feishu users are local rows keyed by external_user_id = 'feishu:<union_id>'.
-- A soft-deleted Feishu user who logs in again gets a fresh account, so the
-- external identity must only be unique among live rows.
DROP INDEX IF EXISTS idx_users_external_user;
CREATE UNIQUE INDEX idx_users_external_user
    ON users(user_type, external_user_id)
    WHERE external_user_id IS NOT NULL AND deleted_at IS NULL;
