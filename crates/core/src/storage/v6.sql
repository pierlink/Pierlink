-- 事件作者与传输设备分开保存；历史事件没有可靠来源，保持为空。
ALTER TABLE sync_events ADD COLUMN by_device TEXT;
