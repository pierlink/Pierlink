-- 命令历史按次保存，只保留在本机数据库及其备份中。
CREATE TABLE command_history (
  id INTEGER PRIMARY KEY,
  scope TEXT NOT NULL,
  cwd TEXT,
  command TEXT NOT NULL,
  exit_code INTEGER,
  used_at_ms INTEGER NOT NULL
);
CREATE INDEX command_history_scope_time ON command_history(scope, used_at_ms);

-- 1 表示跟随全局命令预测设置，0 表示关闭。
ALTER TABLE servers ADD COLUMN command_prediction INTEGER NOT NULL DEFAULT 1;
