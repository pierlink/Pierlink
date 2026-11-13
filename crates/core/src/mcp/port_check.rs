//! 服务器入口的远程转发被拒（服务器上的端口被占用）时，到服务器上查是谁占着：
//! 有没有程序在听、是不是这个用户的、是不是 Pierlink（探测地址回报实例号）、写地址文件的是哪台电脑，
//! 顺便找一个空闲的端口。只读，不改服务器上的任何东西。
use serde::Serialize;

/// 找空闲端口的范围（默认端口 47821 开始的一百个）
pub const FREE_PORT_RANGE: (u16, u16) = (47821, 47920);

/// 查端口的 POSIX sh 正文（经 run_script 从标准输入发送），输出一行 JSON：
/// `{"listening":bool,"own":bool,"probe":"pierlink|http|timeout|none","instance":"…","computer":"…","free":端口或null}`。
/// listening：/proc/net/tcp 或 tcp6 里有程序在 127.0.0.1 等地址上监听这个端口；own：监听的是这个用户；
/// probe：访问探测地址的结果（pierlink：回报了实例号；http：有 HTTP 回应但不是 Pierlink；timeout：连上了但没有
/// HTTP 回应（超时、空回应、连接被对方断开），多半是没断干净的旧 SSH 连接；none：连不上或服务器上没有 curl、wget、python3）；
/// computer：实例号和 ~/.pierlink/mcp.json 里记的一样时，写这个文件的电脑名。
/// 空闲端口：任何状态的连接都没占着本地端口（这段端口在 Linux 的临时端口范围里，可能被别的连接占着）。
pub fn port_check_script(port: u16) -> String {
    let (first, last) = FREE_PORT_RANGE;
    format!(
        r#"port={port}
listening() {{
  h=$(printf ':%04X' "$1")
  for f in /proc/net/tcp /proc/net/tcp6; do
    [ -r "$f" ] || continue
    u=$(awk -v h="$h" '$4 == "0A" && substr($2, length($2) - 4) == h {{ print $8; exit }}' "$f")
    if [ -n "$u" ]; then printf '%s' "$u"; return 0; fi
  done
  return 1
}}
in_use() {{
  h=$(printf ':%04X' "$1")
  # 只交给 awk 读得到的文件（关了 IPv6 的内核没有 tcp6，awk 读不到文件会以非零退出）
  set --
  for f in /proc/net/tcp /proc/net/tcp6; do [ -r "$f" ] && set -- "$@" "$f"; done
  [ $# -gt 0 ] || return 1
  awk -v h="$h" 'FNR > 1 && substr($2, length($2) - 4) == h {{ found = 1 }} END {{ exit !found }}' "$@"
}}
listen=false; own=false
if uid=$(listening "$port"); then
  listen=true
  [ "$uid" = "$(id -u)" ] && own=true
fi
url="http://127.0.0.1:$port/.pierlink-probe"
probe=none; head=''
if command -v curl >/dev/null 2>&1; then
  head=$(curl -s -m 3 --noproxy '*' -o /dev/null -D - "$url" 2>/dev/null); code=$?
  # 28 超时、52 空回应、56 收的时候断了：连上了但没有 HTTP 回应
  case "$code" in 0) probe=http;; 28|52|56) probe=timeout;; esac
elif command -v wget >/dev/null 2>&1; then
  # 不加 -q：头（-S）和错误说明都在 stderr 里。GNU wget 读超时默认重试 20 次（会超过整段脚本的时限），
  # 只试一次；BusyBox 的 wget 不一定认 -t，不加
  tries=''
  case "$(wget --version 2>&1)" in *GNU*) tries='-t 1 --no-proxy';; esac
  head=$(wget -S -O /dev/null -T 3 $tries "$url" 2>&1)
  if printf '%s\n' "$head" | grep -q 'HTTP/'; then probe=http
  elif printf '%s\n' "$head" | grep -qiE 'timed out|timeout|no data|closed|reset'; then probe=timeout; fi
