-- 显示姓名与登录名分离，旧账号保留原登录名、状态及删除标记。
-- SQLite不支持ADD COLUMN IF NOT EXISTS，由既有SQLx迁移版本保证只执行一次。
ALTER TABLE users ADD COLUMN display_name TEXT;
