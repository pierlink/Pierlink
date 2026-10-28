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
const TOUCH_INTERVAL_SECONDS: i64 = 60;
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
    pub(super) fn invalidate(&mut self) {
        self.invite = None;
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
            return Err(error("mobile.rate_limited", "配对尝试过多，请稍后再试"));
        }
        let attempts = self.attempts.entry(peer).or_default();
        if attempts.len() >= 5 {
            return Err(error("mobile.rate_limited", "每个来源每分钟最多尝试五次"));
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
            return Err(error("mobile.stopped", "手机入口已停止"));
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
    ) -> Result<(Device, Zeroizing<String>), AppError> {
        let name = request.device_name.trim();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            return Err(error(
                "mobile.invalid_request",
                "设备名必须为一至八十个可显示字符",
            ));
        }
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
            _ = generation.changed() => return Err(error("mobile.invalid_invite", "邀请已作废")),
            result = tokio::time::timeout(self.limits.approval, self.host.approve_pairing(name, peer)) => {
                result.map_err(|_| error("mobile.pair_timeout", "桌面配对审批已超时"))?
                    .map_err(|_| error("mobile.host_failed", "桌面配对审批失败"))?
            }
        };
        if !approved {
            return Err(error("mobile.pair_denied", "桌面拒绝了配对"));
        }
        // 和作废邀请、撤销全部串行提交，旧审批不能在撤销之后重新创建凭据。
        let pairing = lock(&self.pairing);
        if *generation.borrow() != *pairing.generation.borrow()
            || generation.has_changed().unwrap_or(true)
        {
            return Err(error("mobile.invalid_invite", "邀请已作废"));
        }
        let state = lock(&self.state);
        if state.stopped {
            return Err(error("mobile.stopped", "手机入口已停止"));
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
                "有效设备已达一百二十八台上限，请先撤销不再使用的设备",
            ));
        }
        let token = crate::mcp::token::new_token();
        let device = Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
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
    ) -> Result<Device, AppError> {
        let invalid = || error("mobile.unauthorized", "设备未配对、已撤销或已失效");
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
            // 终端画面每秒轮询一次，活跃时间最多每分钟写一次库
            if let Some(device) = &matched
                && now() - device.last_active_at >= TOUCH_INTERVAL_SECONDS
            {
                db.touch_mobile_device(&device.id, now())?;
            }
        }
        let mut device = matched.ok_or_else(invalid)?;
        let entry = state.connections.get_mut(&connection).ok_or_else(invalid)?;
        if *entry.stop.borrow() || entry.device.as_ref().is_some_and(|id| *id != device.id) {
            return Err(invalid());
        }
        entry.device = Some(device.id.clone());
        device.last_active_at = now();
        Ok(device)
    }
}

#[cfg(test)]
pub(super) fn expire_invite(shared: &Shared) {
    lock(&shared.pairing).invite.as_mut().unwrap().expires = Instant::now();
}

#[cfg(test)]
mod tests {
    use super::*;

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
