#!/bin/sh
# 总线配置由会话生成在专用缓存目录，避免依赖系统 /usr/share/dbus-1。
# dbus-run-session 默认附带 --session，必须去掉，不能与私有配置同时传入。
for argument do
    shift
    if [ "$argument" != --session ]; then set -- "$@" "$argument"; fi
done
exec dbus-daemon --config-file="$XDG_CACHE_HOME/session.conf" "$@"
