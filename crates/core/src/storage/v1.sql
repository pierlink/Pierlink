-- 服务器
CREATE TABLE servers (
  id                TEXT PRIMARY KEY,
  name              TEXT NOT NULL,
  group_name        TEXT,
  favorite          INTEGER NOT NULL DEFAULT 0,
  sort_order        INTEGER NOT NULL DEFAULT 0,
  source            TEXT NOT NULL CHECK (source IN ('manual','ssh_config')),
  ssh_alias         TEXT,                    -- source=ssh_config 时必填
  host              TEXT,                    -- source=manual 时必填
  port              INTEGER CHECK (port BETWEEN 1 AND 65535),
  username          TEXT,
  auth_method       TEXT NOT NULL CHECK (auth_method IN ('key','agent','password','dedicated_key')),
  identity_file     TEXT,                    -- 只记录路径，不存私钥内容
  jump_server_id    TEXT REFERENCES servers(id) ON DELETE SET NULL,
  login_shell       TEXT,                    -- 空 = 服务器默认
  use_tmux          INTEGER NOT NULL DEFAULT 0,
  remember_password INTEGER NOT NULL DEFAULT 0,
  password_cred     TEXT,                    -- 凭据引用
  host_key_sha256   TEXT,                    -- 最近确认的指纹，仅用于展示
  vnc_autostart     INTEGER NOT NULL DEFAULT 0,
  notes             TEXT NOT NULL DEFAULT '',
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  CHECK ((source = 'ssh_config' AND ssh_alias IS NOT NULL) OR (source = 'manual' AND host IS NOT NULL))
);

-- 服务器端 Pierlink 实例（每台服务器最多一个）
CREATE TABLE server_instances (
  server_id         TEXT PRIMARY KEY REFERENCES servers(id) ON DELETE CASCADE,
  instance_id       TEXT NOT NULL,
  syncthing_version TEXT NOT NULL,
  arch              TEXT NOT NULL,
  state_dir         TEXT NOT NULL,
  socket_path       TEXT NOT NULL,
  sync_port         INTEGER NOT NULL,
  device_id         TEXT NOT NULL,           -- 服务器端 Syncthing 设备 ID
  apikey_cred       TEXT NOT NULL,
  gui_password_cred TEXT NOT NULL,
  autostart_consent INTEGER NOT NULL DEFAULT 0,
  deployed_at       INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
);

-- 服务器上保存的项目路径
CREATE TABLE project_paths (
  id          TEXT PRIMARY KEY,
  server_id   TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  remote_path TEXT NOT NULL,
  label       TEXT,
  UNIQUE (server_id, remote_path)
);

-- 同步任务（一个本地目录 + 一台或多台服务器）
CREATE TABLE sync_tasks (
  id                 TEXT PRIMARY KEY,
  folder_id          TEXT NOT NULL UNIQUE,
  name               TEXT NOT NULL,
  local_path         TEXT NOT NULL UNIQUE,   -- 规范化后的绝对路径
  conflict_policy    TEXT NOT NULL DEFAULT 'ask' CHECK (conflict_policy IN ('ask','newest')),
  collab_mode        INTEGER NOT NULL DEFAULT 0,
  watch_delay_s      INTEGER NOT NULL DEFAULT 10 CHECK (watch_delay_s BETWEEN 1 AND 60),
  versioning_enabled INTEGER NOT NULL DEFAULT 1,
  versioning_days    INTEGER NOT NULL DEFAULT 30,
  ignore_rules       TEXT NOT NULL,           -- 每行一条，Syncthing 忽略语法
  editor_id          TEXT REFERENCES editors(id) ON DELETE SET NULL,
  paused             INTEGER NOT NULL DEFAULT 1,  -- 新建时暂停，确认后开始
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);

-- 同步端点（任务里的每台服务器）
CREATE TABLE sync_endpoints (
  task_id     TEXT NOT NULL REFERENCES sync_tasks(id) ON DELETE CASCADE,
  server_id   TEXT NOT NULL REFERENCES servers(id) ON DELETE RESTRICT,
  remote_path TEXT NOT NULL,
  mode        TEXT NOT NULL DEFAULT 'sendreceive' CHECK (mode IN ('sendreceive','receiveonly')),
  PRIMARY KEY (task_id, server_id)
);

-- 外部编辑器
CREATE TABLE editors (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  exe_path     TEXT NOT NULL,
  open_args    TEXT NOT NULL,                -- JSON 字符串数组，含 {path}
  diff_args    TEXT,                         -- JSON 字符串数组，含 {left} {right}；NULL 表示不支持对比
  source       TEXT NOT NULL CHECK (source IN ('detected','custom')),
  show_in_menu INTEGER NOT NULL DEFAULT 0,
  is_default   INTEGER NOT NULL DEFAULT 0
);

-- 命令片段
CREATE TABLE snippets (
  id         TEXT PRIMARY KEY,
  title      TEXT NOT NULL,
  body       TEXT NOT NULL,
  group_name TEXT,                           -- NULL = 所有服务器通用
  sort_order INTEGER NOT NULL DEFAULT 0
);

-- 端口转发
CREATE TABLE port_forwards (
  id          TEXT PRIMARY KEY,
  server_id   TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL CHECK (kind IN ('local','remote','dynamic')),
  local_port  INTEGER NOT NULL CHECK (local_port BETWEEN 1 AND 65535),
  remote_host TEXT,
  remote_port INTEGER CHECK (remote_port BETWEEN 1 AND 65535),
  enabled     INTEGER NOT NULL DEFAULT 0
);

-- 设置（键值，值为 JSON）
CREATE TABLE settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
