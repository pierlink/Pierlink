-- 端口转发支持自定义排序；已有条目保持默认顺序。
ALTER TABLE port_forwards ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;
