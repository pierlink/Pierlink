-- 只有内置 SSH 一种连接方式：应用里添加的服务器全部改成内置。
-- 从 ssh config 导入、按别名连接的记录要读配置文件才能转，由程序启动时处理（转不了的保留并写明原因）。
UPDATE servers SET connection_engine = 'builtin', engine_note = NULL
WHERE connection_engine = 'openssh' AND source = 'manual' AND ssh_alias IS NULL;
