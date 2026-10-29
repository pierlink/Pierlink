#!/bin/sh
# 窗口管理器退出即注销，终端可随时从右键菜单重新打开。
prefix=${0%/*}
xterm -u8 -fa monospace -fs 11 -title 'Pierlink 终端' &
terminal=$!
trap 'kill "$terminal" 2>/dev/null; wait "$terminal" 2>/dev/null' 0
trap 'exit 0' TERM HUP INT
openbox --config-file "$prefix/etc/xdg/openbox/pierlink-rc.xml"
