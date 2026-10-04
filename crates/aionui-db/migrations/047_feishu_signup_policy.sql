-- Feishu login: how a first-time Feishu user is admitted.
-- 'approval' creates the account disabled until a super admin enables it;
-- 'open' activates it immediately (the behavior before this migration).
ALTER TABLE feishu_login_config
    ADD COLUMN signup_policy TEXT NOT NULL DEFAULT 'approval'
    CHECK (signup_policy IN ('approval', 'open'));
