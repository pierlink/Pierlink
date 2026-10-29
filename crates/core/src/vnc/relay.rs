use super::{Tools, VncTarget};
use crate::{error::AppError, ssh::quote::sh_quote};

const SOCAT: &str = "exec socat STDIO \"$1\"";
const PYTHON: &str = r#"import os,select,socket,sys
s=socket.socket(socket.AF_UNIX if sys.argv[1]=='unix' else socket.AF_INET,socket.SOCK_STREAM)
s.settimeout(10)
s.connect(sys.argv[2] if sys.argv[1]=='unix' else ('127.0.0.1',int(sys.argv[2])))
s.settimeout(None)
try:
 while True:
  ready,_,_=select.select([0,s],[],[])
  for source in ready:
   data=os.read(0,65536) if source==0 else s.recv(65536)
   if not data: sys.exit(0)
   if source==0: s.sendall(data)
   else:
    while data:
     n=os.write(1,data)
     data=data[n:]
finally:
 s.close()
"#;
const PERL: &str = r#"use strict; use warnings; use IO::Socket::UNIX; use IO::Socket::INET; use IO::Select;
my ($kind,$target)=@ARGV;
my $s=$kind eq 'unix' ? IO::Socket::UNIX->new(Type=>SOCK_STREAM,Peer=>$target) : IO::Socket::INET->new(PeerAddr=>'127.0.0.1',PeerPort=>$target,Proto=>'tcp',Timeout=>10);
exit 1 unless $s;
my $select=IO::Select->new(\*STDIN,$s);
while(1) {
 for my $source ($select->can_read) {
  my $n=sysread($source,my $data,65536); exit 0 unless defined($n) && $n;
  my $out=fileno($source)==fileno(STDIN) ? $s : \*STDOUT;
  my $offset=0;
  while($offset<$n) { my $w=syswrite($out,$data,$n-$offset,$offset); exit 1 unless defined($w) && $w; $offset+=$w; }
 }
}
"#;

pub(super) fn command(tools: &Tools, target: &VncTarget) -> Result<String, AppError> {
    let (kind, value) = match target {
        VncTarget::UnixPath(path) => ("unix", path.clone()),
        VncTarget::Loopback(port) => ("tcp", port.to_string()),
    };
    let args: Vec<String> = if tools.socat {
        // socat 自己还有一层地址语法；引号保护逗号、冒号和反斜线，shell 转义在下方统一处理。
        let address = match target {
            VncTarget::UnixPath(path) => format!(
                "UNIX-CONNECT:\"{}\"",
                path.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\'', "\\'")
            ),
            VncTarget::Loopback(port) => format!("TCP4:127.0.0.1:{port},connect-timeout=10"),
        };
        vec![
            "/bin/sh".into(),
            "-c".into(),
            SOCAT.into(),
            "pierlink-vnc".into(),
            address,
        ]
    } else if tools.python3 || tools.platform_python {
        vec![
            if tools.python3 {
                "python3"
            } else {
                "/usr/libexec/platform-python"
            }
            .into(),
            "-c".into(),
            PYTHON.into(),
            kind.into(),
            value,
        ]
    } else if tools.perl {
        vec![
            "perl".into(),
            "-e".into(),
            PERL.into(),
            "--".into(),
            kind.into(),
            value,
        ]
    } else {
        return Err(AppError::new("vnc.relay_unavailable")
            .with("reason", "服务器缺少 socat、Python 和 Perl 中继程序"));
    };
    let quoted = args
        .iter()
        .map(|s| sh_quote(s))
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    Ok(format!("exec {quoted}"))
}