elif command -v python3 >/dev/null 2>&1; then
  head=$(python3 - "$url" <<'PIERLINK_PROBE'
import socket, sys, urllib.error, urllib.request
try:
    h = urllib.request.build_opener(urllib.request.ProxyHandler({{}})).open(sys.argv[1], timeout=3).headers
except urllib.error.HTTPError as e:
    h = e.headers
except (socket.timeout, TimeoutError):
    print("timeout")
    sys.exit()
except urllib.error.URLError as e:
    print("timeout" if isinstance(e.reason, (socket.timeout, TimeoutError, ConnectionResetError)) else "none")
    sys.exit()
except ConnectionResetError:
    print("timeout")
    sys.exit()
except Exception:
    print("none")
    sys.exit()
print("x-pierlink-instance: " + (h.get("x-pierlink-instance") or ""))
PIERLINK_PROBE
)
  case "$head" in timeout) probe=timeout; head='';; none|'') ;; *) probe=http;; esac
fi
inst=$(printf '%s\n' "$head" | tr -d '\r' | sed -n 's/^ *[Xx]-[Pp]ierlink-[Ii]nstance: *\([A-Za-z0-9_-]*\).*/\1/p' | head -n 1)
[ -n "$inst" ] && probe=pierlink
computer=''
f="$HOME/.pierlink/mcp.json"
if [ -n "$inst" ] && [ -f "$f" ] && grep -qF "\"instance\":\"$inst\"" "$f"; then
  computer=$(sed -n 's/.*"computer":"\([^"]*\)".*/\1/p' "$f" | head -n 1 | sed 's/[\\"]//g')
fi
free=null
p={first}
while [ "$p" -le {last} ]; do
  if [ "$p" != "$port" ] && ! in_use "$p"; then free=$p; break; fi
  p=$((p + 1))
done
printf '{{"listening":%s,"own":%s,"probe":"%s","instance":"%s","computer":"%s","free":%s}}\n' "$listen" "$own" "$probe" "$inst" "$computer" "$free"
"#
    )
}

/// 端口被谁占着（给界面显示）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Occupant {
    /// 这台电脑自己（另一条连到同一台服务器、同一个用户的入口）
    ThisPc,
    /// 另一台电脑上的 Pierlink
    OtherPc,
    /// 没断干净的旧 SSH 连接（服务器还没发现它已经断了），过一阵会自己释放
    Stale,
    /// 服务器上的其他程序或其他用户
    OtherProgram,
    /// 没有程序在听，服务器却拒绝了转发（可能 sshd 不允许转发，也可能刚好空出来了）
    NotListening,
}

/// 查端口的结论
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortCheck {
    pub occupant: Occupant,
    /// 另一台电脑的名字（知道的话）
    pub computer: Option<String>,
    /// 建议换到的空闲端口
    pub free_port: Option<u16>,
}

/// 把 port_check_script 的输出换成结论。
/// :param output: 脚本输出的 JSON
/// :param mine: 这个 Pierlink 的实例号
/// :return: 结论
pub fn classify(output: &serde_json::Value, mine: &str) -> PortCheck {
    let instance = output["instance"].as_str().unwrap_or_default();
    let listening = output["listening"].as_bool().unwrap_or(false);
    let own = output["own"].as_bool().unwrap_or(false);
    let probe = output["probe"].as_str().unwrap_or_default();
    let occupant = if !instance.is_empty() {
        if instance == mine {
            Occupant::ThisPc
        } else {
            Occupant::OtherPc
        }
    } else if !listening {
        Occupant::NotListening
    } else if probe == "timeout" && own {
        Occupant::Stale
    } else {
        Occupant::OtherProgram
    };
    let computer = output["computer"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty() && occupant == Occupant::OtherPc)
        .map(str::to_owned);
    let free_port = output["free"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| (FREE_PORT_RANGE.0..=FREE_PORT_RANGE.1).contains(p));
    PortCheck {
        occupant,
        computer,
        free_port,
    }
}

/// 找这个进程往上的第一个 sshd（就是这条 SSH 连接在服务器上的会话进程，远程转发的端口也是它开的），
/// 和进程的启动时间（/proc/<pid>/stat 第 22 项，同一个进程号重新用时不一样）。只在有 /proc 的系统上有用。
/// sshd 会话进程不可转储，同一用户也读不到它的 /proc/<pid>/fd，但 stat、status 谁都能读。
const SSHD_HELPERS: &str = r#"sshd_of() {
  p=$1
  while [ -n "$p" ] && [ "$p" -gt 1 ] 2>/dev/null; do
    s=$(cat "/proc/$p/stat" 2>/dev/null) || return 1
    c=${s#*(}; c=${c%)*}
    case "$c" in sshd|sshd-session) printf '%s' "$p"; return 0;; esac
    r=${s##*) }
    set -- $r
    p=$2
  done
  return 1
}
start_of() {
  s=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
  r=${s##*) }
  set -- $r
  shift 19
  printf '%s' "$1"
}
"#;

/// 实例号只留字母数字和 _-（拼进脚本前清理）
fn clean_instance(instance: &str) -> String {
    instance
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect()
}

/// 记下的转发所在连接：sshd 会话进程号和启动时间（两个合起来才唯一，进程号会被重用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardRecord {
    pub pid: u32,
    pub start: u64,
}

