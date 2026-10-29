# 软件源索引只在本次调用使用；所有查询使用同一组私有目录。
apt_command() {
    apt_program=$1; shift
    bounded 600 "$apt_program" -o "Dir::State::Lists=$apt_root/lists" -o "Dir::Cache=$apt_root/cache" \
        -o Debug::NoLocking=true -o Acquire::AllowInsecureRepositories=false \
        -o APT::Get::AllowUnauthenticated=false "$@"
}
# 不保存或返回软件源原始输出，其中可能包含代理口令；只保留固定的故障分类。
apt_attempt() {
    (
        apt_command "$@" 2>&1
        printf '%s' "$?" > "$work/apt-status"
    ) | awk '
        /No space left|not enough free space/ {reason="磁盘可用空间不足"}
        /Permission denied|Operation not permitted/ {reason="没有访问软件源或缓存的权限"}
        /Hash Sum mismatch|Size mismatch/ {reason="软件包校验失败"}
        /NO_PUBKEY|not signed|signatures|signature verification/ {reason="软件源签名校验失败"}
        /Could not resolve|Temporary failure resolving/ {reason="无法解析软件源域名"}
        /Could not connect|Connection failed|Connection refused/ {reason="无法连接软件源"}
        /timed out|Timeout/ {reason="连接软件源超时"}
        /404 |Unable to locate package|no installation candidate/ {reason="软件源中没有可用的软件包"}
        END {print reason}
    ' > "$work/apt-reason"
    apt_status=$(cat "$work/apt-status")
    apt_reason=$(cat "$work/apt-reason")
    if [ -z "$apt_reason" ]; then apt_reason="软件源操作失败，退出码 $apt_status"; fi
    [ "$apt_status" = 0 ]
}
package_error() {
    printf '{"error":"vnc.%s","source":"Apt","package":' "$1"; json "$2"
    printf ',"reason":'; json "$3"; printf '}'; exit 0
}
# depends 的顶层行才是递归展开的真实包；缩进的依赖、提供者和虚包均不直接下载。
dependency_names() {
    awk '/^[a-z0-9][a-z0-9+.-]*(:[a-z0-9][a-z0-9-]*)?$/ && !seen[$0]++ {print}'
}
missing_packages() {
    while IFS= read -r apt_package; do
        [ -n "$apt_package" ] || continue
        installed=$(dpkg-query -W -f='${Status}' "$apt_package" 2>/dev/null) || installed=
        [ "$installed" = 'install ok installed' ] || printf '%s\n' "$apt_package"
    done
}
# 先遍历必需依赖，再处理候选组；未选候选的子依赖不会进入闭包。
dependency_closure() {
    dependency_names < "$work/depends" > "$work/available"
    missing_packages < "$work/available" > "$work/missing"
    awk -v roots="$*" '
        FILENAME==ARGV[1] {available[$0]=1; installed[$0]=1; next}
        FILENAME==ARGV[2] {delete installed[$0]; next}
        /^[a-z0-9][a-z0-9+.-]*(:[a-z0-9][a-z0-9-]*)?$/ {parent=$0; group=0; next}
        /^[^ ]/ {parent=""; next}
        /^ [ |]*(Pre)?Depends: / {
            if(parent=="") next
            if(!group) {group=++groups; owner[group]=parent}
            name=$NF; virtual=(name ~ /^</)
            if(!virtual) candidate[group,++lengths[group]]=name
            last=group
            if($0 !~ /^ *\|/) group=0
            next
        }
        /^    [a-z0-9]/ && virtual && parent!="" {
            candidate[last,++lengths[last]]=$1
        }
        function add(name) {
            if(!selected[name] && !installed[name]) {selected[name]=1; queue[++tail]=name}
        }
        function satisfied(g, i,n) {
            for(i=1;i<=lengths[g];i++) {
                n=candidate[g,i]
                if(selected[n] || installed[n]) return 1
            }
            return 0
        }
        function choose(g, i,n) {
            if(satisfied(g)) return
            for(i=1;i<=lengths[g];i++) {
                n=candidate[g,i]
                if(available[n]) {add(n); return}
            }
            exit 1
        }
        END {
            n=split(roots,root," ")
            for(i=1;i<=n;i++) {if(!available[root[i]]) exit 1; add(root[i])}
            # 入口自身即使已安装也要展开，以便为专用副本补齐依赖。
            for(i=1;i<=n;i++) if(!selected[root[i]]) {selected[root[i]]=1; queue[++tail]=root[i]}
            head=1
            while(1) {
                while(head<=tail) {
                    p=queue[head++]
                    for(g=1;g<=groups;g++) if(owner[g]==p) {
                        if(lengths[g]==1) choose(g)
                        else pending[++pending_count]=g
                    }
                }
                if(!pending_count) break
                g=pending[1]
                for(i=1;i<pending_count;i++) pending[i]=pending[i+1]
                pending_count--
                choose(g)
            }
            for(i=1;i<=tail;i++) print queue[i]
        }
    ' "$work/available" "$work/missing" "$work/depends"
}
apt_unpack() {
    command -v apt-get >/dev/null && command -v apt-cache >/dev/null && command -v dpkg-query >/dev/null && command -v dpkg-deb >/dev/null || fail light_unavailable '需要 apt-get、apt-cache 和 dpkg'
    apt_root=$HOME/.cache/pierlink/apt
    private_dir "$apt_root"
    mkdir "$apt_root/.lock" || fail locked
    apt_private=true
    safe_path "$apt_root/lists"; safe_path "$apt_root/cache"
    mkdir -p "$apt_root/lists/partial" "$apt_root/cache/archives/partial" || fail permission_denied
    apt_attempt apt-get update -o APT::Update::Error-Mode=any || fail download_failed "软件源索引更新失败：$apt_reason"
    apt_command apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts --no-breaks --no-replaces --no-enhances "$@" > "$work/depends" 2>/dev/null || fail download_failed '无法计算发行版依赖'
    dependency_closure "$@" > "$work/closure" || fail download_failed '依赖组中没有可用的软件包'
    missing_packages < "$work/closure" > "$work/packages"
    # 入口程序保留自己的副本；其余依赖（包括字体和总线）优先复用系统已装包。
    printf '%s\n' "$version_package" >> "$work/packages"
    printf '%s\n' "$companion_package" >> "$work/packages"
    sort -u "$work/packages" > "$work/selected"
    total_bytes=0
    : > "$work/plan"
    while IFS= read -r apt_package; do
        # print-uris 只读本次已校验的索引，不开始下载；下载仍使用同一索引。
        apt_command apt-get --print-uris download "$apt_package" > "$work/uri" 2>/dev/null || package_error download_failed "$apt_package" '软件源中没有可下载的包'
        size=$(awk 'NF>=4 && $3 ~ /^[0-9]+$/ {n++;size=$3} END {if(n==1) print size}' "$work/uri")
        case "$size" in ''|*[!0-9]*) package_error invalid_package "$apt_package" '软件源没有提供唯一的下载大小';; esac
        if [ "${#size}" -gt 10 ]; then
            printf '{"error":"vnc.download_limit","limit_bytes":"314572800","total_bytes":'; json "$size"
            printf ',"reason":"单个依赖下载超过 300 MiB"}'; exit 0
        fi
        total_bytes=$((total_bytes+size))
        if [ "$total_bytes" -gt 314572800 ]; then
            printf '{"error":"vnc.download_limit","limit_bytes":"314572800","total_bytes":"%s","reason":"依赖下载总量超过 300 MiB"}' "$total_bytes"; exit 0
        fi
        printf '%s %s\n' "$apt_package" "$size" >> "$work/plan"
    done < "$work/selected"
    downloaded_bytes=0
    mkdir "$work/download" || fail permission_denied
    while read -r apt_package size; do
        cd "$work/download" || fail permission_denied
        apt_attempt apt-get download "$apt_package" || package_error download_failed "$apt_package" "$apt_reason"
        cd "$work" || fail permission_denied
        set -- "$work/download"/*.deb
        [ "$#" = 1 ] && [ -f "$1" ] || package_error invalid_package "$apt_package" '没有得到唯一的软件包'
        [ "$(wc -c < "$1")" = "$size" ] || package_error invalid_package "$apt_package" '软件包大小与索引不符'
        actual=$(dpkg-deb -f "$1" Package) || package_error invalid_package "$apt_package" '无法读取软件包名称'
        [ "$actual" = "${apt_package%%:*}" ] || package_error invalid_package "$apt_package" '软件包名称与请求不符'
        if [ "$actual" = "$version_package" ]; then package_version=$(dpkg-deb -f "$1" Version); fi
        bounded 60 dpkg-deb -x "$1" "$work/unpack" >/dev/null 2>&1 || package_error invalid_package "$apt_package" '软件包解压失败'
        downloaded_bytes=$((downloaded_bytes+size))
        rm -f -- "$1"
    done < "$work/plan"
    case "$package_version" in ''|*[!a-zA-Z0-9.+:~_-]*) fail invalid_package '软件包版本不正确';; esac
    # 版本中的 epoch 分隔符不能直接出现在 PATH 和动态库搜索路径里。
    version=apt-$(printf %s "$package_version" | tr ':' '_')
}
