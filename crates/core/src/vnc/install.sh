prefix_environment "$work/unpack"
[ -x "$binary" ] && [ ! -L "$binary" ] || fail invalid_package
command -v ldd >/dev/null || fail missing_tool '缺少 ldd，无法检查动态库'
libs=$(ldd "$binary" 2>&1 | awk '/not found/ {print $1}')
if [ -n "$libs" ]; then printf '{"error":"vnc.missing_libs","libs":'; json "$libs"; printf '}'; exit 0; fi
output=$(bounded 5 "$binary" -version 2>&1) || fail not_tigervnc
case "$output" in *TigerVNC*) :;; *) fail not_tigervnc;; esac
valid=$(printf %s "$output" | awk 'match($0,/[0-9]+\.[0-9]+/) {v=substr($0,RSTART,RLENGTH);split(v,a,".");if(a[1]>1 || (a[1]==1 && a[2]>=9)) print "yes";exit}')
[ "$valid" = yes ] || fail xvnc_too_old
install_prefix
printf '{"path":'; json "$binroot/$version/$relative"
printf ',"version":'; json "$output"
printf ',"downloaded_bytes":%s}' "${downloaded_bytes:-0}"
