//! 桌面异常通知：Windows 原生 toast（注册 AppUserModelID 后系统通知中心可收）。
//!
//! 使用方：自动签到失败（checkin::run_checkin_cycle）、GLM 代理上游异常
//! （glm_proxy）。[`notify`] 按类别节流（默认 10 分钟一条），避免高峰期
//! 限流类错误刷屏。

use std::collections::HashMap;
use std::sync::Mutex;

const AUM_ID: &str = "SylviaSwitch.Notification";
const APP_NAME: &str = "Sylvia Switch";
/// 同一类别通知的最小间隔（毫秒）。
const THROTTLE_MS: i64 = 10 * 60 * 1000;

static LAST_NOTIFY: Mutex<Option<HashMap<&'static str, i64>>> = Mutex::new(None);

/// XML 文本转义（toast 内容走 XmlDocument 解析）。
fn escape_xml(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '&' => "&amp;".to_string(),
            '\'' => "&apos;".to_string(),
            '"' => "&quot;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// 当前时间毫秒（独立于 config，避免循环依赖）。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn should_throttle(category: &'static str) -> bool {
    let mut last = LAST_NOTIFY.lock().unwrap();
    let map = last.get_or_insert_with(HashMap::new);
    let now = now_ms();
    if map
        .get(category)
        .map(|ts| now - *ts < THROTTLE_MS)
        .unwrap_or(false)
    {
        return true;
    }
    map.insert(category, now);
    false
}

/// 发送桌面 toast；同类通知按 10 分钟节流。非 Windows 或发送失败时静默。
pub fn notify(category: &'static str, title: &str, text: &str) {
    if should_throttle(category) {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = show_toast(title, text) {
            eprintln!("[notify] toast 发送失败: {e}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (title, text);
    }
}

#[cfg(target_os = "windows")]
fn ensure_aum_id() -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(r"Software\Classes\AppUserModelId\SylviaSwitch.Notification")
        .map_err(|e| format!("注册 AUM ID 失败: {e}"))?;
    key.set_value("DisplayName", &APP_NAME)
        .map_err(|e| format!("写入 DisplayName 失败: {e}"))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn show_toast(title: &str, text: &str) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    ensure_aum_id()?;
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape_xml(title),
        escape_xml(text)
    );
    let document = XmlDocument::new().map_err(|e| e.to_string())?;
    document
        .LoadXml(&HSTRING::from(xml))
        .map_err(|e| format!("toast XML 解析失败: {e}"))?;
    let toast = ToastNotification::CreateToastNotification(&document)
        .map_err(|e| e.to_string())?;
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUM_ID))
            .map_err(|e| e.to_string())?;
    notifier.Show(&toast).map_err(|e| format!("toast 显示失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_xml_handles_special_chars() {
        assert_eq!(escape_xml("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&apos;");
    }

    #[test]
    fn throttle_suppresses_repeat_within_window() {
        // 同一类别第一次放行，第二次在节流窗口内被抑制。
        assert!(!should_throttle("test-throttle-cat"));
        assert!(should_throttle("test-throttle-cat"));
    }
}
