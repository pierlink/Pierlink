-- 旧任务的本机方式默认收发。
ALTER TABLE sync_tasks ADD COLUMN local_mode TEXT NOT NULL DEFAULT 'sendreceive'
  CHECK (local_mode IN ('sendreceive', 'receiveonly'));

-- 最近事件按任务级联删除；时间相同则以写入顺序排序。
CREATE TABLE sync_events (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id   TEXT NOT NULL REFERENCES sync_tasks(id) ON DELETE CASCADE,
  folder_id TEXT NOT NULL,
  time_ms   INTEGER NOT NULL,
  kind      TEXT NOT NULL,
  device    TEXT,
  path      TEXT,
  detail    TEXT
);
CREATE INDEX sync_events_task_time ON sync_events(task_id, time_ms DESC, id DESC);
