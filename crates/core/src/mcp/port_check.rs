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
  head=$(curl -s -m 3 -o /dev/null -D - "$url" 2>/dev/null); code=$?
  # 28 超时、52 空回应、56 收的时候断了：连上了但没有 HTTP 回应
  case "$code" in 0) probe=http;; 28|52|56) probe=timeout;; esac
elif command -v wget >/dev/null 2>&1; then
  # 不加 -q：头（-S）和错误说明都在 stderr 里。GNU wget 读超时默认重试 20 次（会超过整段脚本的时限），
  # 只试一次；BusyBox 的 wget 不一定认 -t，不加
  tries=''
  case "$(wget --version 2>&1)" in *GNU*) tries='-t 1';; esac
  head=$(wget -S -O /dev/null -T 3 $tries "$url" 2>&1)
  if printf '%s\n' "$head" | grep -q 'HTTP/'; then probe=http
  elif printf '%s\n' "$head" | grep -qiE 'timed out|timeout|no data|closed|reset'; then probe=timeout; fi
elif command -v python3 >/dev/null 2>&1; then
  head=$(python3 - "$url" <<'PIERLINK_PROBE'
import socket, sys, urllib.error, urllib.request
try:
    h = urllib.request.urlopen(sys.argv[1], timeout=3).headers
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

#[cfg(test)]
mod tests {
    use super::*;
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
