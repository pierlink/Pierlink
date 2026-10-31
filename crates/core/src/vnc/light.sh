source=apt
binroot=$HOME/.local/share/pierlink/vnc-light
private_dir "$binroot"
for installed in "$binroot"/*/.pierlink-created; do
    [ -f "$installed" ] || continue
    prefix=${installed%/.pierlink-created}
    safe_path "$prefix"
    if [ -x "$prefix/session" ]; then
        printf '{"prefix":'; json "$prefix"; printf ',"downloaded_bytes":0}'; exit 0
    fi
done
work=$(mktemp -d "$binroot/.acquire-XXXXXXXX") || fail permission_denied
apt_private=false
cleanup_light() {
    rm -rf -- "$work"
    if [ "$apt_private" = true ]; then
        rm -rf -- "$apt_root/lists" "$apt_root/cache"
        rmdir -- "$apt_root/.lock" 2>/dev/null || :
    fi
}
trap cleanup_light 0
trap 'exit 1' HUP INT TERM
mkdir "$work/unpack" || fail permission_denied
version_package=openbox
companion_package=xterm
# 字体与总线不依赖 recommends；补充用户自行放置 Firefox 所需的 GTK、NSS 和 D-Bus 库，不下载浏览器。
# libgtk-3-bin 会依赖本发行版对应的 GTK 库，也覆盖使用 t64 包名的发行版。
apt_unpack openbox xterm dbus dbus-bin dbus-daemon fontconfig fonts-dejavu-core fonts-wqy-microhei xauth x11-xkb-utils xkb-data libgtk-3-bin libdbus-glib-1-2 libnss3
rm -rf -- "$apt_root/lists" "$apt_root/cache"
rmdir -- "$apt_root/.lock"
apt_private=false
prefix_environment "$work/unpack"
for executable in openbox xterm dbus-run-session dbus-daemon; do
    executable_path=$(command -v "$executable") || package_error invalid_package "$executable" 'vnc.program_missing' '系统和软件包中均缺少必需程序'
    libs=$(ldd "$executable_path" 2>&1 | awk '/not found/ {print $1}')
    [ -z "$libs" ] || package_error missing_libs "$executable" "vnc.missing_libraries" "缺少动态库：$libs"
done
prefix=$binroot/$version
mkdir -p "$work/unpack/etc/fonts" "$work/unpack/etc/xdg/openbox" || fail permission_denied
# 不修改用户原有配置；前缀内的配置只被专用会话显式读取。
cat > "$work/unpack/etc/fonts/pierlink.conf" <<EOF
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
  <dir>/usr/share/fonts</dir>
  <dir>$(xml_escape "$prefix/usr/share/fonts")</dir>
  <cachedir prefix="xdg">fontconfig</cachedir>
</fontconfig>
EOF
cat > "$work/unpack/etc/xdg/openbox/pierlink-menu.xml" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<openbox_menu xmlns="http://openbox.org/3.4/menu">
  <menu id="root-menu" label="Pierlink">
    <item label="Terminal / 终端"><action name="Execute"><command>xterm -u8 -fa monospace -fs 11</command></action></item>
    <item label="Log out / 注销"><action name="Exit"/></item>
  </menu>
</openbox_menu>
EOF
cat > "$work/unpack/etc/xdg/openbox/pierlink-rc.xml" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<openbox_config xmlns="http://openbox.org/3.4/rc">
  <theme><name>Clearlooks</name><titleLayout>NLIMC</titleLayout></theme>
  <menu><file>$(xml_escape "$prefix/etc/xdg/openbox/pierlink-menu.xml")</file></menu>
  <mouse>
    <context name="Root"><mousebind button="Right" action="Press"><action name="ShowMenu"><menu>root-menu</menu></action></mousebind></context>
    <context name="Titlebar">
      <mousebind button="Left" action="Press"><action name="Focus"/><action name="Raise"/></mousebind>
      <mousebind button="Left" action="Drag"><action name="Move"/></mousebind>
      <mousebind button="Left" action="DoubleClick"><action name="ToggleMaximize"/></mousebind>
    </context>
    <context name="Frame">
      <mousebind button="A-Left" action="Drag"><action name="Move"/></mousebind>
      <mousebind button="A-Right" action="Drag"><action name="Resize"/></mousebind>
    </context>
    <context name="Client"><mousebind button="Left" action="Press"><action name="Focus"/><action name="Raise"/></mousebind></context>
    <context name="Close"><mousebind button="Left" action="Click"><action name="Close"/></mousebind></context>
    <context name="Maximize"><mousebind button="Left" action="Click"><action name="ToggleMaximize"/></mousebind></context>
    <context name="Iconify"><mousebind button="Left" action="Click"><action name="Iconify"/></mousebind></context>
  </mouse>
  <keyboard><keybind key="A-Tab"><action name="NextWindow"/></keybind><keybind key="C-A-t"><action name="Execute"><command>xterm -u8 -fa monospace -fs 11</command></action></keybind></keyboard>
</openbox_config>
EOF
printf '%s\n' "$light_session" > "$work/unpack/session"
printf '%s\n' "$light_dbus" > "$work/unpack/dbus-daemon"
chmod 700 "$work/unpack/session" "$work/unpack/dbus-daemon" || fail permission_denied
install_prefix
printf '{"prefix":'; json "$prefix"
printf ',"downloaded_bytes":%s}' "$downloaded_bytes"