impl ForwardRecord {
    /// 从 record_forward_script 的输出里取；没记下时为 None
    pub fn from_output(output: &serde_json::Value) -> Option<Self> {
        if output["recorded"].as_bool() != Some(true) {
            return None;
        }
        Some(Self {
            pid: output["pid"].as_str()?.parse().ok()?,
            start: output["start"].as_str()?.parse().ok()?,
        })
    }

    fn word(self) -> String {
        format!("{}:{}", self.pid, self.start)
    }
}

/// 入口的远程转发每次新连上后（内置 SSH：后台脚本和转发在同一条主连接上）记下这条连接的 sshd 会话进程：
/// 写 `~/.pierlink/forward-<端口>`，一行“进程号 启动时间 实例号”，给 Pierlink 重启以后用；这次运行里的记在内存里。
/// 输出一行 JSON：`{"recorded":bool,"pid":"…","start":"…"}`。`~/.pierlink` 是符号链接时不写。
///
/// :param port: 服务器上的端口
/// :param instance: 这个 Pierlink 的实例号
/// :return: POSIX sh 脚本正文
pub fn record_forward_script(port: u16, instance: &str) -> String {
    let instance = clean_instance(instance);
    format!(
        r#"{SSHD_HELPERS}no() {{ echo '{{"recorded":false}}'; exit 0; }}
umask 077
[ -L "$HOME/.pierlink" ] && no
mkdir -p "$HOME/.pierlink" || no
pid=$(sshd_of $$) || no
start=$(start_of "$pid") || no
f="$HOME/.pierlink/forward-{port}"
printf '%s %s %s\n' "$pid" "$start" "{instance}" > "$f.tmp" && mv "$f.tmp" "$f" || no
printf '{{"recorded":true,"pid":"%s","start":"%s"}}\n' "$pid" "$start"
"#
    )
}

/// 端口被这台电脑没断干净的旧连接占着时：按 `~/.pierlink/forward-<端口>` 的记录核对后结束它。
/// 能结束的只有两种：这次运行里这个入口自己记下的那条（own，隧道已经断了才会走到这里）；这次运行还没记过时，
/// 上一次运行留下的（Pierlink 重启过，那时的连接在本机这边都没了）——这次运行里任何入口记下的（recorded_this_run）一律不碰。
/// 还要：进程还在、是 sshd、属于这个用户、启动时间对得上、实例号是自己的、不是这次查端口用的连接，
/// 动手前再探测一次端口还是不回 HTTP。都对才结束它，等它退出（最多 3 秒）。输出一行 JSON：`{"released":bool,"reason":"…"}`。
///
/// :param port: 服务器上的端口
/// :param instance: 这个 Pierlink 的实例号
/// :param own: 这次运行里这个入口自己记下的
/// :param recorded_this_run: 这次运行里所有入口记下的
/// :return: POSIX sh 脚本正文
pub fn release_stale_script(
    port: u16,
    instance: &str,
    own: Option<ForwardRecord>,
    recorded_this_run: &[ForwardRecord],
) -> String {
    let instance = clean_instance(instance);
    let own = own.map(ForwardRecord::word).unwrap_or_default();
    let forbid = recorded_this_run
        .iter()
        .map(|r| r.word())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"{SSHD_HELPERS}out() {{ printf '{{"released":%s,"reason":"%s"}}\n' "$1" "$2"; exit 0; }}
# 有没有程序在听这个端口（读 /proc/net/tcp，状态 0A 是在听）
listening() {{
  h=$(printf ':%04X' {port})
  for t in /proc/net/tcp /proc/net/tcp6; do
    [ -r "$t" ] || continue
    awk -v h="$h" '$4 == "0A" && substr($2, length($2) - 4) == h {{ found = 1 }} END {{ exit !found }}' "$t" && return 0
  done
  return 1
}}
# 探测端口（不走代理）：0 还有 HTTP 回应（活着），1 连上了但没回应（旧连接），2 没人在听；不确定时按活着处理
probe() {{
  listening || return 2
  u="http://127.0.0.1:{port}/.pierlink-probe"
  if command -v curl >/dev/null 2>&1; then
    curl -s -m 3 --noproxy '*' -o /dev/null "$u"
    case $? in 0) return 0;; 28|52|56) return 1;; 7) return 2;; *) return 0;; esac
  fi
  if command -v wget >/dev/null 2>&1 && wget --version 2>&1 | grep -q GNU; then
    wget -q -T 3 -t 1 --no-proxy -O /dev/null "$u" 2>/dev/null
    case $? in 4) return 1;; *) return 0;; esac
  fi
  return 0
}}
f="$HOME/.pierlink/forward-{port}"
[ -L "$HOME/.pierlink" ] && out false bad_record
[ -f "$f" ] && [ ! -L "$f" ] || out false no_record
read -r pid start inst rest < "$f" || out false no_record
[ "$inst" = "{instance}" ] || out false other_instance
case "$pid" in ''|*[!0-9]*) out false bad_record;; esac
case "$start" in ''|*[!0-9]*) out false bad_record;; esac
own='{own}'
if [ -n "$own" ]; then
  [ "$pid:$start" = "$own" ] || out false not_own
