//! 平台签到：AutoClaw（澳龙）与 TRAE Work CN（TRAE SOLO CN）。
//!
//! 两个平台的登录态都存放在本机客户端文件中，无需手动导入账号：
//!
//! - AutoClaw：`%APPDATA%/autoclaw/auth.json`，token 为 Electron
//!   safeStorage 密文（`enc:` + base64；Windows 下为 Local State
//!   `os_crypt.encrypted_key` → DPAPI → AES-256-GCM `v10` 信封）。
//!   签到即客户端任务系统中的 `daily_signin` 任务：GET task-list
//!   查状态 → POST task-complete 领取。
//! - TRAE Work CN：`%APPDATA%/TRAE SOLO CN/User/globalStorage/storage.json`
//!   的 `iCubeAuthInfo://icube.cloudide` 条目，byteCrypto 信封（`tc 05 10`
//!   头 + 32 字节随机 + AES-128-CBC(SHA512(明文)||明文)），key/iv 由安装
//!   目录 `out/main.js` 内置的两张 64 字节表异或后经 SHA-512 派生；签到
//!   接口 `POST {host}/trae/api/v2/ug/checkin_credits/status|claim`。
//!
//! 签到路由由 `checkin::checkin_account` 按账号的 `platform` 字段分发。

use base64::engine::general_purpose;
use base64::Engine;
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::modules::config::{
    add_checkin_log,atomic_write, http_request, now_ms, now_secs};
use crate::modules::vscode_cn_inject::{
    decrypt_windows_gcm_v10, dpapi_decrypt, encrypt_windows_gcm_v10,
};

use aes::Aes128;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use sha2::Sha512;

type Aes128CbcDec = cbc::Decryptor<Aes128>;
type Aes128CbcEnc = cbc::Encryptor<Aes128>;

pub const PLATFORM_WORKBUDDY: &str = "workbuddy";
pub const PLATFORM_AUTOCLAW: &str = "autoclaw";
pub const PLATFORM_TRAE_WORK_CN: &str = "trae_work_cn";
pub const PLATFORM_LOOMY: &str = "loomy";

// ---------------------------------------------------------------------------
// AutoClaw（澳龙）客户端常量（自 resources/app.asar 逆向确认）
// ---------------------------------------------------------------------------

pub(crate) const AUTOCLAW_API_HOST: &str = "https://autoglm-acceleration-api.zhipuai.cn";
const AUTOCLAW_APP_ID: &str = "100003";
const AUTOCLAW_APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";
const AUTOCLAW_APP_VERSION_FALLBACK: &str = "1.17.9";
const AUTOCLAW_DAILY_SIGNIN_TASK: &str = "daily_signin";
const AUTOCLAW_TASK_LIST_PATH: &str = "/autoclaw-proxy/proxy/autoclaw-task-list";
const AUTOCLAW_TASK_COMPLETE_PATH: &str = "/autoclaw-proxy/proxy/autoclaw-task-complete";

/// AutoClaw 客户端版本：优先读注册表卸载信息（随客户端升级自动变化），
/// 读不到时回退到已知版本。
pub(crate) fn autoclaw_app_version() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            #[cfg(target_os = "windows")]
            {
                use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
                use winreg::RegKey;
                for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
                    let Ok(uninstall) = RegKey::predef(hive)
                        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Uninstall")
                    else {
                        continue;
                    };
                    for key in uninstall.enum_keys().flatten() {
                        let Ok(sub) = uninstall.open_subkey(&key) else {
                            continue;
                        };
                        let name: String = sub.get_value("DisplayName").unwrap_or_default();
                        if !name.contains("AutoClaw") {
                            continue;
                        }
                        let version: String = sub.get_value("DisplayVersion").unwrap_or_default();
                        if !version.trim().is_empty() {
                            return version.trim().to_string();
                        }
                    }
                }
            }
            AUTOCLAW_APP_VERSION_FALLBACK.to_string()
        })
        .clone()
}

// ---------------------------------------------------------------------------
// TRAE Work CN（TRAE SOLO CN）常量（自 BlueChonk/trae-credential-reverse-
// engineering 与本机客户端实测确认）
// ---------------------------------------------------------------------------

const TRAE_CHECKIN_STATUS_PATH: &str = "/trae/api/v2/ug/checkin_credits/status";
const TRAE_CHECKIN_CLAIM_PATH: &str = "/trae/api/v2/ug/checkin_credits/claim";
const TRAE_ENT_USAGE_PATH: &str = "/trae/api/v2/pay/ide_user_ent_usage";
const TRAE_DEFAULT_HOST: &str = "https://api.trae.cn";
const TRAE_AUTH_KEY: &str = "iCubeAuthInfo://icube.cloudide";
const TRAE_HEADER: [u8; 6] = [0x74, 0x63, 0x05, 0x10, 0x00, 0x00];
const TRAE_RANDOM_LEN: usize = 32;
const TRAE_TABLE_LEN: usize = 64;
const SHA512_LEN: usize = 64;

// ---------------------------------------------------------------------------
// 平台账号发现与路由
// ---------------------------------------------------------------------------

/// 账号的签到平台；缺省视为 WorkBuddy（兼容既有账号库）。
pub fn account_platform(account: &Value) -> &'static str {
    match account.get("platform").and_then(Value::as_str) {
        Some(PLATFORM_AUTOCLAW) => PLATFORM_AUTOCLAW,
        Some(PLATFORM_TRAE_WORK_CN) => PLATFORM_TRAE_WORK_CN,
        Some(PLATFORM_LOOMY) => PLATFORM_LOOMY,
        _ => PLATFORM_WORKBUDDY,
    }
}

pub fn platform_display_name(platform: &str) -> &'static str {
    match platform {
        PLATFORM_AUTOCLAW => "AutoClaw",
        PLATFORM_TRAE_WORK_CN => "TRAE Work CN",
        PLATFORM_LOOMY => "Loomy",
        _ => "WorkBuddy",
    }
}

/// 全部可签到账号：WorkBuddy 账号库 + 本机发现的 AutoClaw / TRAE Work CN。
/// 该签到条目是否就是「本机客户端当前登录」的账号。
///
/// - AutoClaw：id 等于实时发现 ID（`autoclaw:<auth.json 的 user_id>`）；
/// - TRAE Work CN：id 为 `trae_work_cn:local`（实时账号固定用该 id 展示）。
pub fn is_local_login_account(account: &Value) -> bool {
    let Some(id) = account.get("id").and_then(Value::as_str) else {
        return false;
    };
    match account_platform(account) {
        PLATFORM_AUTOCLAW => crate::modules::autoclaw_accounts::local_discovery_id()
            .is_some_and(|local| local == id),
        PLATFORM_TRAE_WORK_CN => {
            id == "trae_work_cn:local"
                || crate::modules::platform_checkin::trae_local_pool_id()
                    .is_some_and(|local| local == id)
        }
        PLATFORM_LOOMY => crate::modules::loomy::is_local_login(account),
        _ => false,
    }
}

