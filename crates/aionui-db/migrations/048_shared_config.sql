-- AionEasiful 1.0.1: shared configuration.

-- Custom agents become server-wide like the builtin catalog rows: the super
-- admin manages them and every user can run them.
UPDATE agent_metadata SET user_id = NULL WHERE agent_source = 'custom' AND user_id IS NOT NULL;

-- Server-wide settings that are not per user (e.g. the image generation model).
CREATE TABLE IF NOT EXISTS global_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Carry over the super admin's image generation choice, if one was saved.
INSERT OR IGNORE INTO global_settings (key, value, updated_at)
SELECT 'tools.imageGeneration',
       json_object(
           'provider_id', json_extract(value, '$.id'),
           'model', json_extract(value, '$.use_model'),
           'enabled', CASE WHEN json_extract(value, '$.switch') = 1 THEN json('true') ELSE json('false') END
       ),
       updated_at
FROM client_preferences
WHERE user_id = 'system_default_user'
  AND key = 'tools.imageGenerationModel'
  AND json_valid(value)
  AND json_extract(value, '$.id') IS NOT NULL
  AND json_extract(value, '$.use_model') IS NOT NULL;
