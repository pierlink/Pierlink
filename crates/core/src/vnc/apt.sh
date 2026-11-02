# 软件源索引只在本次调用使用；所有查询使用同一组私有目录。不下载软件包说明的翻译（Ubuntu 上能少占一百多 MB）。
apt_command() {
    apt_program=$1; shift
    bounded 600 "$apt_program" -o "Dir::State::Lists=$apt_root/lists" -o "Dir::Cache=$apt_root/cache" \
        -o Debug::NoLocking=true -o Acquire::AllowInsecureRepositories=false \
        -o APT::Get::AllowUnauthenticated=false -o Acquire::Languages=none "$@"
}
# 私有软件源索引按 256 MiB 算空间（Ubuntu 打开全部组件、不含翻译约 200 MB）。
apt_index_kib=262144
# 不保存或返回软件源原始输出，其中可能包含代理口令；只保留固定的故障分类。
apt_attempt() {
    (
        apt_command "$@" 2>&1
        printf '%s' "$?" > "$work/apt-status"
    ) | awk '
        /No space left|not enough free space/ {reason="磁盘可用空间不足"; reason_key="vnc.apt_no_space"}
        /Permission denied|Operation not permitted/ {reason="没有访问软件源或缓存的权限"; reason_key="vnc.apt_permission"}
        /Hash Sum mismatch|Size mismatch/ {reason="软件包校验失败"; reason_key="vnc.apt_checksum"}
        /NO_PUBKEY|not signed|signatures|signature verification/ {reason="软件源签名校验失败"; reason_key="vnc.apt_signature"}
        /Could not resolve|Temporary failure resolving/ {reason="无法解析软件源域名"; reason_key="vnc.apt_dns"}
        /Could not connect|Connection failed|Connection refused/ {reason="无法连接软件源"; reason_key="vnc.apt_network"}
        /timed out|Timeout/ {reason="连接软件源超时"; reason_key="vnc.apt_timeout"}
        /404 |Unable to locate package|no installation candidate/ {reason="软件源中没有可用的软件包"; reason_key="vnc.apt_unavailable"}
        END {print reason; print reason_key}
    ' > "$work/apt-reason"
    apt_status=$(cat "$work/apt-status")
    apt_reason=$(sed -n '1p' "$work/apt-reason")
    apt_reason_key=$(sed -n '2p' "$work/apt-reason")
    if [ -z "$apt_reason" ]; then apt_reason="软件源操作失败，退出码 $apt_status"; apt_reason_key=vnc.apt_failed; fi
    [ "$apt_status" = 0 ]
}
package_error() {
    printf '{"error":"vnc.%s","source":"Apt","package":' "$1"; json "$2"
    printf ',"reason":'; json "$4"; printf ',"reason_key":'; json "$3"; error_params; printf '}'; exit 0
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
# 软件源里有的包及其中尚未安装的包。逐个查询安装状态较慢，每次获取只准备一次，供多次计算闭包共用。
dependency_index() {
    dependency_names < "$work/depends" > "$work/available"
    missing_packages < "$work/available" > "$work/missing"
}
# 先遍历必需依赖，再处理候选组；未选候选的子依赖不会进入闭包。
# 入口或整组候选在软件源里都没有时，把缺的包名写进 $work/unavailable 并以 1 退出（标准错误已被丢弃）。
dependency_closure() {
    : > "$work/unavailable"
    awk -v roots="$*" -v unavailable="$work/unavailable" '
        FILENAME==ARGV[1] {available[$0]=1; installed[$0]=1; next}
        FILENAME==ARGV[2] {delete installed[$0]; next}
        /^[a-z0-9][a-z0-9+.-]*(:[a-z0-9][a-z0-9-]*)?$/ {parent=$0; group=0; next}
        /^[^ ]/ {parent=""; next}
        /^ [ |]*(Pre)?Depends: / {
            if(parent=="") next
            if(!group) {group=++groups; owner[group]=parent}
            name=$NF; virtual=(name ~ /^</)
            if(!virtual) candidate[group,++lengths[group]]=name
            # 记下这一组依赖的原名（虚包去掉尖括号），整组都没有时据此报告
            wanted=name; gsub(/^<|>$/,"",wanted)
            want[group]=want[group] (want[group]=="" ? "" : " | ") wanted
            last=group
            if($0 !~ /^ *\|/) group=0
            next
        }
        /^    [a-z0-9]/ && virtual && parent!="" {
            candidate[last,++lengths[last]]=$1
        }
        function missing(name) {
            printf "%s", name > unavailable
            close(unavailable)
            exit 1
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
            missing(want[g])
        }
        END {
            n=split(roots,root," ")
            for(i=1;i<=n;i++) {if(!available[root[i]]) missing(root[i]); add(root[i])}
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
    command -v apt-get >/dev/null && command -v apt-cache >/dev/null && command -v dpkg-query >/dev/null && command -v dpkg-deb >/dev/null || fail light_unavailable 'vnc.apt_tools_missing' '需要 apt-get、apt-cache 和 dpkg'
    apt_root=$HOME/.cache/pierlink/apt
    private_dir "$apt_root"
    need_space "$apt_root" $((apt_index_kib + home_margin_kib))
    mkdir "$apt_root/.lock" || fail locked
    apt_private=true
    safe_path "$apt_root/lists"; safe_path "$apt_root/cache"
    mkdir -p "$apt_root/lists/partial" "$apt_root/cache/archives/partial" || fail permission_denied
    apt_attempt apt-get update -o APT::Update::Error-Mode=any || fail download_failed "$apt_reason_key" "软件源索引更新失败：$apt_reason"
    # 可选包（optional_packages）一起查依赖；软件源里没有的名字 apt-cache 不输出，下面跳过。
    apt_command apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts --no-breaks --no-replaces --no-enhances "$@" ${optional_packages:-} > "$work/depends" 2>/dev/null || fail download_failed 'vnc.apt_dependencies' '无法计算发行版依赖'
    dependency_index
    if ! dependency_closure "$@" > "$work/closure"; then
        unavailable_package=$(cat "$work/unavailable" 2>/dev/null)
        # 不是缺包（awk 本身出错等）：没有包名可报
        [ -n "$unavailable_package" ] || fail download_failed 'vnc.apt_dependencies' '无法计算发行版依赖'
        package_error download_failed "$unavailable_package" 'vnc.apt_dependency_unavailable' "软件源里没有 $unavailable_package"
    fi
    # 可选包逐个加入：它或它的依赖在软件源里没有时跳过，不影响必需的包。
    closure_roots=$*
    for optional_package in ${optional_packages:-}; do
        dependency_closure $closure_roots "$optional_package" > "$work/candidate" || continue
        closure_roots="$closure_roots $optional_package"
        mv -f -- "$work/candidate" "$work/closure" || fail permission_denied
    done
    missing_packages < "$work/closure" > "$work/packages"
    # 入口程序保留自己的副本；其余依赖（包括字体和总线）优先复用系统已装包。
    printf '%s\n' "$version_package" >> "$work/packages"
    printf '%s\n' "$companion_package" >> "$work/packages"
    sort -u "$work/packages" > "$work/selected"
    total_bytes=0
    : > "$work/plan"
    while IFS= read -r apt_package; do
        # print-uris 只读本次已校验的索引，不开始下载；下载仍使用同一索引。
        apt_command apt-get --print-uris download "$apt_package" > "$work/uri" 2>/dev/null || package_error download_failed "$apt_package" 'vnc.apt_package_missing' '软件源中没有可下载的包'
        size=$(awk 'NF>=4 && $3 ~ /^[0-9]+$/ {n++;size=$3} END {if(n==1) print size}' "$work/uri")
        case "$size" in ''|*[!0-9]*) package_error invalid_package "$apt_package" 'vnc.apt_size_unknown' '软件源没有提供唯一的下载大小';; esac
        if [ "${#size}" -gt 10 ]; then
            printf '{"error":"vnc.download_limit","limit_bytes":"314572800","total_bytes":'; json "$size"
            printf ',"reason_key":"vnc.apt_single_limit","reason":"单个依赖下载超过 300 MiB"}'; exit 0
        fi
        total_bytes=$((total_bytes+size))
        if [ "$total_bytes" -gt 314572800 ]; then
            printf '{"error":"vnc.download_limit","limit_bytes":"314572800","total_bytes":"%s","reason_key":"vnc.apt_total_limit","reason":"依赖下载总量超过 300 MiB"}' "$total_bytes"; exit 0
        fi
        printf '%s %s\n' "$apt_package" "$size" >> "$work/plan"
    done < "$work/selected"
    # 一个一个下载、解包后删掉：解包后约为下载大小的 3 倍，再加正在解的那个包，按 4 倍算
    need_space "$work" $((total_bytes / 1024 * 4 + home_margin_kib))
    downloaded_bytes=0
    mkdir "$work/download" || fail permission_denied
    while read -r apt_package size; do
        cd "$work/download" || fail permission_denied
        apt_attempt apt-get download "$apt_package" || package_error download_failed "$apt_package" "$apt_reason_key" "$apt_reason"
        cd "$work" || fail permission_denied
        set -- "$work/download"/*.deb
        [ "$#" = 1 ] && [ -f "$1" ] || package_error invalid_package "$apt_package" 'vnc.apt_package_ambiguous' '没有得到唯一的软件包'
        [ "$(wc -c < "$1")" = "$size" ] || package_error invalid_package "$apt_package" 'vnc.apt_size_mismatch' '软件包大小与索引不符'
        actual=$(dpkg-deb -f "$1" Package) || package_error invalid_package "$apt_package" 'vnc.apt_name_unreadable' '无法读取软件包名称'
        [ "$actual" = "${apt_package%%:*}" ] || package_error invalid_package "$apt_package" 'vnc.apt_name_mismatch' '软件包名称与请求不符'
        if [ "$actual" = "$version_package" ]; then package_version=$(dpkg-deb -f "$1" Version); fi
        bounded 60 dpkg-deb -x "$1" "$work/unpack" >/dev/null 2>&1 || package_error invalid_package "$apt_package" 'vnc.apt_extract_failed' '软件包解压失败'
        downloaded_bytes=$((downloaded_bytes+size))
        rm -f -- "$1"
    done < "$work/plan"
    case "$package_version" in ''|*[!a-zA-Z0-9.+:~_-]*) fail invalid_package 'vnc.apt_version_invalid' '软件包版本不正确';; esac
    # 版本中的 epoch 分隔符不能直接出现在 PATH 和动态库搜索路径里。
    version=apt-$(printf %s "$package_version" | tr ':' '_')
}
