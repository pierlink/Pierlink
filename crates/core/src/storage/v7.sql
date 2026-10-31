-- 手机凭据只保存 SHA-256 摘要；权限彼此独立，默认只读。
CREATE TABLE mobile_devices (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 80),
    token_hash BLOB NOT NULL UNIQUE CHECK(length(token_hash) = 32),
    can_view INTEGER NOT NULL DEFAULT 1 CHECK(can_view IN (0, 1)),
    can_input INTEGER NOT NULL DEFAULT 0 CHECK(can_input IN (0, 1)),
    can_confirm INTEGER NOT NULL DEFAULT 0 CHECK(can_confirm IN (0, 1)),
    created_at INTEGER NOT NULL,
    last_active_at INTEGER NOT NULL,
    revoked_at INTEGER
);