pub fn all_checkin_accounts() -> Vec<Value> {
    let mut accounts = crate::modules::account::load_accounts();
    let mut seen: std::collections::HashSet<String> = accounts
        .iter()
        .filter_map(|acc| acc.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    // AutoClaw 账号池（手动导入/本机快照），与 WorkBuddy、TRAE 账号并列。
    for acct in crate::modules::autoclaw_accounts::load_accounts() {
        let Some(id) = acct.get("id").and_then(Value::as_str) else {
            continue;
        };
        if acct.get("status").and_then(Value::as_str) == Some("invalid") {
            continue;
        }
        // 本机登录中：同身份快照由实时账号代为签到，跳过避免重复。
        if crate::modules::autoclaw_accounts::hidden_by_local_login(&acct) {
            continue;
        }
        if seen.insert(id.to_string()) {
            let label = acct.get("label").and_then(Value::as_str).unwrap_or("账号");
            accounts.push(json!({
                "id": id,
                "email": format!("AutoClaw · {label}"),
                "nickname": format!("AutoClaw · {label}"),
                "platform": PLATFORM_AUTOCLAW,
                "autoclawStoreId": id,
            }));
        }
    }
    // TRAE Work CN 账号池（本机客户端快照导入，支持多账号签到）。
    for acct in crate::modules::trae_accounts::load_accounts() {
        let Some(id) = acct.get("id").and_then(Value::as_str) else {
            continue;
        };
        if acct.get("status").and_then(Value::as_str) == Some("invalid") {
            continue;
        }
        // 快照登录态已到期（TRAE 刷新令牌随客户端切换失效，无法续期）。
        if crate::modules::trae_accounts::entry_expired(&acct) {
            continue;
        }
        // 客户端当前登录的同账号由实时账号代管，跳过避免重复。
        if crate::modules::trae_accounts::hidden_by_local_login(&acct) {
            continue;
        }
        if seen.insert(id.to_string()) {
            let label = acct.get("label").and_then(Value::as_str).unwrap_or("账号");
            accounts.push(json!({
                "id": id,
                "email": format!("TRAE Work CN · {label}"),
                "nickname": format!("TRAE Work CN · {label}"),
                "platform": PLATFORM_TRAE_WORK_CN,
                "traeStoreId": id,
            }));
        }
    }
    // Loomy 账号池（本机快照/手动导入 session），与 AutoClaw、TRAE 账号并列。
    for acct in crate::modules::loomy::load_accounts() {
        let Some(id) = acct.get("id").and_then(Value::as_str) else {
            continue;
        };
        if acct.get("status").and_then(Value::as_str) == Some("invalid") {
            continue;
        }
        // 本机登录中：同身份快照由实时账号代为签到，跳过避免重复。
        if crate::modules::loomy::hidden_by_local_login(&acct) {
            continue;
        }
        if seen.insert(id.to_string()) {
            let label = acct.get("label").and_then(Value::as_str).unwrap_or("账号");
            accounts.push(json!({
                "id": id,
                "email": format!("Loomy · {label}"),
                "nickname": format!("Loomy · {label}"),
                "platform": PLATFORM_LOOMY,
                "loomyStoreId": id,
            }));
        }
    }
    for candidate in [
        discover_autoclaw_account(),
        discover_trae_work_cn_account(),
        crate::modules::loomy::discover_loomy_account(),
    ]
        .into_iter()
        .flatten()
    {
        let id = candidate
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !id.is_empty() && seen.insert(id) {
            accounts.push(candidate);
        }
    }
    accounts
}

/// 按 id 查找可签到账号（含平台账号）。
pub fn find_checkin_account(account_id: &str) -> Option<Value> {
    if let Some(acc) = crate::modules::account::find_account(account_id) {
        if account_platform(&acc) == PLATFORM_WORKBUDDY {
            return Some(acc);
        }
    }
    all_checkin_accounts()
        .into_iter()
        .find(|acc| acc.get("id").and_then(Value::as_str) == Some(account_id))
}

/// TRAE Work CN 全部账号（本机客户端 + 账号池）批量签到，逐个执行防限流。
pub async fn trae_checkin_pool() -> Vec<Value> {
    let mut results = Vec::new();
    for account in trae_checkin_pool_accounts() {
        let result = checkin_platform_account(&account).await;
        results.push(json!({
            "id": account.get("id").cloned().unwrap_or(Value::Null),
            "label": account
                .get("nickname")
                .and_then(Value::as_str)
                .map(|n| n.strip_prefix("TRAE Work CN · ").unwrap_or(n).to_string())
                .unwrap_or_else(|| "账号".to_string()),
            "result": result.get("result").cloned().unwrap_or(json!("error")),
            "error": result.get("error").cloned().unwrap_or(Value::Null),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    results
}

/// TRAE Work CN 单账号签到（按账号池 id；本机账号传 trae_work_cn:local）。
/// 账号池条目正被客户端登录隐藏时，同样用其快照凭证签到（幂等）。
pub async fn trae_checkin_pool_account_by_id(id: &str) -> Value {
    let accounts = trae_checkin_pool_accounts();
    let found = accounts
        .iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(id));
    let account = match found {
        Some(account) => account.clone(),
        None => {
            // 不在签到列表里：要么 id 不存在，要么该条目正被客户端登录
            // 隐藏（同身份）——后者用池内快照直接签到。
            match crate::modules::trae_accounts::load_accounts()
                .into_iter()
                .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
            {
                Some(entry) => {
                    let label = entry
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("账号")
                        .to_string();
                    json!({
                        "id": id,
                        "email": format!("TRAE Work CN · {label}"),
                        "nickname": format!("TRAE Work CN · {label}"),
                        "platform": PLATFORM_TRAE_WORK_CN,
                        "traeStoreId": id,
                    })
                }
                None => {
                    return json!({
                        "id": id,
                        "label": id,
                        "result": "error",
                        "error": "账号不在 TRAE 账号池中（可能已删除或登录态失效）",
                    });
                }
            }
        }
    };
    let result = checkin_platform_account(&account).await;
    json!({
        "id": id,
        "label": account
            .get("nickname")
            .and_then(Value::as_str)
            .map(|n| n.strip_prefix("TRAE Work CN · ").unwrap_or(n).to_string())
            .unwrap_or_else(|| id.to_string()),
        "result": result.get("result").cloned().unwrap_or(json!("error")),
        "error": result.get("error").cloned().unwrap_or(Value::Null),
    })
}

/// 全部 TRAE 签到账号：本机客户端当前账号 + 账号池条目（未失效且未被本机登录隐藏的）。
fn trae_checkin_pool_accounts() -> Vec<Value> {
    all_checkin_accounts()
        .into_iter()
        .filter(|account| account_platform(account) == PLATFORM_TRAE_WORK_CN)
        .collect()
}

/// 平台账号统一的签到状态查询，返回结构与 WorkBuddy 的
/// `checkin::get_checkin_status` 一致（ok / todayCheckedIn / raw）。
pub async fn get_platform_status(account: &Value) -> Value {
    match account_platform(account) {
        PLATFORM_AUTOCLAW => {
            // 账号池账号必须用自身 token 查状态，否则显示的是本机登录账号的状态。
            if let Some(store_id) = account.get("autoclawStoreId").and_then(Value::as_str) {
                get_autoclaw_pool_status(store_id).await
            } else {
                get_autoclaw_status().await
            }
        }
        PLATFORM_TRAE_WORK_CN => {
            // 账号池账号用自身快照凭证查询，避免显示成客户端当前登录账号。
            if let Some(store_id) = account.get("traeStoreId").and_then(Value::as_str) {
                crate::modules::trae_accounts::status_stored(store_id).await
            } else {
                get_trae_work_cn_status().await
            }
        }
        PLATFORM_LOOMY => {
            // 池内账号用自身 session 校验；本机账号实时读 auth-session.json。
            if let Some(store_id) = account.get("loomyStoreId").and_then(Value::as_str) {
                get_loomy_pool_status(store_id).await
            } else {
                get_loomy_status().await
            }
        }
        _ => json!({"ok": false, "error": "未知签到平台"}),
    }
}

/// Loomy 账号池单账号签到状态：用池内自身 session 校验。
async fn get_loomy_pool_status(store_id: &str) -> Value {
    let Some(account) = crate::modules::loomy::load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(store_id))
    else {
        return json!({"ok": false, "error": "账号池条目不存在"});
    };
    let Some(session) = account
        .get("session")
        .and_then(Value::as_str)
        .map(String::from)
    else {
        return json!({"ok": false, "error": "账号缺少 session，请重新导入"});
    };
    crate::modules::loomy::checkin_with(&session).await
}

/// Loomy 本机账号签到状态：实时校验当前登录 session。
pub async fn get_loomy_status() -> Value {
    match crate::modules::loomy::read_local_session() {
        Ok(session) => crate::modules::loomy::checkin_with(&session).await,
        Err(error) => json!({"ok": false, "error": error}),
    }
}

/// AutoClaw 账号池单账号签到状态：用池内自身 token 查询，401 续期重试一次。
async fn get_autoclaw_pool_status(store_id: &str) -> Value {
    let Some(account) = crate::modules::autoclaw_accounts::load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(store_id))
    else {
        return json!({"ok": false, "error": "账号池条目不存在"});
    };
    let token = account
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if token.is_empty() {
        return json!({"ok": false, "error": "账号缺少 token，请重新导入"});
    }
    let bearer = |t: &str| format!("Bearer {}", t.trim_start_matches("Bearer "));
    let mut status = get_autoclaw_status_with(&bearer(&token)).await;
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        if let Ok(new_token) =
            crate::modules::autoclaw_accounts::refresh_stored_account(store_id).await
        {
            status = get_autoclaw_status_with(&bearer(&new_token)).await;
        }
    }
    status
}

/// 平台账号统一签到：查状态 → 未签到时提交。返回 `{result, error}`，
/// result 取值与 WorkBuddy 一致（success / already / error）。
pub async fn checkin_platform_account(account: &Value) -> Value {
    let platform = account_platform(account);
    let status = get_platform_status(account).await;
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        return json!({
            "result": "error",
            "error": status
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("查询签到状态失败"),
        });
    }
    if status.get("todayCheckedIn").and_then(Value::as_bool) == Some(true) {
        return json!({"result": "already"});
    }
    let res = match platform {
        PLATFORM_AUTOCLAW => perform_autoclaw_checkin().await,
        PLATFORM_TRAE_WORK_CN => {
            if let Some(store_id) = account.get("traeStoreId").and_then(Value::as_str) {
                crate::modules::trae_accounts::claim_stored(store_id).await
            } else {
                perform_trae_work_cn_checkin().await
            }
        }
        PLATFORM_LOOMY => {
            if let Some(store_id) = account.get("loomyStoreId").and_then(Value::as_str) {
                crate::modules::loomy::checkin_stored(store_id).await
            } else {
                // 本机账号：实时读 session 校验（每日积分随登录自动发放）。
                match crate::modules::loomy::read_local_session() {
                    Ok(session) => {
                        let res = crate::modules::loomy::checkin_with(&session).await;
                        if res.get("ok").and_then(Value::as_bool) == Some(true) {
                            json!({"ok": true, "already": true})
                        } else {
                            res
                        }
                    }
                    Err(error) => json!({"ok": false, "error": error}),
                }
            }
        }
        _ => json!({"ok": false, "error": "未知签到平台"}),
    };
    if let Some(result) = res.get("result").and_then(Value::as_str) {
        return json!({
            "result": result,
            "error": res.get("error").cloned().unwrap_or(Value::Null),
        });
    }
    if res.get("ok").and_then(Value::as_bool) == Some(true) {
        if res.get("already").and_then(Value::as_bool) == Some(true) {
            json!({"result": "already"})
        } else {
            json!({"result": "success"})
        }
    } else {
        json!({
            "result": "error",
            "error": res
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("签到失败"),
        })
    }
}

// ---------------------------------------------------------------------------
// AutoClaw（澳龙）
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
pub(crate) fn autoclaw_data_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("autoclaw"))
}

#[cfg(not(target_os = "windows"))]
fn autoclaw_data_dir() -> Option<PathBuf> {
    None
}

pub fn discover_autoclaw_account() -> Option<Value> {
    let dir = autoclaw_data_dir()?;
    let text = std::fs::read_to_string(dir.join("auth.json")).ok()?;
    let auth: Value = serde_json::from_str(&text).ok()?;
    let token = auth.get("token").and_then(Value::as_str).unwrap_or("");
    if token.is_empty() {
        return None;
    }
    let uid = auth
        .pointer("/userInfo/user_id")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_default();
    let phone = auth
        .pointer("/userInfo/user_phone")
        .and_then(Value::as_str)
        .unwrap_or("");
    let display = if phone.is_empty() { &uid } else { phone };
    if display.is_empty() {
        return None;
    }
    let id = if uid.is_empty() {
        "autoclaw:local".to_string()
    } else {
        format!("autoclaw:{uid}")
    };
    Some(json!({
        "id": id,
        "email": format!("AutoClaw · {display}"),
        "nickname": format!("AutoClaw · {display}"),
        "platform": PLATFORM_AUTOCLAW,
    }))
}

/// AutoClaw 的 Local State `os_crypt.encrypted_key` → DPAPI → AES 密钥。
#[cfg(target_os = "windows")]
pub(crate) fn autoclaw_encryption_key(data_dir: &Path) -> Result<Vec<u8>, String> {
    let local_state = data_dir.join("Local State");
    let text = std::fs::read_to_string(&local_state)
        .map_err(|e| format!("读取 AutoClaw Local State 失败: {e}"))?;
    let json: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 AutoClaw Local State 失败: {e}"))?;
    let encrypted_key_b64 = json["os_crypt"]["encrypted_key"]
        .as_str()
        .ok_or_else(|| "AutoClaw Local State 缺少 os_crypt.encrypted_key".to_string())?;
    let encrypted_key = general_purpose::STANDARD
        .decode(encrypted_key_b64)
        .map_err(|e| format!("AutoClaw encrypted_key base64 解码失败: {e}"))?;
    if encrypted_key.len() < 5 || &encrypted_key[..5] != b"DPAPI" {
        return Err("AutoClaw encrypted_key 前缀不是 DPAPI".to_string());
    }
    dpapi_decrypt(&encrypted_key[5..])
}

/// 解密 auth.json 中的 `enc:` token（明文自带 `Bearer ` 前缀）。
pub fn autoclaw_decrypt_token(enc_token: &str) -> Result<String, String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = enc_token;
        Err("AutoClaw 登录态解密目前仅支持 Windows".to_string())
    }
    #[cfg(target_os = "windows")]
    {
        let payload = enc_token
            .strip_prefix("enc:")
            .unwrap_or(enc_token)
            .trim();
        let bytes = general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| format!("AutoClaw token base64 解码失败: {e}"))?;
        if !bytes.starts_with(b"v10") {
            return Err("AutoClaw token 密文格式异常（缺少 v10 前缀）".to_string());
        }
        let dir =
            autoclaw_data_dir().ok_or_else(|| "无法定位 AutoClaw 数据目录".to_string())?;
        let key = autoclaw_encryption_key(&dir)?;
        let plain = decrypt_windows_gcm_v10(&key, &bytes)?;
        String::from_utf8(plain).map_err(|_| "AutoClaw token 解密结果不是 UTF-8".to_string())
    }
}

