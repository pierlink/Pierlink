# 在 CI 的 WSL Ubuntu 里配一台“Linux 服务器”：用户 pl 密钥登录、用户 plpw 只能密码登录、
# 用户 plotp 要密码加手机验证码（PAM google-authenticator），sshd 监听 2222。
# 只在用完即弃的 CI 机器上以 root 运行；参数是公钥文件、plpw 的密码文件、plotp 的“密码 验证码密钥”文件
# （都是 Windows 那边运行时生成的；第三个可以不给，不给就不建 plotp）。
# 用户 pl 在 sudo 组里，密码是随机生成的、不打印也不保存：只用来让 sudo 出现密码提示。
set -eu
pub=$1
pwfile=$2
id pl >/dev/null 2>&1 || useradd -m -s /bin/bash pl
usermod -aG sudo pl
printf 'pl:%s\n' "$(head -c 24 /dev/urandom | base64 | tr -d '\n')" | chpasswd
install -d -m 700 -o pl -g pl /home/pl/.ssh
tr -d '\r' < "$pub" > /home/pl/.ssh/authorized_keys
chown pl:pl /home/pl/.ssh/authorized_keys
chmod 600 /home/pl/.ssh/authorized_keys
# 只能用密码登录的用户（测 0.3.1“连接时输一次密码，所有功能共用”）；密码不打印
id plpw >/dev/null 2>&1 || useradd -m -s /bin/bash plpw
printf 'plpw:%s\n' "$(tr -d '\r\n' < "$pwfile")" | chpasswd
# 要密码加验证码的用户（测 0.4.0“要验证码的服务器登录一次，所有功能共用”）；密码和密钥都不打印
otpfile=${3:-}
if [ -n "$otpfile" ]; then
    # 文件没有结尾换行时 read 返回 1，不算失败；读出来的两项都不能为空
    read -r otppw otpsecret < "$otpfile" || :
    otpsecret=$(printf '%s' "$otpsecret" | tr -d '\r')
    [ -n "$otppw" ] && [ -n "$otpsecret" ] || { echo "plotp 的密码或验证码密钥是空的"; exit 1; }
    id plotp >/dev/null 2>&1 || useradd -m -s /bin/bash plotp
    printf 'plotp:%s\n' "$otppw" | chpasswd
    printf '%s\n" RATE_LIMIT 100 30\n" WINDOW_SIZE 17\n" TOTP_AUTH\n' "$otpsecret" > /home/plotp/.google_authenticator
    chown plotp:plotp /home/plotp/.google_authenticator
    chmod 400 /home/plotp/.google_authenticator
    # 密码之后再要验证码；没有 .google_authenticator 的用户（pl、plpw）跳过这一步
    grep -q pam_google_authenticator /etc/pam.d/sshd || echo 'auth required pam_google_authenticator.so nullok' >> /etc/pam.d/sshd
fi
mkdir -p /run/sshd
ssh-keygen -A >/dev/null
cat > /etc/ssh/sshd_config.d/pierlink-e2e.conf <<'EOF'
Port 2222
PasswordAuthentication no
KbdInteractiveAuthentication no
Match User plpw
    PasswordAuthentication yes
    PubkeyAuthentication no
Match User plotp
    KbdInteractiveAuthentication yes
    PasswordAuthentication no
    PubkeyAuthentication no
    AuthenticationMethods keyboard-interactive
EOF
/usr/sbin/sshd -t
# 装 openssh-server 时 systemd 会在 22 端口起一个 sshd，WSL2 再把它转发到 Windows 的 localhost:22，
# 盖住 Windows 自己的 sshd（指纹对不上）。关掉它，只用测试自己在前台启动的 2222。
if [ -d /run/systemd/system ]; then
    systemctl disable --now ssh.socket ssh.service >/dev/null 2>&1 || :
fi
pkill -x sshd || :
sleep 1
if ss -ltn | grep -q ':22 '; then echo "WSL 里仍有程序在监听 22 端口："; ss -ltnp; exit 1; fi
echo "WSL 里的环境：$(uname -r)；根目录 $(awk '$2=="/"{print $3}' /proc/mounts)；tmux $(command -v tmux || echo 没有)"
