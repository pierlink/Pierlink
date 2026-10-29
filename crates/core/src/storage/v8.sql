-- 记录配对浏览器及最近认证来源；升级时旧设备的两列均为空。
ALTER TABLE mobile_devices ADD COLUMN user_agent TEXT;
ALTER TABLE mobile_devices ADD COLUMN last_ip TEXT;
