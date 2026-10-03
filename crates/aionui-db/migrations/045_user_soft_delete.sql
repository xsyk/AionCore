-- Soft delete for local accounts: keep the row (and every row that references
-- it) but hide it from login, listings and username uniqueness, so a deleted
-- user's username can be registered again while their data stays readable by
-- the super admin.
ALTER TABLE users ADD COLUMN deleted_at INTEGER;

DROP INDEX IF EXISTS idx_users_local_username;
CREATE UNIQUE INDEX idx_users_local_username
    ON users(username)
    WHERE user_type = 'local' AND username IS NOT NULL AND deleted_at IS NULL;
