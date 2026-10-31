//! 手机“按手机宽度显示”：手机看终端时把终端临时改成手机的列数（Claude Code 等按手机宽度重新排版），
//! 手机离开、45 秒没续期、或电脑上有人在这个终端里打字时恢复成电脑界面的尺寸。
//! 电脑上打过字之后 5 分钟内不接受手机改尺寸，免得两边来回抢。
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 手机多久没续期就恢复
pub const LEASE: Duration = Duration::from_secs(45);
/// 电脑上打过字之后多久不接受手机改尺寸
pub const DESKTOP_HOLD: Duration = Duration::from_secs(300);

/// 调用方要做的事：把终端改成这个尺寸，或者什么都不做
#[derive(Debug, PartialEq, Eq)]
pub enum Resize {
    To(u16, u16),
    Nothing,
}

#[derive(Default)]
struct Inner {
    /// 电脑界面最后设的尺寸（打开时的和之后每次调整的）
    desktop: HashMap<String, (u16, u16)>,
    /// 手机改了尺寸的终端 → 手机要的尺寸和最后一次续期的时间
    leases: HashMap<String, ((u16, u16), Instant)>,
    /// 电脑上最后一次打字的时间
    typed: HashMap<String, Instant>,
}

/// 各终端的尺寸归属
#[derive(Default)]
pub struct PhoneSizes {
    inner: Mutex<Inner>,
}

impl PhoneSizes {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 电脑上打开了终端：记下初始尺寸
    pub fn desktop_opened(&self, id: &str, cols: u16, rows: u16) {
        self.lock().desktop.insert(id.to_owned(), (cols, rows));
    }

    /// 电脑界面改了尺寸：记下来；手机正按手机宽度显示时先不改，手机离开后再用
    pub fn desktop_resize(&self, id: &str, cols: u16, rows: u16) -> Resize {
        let mut s = self.lock();
        s.desktop.insert(id.to_owned(), (cols, rows));
        if s.leases.contains_key(id) {
            Resize::Nothing
        } else {
            Resize::To(cols, rows)
        }
    }

    /// 电脑上在这个终端里打字：手机正改着尺寸就马上恢复，之后一段时间不让手机改
    pub fn desktop_typed(&self, id: &str, now: Instant) -> Resize {
        let mut s = self.lock();
        s.typed.insert(id.to_owned(), now);
        if s.leases.remove(id).is_some()
            && let Some(&(cols, rows)) = s.desktop.get(id)
        {
            return Resize::To(cols, rows);
        }
        Resize::Nothing
    }

    /// 手机要按 cols×rows 显示（也用来续期：尺寸没变时只续期、不改）；电脑上刚有人打过字时返回 None（不改）
    pub fn phone_fit(&self, id: &str, cols: u16, rows: u16, now: Instant) -> Option<Resize> {
        let mut s = self.lock();
        if s.typed
            .get(id)
            .is_some_and(|t| now.saturating_duration_since(*t) < DESKTOP_HOLD)
        {
            return None;
        }
        let same = s
            .leases
            .get(id)
            .is_some_and(|(size, _)| *size == (cols, rows));
        s.leases.insert(id.to_owned(), ((cols, rows), now));
        Some(if same {
            Resize::Nothing
        } else {
            Resize::To(cols, rows)
        })
    }

    /// 手机离开：恢复电脑界面的尺寸
    pub fn phone_restore(&self, id: &str) -> Resize {
        let mut s = self.lock();
        if s.leases.remove(id).is_some()
            && let Some(&(cols, rows)) = s.desktop.get(id)
        {
            return Resize::To(cols, rows);
        }
        Resize::Nothing
    }

    /// 手机太久没续期的终端：返回要恢复的终端和尺寸
    pub fn expired(&self, now: Instant) -> Vec<(String, u16, u16)> {
        let mut s = self.lock();
        let ids: Vec<String> = s
            .leases
            .iter()
            .filter(|(_, (_, t))| now.saturating_duration_since(*t) >= LEASE)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                s.leases.remove(&id);
                s.desktop.get(&id).map(|&(cols, rows)| (id, cols, rows))
            })
            .collect()
    }

    /// 终端关了：忘掉它
    pub fn forget(&self, id: &str) {
        let mut s = self.lock();
        s.desktop.remove(id);
        s.leases.remove(id);
        s.typed.remove(id);
    }
}