else
  case " {forbid} " in *" $pid:$start "*) out false this_run;; esac
fi
s=$(cat "/proc/$pid/stat" 2>/dev/null) || out false gone
c=${{s#*(}}; c=${{c%)*}}
case "$c" in sshd|sshd-session) ;; *) out false not_sshd;; esac
[ "$(start_of "$pid")" = "$start" ] || out false restarted
uid=$(awk '/^Uid:/ {{ print $2; exit }}' "/proc/$pid/status" 2>/dev/null)
[ "$uid" = "$(id -u)" ] || out false other_user
self=$(sshd_of $$) || self=''
[ "$pid" = "$self" ] && out false current
probe
case $? in 0) out false alive;; 2) out false port_free;; esac
kill "$pid" 2>/dev/null || out false kill_failed
i=0
while [ $i -lt 3 ] && [ -d "/proc/$pid" ]; do sleep 1; i=$((i + 1)); done
rm -f "$f"
out true killed
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在本机用 sh 跑脚本（HOME 换成临时目录），返回输出的 JSON
    fn run_local(script: &str, home: &std::path::Path) -> serde_json::Value {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .arg("-s")
            .env("HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn 清理旧连接_没有记录或对不上时什么都不做() {
        let home = crate::test_support::tempdir();
        let release = |own, this_run: &[ForwardRecord]| {
            run_local(
                &release_stale_script(47821, "inst-a", own, this_run),
                home.path(),
            )
        };
        assert_eq!(release(None, &[])["reason"], "no_record");
        // 记录指向一个还活着、但不是 sshd 的进程：不能结束它
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = sleeper.id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let start: u64 = stat
            .rsplit(") ")
            .next()
            .unwrap()
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let rec = ForwardRecord { pid, start };
        let dir = home.path().join(".pierlink");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("forward-47821"), format!("{pid} {start} inst-a\n")).unwrap();
        let other = run_local(
            &release_stale_script(47821, "inst-b", None, &[]),
            home.path(),
        );
        assert_eq!(other["reason"], "other_instance");
        // 这次运行里别的入口记下的不碰；自己记下的不是这条也不碰
        assert_eq!(release(None, &[rec])["reason"], "this_run");
        assert_eq!(
            release(
                Some(ForwardRecord {
                    pid,
                    start: start + 1
                }),
                &[]
            )["reason"],
            "not_own"
        );
        let r = release(Some(rec), &[rec]);
        assert_eq!(r["released"], false);
        assert_eq!(r["reason"], "not_sshd");
        assert!(
            sleeper.try_wait().unwrap().is_none(),
            "不是 sshd 的进程不能被结束"
        );
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert_eq!(release(None, &[])["reason"], "gone");
        // 实例号里的怪字符拼进脚本前去掉
        assert!(!release_stale_script(1, "a\"; rm -rf /; \"", None, &[]).contains("rm -rf /"));
        // 记录脚本的输出能取回进程号和启动时间
        let out = serde_json::json!({"recorded": true, "pid": "12", "start": "34"});
        assert_eq!(
            ForwardRecord::from_output(&out),
            Some(ForwardRecord { pid: 12, start: 34 })
        );
        assert_eq!(
            ForwardRecord::from_output(&serde_json::json!({"recorded": false})),
            None
        );
    }
    use serde_json::json;

    #[test]
    fn 结论按实例号_监听和探测结果() {
        let c = classify(
            &json!({"listening":true,"own":true,"probe":"pierlink","instance":"b","computer":"办公室","free":47822}),
            "a",
        );
        assert_eq!(c.occupant, Occupant::OtherPc);
        assert_eq!(c.computer.as_deref(), Some("办公室"));
        assert_eq!(c.free_port, Some(47822));
        let c = classify(
            &json!({"listening":true,"own":true,"probe":"pierlink","instance":"a","computer":"这台","free":null}),
            "a",
        );
        assert_eq!(c.occupant, Occupant::ThisPc);
        assert_eq!(c.computer, None);
        assert_eq!(c.free_port, None);
        let c = classify(
            &json!({"listening":true,"own":true,"probe":"timeout","instance":"","computer":"","free":47823}),
            "a",
        );
        assert_eq!(c.occupant, Occupant::Stale);
        let c = classify(
            &json!({"listening":true,"own":false,"probe":"timeout","instance":"","computer":"","free":47823}),
            "a",
        );
        assert_eq!(c.occupant, Occupant::OtherProgram);
        let c = classify(
            &json!({"listening":true,"own":true,"probe":"http","instance":"","computer":"","free":47823}),
            "a",
        );
        assert_eq!(c.occupant, Occupant::OtherProgram);
        let c = classify(
            &json!({"listening":false,"own":false,"probe":"none","instance":"","computer":"","free":47821}),
            "a",
        );
        assert_eq!(c.occupant, Occupant::NotListening);
        // 范围外的端口不当建议
        assert_eq!(classify(&json!({"free":80}), "a").free_port, None);
    }

    #[cfg(target_os = "linux")]
    mod shell {
        use super::*;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::process::{Command, Stdio};

        fn run(home: &std::path::Path, script: &str) -> serde_json::Value {
            let mut child = Command::new("/bin/sh")
                .arg("-s")
                .env("HOME", home)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
                panic!("输出不是 JSON：{}", String::from_utf8_lossy(&out.stdout))
            })
        }

        #[test]
        fn 在本机查_有_pierlink_在听和没人听() {
            let home = crate::test_support::tempdir();
            std::fs::create_dir(home.path().join(".pierlink")).unwrap();
            std::fs::write(
                home.path().join(".pierlink/mcp.json"),
                r#"{"url":"u","token":"t","instance":"inst-b","computer":"办公室"}"#,
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for stream in listener.incoming().take(1) {
                    let mut stream = stream.unwrap();
                    let mut buf = [0; 1024];
                    let _ = stream.read(&mut buf);
                    let _ = stream.write_all(
                        b"HTTP/1.1 204 No Content\r\nx-pierlink-instance: inst-b\r\nConnection: close\r\n\r\n",
                    );
                }
            });
            let v = run(home.path(), &port_check_script(port));
            assert_eq!(v["listening"], true, "{v}");
            assert_eq!(v["own"], true, "{v}");
            assert_eq!(v["instance"], "inst-b", "{v}");
            assert_eq!(v["computer"], "办公室", "{v}");
            let c = classify(&v, "inst-a");
            assert_eq!(c.occupant, Occupant::OtherPc);
            assert_eq!(c.computer.as_deref(), Some("办公室"));
            // 连上就被断开（没断干净的 SSH 转发对面已经没有程序时就是这样）：算没回应的旧连接
            let dead = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = dead.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for stream in dead.incoming().take(1) {
                    drop(stream);
                }
            });
            let v = run(home.path(), &port_check_script(port));
            assert_eq!(v["probe"], "timeout", "{v}");
            assert_eq!(classify(&v, "inst-a").occupant, Occupant::Stale, "{v}");
            // 没人听的端口
            let free = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = free.local_addr().unwrap().port();
            drop(free);
            let v = run(home.path(), &port_check_script(port));
            assert_eq!(v["listening"], false, "{v}");
            assert_eq!(classify(&v, "inst-a").occupant, Occupant::NotListening);
        }
    }
}
