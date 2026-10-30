exec 2>/dev/null
LC_ALL=C; export LC_ALL
{
# 测试替身只改变读取路径；正式连接不设置这两个变量。
proc=${PIERLINK_MONITOR_PROC_ROOT:-/proc}
os_release=${PIERLINK_MONITOR_OS_RELEASE:-/etc/os-release}
export proc os_release
[ -r "$proc/stat" ] || { printf 'PLMON1 {"unsupported":"no_proc"} PLMON1\n'; exit 0; }
interval=15
# 磁盘、GPU 按单调时间单独计时，改基础间隔不触发重复采样。
disk_period=10
gpu_period=2
# 保存控制输入；采样命令继续从 /dev/null 读取，不能吞掉控制行。
exec 4<&0
trap 'interval_elapsed=1' USR1
wait_interval() {
    interval_elapsed=0
    (
        sleep "$interval" & timer=$!
        trap 'kill "$timer" 2>/dev/null; wait "$timer"; exit' HUP INT TERM
        wait "$timer"
        kill -USR1 "$$" 2>/dev/null
    ) & interval_timer=$!
    IFS=' ' read -r next_interval next_gpu <&4
    read_status=$?
    kill -TERM "$interval_timer" 2>/dev/null || :
    wait "$interval_timer" 2>/dev/null || :
    if [ "$read_status" -eq 0 ]; then
        case "$next_interval" in ''|*[!0-9.]*|.*|*.*.*) return;; esac
        case "$next_gpu" in 2|10) :;; *) return;; esac
        interval=$next_interval; gpu_period=$next_gpu
    elif [ "$interval_elapsed" -eq 0 ]; then
        exit 0
    fi
}

# 包装命令以便 BusyBox timeout 只通知直接子进程时，也能清理其后代。
# 不使用临时文件，不记录命令原文或 GPU 错误原文。
guard='
kill_tree() {
    kill -STOP "$1" 2>/dev/null || return
    descendants=
    if [ -r "/proc/$1/task/$1/children" ]; then
        read -r descendants < "/proc/$1/task/$1/children" || :
    fi
    for descendant in $descendants; do kill_tree "$descendant"; done
    kill -KILL "$1" 2>/dev/null || :
}
"$@" & job=$!
trap '\''kill_tree "$job"; wait "$job"; exit 124'\'' HUP INT TERM
wait "$job"; result=$?
trap - HUP INT TERM
exit "$result"
'
timeout_mode=none
if command -v timeout >/dev/null 2>&1; then
    if timeout 1 /bin/sh -c 'exit 0'; then timeout_mode=modern
    elif timeout -t 1 /bin/sh -c 'exit 0'; then timeout_mode=legacy; fi
fi
bounded() {
    case "$timeout_mode" in
        modern) timeout 5 /bin/sh -c "$guard" plmon "$@"; return $?;;
        legacy) timeout -t 5 /bin/sh -c "$guard" plmon "$@"; return $?;;
    esac
    /bin/sh -c "$guard" plmon "$@" & task=$!
    (
        sleep 5 & timer=$!
        trap 'kill "$timer" 2>/dev/null; wait "$timer"; exit' HUP INT TERM
        wait "$timer"
        kill -TERM "$task" 2>/dev/null
    ) >/dev/null 2>&1 & watchdog=$!
    wait "$task"; result=$?
    kill -TERM "$watchdog" 2>/dev/null || :
    wait "$watchdog" 2>/dev/null || :
    return "$result"
}