/// 电脑上写进终端的数据是不是“有人在打字”：普通文字、控制键、Esc、方向键和编辑键、Alt+键、粘贴算；
/// 程序查询后 xterm 自动回的应答（设备属性、光标位置、颜色、焦点进出、鼠标报告）不算
pub fn looks_typed(data: &str) -> bool {
    let Some(rest) = data.strip_prefix('\x1b') else {
        return true;
    };
    if rest.is_empty() || rest.starts_with("[200~") {
        return true;
    }
    let mut chars = rest.chars();
    match chars.next() {
        // CSI：方向键、Home/End、Shift+Tab（可带 1;5 这类修饰键参数），编辑键和 F5 以上（数字加 ~），
        // kitty 键盘协议的按键（数字加 u，如 Shift+Enter 是 13;2u；应答带 ? 不算）
        Some('[') => {
            let body = &rest[1..];
            let Some(last) = body.chars().last() else {
                return false;
            };
            let params = &body[..body.len() - last.len_utf8()];
            let numeric = params.chars().all(|c| c.is_ascii_digit() || c == ';');
            match last {
                'A'..='D' | 'H' | 'F' | 'Z' => numeric,
                '~' | 'u' => numeric && !params.is_empty(),
                _ => false,
            }
        }
        // SS3：应用模式的方向键、Home/End、F1–F4
        Some('O') => {
            matches!(chars.next(), Some('A'..='D' | 'H' | 'F' | 'P'..='S'))
                && chars.next().is_none()
        }
        // OSC、DCS 等应答
        Some(']' | 'P' | '_' | '^' | 'X') => false,
        // Alt+键：ESC 后面跟一个字符
        Some(_) => chars.next().is_none(),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 手机改尺寸离开时恢复电脑的尺寸() {
        let s = PhoneSizes::default();
        let t0 = Instant::now();
        s.desktop_opened("a", 160, 40);
        assert_eq!(s.phone_fit("a", 48, 30, t0), Some(Resize::To(48, 30)));
        // 同样的尺寸续期：只续期，不再改
        assert_eq!(s.phone_fit("a", 48, 30, t0), Some(Resize::Nothing));
        // 手机看着的时候电脑窗口变了：记下但不改
        assert_eq!(s.desktop_resize("a", 150, 42), Resize::Nothing);
        assert_eq!(s.phone_restore("a"), Resize::To(150, 42));
        // 已经恢复过：再恢复不做什么
        assert_eq!(s.phone_restore("a"), Resize::Nothing);
        assert_eq!(s.desktop_resize("a", 140, 40), Resize::To(140, 40));
    }

    #[test]
    fn 电脑上打字马上恢复并且五分钟内不让手机改() {
        let s = PhoneSizes::default();
        let t0 = Instant::now();
        s.desktop_opened("a", 160, 40);
        assert!(s.phone_fit("a", 48, 30, t0).is_some());
        assert_eq!(
            s.desktop_typed("a", t0 + Duration::from_secs(1)),
            Resize::To(160, 40)
        );
        assert_eq!(s.phone_fit("a", 48, 30, t0 + Duration::from_secs(60)), None);
        assert!(
            s.phone_fit("a", 48, 30, t0 + Duration::from_secs(1) + DESKTOP_HOLD)
                .is_some()
        );
    }

    #[test]
    fn 手机四十五秒没续期就恢复() {
        let s = PhoneSizes::default();
        let t0 = Instant::now();
        s.desktop_opened("a", 160, 40);
        s.desktop_opened("b", 120, 30);
        s.phone_fit("a", 48, 30, t0);
        s.phone_fit("b", 48, 30, t0 + Duration::from_secs(30));
        assert!(s.expired(t0 + Duration::from_secs(44)).is_empty());
        assert_eq!(s.expired(t0 + LEASE), vec![("a".to_owned(), 160, 40)]);
        // 续期了的不恢复
        s.phone_fit("b", 48, 30, t0 + Duration::from_secs(60));
        assert!(s.expired(t0 + Duration::from_secs(100)).is_empty());
        s.forget("b");
        assert!(s.expired(t0 + Duration::from_secs(1000)).is_empty());
    }

    #[test]
    fn 终端应答不算打字() {
        assert!(looks_typed("ls\r"));
        assert!(looks_typed("\x03"));
        assert!(looks_typed("\x1b[200~text\x1b[201~"));
        assert!(!looks_typed("\x1b[?1;2c"));
        assert!(!looks_typed("\x1b[12;40R"));
        assert!(!looks_typed("\x1b]11;rgb:0000/0000/0000\x1b\\"));
        // 焦点进出、鼠标报告
        assert!(!looks_typed("\x1b[I"));
        assert!(!looks_typed("\x1b[O"));
        assert!(!looks_typed("\x1b[<0;10;5M"));
        // kitty 键盘协议的查询应答
        assert!(!looks_typed("\x1b[?1u"));
    }

    #[test]
    fn 方向键_esc_编辑键和_alt_组合键算打字() {
        for keys in [
            "\x1b",
            "\x1b[A",
            "\x1b[D",
            "\x1bOB",
            "\x1b[Z",
            "\x1b[H",
            "\x1b[1;5C",
            "\x1b[3~",
            "\x1b[5~",
            "\x1b[15~",
            "\x1b[13;2u",
            "\x1bOP",
            "\x1bb",
            "\x1b\x7f",
        ] {
            assert!(looks_typed(keys), "{keys:?}");
        }
    }
}
