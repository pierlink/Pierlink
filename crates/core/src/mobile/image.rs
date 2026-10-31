//! 图片入口仅校验类型和魔数，存储及终端路径输入由宿主完成。
use super::*;
use std::collections::VecDeque;
use tokio::time::Instant;

pub(super) const MAX_IMAGE_BODY: usize = 8 * 1024 * 1024;
const WINDOW: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct ImageRate {
    attempts: HashMap<String, VecDeque<Instant>>,
}
impl ImageRate {
    pub(super) fn take(&mut self, device: &str, now: Instant) -> Result<(), AppError> {
        // 每台设备最多十个时间点，最多保留 128 台；每次请求清掉一分钟以前的记录。
        self.attempts.retain(|_, attempts| {
            while attempts
                .front()
                .is_some_and(|at| now.duration_since(*at) >= WINDOW)
            {
                attempts.pop_front();
            }
            !attempts.is_empty()
        });
        if (self.attempts.len() >= MAX_DEVICES && !self.attempts.contains_key(device))
            || self
                .attempts
                .get(device)
                .is_some_and(|attempts| attempts.len() >= 10)
        {
            return Err(error(
                "mobile.rate_limited",
                "mobile.at_most_ten_images_per_device_per_minute",
                "每台设备每分钟最多上传十张图片，请稍后再试",
            ));
        }
        self.attempts
            .entry(device.into())
            .or_default()
            .push_back(now);
        Ok(())
    }
}

pub(super) fn extension(
    content_type: Option<&str>,
    bytes: &[u8],
) -> Result<&'static str, AppError> {
    let ext = match content_type {
        Some("image/png") if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => "png",
        Some("image/jpeg") if bytes.starts_with(b"\xff\xd8\xff") => "jpg",
        Some("image/webp") if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") => {
            "webp"
        }
        Some("image/gif") if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => "gif",
        Some("image/png" | "image/jpeg" | "image/webp" | "image/gif") => {
            return Err(error(
                "mobile.invalid_image",
                "mobile.image_signature_does_not_match_content_type",
                "图片文件头与声明的类型不一致或不完整",
            ));
        }
        _ => {
            return Err(error(
                "mobile.unsupported_image_type",
                "mobile.image_type_must_be_png_jpeg_webp_or_gif",
                "图片类型必须为 PNG、JPEG、WebP 或 GIF",
            ));
        }
    };
    Ok(ext)
}

impl Shared {
    pub(super) async fn image(
        &self,
        id: &str,
        bytes: Vec<u8>,
        ext: &'static str,
    ) -> Result<serde_json::Value, AppError> {
        let _guard = self.input_gate.lock().await;
        self.check_password(id).await?;
        // 图片路径也会改变当前输入行，成功、失败或取消后都须在下一次提交时读屏核对。
        {
            let mut lines = lock(&self.pending_lines);
            if lines.len() >= 128 && !lines.contains_key(id) {
                return Err(error(
                    "mobile.input_limit",
                    "mobile.too_many_terminals_have_pending_input",
                    "尚未提交输入的终端数量过多",
                ));
            }
            lines.entry(id.into()).or_default().edited = true;
        }
        self.host.terminal_image(id, bytes, ext).await.map_err(|_| {
            error(
                "mobile.host_failed",
                "mobile.desktop_terminal_image_failed",
                "桌面终端图片输入失败",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 图片窗口一分钟后释放且设备记录总量有界() {
        let mut rate = ImageRate::default();
        let at = Instant::now();
        for _ in 0..10 {
            rate.take("手机", at).unwrap();
        }
        assert_eq!(
            rate.take("手机", at + WINDOW - Duration::from_millis(1))
                .unwrap_err()
                .code,
            "mobile.rate_limited"
        );
        rate.take("手机", at + WINDOW).unwrap();
        assert_eq!(rate.attempts["手机"].len(), 1);
        for n in 1..MAX_DEVICES {
            rate.take(&n.to_string(), at + WINDOW).unwrap();
        }
        assert!(rate.take("新增设备", at + WINDOW).is_err());
        assert_eq!(rate.attempts.len(), MAX_DEVICES);
        rate.take("新增设备", at + WINDOW * 2).unwrap();
        assert_eq!(rate.attempts.len(), 1);
    }
}
