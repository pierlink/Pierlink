-- 显式转换时冻结身份文件和主机密钥别名，迁移本身不读取 SSH 配置。
ALTER TABLE servers ADD COLUMN identity_files TEXT NOT NULL DEFAULT '[]';
ALTER TABLE servers ADD COLUMN identities_only INTEGER NOT NULL DEFAULT 0;
ALTER TABLE servers ADD COLUMN host_key_alias TEXT;