cores=$(nproc 2>/dev/null || grep -c '^processor' "$proc/cpuinfo")
case "$cores" in ''|*[!0-9]*|0) cores=1;; esac
previous_total=; previous_idle=; first=1; last_disk=0; last_gpu=0
disk_rows=; gpu_data=; gpu_status=0
while :; do
    {
    read -r clock_uptime clock_idle < /proc/uptime || exit 1
    clock_s=${clock_uptime%%.*}
    # guest 与 guest_nice 已包含在 user、nice 中，不能重复累加。
    read -r label user nice system idle iowait irq softirq steal rest < "$proc/stat" || exit 1
    total=$((user + nice + system + idle + ${iowait:-0} + ${irq:-0} + ${softirq:-0} + ${steal:-0}))
    idle=$((idle + ${iowait:-0}))
    delta=0; idle_delta=0
    if [ -n "$previous_total" ]; then
        delta=$((total - previous_total)); idle_delta=$((idle - previous_idle))
    fi
    previous_total=$total; previous_idle=$idle

    # 按挂载点排序后按设备去重；仅查询本地数据文件系统，避免网络盘阻塞。
    if [ "$first" -eq 1 ] || [ $((clock_s - last_disk)) -ge "$disk_period" ]; then
    last_disk=$clock_s
    disk_rows=$(
        awk '
        function decode(s) {
            gsub(/\\040/, " ", s); gsub(/\\011/, "\t", s); gsub(/\\012/, "\n", s)
            gsub(/\\134/, sprintf("%c", 92), s); return s
        }
        { source[$2]=$1; filesystem[$2]=$3 }
        END {
            # 同一目录被覆盖挂载时只看最后一条，不能对已被网络盘覆盖的旧本地挂载跑 df。
            for(encoded in filesystem) {
                fs=filesystem[encoded]
                if(fs !~ /^(ext2|ext3|ext4|xfs|btrfs|zfs|f2fs|vfat|exfat|ntfs|ntfs3)$/ && !(fs=="fuseblk" && source[encoded] ~ /^\/dev\//)) continue
                m=decode(encoded)
                if(substr(m,1,1)!="/") continue
                # 插入排序不依赖 GNU sort，根目录自然排在最前。
                n++; i=n
                while(i>1 && names[i-1]>m) {
                    names[i]=names[i-1]; rows[i]=rows[i-1]; devices[i]=devices[i-1]; i--
                }
                names[i]=m; rows[i]=encoded "\t" fs; devices[i]=source[encoded]
            }
            for(i=1;i<=n && count<16;i++) {
                if(seen[devices[i]]++) continue
                print rows[i]; count++
            }
        }
        ' "$proc/mounts" |
        {
        started=$(date +%s)
        while IFS="$(printf '\t')" read -r encoded fs; do
            # 一轮的预算固定，多个慢盘不能把采样拖到几十秒。
            [ $(($(date +%s) - started)) -lt 5 ] || break
            mount=$(printf '%b.' "$encoded"); mount=${mount%.}
            output=$(bounded df -Pk "$mount") || continue
            stats=$(printf '%s\n' "$output" | awk '
                NR>1 && $2~/^[0-9]+$/ && $3~/^[0-9]+$/ && $4~/^[0-9]+$/ { printf "%.0f\t%.0f\t%.0f", $2*1024, $4*1024, $3*1024; exit }
                NR>1 && $1~/^[0-9]+$/ && $2~/^[0-9]+$/ && $3~/^[0-9]+$/ { printf "%.0f\t%.0f\t%.0f", $1*1024, $3*1024, $2*1024; exit }
            ')
            [ -n "$stats" ] && printf '%s\t%s\t%s\n' "$encoded" "$fs" "$stats"
        done
        }
    )
    fi
    if [ "$first" -eq 1 ] || [ $((clock_s - last_gpu)) -ge "$gpu_period" ]; then
        last_gpu=$clock_s
        gpu_data=; gpu_status=0
        if command -v nvidia-smi >/dev/null 2>&1; then
            gpu_data=$(bounded nvidia-smi --query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu --format=csv,noheader,nounits)
            gpu_status=$?
        fi
    fi
    at=$(date +%s)
    {
        printf '%s\n' "$disk_rows"
        printf '__PLMON_GPU__\n'
        printf '%s\n' "$gpu_data"
    } | awk -v at="$at" -v cores="$cores" -v delta="$delta" -v idle_delta="$idle_delta" -v first="$first" -v gpu_status="$gpu_status" '
    function json(s,    i,c,out) {
        out="\""
        for(i=1;i<=length(s);i++) {
            c=substr(s,i,1)
            if(c=="\"" || c=="\\") out=out "\\" c
            else if(c in control) out=out sprintf("\\u%04x",control[c])
            else out=out c
        }
        return out "\""
    }
    function decode(s) {
        gsub(/\\040/, " ", s); gsub(/\\011/, "\t", s); gsub(/\\012/, "\n", s)
        gsub(/\\134/, sprintf("%c",92), s); return s
    }
    function trim(s) { sub(/^[ \t]+/,"",s); sub(/[ \t\r]+$/,"",s); return s }
    function number(s, scale) { return s~/^[0-9]+$/ ? sprintf("%.0f",s*scale) : "null" }
    # NVIDIA 的 CSV 名称可能带逗号、引号；识别双引号字段及双引号转义。
    function csv(s, fields,    i,c,n,quoted,value) {
        n=1; quoted=0; value=""
        for(i=1;i<=length(s);i++) {
            c=substr(s,i,1)
            if(c=="\"") {
                if(quoted && substr(s,i+1,1)=="\"") { value=value "\""; i++ }
                else quoted=!quoted
            } else if(c=="," && !quoted) { fields[n++]=trim(value); value="" }
            else value=value c
        }
        fields[n]=trim(value)
        return quoted ? 0 : n
    }
    function os_name(    line,value,i,c,out) {
        while((getline line < ENVIRON["os_release"])>0) {
            if(line !~ /^PRETTY_NAME=/) continue
            value=substr(line,13)
            if(substr(value,1,1)=="\"") {
                value=substr(value,2,length(value)-2); out=""
                for(i=1;i<=length(value);i++) {
                    c=substr(value,i,1)
                    if(c=="\\" && substr(value,i+1,1)~/[\\"$`]/) c=substr(value,++i,1)
                    out=out c
                }
                return json(out)
            }
            if(substr(value,1,1)==sprintf("%c",39)) value=substr(value,2,length(value)-2)
            return json(value)
        }
        return "null"
    }
    BEGIN {
        for(i=1;i<32;i++) control[sprintf("%c",i)]=i
        disks=""; gpus=""; gpu_section=0; bad_gpu=0
        p=ENVIRON["proc"]
        getline uptime < (p "/uptime"); split(uptime,u," ")
        rx=0; tx=0; net_ok=0; net_bad=0; nets=""; net_count=0
        while((getline line < (p "/net/dev"))>0) {
            if(index(line, ":")==0) continue
            sub(/^[ \t]+/, "", line)
            net_ok=1
            split(line, interface, ":"); name=trim(interface[1])
            if(name=="lo") continue
            fields=split(trim(interface[2]), counters, /[ \t]+/)
            if(fields<16 || counters[1]!~/^[0-9]+$/ || counters[9]!~/^[0-9]+$/) { net_bad=1; continue }
            rx+=counters[1]; tx+=counters[9]; net_ok=1
            gsub(/[[:cntrl:]]/, "", name)
            # 内核网卡名最多 15 字节；接收端再按 Unicode 字符截到 32，避免截断 UTF-8。
            if(name!="" && !net_seen[name]++ && net_count<16) {
                nets=(nets) (nets=="" ? "" : ",") "{\"name\":" json(name) ",\"rx_bytes\":" counters[1] ",\"tx_bytes\":" counters[9] "}"
                net_count++
            }
        }
        net_rx=(net_ok && !net_bad) ? sprintf("%.0f",rx) : "null"
        net_tx=(net_ok && !net_bad) ? sprintf("%.0f",tx) : "null"

        getline load < (p "/loadavg"); split(load,l," ")
        while((getline line < (p "/meminfo"))>0) {
            split(line,m,/[ \t]+/); memory[m[1]]=m[2]+0
        }
    }
    /^__PLMON_GPU__$/ { gpu_section=1; next }
    !gpu_section && NF {
        split($0,d,"\t")
        disks=(disks) (disks=="" ? "" : ",") "{\"mount\":" json(decode(d[1])) ",\"fs_type\":" json(d[2]) ",\"total\":" d[3] ",\"free\":" d[4] ",\"used\":" d[5] "}"
        next
    }
    gpu_section && NF && gpu_status==0 {
        if(csv($0,g)!=6 || g[1]!~/^[0-9]+$/ || g[2]=="") { bad_gpu=1; next }
        gpus=(gpus) (gpus=="" ? "" : ",") "{\"index\":" number(g[1],1) ",\"name\":" json(g[2]) ",\"util_percent\":" number(g[3],1) ",\"mem_used\":" number(g[4],1048576) ",\"mem_total\":" number(g[5],1048576) ",\"temp_c\":" number(g[6],1) "}"
    }
    END {
        cpu="null"
        if(!first && delta>0 && idle_delta>=0 && idle_delta<=delta) cpu=sprintf("%.2f",100*(delta-idle_delta)/delta)
        available=("MemAvailable:" in memory) ? memory["MemAvailable:"] : memory["MemFree:"]+memory["Buffers:"]+memory["Cached:"]
        gpu_error="null"
        if(gpu_status==124 || gpu_status==137 || gpu_status==143) gpu_error=json("采样超时")
        else if(gpu_status!=0) gpu_error=json("采样失败")
        else if(bad_gpu) gpu_error=json("输出格式无效")
        printf "PLMON1 {\"at_ms\":%.0f,\"uptime_s\":%.0f,\"load\":[%g,%g,%g],\"cpu_percent\":%s,\"cpu_cores\":%d,\"net_at_ms\":%.0f,\"net_rx_bytes\":%s,\"net_tx_bytes\":%s,\"net_interfaces\":[%s],\"mem_total\":%.0f,\"mem_available\":%.0f,\"swap_total\":%.0f,\"swap_free\":%.0f,\"disks\":[%s],\"gpus\":[%s],\"gpu_error\":%s,\"os_name\":%s} PLMON1\n", at*1000,int(u[1]),l[1]+0,l[2]+0,l[3]+0,cpu,cores,u[1]*1000,net_rx,net_tx,nets,memory["MemTotal:"]*1024,available*1024,memory["SwapTotal:"]*1024,memory["SwapFree:"]*1024,disks,gpus,gpu_error,first ? os_name() : "null"
    }' || exit 1
    } </dev/null
    first=0
    wait_interval
done
}
