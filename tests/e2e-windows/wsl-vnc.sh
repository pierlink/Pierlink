#!/bin/sh
# 在 CI 的 WSL Ubuntu 里以用户 pl 运行：起两个临时 VNC 桌面给服务器桌面的端到端测试用，一直运行到被结束。
#   :5（VNC_A）只听本机 TCP 5900+显示号，要 VNC 密码（参数 1），底色蓝 #4080ff
#   :6（VNC_B）只开 Unix 套接字（0600），不要密码，底色绿 #30c060
# 两个都带专用 -auth cookie。准备好后输出一行 ready。显示号可用 VNC_A、VNC_B 改（本机试跑时用 :9x）。
set -eu
pw=$1
a=${VNC_A:-5}
b=${VNC_B:-6}
d=$HOME/.vnc-e2e
mkdir -p "$d"
chmod 700 "$d"
printf '%s\n' "$pw" | vncpasswd -f > "$d/passwd"
chmod 600 "$d/passwd"
for n in $a $b; do
  rm -f "$d/x$n.auth"
  xauth -q -f "$d/x$n.auth" add ":$n" MIT-MAGIC-COOKIE-1 "$(mcookie)" 2>/dev/null
done
Xtigervnc ":$a" -localhost=1 -rfbport "$((5900 + a))" -SecurityTypes VncAuth -PasswordFile "$d/passwd" -auth "$d/x$a.auth" \
  -geometry 800x600 -desktop "e2e-$a" >"$d/x$a.log" 2>&1 &
pa=$!
Xtigervnc ":$b" -rfbport -1 -rfbunixpath "$d/x$b.sock" -rfbunixmode 0600 -SecurityTypes None -auth "$d/x$b.auth" \
  -geometry 800x600 -desktop "e2e-$b" >"$d/x$b.log" 2>&1 &
pb=$!
trap 'kill $pa $pb 2>/dev/null || true' EXIT INT TERM
i=0
until [ -S "$d/x$b.sock" ] && DISPLAY=":$a" XAUTHORITY="$d/x$a.auth" xsetroot -solid '#4080ff' 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -gt 150 ]; then
    echo "VNC 桌面 15 秒内没有起来"
    tail -n 20 "$d/x$a.log" "$d/x$b.log"
    exit 1
  fi
  sleep 0.1
done
DISPLAY=":$b" XAUTHORITY="$d/x$b.auth" xsetroot -solid '#30c060'
echo ready
wait