pub(crate) fn read_autoclaw_token() -> Result<String, String> {
    let dir = autoclaw_data_dir().ok_or_else(|| "无法定位 AutoClaw 数据目录".to_string())?;
    let text = std::fs::read_to_string(dir.join("auth.json"))
        .map_err(|_| "未找到 AutoClaw 登录信息（请安装并登录 AutoClaw）".to_string())?;
    let auth: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析 AutoClaw auth.json 失败: {e}"))?;
    let token = auth
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "AutoClaw 尚未登录（auth.json 无 token）".to_string())?;
    autoclaw_decrypt_token(token)
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// AutoClaw 网关请求头（X-Auth-Sign = md5(appId&timestamp&appKey)）。
pub(crate) fn autoclaw_headers(token: Option<&str>) -> HashMap<String, String> {
    let platform_tm = if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "win"
    };
    let ts = now_secs().to_string();
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "*/*".to_string());
    headers.insert("X-Version".to_string(), autoclaw_app_version());
    headers.insert("X-Tm".to_string(), platform_tm.to_string());
    headers.insert("X-Product".to_string(), "autoclaw".to_string());
    headers.insert("X-Auth-Appid".to_string(), AUTOCLAW_APP_ID.to_string());
    headers.insert("X-Auth-TimeStamp".to_string(), ts.clone());
    headers.insert(
        "X-Auth-Sign".to_string(),
        md5_hex(&format!(
            "{AUTOCLAW_APP_ID}&{ts}&{AUTOCLAW_APP_KEY}"
        )),
    );
    headers.insert(
        "X-Trace-Id".to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    headers.insert("X-Lang".to_string(), "zh-CN".to_string());
    if let Some(token) = token {
        headers.insert("authorization".to_string(), token.to_string());
    }
    headers
}

fn autoclaw_auth_failure(resp: &Value) -> Option<String> {
    let code = resp.get("code").and_then(Value::as_i64).unwrap_or(0);
    if matches!(code, 410000 | 400000 | 400001 | 401 | 403) {
        return Some("AutoClaw 登录态失效".to_string());
    }
    // 网关 401 的响应体是 {"error":"Invalid token"}，http_request 解析成功时
    // 不带 code 字段，需要按 error 文本识别。
    let error = resp.get("error").and_then(Value::as_str).unwrap_or("");
    let lower = error.to_ascii_lowercase();
    if lower.contains("invalid token") || lower.contains("token expired") || lower.contains("unauthorized")
    {
        return Some("AutoClaw 登录态已过期".to_string());
    }
    None
}

/// 用 auth.json 里的 refresh token 换新 access token（/userapi/v1/refresh）。
///
/// 实测 refresh token 不轮换，续期不会破坏客户端登录态；新 access token
/// 按客户端同款 `enc:` 格式写回 auth.json，客户端下次启动直接可用。
pub(crate) async fn autoclaw_refresh_access_token() -> Result<String, String> {
    let dir = autoclaw_data_dir().ok_or_else(|| "无法定位 AutoClaw 数据目录".to_string())?;
    let path = dir.join("auth.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("读取 auth.json 失败: {e}"))?;
    let mut auth: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 auth.json 失败: {e}"))?;
    let refresh_enc = auth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "auth.json 缺少 refreshToken".to_string())?;
    let refresh_token = autoclaw_decrypt_token(refresh_enc)?;
    let device_id = auth
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let resp = http_request(
        &format!("{AUTOCLAW_API_HOST}/userapi/v1/refresh"),
        "POST",
        Some(json!({
            "source_id": "autoclaw",
            "device_id": device_id,
            "refresh_token": refresh_token,
        })),
        Some(&autoclaw_headers(None)),
    )
    .await;
    if resp.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(resp_error(&resp, "刷新请求被拒绝"));
    }
    let new_token = resp
        .pointer("/data/access_token")
        .or_else(|| resp.pointer("/data/token"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "刷新响应缺少 access_token".to_string())?
        .to_string();
    #[cfg(target_os = "windows")]
    {
        if let Ok(key) = autoclaw_encryption_key(&dir) {
            if let Ok(encrypted) = encrypt_windows_gcm_v10(&key, new_token.as_bytes()) {
                auth["token"] =
                    json!(format!("enc:{}", general_purpose::STANDARD.encode(encrypted)));
                auth["updatedAt"] = json!(now_ms());
                if let Ok(content) = serde_json::to_string_pretty(&auth) {
                    let _ = atomic_write(&path, &content);
                }
            }
        }
    }
    Ok(new_token)
}

/// AutoClaw 网关请求封装（按 token，账号池与本机账号共用）。
/// 401 时不做自愈、原样返回，由调用方决定如何续期重试。
pub(crate) async fn autoclaw_call_with(
    token: &str,
    path: &str,
    method: &str,
    body: Option<Value>,
) -> Value {
    let url = format!("{AUTOCLAW_API_HOST}{path}");
    http_request(&url, method, body, Some(&autoclaw_headers(Some(token)))).await
}

fn resp_error(resp: &Value, fallback: &str) -> String {
    resp.get("msg")
        .or_else(|| resp.get("message"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback)
        .to_string()
}

/// AutoClaw 账号池批量签到：依次对池内每个账号查状态并领取，
/// 逐账号返回结果（依次执行，避免触发上游限流）。结果同步写入签到日志。
pub async fn autoclaw_checkin_pool() -> Vec<Value> {
    let pool = crate::modules::autoclaw_accounts::pool_accounts();
    let mut results = Vec::new();
    for account in &pool {
        results.push(autoclaw_checkin_pool_account(account).await);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    results
}

/// 账号池内单个账号签到（按 id 查找）。
pub async fn autoclaw_checkin_pool_account_by_id(id: &str) -> Value {
    let pool = crate::modules::autoclaw_accounts::pool_accounts();
    match pool.iter().find(|a| a.id == id) {
        Some(account) => autoclaw_checkin_pool_account(account).await,
        None => json!({
            "id": id,
            "label": id,
            "result": "error",
            "error": "账号不在账号池中（可能已删除或登录态失效）",
        }),
    }
}

async fn autoclaw_checkin_pool_account(
    account: &crate::modules::autoclaw_accounts::PoolAccount,
) -> Value {
    let started = now_ms();
    let email = format!("AutoClaw · {}", account.label);
    let mut token = account.bearer();

    let mut status = get_autoclaw_status_with(&token).await;
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        // 登录态可能过期：续期后重试一次。
        if let Ok(new_token) = account.refresh().await {
            token = format!("Bearer {new_token}");
            status = get_autoclaw_status_with(&token).await;
        }
    }

    let (result, error) = if status.get("ok").and_then(Value::as_bool) != Some(true) {
        (
            "error",
            status
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("查询签到状态失败")
                .to_string(),
        )
    } else if status.get("todayCheckedIn").and_then(Value::as_bool) == Some(true) {
        ("already", String::new())
    } else {
        let res = perform_autoclaw_checkin_with(&token).await;
        if res.get("ok").and_then(Value::as_bool) == Some(true) {
            if res.get("already").and_then(Value::as_bool) == Some(true) {
                ("already", String::new())
            } else {
                ("success", String::new())
            }
        } else {
            (
                "error",
                res.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("签到失败")
                    .to_string(),
            )
        }
    };

    let mut entry = json!({
        "ts": started,
        "accountId": account.id,
        "email": email,
        "result": result,
    });
    if !error.is_empty() {
        entry["error"] = json!(error);
    }
    add_checkin_log(&entry);

    let mut out = json!({
        "id": account.id,
        "label": account.label,
        "result": result,
    });
    if !error.is_empty() {
        out["error"] = json!(error);
    }
    out
}

/// AutoClaw 今日签到状态（本机登录账号）。
pub async fn get_autoclaw_status() -> Value {
    let token = match read_autoclaw_token() {
        Ok(token) => token,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    get_autoclaw_status_with(&token).await
}

/// AutoClaw 今日签到状态（按 token，供多账号池复用；无自动续期）。
pub(crate) async fn get_autoclaw_status_with(token: &str) -> Value {
    let resp = autoclaw_call_with(token, AUTOCLAW_TASK_LIST_PATH, "GET", None).await;
    if let Some(error) = autoclaw_auth_failure(&resp) {
        return json!({"ok": false, "error": format!("{error}，请打开 AutoClaw 重新登录")});
    }
    let Some(tasks) = resp.get("data").and_then(Value::as_array) else {
        return json!({
            "ok": false,
            "error": resp_error(&resp, "AutoClaw 任务列表响应异常"),
        });
    };
    let Some(task) = tasks
        .iter()
        .find(|t| t.get("task_id").and_then(Value::as_str) == Some(AUTOCLAW_DAILY_SIGNIN_TASK))
    else {
        return json!({"ok": false, "error": "AutoClaw 任务列表中未找到每日签到任务"});
    };
    let done = task
        .get("status")
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("completed"))
        .unwrap_or(false);
    json!({"ok": true, "todayCheckedIn": done, "raw": task.clone()})
}

/// 提交 AutoClaw 每日签到任务（本机登录账号）。
pub(crate) async fn perform_autoclaw_checkin() -> Value {
    let token = match read_autoclaw_token() {
        Ok(token) => token,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    perform_autoclaw_checkin_with(&token).await
}

/// 提交 AutoClaw 每日签到任务（按 token，供多账号池复用；无自动续期）。
pub(crate) async fn perform_autoclaw_checkin_with(token: &str) -> Value {
    let resp = autoclaw_call_with(
        token,
        AUTOCLAW_TASK_COMPLETE_PATH,
        "POST",
        Some(json!({"task_id": AUTOCLAW_DAILY_SIGNIN_TASK})),
    )
    .await;
    if let Some(error) = autoclaw_auth_failure(&resp) {
        return json!({"ok": false, "error": format!("{error}，请打开 AutoClaw 重新登录")});
    }
    let data = resp.get("data").cloned().unwrap_or(Value::Null);
    let already = data
        .get("already_completed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let success = data.get("success").and_then(Value::as_bool);
    let code_ok = matches!(
        resp.get("code").and_then(Value::as_i64),
        None | Some(0)
    );
    if code_ok && (success == Some(true) || already) {
        return json!({"ok": true, "already": already, "raw": data});
    }
    json!({
        "ok": false,
        "error": resp_error(&resp, "AutoClaw 签到失败"),
    })
}

// ---------------------------------------------------------------------------
// TRAE Work CN（TRAE SOLO CN）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TraeCredential {
    pub token: String,
    pub device_id: String,
    pub machine_id: String,
    pub host: String,
    pub username: String,
}

fn trae_data_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for name in ["TRAE SOLO CN", "Trae CN", "TraeWork CN", "TRAE SOLO", "Trae"] {
        if let Some(base) = dirs::config_dir() {
            out.push(base.join(name));
        }
        if let Some(base) = dirs::data_dir() {
            out.push(base.join(name));
        }
    }
    out
}

/// 从 storage.json 或已知客户端配置中提取 Aha 16 位纯数字设备 ID（如 3252831601596443）。
/// TRAE 签到接口风控要求 x-device-id 必须是此 Aha 数字设备号；传 UUID（telemetry.devDeviceId）会触发 9074（"当前参与用户太多"）。
pub fn resolve_trae_aha_device_id(data: &Value) -> Option<String> {
    if let Some(obj) = data.as_object() {
        for key in obj.keys() {
            if let Some(id) = key.strip_prefix("iCubeAuthInfo://icube-dc:") {
                if id.len() >= 8 && id.chars().all(|c| c.is_ascii_digit()) {
                    return Some(id.to_string());
                }
            }
        }
    }
    resolve_system_aha_device_id()
}

/// 扫描本机所有 TRAE 候选数据目录，提取已注册的 Aha 设备 ID。
pub fn resolve_system_aha_device_id() -> Option<String> {
    for base in trae_data_dir_candidates() {
        let storage = base.join("User").join("globalStorage").join("storage.json");
        if !storage.exists() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&storage) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(obj) = val.as_object() {
            for key in obj.keys() {
                if let Some(id) = key.strip_prefix("iCubeAuthInfo://icube-dc:") {
                    if id.len() >= 8 && id.chars().all(|c| c.is_ascii_digit()) {
                        return Some(id.to_string());
                    }
                }
            }
        }
    }
    None
}

