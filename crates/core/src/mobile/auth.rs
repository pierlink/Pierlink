//! 邀请、来源限速和设备凭据；摘要比较使用密码库的常量时间实现。
use super::*;
use hmac::digest::{CtOutput, OutputSizeUser, consts::U32};
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use tokio::time::Instant;
use zeroize::Zeroizing;

const INVITE_LIFETIME: Duration = Duration::from_secs(600);
const RATE_WINDOW: Duration = Duration::from_secs(60);
pub(super) const TOUCH_INTERVAL_SECONDS: i64 = 60;
struct TokenDigest;
impl OutputSizeUser for TokenDigest {
    type OutputSize = U32;
}

pub(super) fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}
fn equal(a: [u8; 32], b: [u8; 32]) -> bool {
    CtOutput::<TokenDigest>::new(a.into()) == CtOutput::<TokenDigest>::new(b.into())
}

pub(super) struct Pairing {
    invite: Option<PendingInvite>,
    attempts: HashMap<IpAddr, VecDeque<Instant>>,
    generation: watch::Sender<u64>,
}
struct PendingInvite {
    hash: [u8; 32],
    expires: Instant,
    failures: usize,
}
impl Default for Pairing {
    fn default() -> Self {
        Self {
            invite: None,
            attempts: HashMap::new(),
            generation: watch::channel(0).0,
        }
    }
}
impl Pairing {
    pub(super) fn cancel_invites(&mut self) {
        self.invite = None;
    }
    pub(super) fn invalidate(&mut self) {
        self.cancel_invites();
        self.generation.send_modify(|v| *v = v.wrapping_add(1));
    }
    fn consume(&mut self, code: &str, peer: IpAddr) -> Result<watch::Receiver<u64>, AppError> {
        let time = Instant::now();
        self.attempts.retain(|_, attempts| {
            while attempts
                .front()
                .is_some_and(|t| time.duration_since(*t) >= RATE_WINDOW)
            {
                attempts.pop_front();
            }
            !attempts.is_empty()
        });
        // 来源记录最多 1024 个，窗口结束自动清理；满额时拒绝新来源而不驱逐已有的限速记录。
        if self.attempts.len() >= 1024 && !self.attempts.contains_key(&peer) {
            return Err(error(
                "mobile.rate_limited",
                "mobile.too_many_pairing_attempts_try_again_later",
                "配对尝试过多，请稍后再试",
            ));
        }
        let attempts = self.attempts.entry(peer).or_default();
        if attempts.len() >= 5 {
            return Err(error(
                "mobile.rate_limited",
                "mobile.at_most_five_attempts_per_source_per_minute",
                "每个来源每分钟最多尝试五次",
            ));
        }
        attempts.push_back(time);
        if let Some(invite) = &mut self.invite {
            if time < invite.expires && equal(digest(code), invite.hash) {
                self.invite = None;
                return Ok(self.generation.subscribe());
            }
            invite.failures += 1;
            if time >= invite.expires || invite.failures >= 10 {
                self.invite = None;
            }
        }
        Err(error(
            "mobile.invalid_invite",
            "mobile.invitation_code_is_incorrect_used_or_expired",
            "邀请码不正确、已使用或已过期",
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PairRequest {
    pub code: String,
    pub device_name: String,
}
impl Drop for PairRequest {
    fn drop(&mut self) {
        self.code.zeroize();
    }
}

impl Shared {
    pub(super) fn create_invite(&self) -> Result<Invite, AppError> {
        let mut pairing = lock(&self.pairing);
        if lock(&self.state).stopped {
            return Err(error(
                "mobile.stopped",
                "mobile.mobile_entry_has_stopped",
                "手机入口已停止",
            ));
        }
        let alphabet = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut rng = rand::rng();
        let code: String = (0..8)
            .map(|_| alphabet[rng.random_range(0..alphabet.len())] as char)
            .collect();
        pairing.invalidate();
        pairing.invite = Some(PendingInvite {
            hash: digest(&code),
            expires: Instant::now() + INVITE_LIFETIME,
            failures: 0,
        });
        Ok(Invite {
            code,
            expires_at: now() + INVITE_LIFETIME.as_secs() as i64,
        })
    }

    pub(super) async fn pair(
        &self,
        request: PairRequest,
        peer: IpAddr,
        user_agent: Option<&str>,
    ) -> Result<(Device, Zeroizing<String>), AppError> {
        let name = device_name(&request.device_name)?;
        let consumed = lock(&self.pairing).consume(&request.code, peer);
        let mut generation = match consumed {
            Ok(generation) => generation,
            Err(e) => {
                tokio::time::sleep(self.limits.failure_delay).await;
                return Err(e);
            }
        };
        let approved = tokio::select! {
            biased;
            _ = generation.changed() => return Err(error("mobile.invalid_invite", "mobile.invitation_was_revoked", "邀请已作废")),
            result = tokio::time::timeout(self.limits.approval, self.host.approve_pairing(name, peer)) => {
                result.map_err(|_| error("mobile.pair_timeout", "mobile.desktop_pairing_approval_timed_out", "桌面配对审批已超时"))?
                    .map_err(|_| error("mobile.host_failed", "mobile.desktop_pairing_approval_failed", "桌面配对审批失败"))?
            }
        };
        if !approved {
            return Err(error(
                "mobile.pair_denied",
                "mobile.desktop_denied_pairing",
                "桌面拒绝了配对",
            ));
        }
        // 和更换邀请码、撤销全部串行提交，旧审批不能在撤销之后重新创建凭据。
        let pairing = lock(&self.pairing);
        if *generation.borrow() != *pairing.generation.borrow()
            || generation.has_changed().unwrap_or(true)
        {
            return Err(error(
                "mobile.invalid_invite",
                "mobile.invitation_was_revoked",
                "邀请已作废",
            ));
        }
        let state = lock(&self.state);
        if state.stopped {
            return Err(error(
                "mobile.stopped",
                "mobile.mobile_entry_has_stopped",
                "手机入口已停止",
            ));
        }
        let db = lock(&state.db);
        db.prune_mobile_devices(now())?;
        if db
            .mobile_devices()?
            .iter()
            .filter(|d| d.device.active(now()))
            .count()
            >= MAX_DEVICES
        {
            return Err(error(
                "mobile.device_limit",
                "mobile.limit_of_128_active_devices_reached_revoke_unused_devices_first",
                "有效设备已达一百二十八台上限，请先撤销不再使用的设备",
            ));
        }
        let token = crate::mcp::token::new_token();
        let device = Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            user_agent: user_agent
                .map(clean_user_agent)
                .filter(|value| !value.is_empty()),
            last_ip: Some(peer.to_string()),
            permissions: Permissions::default(),
            created_at: now(),
            last_active_at: now(),
            revoked_at: None,
        };
        db.insert_mobile_device(&device, &digest(&token))?;
        Ok((device, token))
    }

    pub(super) fn authenticate(
        &self,
        cookie: Option<&str>,
        connection: u64,
        peer: IpAddr,
    ) -> Result<Device, AppError> {
        let invalid = || {
            error(
                "mobile.unauthorized",
                "mobile.device_is_not_paired_revoked_or_expired",
                "设备未配对、已撤销或已失效",
            )
        };
        let cookie = cookie.ok_or_else(invalid)?;
        let mut tokens = cookie.split(';').filter_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == "pl_device").then_some(value)
        });
        let token = tokens.next().ok_or_else(invalid)?;
        if tokens.next().is_some()
            || token.len() != 43
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid());
        }
        let hash = digest(token);
        let mut state = lock(&self.state);
        let mut matched = None;
        {
            let db = lock(&state.db);
            for stored in db.mobile_devices()? {
                let matches = equal(hash, stored.token_hash);
                if matches && stored.device.active(now()) {
                    matched = Some(stored.device);
                }
            }
        }
        let mut device = matched.ok_or_else(invalid)?;
        let entry = state.connections.get_mut(&connection).ok_or_else(invalid)?;
        if *entry.stop.borrow() || entry.device.as_ref().is_some_and(|id| *id != device.id) {
            return Err(invalid());
        }
        entry.device = Some(device.id.clone());
        let at = now();
        let peer = peer.to_string();
        // 相同来源最多每分钟写一次；切换网络时立即记录，认证失败不刷新设备信息。
        if at - device.last_active_at >= TOUCH_INTERVAL_SECONDS
            || device.last_ip.as_deref() != Some(&peer)
        {
            lock(&state.db).touch_mobile_device(&device.id, at, Some(&peer))?;
        }
        device.last_active_at = device.last_active_at.max(at);
        device.last_ip = Some(peer);
        Ok(device)
    }
}

