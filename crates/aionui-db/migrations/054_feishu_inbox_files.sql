-- Preserve deployed inbox rows and atomically persist image paths with text.
ALTER TABLE feishu_inbox ADD COLUMN files TEXT NOT NULL DEFAULT '[]';