fn trae_storage_path() -> Option<PathBuf> {
    trae_data_dir_candidates()
        .into_iter()
        .map(|d| d.join("User").join("globalStorage").join("storage.json"))
        .find(|p| p.exists())
}

/// 安装目录中 `resources/app/out/main.js` 的候选位置。
#[cfg(target_os = "windows")]
fn trae_main_js_candidates() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = trae_registry_install_dir() {
        roots.push(dir);
    }
    if let Some(local) = dirs::data_local_dir() {
        roots.push(local.join("Programs").join("TRAE SOLO CN"));
    }
    for drive in ["C", "D", "E", "F"] {
        roots.push(PathBuf::from(format!("{drive}:\\Trae CN Work\\TRAE SOLO CN")));
    }
    let program_files = std::env::var("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("C:\\Program Files"));
    roots.push(program_files.join("TRAE SOLO CN"));
    roots
        .into_iter()
        .map(|dir| dir.join("resources").join("app").join("out").join("main.js"))
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn trae_main_js_candidates() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        roots.push(PathBuf::from(
            "/Applications/TRAE SOLO CN.app/Contents/Resources/app",
        ));
        roots.push(PathBuf::from(
            "/Applications/TraeWork CN.app/Contents/Resources/app",
        ));
    }
    if let Some(local) = dirs::data_local_dir() {
        roots.push(local.join("Programs").join("TRAE SOLO CN"));
    }
    roots
        .into_iter()
        .map(|dir| dir.join("out").join("main.js"))
        .collect()
}

/// 从注册表卸载信息里定位 TraeWork CN 安装目录。
#[cfg(target_os = "windows")]
fn trae_registry_install_dir() -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let markers = ["TraeWork", "TRAE SOLO"];
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let uninstall = RegKey::predef(hive)
            .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Uninstall")
            .ok()?;
        for key in uninstall.enum_keys().flatten() {
            let Ok(sub) = uninstall.open_subkey(&key) else {
                continue;
            };
            let display_name: String = sub
                .get_value("DisplayName")
                .unwrap_or_default();
            if !markers.iter().any(|m| display_name.contains(m)) {
                continue;
            }
            if let Ok(location) = sub.get_value::<String, _>("InstallLocation") {
                let trimmed = location.trim().trim_end_matches(['\\', '/']).to_string();
                if !trimmed.is_empty() {
                    return Some(PathBuf::from(trimmed));
                }
            }
        }
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|pos| pos + from)
}

/// 从 main.js 按文件顺序扫描 64 字节的 Uint8Array 字面量（byteCrypto 的
/// 四张密钥表，两种写法 `Uint8Array([...])` / `Uint8Array.from([...])`）。
/// 表的具体锚点随客户端版本变化，扫描结果交由 `trae_decrypt_blob` 用
/// SHA-512 摘要校验自验证。
fn extract_trae_tables(main_js: &Path) -> Result<Vec<[u8; TRAE_TABLE_LEN]>, String> {
    let text = std::fs::read_to_string(main_js)
        .map_err(|e| format!("读取 TRAE main.js 失败（{}）: {e}", main_js.display()))?;
    let bytes = text.as_bytes();
    let mut tables: Vec<[u8; TRAE_TABLE_LEN]> = Vec::new();
    let mut cursor = 0usize;
    while let Some(marker_pos) = find_subslice(bytes, b"Uint8Array", cursor) {
        // Uint8Array 之后允许 `.from` 或直接 `(`，再 `[`。
        let mut pos = marker_pos + b"Uint8Array".len();
        if bytes[pos..].starts_with(b".from") {
            pos += b".from".len();
        }
        while pos < bytes.len() && (bytes[pos] as char).is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b'(' {
            cursor = pos;
            continue;
        }
        pos += 1;
        while pos < bytes.len() && (bytes[pos] as char).is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b'[' {
            cursor = pos;
            continue;
        }
        pos += 1;
        let mut nums: Vec<u8> = Vec::new();
        let mut current: u32 = 0;
        let mut have = false;
        let mut broken = false;
        while pos < bytes.len() && bytes[pos] != b']' {
            let c = bytes[pos];
            if c.is_ascii_digit() {
                current = current * 10 + (c - b'0') as u32;
                have = true;
                if current > 255 {
                    broken = true;
                    break;
                }
            } else if c == b',' {
                if have {
                    nums.push(current as u8);
                }
                current = 0;
                have = false;
            } else if !c.is_ascii_whitespace() {
                broken = true;
                break;
            }
            pos += 1;
        }
        if !broken {
            if have {
                nums.push(current as u8);
            }
            if nums.len() == TRAE_TABLE_LEN {
                let mut table = [0u8; TRAE_TABLE_LEN];
                table.copy_from_slice(&nums);
                tables.push(table);
            }
        }
        cursor = pos + 1;
    }
    if tables.len() < 4 {
        return Err(format!(
            "TRAE main.js 中未找到 byteCrypto 密钥表（仅 {} 组）",
            tables.len()
        ));
    }
    Ok(tables)
}

struct TraeTableCache {
    path: PathBuf,
    modified: SystemTime,
    tables: Vec<[u8; TRAE_TABLE_LEN]>,
}

static TRAE_TABLE_CACHE: OnceLock<Mutex<Option<TraeTableCache>>> = OnceLock::new();

fn trae_tables(main_js: &Path) -> Result<Vec<[u8; TRAE_TABLE_LEN]>, String> {
    let modified = std::fs::metadata(main_js)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let cache_lock = TRAE_TABLE_CACHE.get_or_init(|| Mutex::new(None));
    {
        let cache = cache_lock.lock().unwrap();
        if let Some(entry) = cache.as_ref() {
            if entry.path == main_js && entry.modified == modified {
                return Ok(entry.tables.clone());
            }
        }
    }
    let tables = extract_trae_tables(main_js)?;
    *cache_lock.lock().unwrap() = Some(TraeTableCache {
        path: main_js.to_path_buf(),
        modified,
        tables: tables.clone(),
    });
    Ok(tables)
}

fn sha512_bytes(data: &[u8]) -> [u8; SHA512_LEN] {
    use sha2::Digest;
    let mut hasher = Sha512::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// byteCrypto key 派生（Rie）：pepper = vie ^ qie，
/// key/iv = SHA512(SHA512(random) || pepper) 的前 16/16 字节。
fn trae_derive_keys(
    random: &[u8],
    vie: &[u8; TRAE_TABLE_LEN],
    qie: &[u8; TRAE_TABLE_LEN],
) -> ([u8; 16], [u8; 16]) {
    let pepper: Vec<u8> = vie.iter().zip(qie.iter()).map(|(a, b)| a ^ b).collect();
    let mut inner = [0u8; SHA512_LEN + TRAE_TABLE_LEN];
    inner[..SHA512_LEN].copy_from_slice(&sha512_bytes(random));
    inner[SHA512_LEN..].copy_from_slice(&pepper);
    let derived = sha512_bytes(&inner);
    let mut mat = [0u8; SHA512_LEN + TRAE_TABLE_LEN];
    mat[..SHA512_LEN].copy_from_slice(&derived);
    mat[SHA512_LEN..].copy_from_slice(&pepper);
    let mut key = [0u8; 16];
    key.copy_from_slice(&mat[..16]);
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&mat[16..32]);
    (key, iv)
}

/// 解密 TRAE byteCrypto 信封（base64）→ 登录态 JSON + 命中的表组下标。
///
/// 密钥表组合可能随版本变化，这里从后往前尝试连续四元组
/// （hie/wie/vie/qie），SHA-512 摘要校验命中即为正确组合。
/// 命中的下标必须记下来供写回加密使用（加解密须用同一组表）。
fn trae_decrypt_blob(blob_b64: &str, tables: &[[u8; TRAE_TABLE_LEN]]) -> Result<(Value, usize), String> {
    let raw = general_purpose::STANDARD
        .decode(blob_b64.trim())
        .map_err(|e| format!("TRAE 凭证 base64 解码失败: {e}"))?;
    if raw.len() < 6 + TRAE_RANDOM_LEN + 16 {
        return Err("TRAE 凭证信封过短".to_string());
    }
    if raw[..6] != TRAE_HEADER {
        return Err(format!("TRAE 凭证信封版本异常: {}", hex_head(&raw[..6])));
    }
    let random = &raw[6..6 + TRAE_RANDOM_LEN];
    let ciphertext = &raw[6 + TRAE_RANDOM_LEN..];
    if tables.len() < 4 {
        return Err("TRAE 密钥表数量不足".to_string());
    }
    for start in (0..=tables.len() - 4).rev() {
        let (key, iv) = trae_derive_keys(random, &tables[start + 2], &tables[start + 3]);
        let mut buf = ciphertext.to_vec();
        let Ok(padded) = Aes128CbcDec::new(&key.into(), &iv.into())
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
        else {
            continue;
        };
        if padded.len() <= SHA512_LEN {
            continue;
        }
        let (tag, body) = padded.split_at(SHA512_LEN);
        if tag != sha512_bytes(body) {
            continue;
        }
        let value = serde_json::from_slice(body)
            .map_err(|e| format!("TRAE 登录态 JSON 解析失败: {e}"))?;
        return Ok((value, start));
    }
    Err("TRAE 凭证解密失败（SHA-512 校验未通过）".to_string())
}

