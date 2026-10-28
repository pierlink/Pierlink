-- 保留链接型别名的原配置；应用内记录切为内置，实际功能仍由开关逐步启用。
ALTER TABLE servers ADD COLUMN connection_engine TEXT NOT NULL DEFAULT 'openssh'
    CHECK (connection_engine IN ('builtin', 'openssh'));
ALTER TABLE servers ADD COLUMN engine_note TEXT;

UPDATE servers SET connection_engine = 'builtin' WHERE source = 'manual';
UPDATE servers SET engine_note = '从 ssh config 导入，按原配置连接' WHERE source = 'ssh_config';
