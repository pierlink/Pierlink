private_dir "$binroot"
prune_uploads
work=$(mktemp -d "$binroot/.acquire-XXXXXXXX") || fail permission_denied
apt_private=false
cleanup_package() {
    rm -rf -- "$work"
    if [ "${install_locked:-false}" = true ]; then rmdir -- "$binroot/.install-lock" 2>/dev/null || :; fi
    if [ "$apt_private" = true ]; then
        rm -rf -- "$apt_root/lists" "$apt_root/cache"
        rmdir -- "$apt_root/.lock" 2>/dev/null || :
    fi
}
trap cleanup_package 0
trap 'exit 1' HUP INT TERM
mkdir "$work/unpack" || fail permission_denied
if [ "$source" = apt ]; then
    version_package=tigervnc-standalone-server
    companion_package=tigervnc-common
    apt_unpack tigervnc-standalone-server tigervnc-common xauth x11-xkb-utils xkb-data
    binary=$work/unpack/usr/bin/Xtigervnc
    relative=usr/bin/Xtigervnc
else
    package=$work/package.tar.gz
    if command -v curl >/dev/null; then
        bounded 125 curl -fL --connect-timeout 15 --max-time 120 --max-filesize 67108864 --proto '=https' --proto-redir '=https' -o "$package" "$url" >/dev/null 2>&1 || fail download_failed 'vnc.server_download_failed' '服务器下载失败'
    elif command -v wget >/dev/null; then
        bounded 120 sh -c 'ulimit -f 131072; exec wget "$@"' sh --https-only --timeout=30 --tries=1 -O "$package" "$url" >/dev/null 2>&1 || fail download_failed 'vnc.server_download_failed' '服务器下载失败'
    else fail download_failed 'vnc.download_tool_missing' '服务器没有 curl 或 wget'; fi
    [ "$(wc -c < "$package")" -le 67108864 ] || fail download_failed 'vnc.package_size_limit' '下载包超过大小限制'
    downloaded_bytes=$(wc -c < "$package")
    [ "$(sha256_of < "$package")" = "$expected" ] || fail sha_mismatch
    # 只提取固定的两个普通文件，不将包内路径交给 tar 写入文件系统。
    prefix=tigervnc-1.16.2.x86_64
    for name in Xvnc vncpasswd; do
        tar -xOzf "$package" "$prefix/usr/bin/$name" > "$work/unpack/$name" || fail invalid_package
        [ -s "$work/unpack/$name" ] || fail invalid_package
        chmod 755 "$work/unpack/$name" || fail permission_denied
    done
    version=1.16.2
    binary=$work/unpack/Xvnc
    relative=Xvnc
fi

if [ "$apt_private" = true ]; then
    rm -rf -- "$apt_root/lists" "$apt_root/cache"
    rmdir -- "$apt_root/.lock"
    apt_private=false
fi