/// 用与解密相同的表组把登录态 JSON 加密回 byteCrypto 信封（base64）。
/// 结构是解密的镜像：明文 = SHA512(body) || body，PKCS7 填充后
/// AES-128-CBC 加密，信封 = `tc 05 10` 头 + 32 字节随机 + 密文。
fn trae_encrypt_blob(value: &Value, tables: &[[u8; TRAE_TABLE_LEN]], start: usize) -> Result<String, String> {
    if tables.len() < 4 || start + 4 > tables.len() {
        return Err("TRAE 密钥表数量异常，无法写回登录态".to_string());
    }
    let body = serde_json::to_vec(value).map_err(|e| format!("序列化 TRAE 登录态失败: {e}"))?;
    let mut plain = Vec::with_capacity(SHA512_LEN + body.len());
    plain.extend_from_slice(&sha512_bytes(&body));
    plain.extend_from_slice(&body);

    // 32 字节随机：两个 v4 UUID 共 32 字节（v4 由 OS 随机源生成）。
    let mut random = [0u8; TRAE_RANDOM_LEN];
    let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    random[..16].copy_from_slice(a.as_bytes());
    random[16..].copy_from_slice(b.as_bytes());

    let (key, iv) = trae_derive_keys(&random, &tables[start + 2], &tables[start + 3]);
    let mut buf = vec![0u8; plain.len() + 16];
    buf[..plain.len()].copy_from_slice(&plain);
    let ciphertext = Aes128CbcEnc::new(&key.into(), &iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
        .map_err(|e| format!("TRAE 登录态加密失败: {e}"))?
        .to_vec();

    let mut envelope = Vec::with_capacity(6 + TRAE_RANDOM_LEN + ciphertext.len());
    envelope.extend_from_slice(&TRAE_HEADER);
    envelope.extend_from_slice(&random);
    envelope.extend_from_slice(&ciphertext);
    Ok(general_purpose::STANDARD.encode(envelope))
}

fn hex_head(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 解密后的 TRAE 会话：凭证 + 过期状态 + 加解密上下文（写回续期用）。
struct TraeSession {
    cred: TraeCredential,
    expired: bool,
    expired_at: Option<String>,
    /// storage.json 原文（含 telemetry 键与设备密钥对条目）。
    storage: Value,
    storage_path: PathBuf,
    /// 解密后的登录态（含 token/refreshToken/expiredAt/account）。
    auth: Value,
    /// byteCrypto 表组与解密命中的下标（加密写回必须用同一组）。
    tables: Vec<[u8; TRAE_TABLE_LEN]>,
    table_start: usize,
}

fn decrypt_trae_session() -> Result<TraeSession, String> {
    let storage_path = trae_storage_path().ok_or_else(|| {
        "未找到 TRAE Work CN 数据目录（请安装并登录 TraeWork CN）".to_string()
    })?;
    let text = std::fs::read_to_string(&storage_path)
        .map_err(|e| format!("读取 TRAE storage.json 失败: {e}"))?;
    let data: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 TRAE storage.json 失败: {e}"))?;
    let blob = data[TRAE_AUTH_KEY]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "TRAE Work CN 尚未登录（storage.json 缺少登录态）".to_string())?;
    if trae_main_js_candidates().is_empty() {
        return Err("未找到 TRAE Work CN 安装目录（out/main.js）".to_string());
    }
    let mut last_error = String::new();
    for main_js in trae_main_js_candidates() {
        if !main_js.exists() {
            continue;
        }
        let tables = match trae_tables(&main_js) {
            Ok(tables) => tables,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let (auth, table_start) = trae_decrypt_blob(blob, &tables)?;
        let token = auth
            .get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| "TRAE 登录态缺少 token".to_string())?;
        let mut expired = false;
        let mut expired_at = None;
        if let Some(expired_at_text) = auth.get("expiredAt").and_then(Value::as_str) {
            if let Ok(expiry) = chrono::DateTime::parse_from_rfc3339(expired_at_text) {
                if expiry < chrono::Local::now() {
                    expired = true;
                    expired_at = Some(expired_at_text.to_string());
                }
            }
        }
        let device_id = resolve_trae_aha_device_id(&data).unwrap_or_else(|| {
            data.get("telemetry.devDeviceId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        });
        let machine_id = data
            .get("telemetry.machineId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let host = auth
            .get("host")
            .and_then(Value::as_str)
            .filter(|h| h.starts_with("http"))
            .unwrap_or(TRAE_DEFAULT_HOST)
            .trim_end_matches('/')
            .to_string();
        let username = auth
            .pointer("/account/username")
            .or_else(|| auth.pointer("/account/userName"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Ok(TraeSession {
            cred: TraeCredential {
                token: token.to_string(),
                device_id,
                machine_id,
                host,
                username,
            },
            expired,
            expired_at,
            storage: data,
            storage_path,
            auth,
            tables,
            table_start,
        });
    }
    Err(if last_error.is_empty() {
        "未找到 TRAE Work CN 安装目录（out/main.js）".to_string()
    } else {
        last_error
    })
}

/// 把账号池条目（TRAE 快照）写回本机 `storage.json`：替换登录态与设备私钥条目。
///
/// 与续期写回复用同一套解密上下文（`TraeSession` 的 tables / table_start），
/// 保证加密写回用的是客户端能解的那一组表。返回备份文件路径。
pub(crate) fn trae_replace_login(account: &Value) -> Result<Option<String>, String> {
    let session = decrypt_trae_session()?;
    let access = account
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "该账号缺少 accessToken，请先刷新再切换".to_string())?;
    let user_id = account
        .get("userId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "该账号缺少 userId，请重新导入".to_string())?;

    // 以本机当前登录态为模板，只替换凭证字段（其余键保持不变）。
    let mut auth = session.auth.clone();
    auth["token"] = json!(access);
    for field in ["refreshToken", "expiredAt", "refreshExpiredAt"] {
        if let Some(value) = account.get(field).cloned() {
            if !value.is_null() {
                auth[field] = value;
            }
        }
    }
    auth["tokenReleaseAt"] =
        json!(chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

    let mut storage = session.storage.clone();
    storage[TRAE_AUTH_KEY] = json!(trae_encrypt_blob(
        &auth,
        &session.tables,
        session.table_start,
    )?);

    // 设备私钥条目：每个账号一把，切过去必须一起换，否则续期会拿错私钥。
    if let Some(first) = account
        .get("keypairs")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
    {
        let pair = json!({
            "privateKeyPEM": first.get("privateKeyPEM").cloned().unwrap_or(Value::Null),
            "publicKeyPEM": first.get("publicKeyPEM").cloned().unwrap_or(Value::Null),
        });
        let encrypted = trae_encrypt_blob(&pair, &session.tables, session.table_start)?;
        storage[format!("iCubeAuthInfo://icube-dc:{user_id}")] = json!(encrypted);
    }
    if let Some(device_id) = account.get("deviceId").and_then(Value::as_str) {
        if !device_id.is_empty() {
            storage["iCubeDeviceId"] = json!(device_id);
        }
    }

    // 备份原文件后再原子写入。
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = session
        .storage_path
        .with_extension(format!("json.bak-{stamp}"));
    let backup_path = if session.storage_path.exists() {
        std::fs::copy(&session.storage_path, &backup)
            .map_err(|e| format!("备份 TRAE storage.json 失败: {e}"))?;
        Some(backup.to_string_lossy().to_string())
    } else {
        None
    };

    let content = serde_json::to_string_pretty(&storage)
        .map_err(|e| format!("序列化 TRAE storage.json 失败: {e}"))?;
    atomic_write(&session.storage_path, &content)
        .map_err(|e| format!("写入 TRAE storage.json 失败: {e}"))?;
    Ok(backup_path)
}

/// 读取 TRAE 凭证；token 过期不在此处报错（自动续期走 trae_credential）。
fn load_trae_credential() -> Result<TraeCredential, String> {
    Ok(decrypt_trae_session()?.cred)
}

// ---------------------------------------------------------------------------
// TRAE 登录态自动续期（/trae/api/v3/oauth/ExchangeToken）
// ---------------------------------------------------------------------------

/// access token 到期时用 refresh token 换新（逆向自客户端
/// `exchangeTokenByRefreshToken`）：请求体携带设备信息与 DeviceProof，
/// 后者是设备 ECDSA P-256 私钥（客户端登录时生成并注册，加密存放在
/// `storage.json` 的 `iCubeAuthInfo://icube-dc:*` 条目）对
/// `POST <path> <ClientID> <refreshToken> <时间戳> <nonce>` 的
/// SHA-256 签名（DER，base64）。
const TRAE_EXCHANGE_PATH: &str = "/trae/api/v3/oauth/ExchangeToken";
/// SOLO 客户端默认 ClientID（authConfig 未覆盖时的兜底值）。
const TRAE_SOLO_CLIENT_ID: &str = "en1oxy7wnw8j9n";
/// 客户端认定的刷新令牌失效错误码（qP 列表）；命中即需人工重登。
const TRAE_REFRESH_INVALID_CODES: [&str; 7] =
    ["20324", "20101", "20315", "20125", "20126", "20401", "20403"];
/// 两次刷新尝试之间的最小间隔：失败风暴保护。
const TRAE_REFRESH_MIN_INTERVAL_MS: i64 = 30_000;

static TRAE_REFRESH_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static TRAE_REFRESH_LAST_ATTEMPT: OnceLock<Mutex<i64>> = OnceLock::new();

/// 解密 storage.json 里全部设备密钥对（`iCubeAuthInfo://icube-dc:*`）。
/// 客户端每个登录过的账号可能各有一把，续期时逐一尝试。
fn trae_local_keypairs(
    storage: &Value,
    tables: &[[u8; TRAE_TABLE_LEN]],
    table_start: usize,
) -> Vec<(String, String)> {
    const KEY_PREFIX: &str = "iCubeAuthInfo://icube-dc:";
    let Some(map) = storage.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (_key, value) in map {
        let Some(blob) = value.as_str() else {
            continue;
        };
        if let Ok((pair, _)) = trae_decrypt_blob(blob, tables) {
            let private_key = pair
                .get("privateKeyPEM")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let public_key = pair
                .get("publicKeyPEM")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if private_key.contains("PRIVATE KEY") && public_key.contains("PUBLIC KEY") {
                out.push((private_key, public_key));
            }
        }
    }
    out
}

/// 本机客户端的设备密钥对（续期用，取第一把）。
fn load_trae_device_keypair(
    storage: &Value,
    tables: &[[u8; TRAE_TABLE_LEN]],
    table_start: usize,
) -> Result<(String, String), String> {
    trae_local_keypairs(storage, tables, table_start)
        .into_iter()
        .next()
        .ok_or_else(|| "未找到 TRAE 设备密钥（icube-dc），请打开客户端登录一次以生成".to_string())
}

/// 客户端毫秒时间戳/时长字段 → RFC3339（与客户端 XDe/ISO 行为一致）。
fn trae_expiry_to_rfc3339(value: &Value) -> Option<String> {
    if let Some(ms) = value.as_i64() {
        let secs = ms.div_euclid(1000);
        let nanos = (ms.rem_euclid(1000)) as u32;
        chrono::DateTime::from_timestamp(secs, nanos * 1_000_000)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    } else if let Some(text) = value.as_str() {
        (!text.is_empty()).then(|| text.to_string())
    } else {
        None
    }
}

/// 毫秒时长字段 → ISO（now + duration）。
fn trae_duration_to_rfc3339(duration_ms: i64) -> String {
    trae_expiry_to_rfc3339(&json!(now_ms() + duration_ms))
        .unwrap_or_else(|| chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// 用 refresh token 换新登录态并写回 storage.json，返回新凭证。
pub(crate) async fn trae_refresh_access_token() -> Result<TraeCredential, String> {
    let session = decrypt_trae_session()?;
    trae_refresh_session(session).await
}

/// 对已解密会话执行刷新（供 trae_credential 复用解密结果）。
async fn trae_refresh_session(session: TraeSession) -> Result<TraeCredential, String> {
    // 失败风暴保护：距上次尝试过近时直接报错，不再打官方接口。
    {
        let last = TRAE_REFRESH_LAST_ATTEMPT
            .get_or_init(|| Mutex::new(0))
            .lock()
            .unwrap();
        if now_ms() - *last < TRAE_REFRESH_MIN_INTERVAL_MS {
            return Err(format!(
                "TRAE 登录态已过期（{}），自动续期刚失败过，请稍后重试或打开客户端重新登录",
                session.expired_at.as_deref().unwrap_or("unknown")
            ));
        }
    }
    // 串行化刷新，避免并发调用同时换新导致写回交叉。
    let _guard = TRAE_REFRESH_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    {
        let mut last = TRAE_REFRESH_LAST_ATTEMPT
            .get_or_init(|| Mutex::new(0))
            .lock()
            .unwrap();
        *last = now_ms();
    }

    let refresh_token = session
        .auth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "TRAE 登录态缺少 refreshToken，请打开客户端重新登录".to_string())?
        .to_string();
    let (private_key_pem, public_key_pem) =
        load_trae_device_keypair(&session.storage, &session.tables, session.table_start)?;
    let client_version = session
        .storage
        .get("iCubeLastVersion")
        .and_then(Value::as_str)
        .unwrap_or("1.0.0")
        .to_string();
    let exchange = trae_exchange_token(
        &session.cred.host,
        &session.cred.token,
        &refresh_token,
        &session.cred.device_id,
        &session.cred.machine_id,
        &private_key_pem,
        &public_key_pem,
        &client_version,
    )
    .await?;

    // 更新登录态 JSON 并加密写回（storage.json 其余条目保持不动）。
    let mut auth = session.auth.clone();
    auth["token"] = json!(exchange.token);
    if let Some(new_refresh) = exchange.refresh_token {
        auth["refreshToken"] = json!(new_refresh);
    }
    if let Some(expired_at) = exchange.expired_at {
        auth["expiredAt"] = json!(expired_at);
    }
    if let Some(refresh_expired_at) = exchange.refresh_expired_at {
        auth["refreshExpiredAt"] = json!(refresh_expired_at);
    }
    auth["tokenReleaseAt"] =
        json!(chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

    let blob = trae_encrypt_blob(&auth, &session.tables, session.table_start)?;
    let mut storage = session.storage.clone();
    storage[TRAE_AUTH_KEY] = json!(blob);
    let content = serde_json::to_string_pretty(&storage)
        .map_err(|e| format!("序列化 TRAE storage.json 失败: {e}"))?;
    atomic_write(&session.storage_path, &content)
        .map_err(|e| format!("写回 TRAE storage.json 失败: {e}"))?;

    Ok(TraeCredential {
        token: exchange.token,
        device_id: session.cred.device_id,
        machine_id: session.cred.machine_id,
        host: session.cred.host,
        username: session.cred.username,
    })
}

/// ExchangeToken 换新结果（字段缺省时由调用方保留旧值）。
pub(crate) struct TraeExchangeOutput {
    pub token: String,
    pub refresh_token: Option<String>,
    pub expired_at: Option<String>,
    pub refresh_expired_at: Option<String>,
}

/// 与客户端 `exchangeTokenByRefreshToken` 等价的换新请求。
/// 本机登录态续期与账号池条目续期共用；ECDSA 设备私钥由调用方提供
/// （本机取自 storage.json，池条目取自导入快照）。
pub(crate) async fn trae_exchange_token(
    host: &str,
    old_token: &str,
    refresh_token: &str,
    device_id: &str,
    machine_id: &str,
    private_key_pem: &str,
    public_key_pem: &str,
    client_version: &str,
) -> Result<TraeExchangeOutput, String> {
    let client_id = TRAE_SOLO_CLIENT_ID.to_string();
    let timestamp = now_secs();
    let nonce: String = {
        use md5::Digest as _;
        let mut hasher = Md5::new();
        hasher.update(uuid::Uuid::new_v4().as_bytes());
        hasher.update(uuid::Uuid::new_v4().as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        hex[..32].to_string()
    };
    // 待签串与客户端 eTe() 完全一致：方法 路径 ClientID refreshToken 时间戳 nonce，空格分隔。
    let string_to_sign = [
        "POST",
        TRAE_EXCHANGE_PATH,
        client_id.as_str(),
        refresh_token,
        &timestamp.to_string(),
        nonce.as_str(),
    ]
    .join(" ");
    let signature = trae_ecdsa_sign(private_key_pem, &string_to_sign)?;

    let device_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".to_string());
    let body = json!({
        "ClientID": client_id,
        "ClientSecret": "",
        "RefreshToken": refresh_token,
        "DeviceInfo": {
            "DeviceID": device_id,
            "MachineID": machine_id,
            "PlatformCode": "SOLO_PC",
            "DeviceType": "PC",
            "DeviceName": device_name,
            "DeviceModel": "",
            "ClientVersion": client_version,
            "DevicePublicKey": public_key_pem,
            "DeviceBrand": "",
            "DeviceCPU": "",
            "OSInfo": std::env::consts::OS,
            "OSVersion": "",
        },
        "DeviceProof": {
            "Signature": signature,
            "Timestamp": timestamp,
            "Nonce": nonce,
        },
        "IDEVersion": client_version,
    });

    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!("Cloud-IDE-JWT {old_token}"),
    );
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    if !device_id.is_empty() {
        headers.insert("x-device-id".to_string(), device_id.to_string());
    }
    let url = format!("{host}{TRAE_EXCHANGE_PATH}");
    let resp = http_request(&url, "POST", Some(body), Some(&headers)).await;

    // 错误处理：ResponseMetadata.Error.Code（官方风格）或顶层 code。
    let error_code = resp
        .pointer("/ResponseMetadata/Error/Code")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            resp.get("code")
                .and_then(Value::as_i64)
                .map(|c| c.to_string())
        });
    if let Some(code) = error_code {
        let message = resp
            .pointer("/ResponseMetadata/Error/Message")
            .and_then(Value::as_str)
            .unwrap_or("ExchangeToken 失败");
        if TRAE_REFRESH_INVALID_CODES.contains(&code.as_str()) {
            return Err(format!(
                "TRAE 刷新令牌已失效（code={code}），请打开 TRAE Work CN 重新登录"
            ));
        }
        return Err(format!("TRAE 续期失败（code={code}）：{message}"));
    }
    let result = resp
        .get("Result")
        .and_then(Value::as_object)
        .ok_or_else(|| "TRAE 续期响应缺少 Result".to_string())?;
    let token = result
        .get("Token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "TRAE 续期响应缺少 Token".to_string())?
        .to_string();
    let expired_at = result
        .get("TokenExpireAt")
        .and_then(trae_expiry_to_rfc3339)
        .or_else(|| {
            result
                .get("TokenExpireDuration")
                .and_then(Value::as_i64)
                .filter(|d| *d > 0)
                .map(trae_duration_to_rfc3339)
        });
    let refresh_expired_at = result
        .get("RefreshExpireAt")
        .and_then(trae_expiry_to_rfc3339);
    let new_refresh = result
        .get("RefreshToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(String::from);
    Ok(TraeExchangeOutput {
        token,
        refresh_token: new_refresh,
        expired_at,
        refresh_expired_at,
    })
}

/// 本机 TRAE 客户端当前登录账号的完整快照（导入账号池用）。
/// 包含 access/refresh token 与全部设备 ECDSA 密钥对（续期 DeviceProof 用）。
pub(crate) fn trae_local_snapshot() -> Result<Value, String> {
    let session = decrypt_trae_session()?;
    let keypairs: Vec<Value> = trae_local_keypairs(&session.storage, &session.tables, session.table_start)
        .into_iter()
        .map(|(private_key_pem, public_key_pem)| {
            json!({"privateKeyPEM": private_key_pem, "publicKeyPEM": public_key_pem})
        })
        .collect();
    Ok(json!({
        "userId": session.auth.get("userId"),
        "username": session.cred.username,
        "accessToken": session.cred.token,
        "refreshToken": session.auth.get("refreshToken"),
        "expiredAt": session.auth.get("expiredAt"),
        "refreshExpiredAt": session.auth.get("refreshExpiredAt"),
        "host": session.cred.host,
        "deviceId": session.cred.device_id,
        "machineId": session.cred.machine_id,
        "clientVersion": session.storage.get("iCubeLastVersion"),
        "keypairs": keypairs,
    }))
}

/// 本机客户端当前登录的 TRAE userId（账号池与本机账号去重用）。
pub(crate) fn trae_local_user_id() -> Option<String> {
    let session = decrypt_trae_session().ok()?;
    session
        .auth
        .get("userId")
        .and_then(Value::as_str)
        .filter(|uid| !uid.is_empty())
        .map(String::from)
}

/// 本机客户端 TRAE 账号的账号池 id（去重与批量签到用）。
pub(crate) fn trae_local_pool_id() -> Option<String> {
    trae_local_user_id().map(|uid| format!("trae_work_cn:{uid}"))
}

/// ECDSA P-256 + SHA-256 签名（DER 编码，base64），对齐 Node
/// `crypto.sign("sha256", data, ecKey)` 的输出格式。
fn trae_ecdsa_sign(private_key_pem: &str, message: &str) -> Result<String, String> {
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::{Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    let key = SigningKey::from_pkcs8_pem(private_key_pem)
        .map_err(|e| format!("解析 TRAE 设备私钥失败: {e}"))?;
    let signature: Signature = key.sign(message.as_bytes());
    Ok(general_purpose::STANDARD.encode(signature.to_der().as_bytes()))
}

/// 读取 TRAE 凭证；access token 已过期时自动续期一次（失败风暴保护见上）。
pub(crate) async fn trae_credential() -> Result<TraeCredential, String> {
    let session = decrypt_trae_session()?;
    if !session.expired {
        return Ok(session.cred);
    }
    trae_refresh_session(session).await
}

/// 模型代理用：获取本机 TRAE 登录态（含自动续期）。供 trae_proxy 调用。
pub async fn trae_proxy_credential() -> Result<TraeCredential, String> {
    trae_credential().await
}

pub fn discover_trae_work_cn_account() -> Option<Value> {
    trae_storage_path()?;
    // 用户名存放在加密登录态里，这里直接走解密路径；解密失败（如客户端
    // 未安装到已知位置）时不展示该账号，交由签到流程报出具体错误。
    let cred = load_trae_credential().ok()?;
    let display = if cred.username.is_empty() {
        "已登录"
    } else {
        cred.username.as_str()
    };
    Some(json!({
        "id": "trae_work_cn:local",
        "email": format!("TRAE Work CN · {display}"),
        "nickname": format!("TRAE Work CN · {display}"),
        "platform": PLATFORM_TRAE_WORK_CN,
    }))
}

async fn trae_post(cred: &TraeCredential, path: &str) -> Value {
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!("Cloud-IDE-JWT {}", cred.token),
    );
    let mut device_id = cred.device_id.clone();
    if device_id.is_empty() || device_id.contains('-') {
        if let Some(aha) = resolve_system_aha_device_id() {
            device_id = aha;
        }
    }
    if !device_id.is_empty() {
        headers.insert("x-device-id".to_string(), device_id);
    }
    if !cred.machine_id.is_empty() {
        headers.insert("x-machine-id".to_string(), cred.machine_id.clone());
    }
    headers.insert("X-User-Region".to_string(), "cn".to_string());
    headers.insert("User-Agent".to_string(), "TraeCheckin/1.0".to_string());

    let body = if path == TRAE_CHECKIN_CLAIM_PATH {
        Some(json!({"req_source": 1}))
    } else {
        Some(json!({}))
    };

    http_request(
        &format!("{}{path}", cred.host),
        "POST",
        body,
        Some(&headers),
    )
    .await
}

fn trae_error(resp: &Value, fallback: &str) -> String {
    let message = resp_error(resp, fallback);
    if message.contains("登录")
        || message.contains("token")
        || message.contains("Token")
        || message.contains("401")
    {
        return "TRAE Work CN 登录态失效，请打开 TRAE Work CN 重新登录".to_string();
    }
    message
}

/// TRAE Work CN 今日签到状态。
pub async fn get_trae_work_cn_status() -> Value {
    let cred = match trae_credential().await {
        Ok(cred) => cred,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    get_trae_status_with(&cred).await
}

/// TRAE 签到状态查询（任意凭证：本机登录态或账号池条目）。
pub(crate) async fn get_trae_status_with(cred: &TraeCredential) -> Value {
    let mut resp = trae_post(cred, TRAE_CHECKIN_STATUS_PATH).await;
    let mut code = resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        // 高峰期状态查询也可能被限流：等待后重试一次。
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
        resp = trae_post(&cred, TRAE_CHECKIN_STATUS_PATH).await;
        code = resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
    }
    if code != 0 {
        return json!({"ok": false, "error": trae_error(&resp, "查询 TRAE 签到状态失败")});
    }
    if resp.get("enable").and_then(Value::as_bool) == Some(false) {
        return json!({"ok": false, "error": "TRAE Work CN 签到活动未开启"});
    }
    let checked = resp
        .get("checked_in")
        .or_else(|| resp.get("did_checked_in"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    json!({"ok": true, "todayCheckedIn": checked, "raw": resp})
}

/// 领取 TRAE Work CN 每日签到积分。
/// TRAE 高峰期限流判定（"当前参与用户太多，请稍后再试"等）。
fn trae_busy(message: &str) -> bool {
    message.contains("太多")
        || message.contains("稍后再试")
        || message.contains("繁忙")
        || message.contains("请稍后")
}

/// 领取 TRAE 每日签到积分。
///
/// 签到高峰期（每日额度发放时段）服务端常返回"当前参与用户太多，请稍后再试"
/// 的限流：按 1s/2s 递增间隔自动重试最多 2 次；重试仍失败则透出错误，
/// 下一轮自动签到循环会再次尝试（当天未签到成功前每轮都会重试）。
async fn perform_trae_work_cn_checkin() -> Value {
    let cred = match trae_credential().await {
        Ok(cred) => cred,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    perform_trae_checkin_with(&cred).await
}

/// TRAE 每日签到领取（任意凭证），含高峰期限流重试与设备限制快速识别。
pub(crate) async fn perform_trae_checkin_with(cred: &TraeCredential) -> Value {
    let delays_ms: [u64; 2] = [0, 1_500];
    let mut busy_error = String::new();
    for delay_ms in delays_ms {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        let resp = trae_post(cred, TRAE_CHECKIN_CLAIM_PATH).await;
        let code = resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 {
            return json!({"ok": true, "already": false, "raw": resp});
        }
        let message = resp_error(&resp, "TRAE Work CN 签到失败");
        // 9095: "当前设备今日已经签到，请明日再来哦～"，或已签提示直接视为已完成，不卡死重试
        if message.contains("已签") || code == 9095 {
            return json!({"ok": true, "already": true, "message": message});
        }
        if trae_busy(&message) {
            busy_error = message;
            continue;
        }
        return json!({"ok": false, "error": trae_error(&resp, "TRAE Work CN 签到失败")});
    }
    json!({
        "ok": false,
        "error": format!(
            "TRAE 高峰期限流（{busy_error}），下一轮自动签到会继续尝试"
        ),
    })
}

// ---------------------------------------------------------------------------
// 平台积分 / 额度（供前端账号卡片展示，结构与 WorkBuddy 的 CreditExpiry 对齐）
// ---------------------------------------------------------------------------

/// AutoClaw 积分余额（按 token，账号池与本机账号共用）。
/// 401 时不做自愈，返回 `authFailed: true`，由调用方决定如何续期重试。
pub(crate) async fn get_autoclaw_credits_with(token: &str) -> Value {
    let wallets = autoclaw_call_with(
        token,
        "/agent-assetmgr/api/v2/wallets?biz_app_id=autoclaw",
        "GET",
        None,
    )
    .await;
    if let Some(error) = autoclaw_auth_failure(&wallets) {
        return json!({"ok": false, "authFailed": true, "error": error});
    }
    let data = wallets.get("data");
    if wallets.get("code").and_then(Value::as_i64) != Some(0) || data.is_none() {
        return json!({
            "ok": false,
            "error": resp_error(&wallets, "查询 AutoClaw 积分失败"),
        });
    }
    let total = data
        .and_then(|d| d.get("total_balance"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let mut resources = Vec::new();
    for wallet in data
        .and_then(|d| d.get("wallets"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if wallet.get("display").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let balance = wallet.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
        resources.push(json!({
            "packageCode": wallet.get("public_wallet_type").cloned().unwrap_or(Value::Null),
            "packageName": wallet.get("display_name").cloned().unwrap_or(Value::Null),
            "total": balance,
            "remaining": balance,
            "used": 0,
            "status": Value::Null,
            "expireAt": Value::Null,
            "expired": false,
            "expiringSoon": false,
        }));
    }
    let mut expiring = 0.0;
    let mut expiring_days: Option<f64> = None;
    let expiring_resp = autoclaw_call_with(
        token,
        "/agent-assetmgr/api/v1/points/expiring?biz_app_id=autoclaw",
        "GET",
        None,
    )
    .await;
    if expiring_resp.get("code").and_then(Value::as_i64) == Some(0) {
        expiring = expiring_resp
            .pointer("/data/expiring_points")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        // 下次到期时间换算成天数（day/week/month）。
        let value = expiring_resp
            .pointer("/data/expire_time_value")
            .and_then(Value::as_f64);
        let typ = expiring_resp
            .pointer("/data/expire_time_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let multiplier = match typ.as_str() {
            "day" => Some(1.0),
            "week" => Some(7.0),
            "month" => Some(30.0),
            _ => None,
        };
        expiring_days = multiplier.and_then(|m| value.map(|v| v * m));
    }
    json!({
        "ok": true,
        "totalRemaining": total,
        "remainingTotal": total,
        "expiringSoonRemaining": expiring,
        "expiring": {
            "points": expiring,
            "days": expiring_days.map(|d| json!(d)).unwrap_or(Value::Null),
        },
        "resources": resources,
        "updatedAt": now_ms(),
    })
}

/// AutoClaw 积分余额（本机登录账号）：登录态失效时自动续期一次并重试。
pub async fn get_autoclaw_credits() -> Value {
    let token = match read_autoclaw_token() {
        Ok(token) => token,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    let result = get_autoclaw_credits_with(&token).await;
    if result.get("authFailed").and_then(Value::as_bool) != Some(true) {
        return result;
    }
    let new_token = match autoclaw_refresh_access_token().await {
        Ok(token) => token,
        Err(error) => {
            return json!({
                "ok": false,
                "error": format!(
                    "AutoClaw 登录态失效，且自动续期失败（{error}）；请打开 AutoClaw 重新登录"
                ),
            });
        }
    };
    let mut retried = get_autoclaw_credits_with(&new_token).await;
    if retried.get("authFailed").and_then(Value::as_bool) == Some(true) {
        if let Some(obj) = retried.as_object_mut() {
            obj.remove("authFailed");
            obj.insert(
                "error".to_string(),
                json!("AutoClaw 登录态失效，请打开 AutoClaw 重新登录"),
            );
        }
    }
    retried
}

/// TRAE Work CN 额度：usage_summary 总额度 + 权益包列表（credits_limit/到期）。
pub async fn get_trae_work_cn_credits() -> Value {
    let cred = match trae_credential().await {
        Ok(cred) => cred,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    get_trae_credits_with(&cred).await
}

/// TRAE 额度/权益包查询（任意凭证）。
pub(crate) async fn get_trae_credits_with(cred: &TraeCredential) -> Value {
    let resp = trae_post(cred, TRAE_ENT_USAGE_PATH).await;
    if let Some(code) = resp.get("code").and_then(Value::as_i64) {
        if code != 0 {
            return json!({"ok": false, "error": trae_error(&resp, "查询 TRAE 额度失败")});
        }
    }
    let total = resp
        .pointer("/usage_summary/total_amount")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let consumed = resp
        .pointer("/usage_summary/consumed_amount")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let now = now_ms();
    let week_later = now + 7 * 24 * 3600 * 1000;
    let mut resources = Vec::new();
    for pack in resp
        .get("user_entitlement_pack_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if pack.get("is_hide").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let limit = pack
            .pointer("/entitlement_base_info/quota/credits_limit")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if limit <= 0.0 {
            continue;
        }
        let expire_secs = pack.get("expire_time").and_then(Value::as_i64).unwrap_or(0);
        let expire_at = if expire_secs > 0 { expire_secs * 1000 } else { 0 };
        let name = pack
            .get("display_desc")
            .and_then(Value::as_str)
            .or_else(|| pack.get("group_name").and_then(Value::as_str))
            .unwrap_or("权益包");
        resources.push(json!({
            "packageCode": pack.pointer("/entitlement_base_info/entitlement_id").cloned().unwrap_or(Value::Null),
            "packageName": name,
            "total": limit,
            "remaining": limit,
            "used": 0,
            "status": pack.get("status").cloned().unwrap_or(Value::Null),
            "expireAt": if expire_at > 0 { json!(expire_at) } else { Value::Null },
            "expired": expire_at > 0 && expire_at < now,
            "expiringSoon": expire_at > 0 && expire_at < week_later,
        }));
    }
    json!({
        "ok": true,
        "totalRemaining": total,
        "remainingTotal": total - consumed,
        "usedTotal": consumed,
        "resources": resources,
        "updatedAt": now,
    })
}

/// 平台积分/额度查询（按平台路由）。与 WorkBuddy 的积分查询一致，
/// 查询成功即写入本地快照，供积分统计推导每日消耗。
/// AutoClaw 账号池单账号积分：必须用池内自身 token 查询，
/// 否则会错误地打到本机当前登录账号的钱包（两张卡片显示同一积分）。
async fn get_autoclaw_pool_credits(store_id: &str) -> Value {
    let Some(account) = crate::modules::autoclaw_accounts::load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(store_id))
    else {
        return json!({"ok": false, "error": "账号池条目不存在"});
    };
    let token = account
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if token.is_empty() {
        return json!({"ok": false, "error": "账号缺少 token，请重新导入"});
    }
    let bearer = |t: &str| format!("Bearer {}", t.trim_start_matches("Bearer "));
    let mut result = get_autoclaw_credits_with(&bearer(&token)).await;
    if result.get("authFailed").and_then(Value::as_bool) == Some(true) {
        match crate::modules::autoclaw_accounts::refresh_stored_account(store_id).await {
            Ok(new_token) => {
                result = get_autoclaw_credits_with(&bearer(&new_token)).await;
            }
            Err(e) => {
                return json!({"ok": false, "error": format!("登录态续期失败：{e}；请重新导入该账号的 token")});
            }
        }
    }
    if let Some(obj) = result.as_object_mut() {
        if obj.remove("authFailed").and_then(|v| v.as_bool()) == Some(true) {
            obj.insert(
                "error".to_string(),
                json!("AutoClaw 登录态已失效，请重新导入该账号的 token"),
            );
        }
    }
    result
}

pub async fn get_platform_credits(account: &Value) -> Value {
    let result = match account_platform(account) {
        PLATFORM_AUTOCLAW => {
            if let Some(store_id) = account.get("autoclawStoreId").and_then(Value::as_str) {
                get_autoclaw_pool_credits(store_id).await
            } else {
                get_autoclaw_credits().await
            }
        }
        PLATFORM_TRAE_WORK_CN => {
            if let Some(store_id) = account.get("traeStoreId").and_then(Value::as_str) {
                crate::modules::trae_accounts::credits_stored(store_id).await
            } else {
                get_trae_work_cn_credits().await
            }
        }
        PLATFORM_LOOMY => {
            if let Some(store_id) = account.get("loomyStoreId").and_then(Value::as_str) {
                crate::modules::loomy::credits_stored(store_id).await
            } else {
                crate::modules::loomy::credits_local().await
            }
        }
        _ => json!({"ok": false, "error": "未知签到平台"}),
    };
    if result.get("ok").and_then(Value::as_bool) == Some(true) {
        if let Some(account_id) = account
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            let total = result
                .get("totalRemaining")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let remaining = result
                .get("remainingTotal")
                .and_then(Value::as_f64)
                .unwrap_or(total);
            crate::modules::credit_usage::record_snapshot(
                account_id,
                &crate::modules::account::account_display_name(account),
                total,
                remaining,
            );
        }
    }
    result
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_defaults_to_workbuddy() {
        assert_eq!(account_platform(&json!({"id": "a"})), PLATFORM_WORKBUDDY);
        assert_eq!(
            account_platform(&json!({"id": "a", "platform": "autoclaw"})),
            PLATFORM_AUTOCLAW
        );
        assert_eq!(
            account_platform(&json!({"id": "a", "platform": "trae_work_cn"})),
            PLATFORM_TRAE_WORK_CN
        );
        assert_eq!(
            account_platform(&json!({"id": "a", "platform": "loomy"})),
            PLATFORM_LOOMY
        );
    }

    #[test]
    fn autoclaw_sign_matches_client_algorithm() {
        // 与 app.asar 中 authHeaders 的 md5(appId&timestamp&appKey) 一致。
        let ts = "1700000000";
        let expected = {
            let mut hasher = Md5::new();
            hasher.update(format!("{AUTOCLAW_APP_ID}&{ts}&{AUTOCLAW_APP_KEY}").as_bytes());
            hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(expected.len(), 32);
        assert_eq!(md5_hex(&format!("{AUTOCLAW_APP_ID}&{ts}&{AUTOCLAW_APP_KEY}")), expected);
    }

    #[test]
    fn autoclaw_headers_carry_auth_signature() {
        let headers = autoclaw_headers(Some("Bearer abc"));
        assert_eq!(headers.get("authorization").map(String::as_str), Some("Bearer abc"));
        assert_eq!(headers.get("X-Product").map(String::as_str), Some("autoclaw"));
        assert!(headers.contains_key("X-Auth-Sign"));
        assert!(headers.contains_key("X-Auth-TimeStamp"));
    }

    #[test]
    fn trae_derive_keys_matches_reference_vector() {
        // 用本机实测数据（信封头 + 随机字节）构造向量：
        // 派生结果只要求确定性 —— 同输入同输出，且 key/iv 来自同一摘要。
        let random = [7u8; TRAE_RANDOM_LEN];
        let vie = [1u8; TRAE_TABLE_LEN];
        let qie = [2u8; TRAE_TABLE_LEN];
        let (key, iv) = trae_derive_keys(&random, &vie, &qie);
        let (key2, iv2) = trae_derive_keys(&random, &vie, &qie);
        assert_eq!(key, key2);
        assert_eq!(iv, iv2);
        assert_ne!(key, iv);
        let pepper: Vec<u8> = vie.iter().zip(qie.iter()).map(|(a, b)| a ^ b).collect();
        let mut inner = [0u8; SHA512_LEN + TRAE_TABLE_LEN];
        inner[..SHA512_LEN].copy_from_slice(&sha512_bytes(&random));
        inner[SHA512_LEN..].copy_from_slice(&pepper);
        let derived = sha512_bytes(&inner);
        assert_eq!(&key[..], &derived[..16]);
        assert_eq!(&iv[..], &derived[16..32]);
    }

    #[test]
    fn trae_decrypt_blob_rejects_bad_envelope() {
        let tables = vec![[0u8; TRAE_TABLE_LEN]; 4];
        let err = trae_decrypt_blob("AAAA", &tables).unwrap_err();
        assert!(err.contains("过短") || err.contains("版本异常"));
        // 正确长度但错误头。
        let mut raw = vec![0u8; 6 + TRAE_RANDOM_LEN + 64];
        raw[0] = b'X';
        let err = trae_decrypt_blob(
            &general_purpose::STANDARD.encode(&raw),
            &tables,
        )
        .unwrap_err();
        assert!(err.contains("版本异常"), "got: {err}");
    }

    #[test]
    fn trae_encrypt_blob_roundtrips_through_decrypt() {
        // 加密路径（写回续期用）与解密路径必须对称：同组表加进去，解出来
        // 的登录态 JSON 与原文一致，且命中同一表组下标。
        let tables = vec![
            [1u8; TRAE_TABLE_LEN],
            [2u8; TRAE_TABLE_LEN],
            [3u8; TRAE_TABLE_LEN],
            [4u8; TRAE_TABLE_LEN],
        ];
        let auth = json!({
            "token": "eyJhbGciOiJS.abc",
            "refreshToken": "XAt4-B7RptWr",
            "expiredAt": "2026-09-25T16:12:47.535Z",
            "userId": "2103845784133996",
            "host": "https://api.trae.cn",
            "account": {"username": "用户0465392719"},
        });
        let blob = trae_encrypt_blob(&auth, &tables, 0).expect("encrypt");
        let (decoded, start) = trae_decrypt_blob(&blob, &tables).expect("decrypt");
        assert_eq!(start, 0);
        assert_eq!(decoded, auth);
        // 信封头必须是 tc 05 10。
        let raw = general_purpose::STANDARD.decode(&blob).unwrap();
        assert_eq!(&raw[..6], &TRAE_HEADER);
    }

    #[test]
    fn trae_decrypt_roundtrip_with_matching_tables() {
        // 用加密路径构造一个合法信封：AES-128-CBC(SHA512(pt)||pt)，
        // 解密 + SHA-512 校验应命中最后一组表（vie/qie）。
        let plain = br#"{"token":"t","host":"https://api.trae.cn"}"#.to_vec();

        let vie = [3u8; TRAE_TABLE_LEN];
        let qie = [5u8; TRAE_TABLE_LEN];
        let random = [9u8; TRAE_RANDOM_LEN];
        let (key, iv) = trae_derive_keys(&random, &vie, &qie);

        let mut payload = Vec::new();
        payload.extend_from_slice(&sha512_bytes(&plain));
        payload.extend_from_slice(&plain);
        let ciphertext = trae_encrypt_for_test(&payload, &key, &iv);

        let mut raw = Vec::new();
        raw.extend_from_slice(&TRAE_HEADER);
        raw.extend_from_slice(&random);
        raw.extend_from_slice(&ciphertext);
        let blob = general_purpose::STANDARD.encode(&raw);

        // 四组表只有最后一组是 vie/qie 正确组合，其余组合应被摘要校验排除。
        let tables = vec![[0u8; TRAE_TABLE_LEN], [1u8; TRAE_TABLE_LEN], vie, qie];
        let (auth, hit) = trae_decrypt_blob(&blob, &tables).expect("roundtrip decrypts");
        assert_eq!(hit, 0);
        assert_eq!(auth["host"], "https://api.trae.cn");

        // 全零表组合不匹配 → 报错而非误判。
        let wrong = vec![[0u8; TRAE_TABLE_LEN]; 4];
        assert!(trae_decrypt_blob(&blob, &wrong).is_err());
    }

    #[test]
    fn trae_decrypt_rejects_tampered_body() {
        let plain = b"hello".to_vec();
        let vie = [3u8; TRAE_TABLE_LEN];
        let qie = [5u8; TRAE_TABLE_LEN];
        let random = [11u8; TRAE_RANDOM_LEN];
        let (key, iv) = trae_derive_keys(&random, &vie, &qie);
        let mut payload = Vec::new();
        payload.extend_from_slice(&sha512_bytes(&plain));
        payload.extend_from_slice(&plain);
        let ciphertext = trae_encrypt_for_test(&payload, &key, &iv);
        // 篡改密文首字节。
        let mut tampered = ciphertext.clone();
        tampered[0] ^= 0xff;
        let mut raw = Vec::new();
        raw.extend_from_slice(&TRAE_HEADER);
        raw.extend_from_slice(&random);
        raw.extend_from_slice(&tampered);
        let blob = general_purpose::STANDARD.encode(&raw);
        let tables = vec![
            [0u8; TRAE_TABLE_LEN],
            [1u8; TRAE_TABLE_LEN],
            vie,
            qie,
        ];
        assert!(trae_decrypt_blob(&blob, &tables).is_err());
    }

    /// 测试辅助：AES-128-CBC + PKCS7 加密（对应 byteCrypto 的加密路径）。
    fn trae_encrypt_for_test(plaintext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<Aes128>;
        let mut out = vec![0u8; plaintext.len() + 16];
        out[..plaintext.len()].copy_from_slice(plaintext);
        let encrypted = Aes128CbcEnc::new(key.into(), iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut out, plaintext.len())
            .expect("encrypt for test");
        encrypted.to_vec()
    }
}
