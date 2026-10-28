//! 不引入日期依赖的 UTC 毫秒与 RFC3339 转换。
use super::invalid_response;
use crate::error::AppError;
use std::time::{SystemTime, UNIX_EPOCH};
pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_millis() as i64)
        .unwrap_or(0)
}
fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * mp + 2) / 5 + d - 1 - 719468
}
pub(super) fn format_ms(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let z = seconds.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + i64::from(m <= 2);
    let t = seconds.rem_euclid(86400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        t / 3600,
        t / 60 % 60,
        t % 60,
        ms.rem_euclid(1000)
    )
}
pub(super) fn parse_ms(s: &str) -> Result<i64, AppError> {
    let bad = invalid_response;
    if !s.is_ascii() || s.len() < 20 {
        return Err(bad());
    }
    let n = |a, b| {
        s.get(a..b)
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or_else(bad)
    };
    let (y, m, d, h, min, sec) = (
        n(0, 4)?,
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    );
    if &s[4..5] != "-"
        || &s[7..8] != "-"
        || &s[10..11] != "T"
        || &s[13..14] != ":"
        || &s[16..17] != ":"
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&d)
        || h > 23
        || min > 59
        || sec > 59
    {
        return Err(bad());
    }
    let day = days(y, m, d);
    if format_ms(day * 86400000).get(..10) != s.get(..10) {
        return Err(bad());
    }
    let mut tail = &s[19..];
    let mut ms = 0;
    if let Some(f) = tail.strip_prefix('.') {
        let len = f.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return Err(bad());
        }
        ms = format!("{0:0<3}", &f[..len.min(3)])
            .parse::<i64>()
            .map_err(|_| bad())?;
        tail = &f[len..];
    }
    let offset = if tail == "Z" {
        0
    } else if tail.len() == 6 && matches!(&tail[..1], "+" | "-") && &tail[3..4] == ":" {
        let h = tail[1..3].parse::<i64>().map_err(|_| bad())?;
        let m = tail[4..6].parse::<i64>().map_err(|_| bad())?;
        if h > 23 || m > 59 {
            return Err(bad());
        }
        (h * 60 + m) * 60 * if tail.starts_with('-') { -1 } else { 1 }
    } else {
        return Err(bad());
    };
    Ok((day * 86400 + h * 3600 + min * 60 + sec - offset) * 1000 + ms)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 日期往返及偏移() {
        for ms in [0, -1, 1709251200123, now_ms()] {
            assert_eq!(parse_ms(&format_ms(ms)).unwrap(), ms);
        }
        assert_eq!(parse_ms("1970-01-01T08:00:00+08:00").unwrap(), 0);
        assert!(parse_ms("2025-02-29T00:00:00Z").is_err());
    }
}