fn clean_user_agent(value: &str) -> String {
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    cleaned.trim().chars().take(256).collect()
}

#[cfg(test)]
pub(super) fn expire_invite(shared: &Shared) {
    lock(&shared.pairing).invite.as_mut().unwrap().expires = Instant::now();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 浏览器标识去控制字符和首尾空白并按字符截断() {
        assert_eq!(clean_user_agent(" \t浏\0览\n器\u{7f}\u{85} \r"), "浏览器");
        assert_eq!(
            clean_user_agent(&format!("  {}尾  ", "机".repeat(256))),
            "机".repeat(256)
        );
        assert_eq!(clean_user_agent(" \t\r\n "), "");
    }

    #[test]
    fn 来源窗口滑动过期并且记录总量有界() {
        let peer: IpAddr = "100.64.0.2".parse().unwrap();
        let mut pairing = Pairing::default();
        for _ in 0..5 {
            assert_eq!(
                pairing.consume("错误", peer).err().unwrap().code,
                "mobile.invalid_invite"
            );
        }
        assert_eq!(
            pairing.consume("错误", peer).err().unwrap().code,
            "mobile.rate_limited"
        );
        pairing.attempts.get_mut(&peer).unwrap()[0] = Instant::now() - RATE_WINDOW;
        assert_eq!(
            pairing.consume("错误", peer).err().unwrap().code,
            "mobile.invalid_invite"
        );
        pairing.attempts.clear();
        for n in 0..1024u32 {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(0x0a000000 + n));
            pairing
                .attempts
                .insert(ip, VecDeque::from([Instant::now()]));
        }
        assert_eq!(
            pairing.consume("错误", peer).err().unwrap().code,
            "mobile.rate_limited"
        );
        assert_eq!(pairing.attempts.len(), 1024);
        for attempts in pairing.attempts.values_mut() {
            attempts[0] = Instant::now() - RATE_WINDOW;
        }
        assert_eq!(
            pairing.consume("错误", peer).err().unwrap().code,
            "mobile.invalid_invite"
        );
        assert_eq!(pairing.attempts.len(), 1);
    }
}
