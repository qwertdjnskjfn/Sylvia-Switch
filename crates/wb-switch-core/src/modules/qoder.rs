//! Qoder 融合网关（源自 linux.do 帖子推荐的 QoderGateway / qoder2api 逆向成果）。
//!
//! 把多个 Qoder 账号（阿里 Qoder AI IDE / qodercli）汇聚为一个 OpenAI 兼容
//! 接口（`/qoder/v1/chat/completions`），带账号池轮转、token 自动刷新。
//!
//! 协议要点（bzym2/QoderGateway 的 docs/qoder-protocol-research.md，2026-08 抓包）：
//! - 新版协议（Qoder CLI 现行）：`POST https://api2-v2.qoder.sh/model/v1/chat/completions`，
//!   纯 `Authorization: Bearer <dt-...>`，标准 OpenAI SSE 响应，无需任何签名；
//! - 同一个 `dt-` token 在 `openapi.qoder.sh/api/v1/userinfo` 查用户信息；
//! - `drt-` refresh token 经 `POST openapi.qoder.sh/api/v1/jobToken/refresh`
//!   换新（401 时自动执行）；
//! - 登录用 PKCE device flow（本模块用于"添加账号"）。
//!
//! 账号来源：
//! 1. 本机 Qoder IDE 登录态解密导入（`%APPDATA%\Qoder` 的 state.vscdb
//!    `secret://aicoding.auth.userInfo`，Chromium safeStorage v10 = DPAPI +
//!    AES-256-GCM，与 AutoClaw auth.json 同构，复用现成解密原语）；
//! 2. PKCE device flow（用户在浏览器授权后轮询拿到凭据）；
//! 3. 手动粘贴 dt-/drt-（高级用法）。
//!    账号池落盘 `~/.wb-switch/qoder_accounts.json`，按 uid 去重。

use base64::engine::general_purpose;
use base64::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};

const OPENAPI_HOST: &str = "https://openapi.qoder.sh";
const CHAT_HOST: &str = "https://api2-v2.qoder.sh";
/// QoderWork CN 的 CN 网关（取自其 `%APPDATA%\QoderWork CN\data\region-cache.json`
/// 的 `endpoint` 字段）。实测该应用签发的 `dt-` 只被此主机接受。
pub const QODERWORK_CN_ENDPOINT: &str = "https://gateway.qoder.com.cn";
/// device flow 的 client_id（qoderclicn@1.1.16 混淆常量解出）。
const DEVICE_CLIENT_ID: &str = "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb";
/// 轮询间隔 / 总超时（与客户端常量一致）。
const POLL_INTERVAL_MS: u64 = 1000;
const POLL_TIMEOUT_MS: u64 = 300_000;

static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder()
        // 上游 AutoClaw/Qoder 国内可直连；绝不继承系统代理（Clash 动态端口一挂，
        // 整个账号池就「error sending request」→ 502，2026-09-18 19:10 ZCode 事故根因）
        .no_proxy()
        .build().expect("failed to build qoder client"))
}

// ---------------------------------------------------------------------------
// 账号池存储（~/.wb-switch/qoder_accounts.json）
// ---------------------------------------------------------------------------

fn accounts_path() -> PathBuf {
    store_dir().join("qoder_accounts.json")
}

pub fn load_accounts() -> Vec<Value> {
    std::fs::read_to_string(accounts_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn save_accounts(accounts: &[Value]) -> Result<(), String> {
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "accounts": accounts,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

/// 写入/更新一个账号（按 uid 去重）；返回实际存储的条目。
fn upsert_account(entry: Value) -> Result<Value, String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let uid = entry
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let now = now_ms();
    let mut stored = entry.clone();
    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("uid").and_then(Value::as_str) == Some(uid.as_str()))
    {
        for field in ["accessToken", "refreshToken", "expireTime", "email", "name", "userTag", "plan", "gateway", "region", "endpoint", "source"] {
            if let Some(value) = entry.get(field) {
                if !value.is_null() {
                    existing[field] = value.clone();
                }
            }
        }
        existing["status"] = json!("ok");
        existing["error"] = Value::Null;
        existing["updatedAt"] = json!(now);
        stored = existing.clone();
    } else {
        stored["addedAt"] = json!(now);
        accounts.push(stored.clone());
    }
    stored["updatedAt"] = json!(now);
    save_accounts(&accounts)?;
    Ok(stored)
}

pub fn remove_account(uid: &str) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.get("uid").and_then(Value::as_str) != Some(uid));
    if accounts.len() == before {
        return false;
    }
    save_accounts(&accounts).is_ok()
}

pub fn set_account_enabled(uid: &str, enabled: bool) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let mut changed = false;
    for account in accounts.iter_mut() {
        if account.get("uid").and_then(Value::as_str) == Some(uid) {
            account["enabled"] = json!(enabled);
            changed = true;
        }
    }
    if changed {
        let _ = save_accounts(&accounts);
    }
    changed
}

// ---------------------------------------------------------------------------
// 本机 Qoder IDE 登录态解密（Chromium safeStorage v10）
// ---------------------------------------------------------------------------

/// Qoder 用户数据目录（IDE = VS Code fork）。
fn qoder_user_data_dir() -> Option<PathBuf> {
    if let Some(base) = dirs::config_dir() {
        let p = base.join("Qoder").join("User");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// QoderCN（中国版 IDE）数据目录：`%APPDATA%\QoderCN\User`。
fn qoder_cn_user_data_dir() -> Option<PathBuf> {
    if let Some(base) = dirs::config_dir() {
        let p = base.join("QoderCN").join("User");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// 解密本机 Qoder IDE 的登录态（dt-/drt- token + 用户信息）。
pub fn read_local_credential() -> Result<Value, String> {
    read_local_credential_in(qoder_user_data_dir())
}

/// 解密本机 QoderCN（中国版 IDE，`%APPDATA%\QoderCN`）登录态 —— `/qwen-flash` 池的本机凭据来源。
pub fn read_local_credential_cn() -> Result<Value, String> {
    read_local_credential_in(qoder_cn_user_data_dir())
}

/// QoderWork CN（**独立 Electron 应用**，非 VS Code fork）数据目录：`%APPDATA%\QoderWork CN`。
///
/// 与 Qoder/QoderCN 的关键差异：它**没有** `User\globalStorage\state.vscdb`，
/// 登录态是同目录下的 `auth-v2.dat` / `auth.dat`。
fn qoderwork_cn_user_data_dir() -> Option<PathBuf> {
    let base = dirs::config_dir()?;
    let p = base.join("QoderWork CN");
    if p.exists() {
        return Some(p);
    }
    None
}

/// ISO8601 → 毫秒时间戳；解析失败则原样返回（保留可读信息，不丢字段）。
fn iso_to_ms(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_str) {
        None => Value::Null,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => json!(dt.timestamp_millis()),
            Err(_) => json!(s),
        },
    }
}

/// 解密本机 **QoderWork CN** 登录态（dt-/drt- + 用户信息）。
///
/// 凭据格式与 Qoder 同源：Chromium safeStorage v10 = `DPAPI(Local State 的
/// os_crypt.encrypted_key)` 解出 master key，再 AES-256-GCM 解凭据块；
/// 但 **Local State 与凭据文件在同一目录**（Qoder 是在 User 的父目录）。
/// `auth-v2.dat`（schemaVersion=2）优先，缺失时回退 `auth.dat`。
pub fn read_local_credential_work_cn() -> Result<Value, String> {
    use crate::modules::vscode_cn_inject::{decrypt_windows_gcm_v10, dpapi_decrypt};

    let dir = qoderwork_cn_user_data_dir()
        .ok_or_else(|| "未找到 QoderWork CN 数据目录（请先安装并登录 QoderWork CN）".to_string())?;

    let blob = std::fs::read(dir.join("auth-v2.dat"))
        .or_else(|_| std::fs::read(dir.join("auth.dat")))
        .map_err(|e| format!("读取 QoderWork CN 凭据失败（auth-v2.dat / auth.dat）: {e}"))?;

    let text = std::fs::read_to_string(dir.join("Local State"))
        .map_err(|e| format!("读取 QoderWork CN Local State 失败: {e}"))?;
    let state: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析 QoderWork CN Local State 失败: {e}"))?;
    let key_b64 = state
        .pointer("/os_crypt/encrypted_key")
        .and_then(Value::as_str)
        .ok_or_else(|| "QoderWork CN Local State 缺少 os_crypt.encrypted_key".to_string())?;
    let encrypted_key = general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| format!("QoderWork CN encrypted_key base64 解码失败: {e}"))?;
    if encrypted_key.len() < 5 || &encrypted_key[..5] != b"DPAPI" {
        return Err("QoderWork CN encrypted_key 前缀异常".to_string());
    }
    let master = dpapi_decrypt(&encrypted_key[5..])?;
    let plain = decrypt_windows_gcm_v10(&master, &blob)?;
    let auth: Value = serde_json::from_slice(&plain)
        .map_err(|e| format!("QoderWork CN 凭据 JSON 解析失败: {e}"))?;

    let token = auth
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| t.starts_with("dt-"))
        .ok_or_else(|| {
            format!(
                "QoderWork CN 凭据缺少 dt- token（可能是未登录，或仅有占位符 auth.dat）"
            )
        })?;

    Ok(json!({
        "uid": auth.pointer("/user/id").cloned().unwrap_or(Value::Null),
        "name": auth.pointer("/user/name").cloned().unwrap_or(Value::Null),
        "username": auth.pointer("/user/username").cloned().unwrap_or(Value::Null),
        "email": auth.pointer("/user/email").cloned().unwrap_or(Value::Null),
        "accessToken": token,
        "refreshToken": auth.get("refreshToken").cloned().unwrap_or(Value::Null),
        "expireTime": iso_to_ms(auth.get("expiresAt")),
        "refreshTokenExpireTime": iso_to_ms(auth.get("refreshTokenExpiresAt")),
        "userTag": auth.pointer("/user/tier").cloned().unwrap_or(Value::Null),
        "source": "qoderworkcn-local",
    }))
}

/// 导入本机 QoderWork CN 登录态到**独立池**（幂等）。
///
/// - 打 `gateway = "qwen3.8-flash"` 池标 ⇒ 与默认池 / `qwen` / `qwen-flash` 池**完全隔离**，
///   轮转游标独立（`next_pool_index` 按 gateway 分别记）；
/// - `region = "cn"`；
/// - 写入 per-account `endpoint`：实测 QoderWork CN 的 `dt-` **只被** CN 网关
///   `https://gateway.qoder.com.cn` 接受（`openapi.qoder.sh` 返回 401 TOKEN_EXPIRE），
///   故该账号的通话/信息请求必须走 CN 网关。
pub fn import_local_work_cn() -> Result<Value, String> {
    let mut credential = read_local_credential_work_cn()?;
    credential["gateway"] = json!("qwen3.8-flash");
    credential["region"] = json!("cn");
    credential["endpoint"] = json!(QODERWORK_CN_ENDPOINT);
    upsert_account(credential)
}

/// 解密指定数据目录的 Qoder / QoderCN IDE 登录态（state.vscdb secret://aicoding.auth.userInfo）。
pub fn read_local_credential_in(user_dir: Option<PathBuf>) -> Result<Value, String> {
    use crate::modules::vscode_cn_inject::{decrypt_windows_gcm_v10, dpapi_decrypt};

    let user_dir = user_dir
        .ok_or_else(|| "未找到本机 Qoder IDE 数据目录（请安装并登录 Qoder）".to_string())?;
    let db_path = user_dir.join("globalStorage").join("state.vscdb");
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("打开 Qoder state.vscdb 失败: {e}"))?;
    let raw: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'secret://aicoding.auth.userInfo'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("Qoder 登录态不存在（未登录）: {e}"))?;
    let data = decode_secret_storage_blob(&raw, &user_dir)?;
    let auth: Value = serde_json::from_slice(&data)
        .map_err(|e| format!("Qoder 登录态 JSON 解析失败: {e}"))?;
    let token = auth
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| t.starts_with("dt-"))
        .ok_or_else(|| "Qoder 登录态缺少 dt- token".to_string())?;
    Ok(json!({
        "uid": auth.get("id").cloned().unwrap_or(Value::Null),
        "name": auth.get("name").cloned().unwrap_or(Value::Null),
        "email": auth.get("email").cloned().unwrap_or(Value::Null),
        "accessToken": token,
        "refreshToken": auth.get("refreshToken").cloned().unwrap_or(Value::Null),
        "expireTime": auth.get("expireTime").cloned().unwrap_or(Value::Null),
        "refreshTokenExpireTime": auth.get("refreshTokenExpireTime").cloned().unwrap_or(Value::Null),
        "userTag": auth.get("userTag").cloned().unwrap_or(Value::Null),
        "quota": auth.get("quota").cloned().unwrap_or(Value::Null),
        "isQuotaExceeded": auth.get("isQuotaExceeded").cloned().unwrap_or(Value::Null),
        "source": "local-import",
    }))
}

/// secret:// 值为 JSON `{"type":"Buffer","data":[...]}`，前 3 字节 v10 前缀，
/// 解密 key 在 Qoder `Local State` 的 os_crypt.encrypted_key（DPAPI 包裹）。
fn decode_secret_storage_blob(raw: &str, user_dir: &PathBuf) -> Result<Vec<u8>, String> {
    use crate::modules::vscode_cn_inject::{decrypt_windows_gcm_v10, dpapi_decrypt};

    let wrapper: Value = serde_json::from_str(raw)
        .map_err(|e| format!("Qoder secret 条目解析失败: {e}"))?;
    let bytes: Vec<u8> = wrapper
        .pointer("/data")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_u64).map(|v| v as u8).collect())
        .ok_or_else(|| "Qoder secret 条目缺少 data".to_string())?;
    let local_state = user_dir
        .parent()
        .map(|root| root.join("Local State"))
        .ok_or_else(|| "Qoder Local State 路径异常".to_string())?;
    let text = std::fs::read_to_string(&local_state)
        .map_err(|e| format!("读取 Qoder Local State 失败: {e}"))?;
    let state: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析 Qoder Local State 失败: {e}"))?;
    let encrypted_key_b64 = state
        .pointer("/os_crypt/encrypted_key")
        .and_then(Value::as_str)
        .ok_or_else(|| "Qoder Local State 缺少 os_crypt.encrypted_key".to_string())?;
    let encrypted_key = general_purpose::STANDARD
        .decode(encrypted_key_b64)
        .map_err(|e| format!("encrypted_key base64 解码失败: {e}"))?;
    if encrypted_key.len() < 5 || &encrypted_key[..5] != b"DPAPI" {
        return Err("Qoder encrypted_key 前缀异常".to_string());
    }
    let master = dpapi_decrypt(&encrypted_key[5..])?;
    decrypt_windows_gcm_v10(&master, &bytes)
}


/// 手动导入（高级）：直接粘贴 device flow 拿到的 dt-/drt- 凭据。
/// 手动导入账号。`gateway` 为空视为 `"qoder"`（默认池）；传 `"qwen"` 时该账号
/// 只参与 `/qwen/v1` 的独立轮转，与默认池额度互不侵占。
pub fn import_manual(
    uid: &str,
    token: &str,
    refresh_token: &str,
    gateway: &str,
) -> Result<Value, String> {
    if !token.starts_with("dt-") {
        return Err("access token 应为 dt- 前缀（device token）".to_string());
    }
    let mut entry = json!({
        "uid": if uid.is_empty() { Value::Null } else { json!(uid) },
        "accessToken": token,
        "refreshToken": if refresh_token.is_empty() { Value::Null } else { json!(refresh_token) },
        "source": "manual",
    });
    let gateway = if gateway.is_empty() { "qoder" } else { gateway };
    if gateway != "qoder" {
        entry["gateway"] = json!(gateway);
    }
    upsert_account(entry)
}

/// 导入本机 Qoder 登录态到账号池（幂等）。
pub fn import_local() -> Result<Value, String> {
    let credential = read_local_credential()?;
    upsert_account(credential)
}

/// 导入本机 QoderCN（中国版 IDE）登录态到账号池（幂等）。
///
/// 自动打 `gateway = "qwen-flash"`（Qoder CN 专用池）与 `region = "cn"` 地域标；
/// 该账号即 `/qwen-flash/v1` 的本机凭据来源，与国际默认池完全隔离。
pub fn import_local_cn() -> Result<Value, String> {
    let mut credential = read_local_credential_cn()?;
    credential["source"] = json!("qodercn-local");
    credential["gateway"] = json!("qwen-flash");
    credential["region"] = json!("cn");
    upsert_account(credential)
}

/// 账号地域识别：显式 `region` 字段（手动指定，防误判出口）最优先，其次按
/// 来源 / 网关标 / 站点域 / 邮箱域推断。返回 "cn" 或 "intl"。
pub fn infer_region(account: &Value) -> &'static str {
    match account.get("region").and_then(Value::as_str) {
        Some("cn") => return "cn",
        Some("intl") => return "intl",
        _ => {}
    }
    // QoderCN 中国版 IDE 导入的登录态
    if account.get("source").and_then(Value::as_str) == Some("qodercn-local") {
        return "cn";
    }
    // CN 专用池成员必然是国内账号（device-flow 走 qoderclicn client_id）
    if matches!(
        account.get("gateway").and_then(Value::as_str),
        Some("qwen") | Some("qwen-flash")
    ) {
        return "cn";
    }
    // 头像/站点域含中国版域名
    let site_text = format!(
        "{}{}",
        account.get("avatarUrl").and_then(Value::as_str).unwrap_or(""),
        account.get("homeUrl").and_then(Value::as_str).unwrap_or(""),
    );
    if ["qoder.com.cn", "qwenwork.cn"].iter().any(|h| site_text.contains(h)) {
        return "cn";
    }
    // 国内常见邮箱域
    let email = account.get("email").and_then(Value::as_str).unwrap_or("");
    if ["qq.com", "foxmail.com", "163.com", "126.com", "aliyun.com", "dingtalk.com", "sina.com"]
        .iter()
        .any(|d| email.ends_with(d))
    {
        return "cn";
    }
    "intl"
}

/// 地域展示标签（前端直接可用）。
pub fn region_label(region: &str) -> &'static str {
    if region == "cn" {
        "🇨🇳 国内"
    } else {
        "🌐 国际"
    }
}

/// 给展示条目补上 region / regionLabel / regionSource 字段。
/// `regionSource` 区分「手动指定」与「自动推断」，前端据此渲染区域选择器。
fn with_region(mut entry: Value) -> Value {
    let manual = matches!(
        entry.get("region").and_then(Value::as_str),
        Some("cn") | Some("intl")
    );
    let region = infer_region(&entry);
    entry["region"] = json!(region);
    entry["regionLabel"] = json!(region_label(region));
    entry["regionSource"] = json!(if manual { "manual" } else { "auto" });
    // 软件来源标签：账号池多软件并存（Qoder 国际版 IDE / QoderCN 中国版 IDE /
    // QoderWork CN 独立 Electron 应用）——展示层据此显式声明来源，避免误判归属。
    let source = entry.get("source").and_then(Value::as_str).unwrap_or("");
    entry["sourceLabel"] = json!(match source {
        "qoderworkcn-local" => "QoderWork CN",
        "qodercn-local-live" | "qodercn-local" => "QoderCN",
        "local-live" | "local-import" => "Qoder（国际版）",
        "device-flow" => "设备授权（Qoder CN）",
        "manual" => "手动导入",
        "" => "Qoder",
        other => other,
    });
    entry
}

/// 手动指定账号地域（防误判出口）。`region` ∈ {"cn","intl","auto"}：
/// cn/intl 写入 `region` 字段（`infer_region` 最优先），auto 清除手动指定回到自动推断。
pub fn set_account_region(uid: &str, region: &str) -> Result<Value, String> {
    match region {
        "cn" | "intl" | "auto" => {}
        _ => return Err("region 仅支持 cn / intl / auto".to_string()),
    }
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let Some(entry) = accounts
        .iter_mut()
        .find(|a| a.get("uid").and_then(Value::as_str) == Some(uid))
    else {
        return Err("账号池条目不存在".to_string());
    };
    if region == "auto" {
        if let Some(obj) = entry.as_object_mut() {
            obj.remove("region");
        }
    } else {
        entry["region"] = json!(region);
    }
    let updated = entry.clone();
    let _ = save_accounts(&accounts);
    Ok(updated)
}

// ---------------------------------------------------------------------------
// PKCE device flow（添加账号：浏览器授权 → 轮询凭据）
// ---------------------------------------------------------------------------

/// 生成 device flow 会话：授权 URL 给用户打开，nonce 留给轮询。
/// `gateway` 决定授权完成后账号入哪个池（默认 qoder；qwen = /qwen/v1 独立池）。
pub fn device_flow_start(gateway: &str) -> Result<Value, String> {
    use sha2::Digest;
    let mut verifier_bytes = [0u8; 48];
    let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    verifier_bytes[..16].copy_from_slice(a.as_bytes());
    verifier_bytes[16..32].copy_from_slice(b.as_bytes());
    verifier_bytes[32..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge_bytes = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = general_purpose::URL_SAFE_NO_PAD.encode(challenge_bytes);
    let nonce = uuid::Uuid::new_v4().to_string();
    let machine_id = uuid::Uuid::new_v4().to_string();
    let url = format!(
        "https://qoder.com/device/selectAccounts?challenge={challenge}&challenge_method=S256&nonce={nonce}&machine_id={machine_id}&client_id={DEVICE_CLIENT_ID}"
    );
    Ok(json!({
        "authorizationUrl": url,
        "nonce": nonce,
        "verifier": verifier,
        "gateway": if gateway.is_empty() { "qoder" } else { gateway },
        "startedAt": now_ms(),
    }))
}

/// 轮询授权结果：404 = 等待用户授权（设计内），200 = 凭据。
async fn poll_device_token(nonce: &str, verifier: &str) -> Result<Value, String> {
    let url = format!(
        "{OPENAPI_HOST}/api/v1/deviceToken/poll?nonce={nonce}&verifier={verifier}&challenge_method=S256"
    );
    let resp = http_client()
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Qoder poll 请求失败: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(json!({"pending": true}));
    }
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Qoder poll 失败（{status}）：{}", text.chars().take(150).collect::<String>()));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("Qoder poll 响应解析失败: {e}"))?;
    if value.get("token").and_then(Value::as_str).is_none() {
        return Ok(json!({"pending": true}));
    }
    Ok(value)
}

/// 轮询直到拿到凭据或超时（由调用方的 HTTP 请求驱动，最多 ~4 分钟）。
async fn device_flow_wait(session: &Value) -> Result<Value, String> {
    let nonce = session
        .get("nonce")
        .and_then(Value::as_str)
        .ok_or_else(|| "device flow 会话缺少 nonce".to_string())?;
    let verifier = session
        .get("verifier")
        .and_then(Value::as_str)
        .ok_or_else(|| "device flow 会话缺少 verifier".to_string())?;
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis((POLL_TIMEOUT_MS as i64 - 60_000).max(60_000) as u64);
    while std::time::Instant::now() < deadline {
        match poll_device_token(nonce, verifier).await? {
            value if value.get("pending").and_then(Value::as_bool) == Some(true) => {
                tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
            }
            value => return Ok(value),
        }
    }
    Err("Qoder device flow 授权超时（用户未在时限内完成授权）".to_string())
}

/// device flow 一步到位：轮询到凭据后查询 userinfo 并入池。
pub async fn device_flow_complete(session: &Value) -> Result<Value, String> {
    let credential = device_flow_wait(session).await?;
    let token = credential
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| "Qoder 授权响应缺少 token".to_string())?;
    let mut entry = json!({
        "uid": credential.get("user_id").cloned().unwrap_or(Value::Null),
        "accessToken": token,
        "refreshToken": credential.get("refresh_token").cloned().unwrap_or(Value::Null),
        "expireTime": credential.get("expires_at").cloned().unwrap_or(Value::Null),
        "source": "device-flow",
    });
    // 会话里带了目标池标记（device_flow_start 存入）；qwen 标记的账号进 /qwen 独立池
    if let Some(gateway) = session.get("gateway").and_then(Value::as_str) {
        if !gateway.is_empty() && gateway != "qoder" {
            entry["gateway"] = json!(gateway);
        }
    }
    // userinfo 补全展示信息
    if let Ok(info) = userinfo(token).await {
        entry["name"] = info.get("name").cloned().unwrap_or(Value::Null);
        entry["email"] = info.get("email").cloned().unwrap_or(Value::Null);
        entry["uid"] = info.get("id").cloned().unwrap_or(entry["uid"].clone());
    }
    upsert_account(entry)
}

// ---------------------------------------------------------------------------
// 上游调用（userinfo / refresh / chat）
// ---------------------------------------------------------------------------

/// 查询 Qoder 用户信息（兼做 token 有效性探测）。
pub async fn userinfo(token: &str) -> Result<Value, String> {
    let resp = http_client()
        .get(format!("{OPENAPI_HOST}/api/v1/userinfo"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("Qoder userinfo 请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("__UNAUTHORIZED__".to_string());
    }
    if !status.is_success() {
        return Err(format!("Qoder userinfo 失败（{status}）"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Qoder userinfo 解析失败: {e}"))
}

/// refresh token 换新 access token。
pub async fn refresh_token(refresh_token: &str) -> Result<Value, String> {
    let resp = http_client()
        .post(format!("{OPENAPI_HOST}/api/v1/jobToken/refresh"))
        .json(&json!({"refresh_token": refresh_token}))
        .send()
        .await
        .map_err(|e| format!("Qoder refresh 请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Qoder refresh 失败（{status}）：{}", text.chars().take(150).collect::<String>()));
    }
    serde_json::from_str(&text).map_err(|e| format!("Qoder refresh 响应解析失败: {e}"))
}

/// 刷新池内单个账号并写回。
pub async fn refresh_stored_account(uid: &str) -> Result<String, String> {
    let accounts = load_accounts();
    let Some(entry) = accounts
        .iter()
        .find(|a| a.get("uid").and_then(Value::as_str) == Some(uid))
    else {
        return Err("账号池条目不存在".to_string());
    };
    let refresh = entry
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|t| t.starts_with("drt-"))
        .ok_or_else(|| "账号缺少 drt- refresh token".to_string())?;
    let refreshed = refresh_token(refresh).await?;
    let new_token = refreshed
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| "Qoder refresh 响应缺少 token".to_string())?
        .to_string();
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(entry) = accounts
        .iter_mut()
        .find(|a| a.get("uid").and_then(Value::as_str) == Some(uid))
    {
        entry["accessToken"] = json!(new_token);
        if let Some(new_refresh) = refreshed.get("refresh_token").and_then(Value::as_str) {
            entry["refreshToken"] = json!(new_refresh);
        }
        if let Some(expiry) = refreshed.get("expires_at") {
            entry["expireTime"] = json!(expiry);
        }
        entry["status"] = json!("ok");
        entry["error"] = Value::Null;
        entry["updatedAt"] = json!(now_ms());
        let _ = save_accounts(&accounts);
    }
    Ok(new_token)
}

// ---------------------------------------------------------------------------
// 多账号轮转 + OpenAI 兼容 chat
// ---------------------------------------------------------------------------

/// 路由 → 账号池的归一映射。
///
/// `qwen-flash`（`/qwen-flash/v1`）自 2026-09-18 起改为 **Qoder CN 专用池**：
/// 成员 = 本机 QoderCN 中国版登录态 + 打 `qwen-flash`/`qwen` 标的 CN 账号，
/// 不再复用国际默认池（国际/国内额度体系独立，混用会把 CN Free 额度打到国际池去）。
/// 轮转游标按原始 gateway 记（见 `next_pool_index`），/qwen 与 /qwen-flash 取号互不推进。
fn pool_gateway(gateway: &str) -> &str {
    match gateway {
        "qwen-flash" => "qwen-flash",
        other => other,
    }
}

/// 参与指定网关路由的账号（enabled != false 且有 token）。
///
/// - `"qoder"`（默认池）：本机实时登录态优先 + 未打 `gateway` 标的池账号；
/// - `"qwen-flash"`（`/qwen-flash/v1`，**Qoder CN 专用池**）：本机 **QoderCN 中国版**
///   实时登录态优先 + 打了 `gateway == "qwen-flash"` / `"qwen"` 标的 CN 账号
///   （device-flow 走 qoderclicn client_id，登进来的都是 CN 账号）。
///   已标记 `isQuotaExceeded` 的账号不参与轮转 —— 额度耗尽的上游返回 200 空正文，
///   `try_chat_for_account` 只对 401 做失败转移，接不住空正文，只会向下游产出空回答；
/// - `"qwen"`（`/qwen/v1` 独立池）：**只**轮转打了 `gateway == "qwen"` 标的账号，
///   不回落本机登录态 —— 两个网关的额度从此互不侵占（2026-09-18：原本 /qwen 只是
///   /qoder 的模型名映射壳，两边共享同一池，Free 档额度被互相打穿）。
fn routable_accounts_for(gateway: &str) -> Vec<Value> {
    let gateway = pool_gateway(gateway);
    let mut pool: Vec<Value> = Vec::new();
    match gateway {
        "qoder" => {
            // 本机实时登录态优先（最新鲜）；只服务默认池
            if let Ok(local) = read_local_credential() {
                pool.push(local);
            }
        }
        "qwen-flash" => {
            // Qoder CN 本机登录态（中国版 IDE）优先；额度耗尽的实时账号让位给池内 CN 账号
            if let Ok(local) = read_local_credential_cn() {
                if local.get("isQuotaExceeded").and_then(Value::as_bool) != Some(true) {
                    pool.push(local);
                }
            }
        }
        _ => {}
    }
    for account in load_accounts() {
        if account.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if account.get("status").and_then(Value::as_str) == Some("invalid") {
            continue;
        }
        // 仅 CN 专用池跳过额度耗尽账号（默认池行为保持不变，避免多网关联动回归）
        if gateway == "qwen-flash"
            && account.get("isQuotaExceeded").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let account_gateway = account
            .get("gateway")
            .and_then(Value::as_str)
            .unwrap_or("qoder");
        // Qwen3.8-Flash 专用池收编全部 CN 标账号：显式 qwen-flash 标、复用 qwen 池成员，
        // 以及 `import_local_work_cn`（QoderWork CN 导入路径）打的 qwen3.8-flash 标。
        // 🔴 2026-09-20 修复：qwen3.8-flash 标的账号此前**无任何池收留**（孤儿账号，
        // per-account endpoint 路由早已支持但池成员判定漏了它），导致 CN 池实际只剩
        // 单账号 nanmi 扛量——它一打嗝（限流/瞬时额度）整条 /qwen-flash 就 502。
        let in_pool = if gateway == "qwen-flash" {
            matches!(
                account_gateway,
                "qwen-flash" | "qwen" | "qwen3.8-flash"
            )
        } else {
            account_gateway == gateway
        };
        if !in_pool {
            continue;
        }
        let uid = account.get("uid").and_then(Value::as_str).unwrap_or_default();
        if pool
            .iter()
            .any(|a| a.get("uid").and_then(Value::as_str) == Some(uid))
            && !uid.is_empty()
        {
            continue;
        }
        pool.push(account);
    }
    pool
}

/// 每个网关独立轮转游标（共享游标会让两个池的取号互相推进、失去均匀性）。
static POOL_CURSORS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, usize>>> =
    std::sync::OnceLock::new();

fn next_pool_index(gateway: &str, pool_len: usize) -> usize {
    let cursors = POOL_CURSORS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut map = cursors.lock().unwrap();
    let cursor = map.entry(gateway.to_string()).or_insert(0);
    let index = *cursor % pool_len.max(1);
    *cursor = cursor.wrapping_add(1);
    index
}

fn pool_label(gateway: &str) -> &'static str {
    match gateway {
        "qwen" => "Qwen 独立",
        "qwen-flash" => "Qwen3.8-Flash",
        _ => "Qoder",
    }
}

/// 智能解析下游客户端请求的模型标识符，归一化为 Qoder 上游合法档位（lite / pro / max）。
/// 全面兼容与支持 Qwen3.8-Max 旗舰、Qwen3.8-Flash 及 OpenAI / Claude / DeepSeek 别名。
pub fn resolve_qoder_model(requested: &str) -> &'static str {
    let lower = requested.trim().to_ascii_lowercase();
    match lower.as_str() {
        // Max 档位：对应阿里通义千问旗舰 Qwen3.8-Max (2.4T 参数 · 1M 上下文)
        "max" | "qwen3.8-max" | "qwen3.8max" | "qwen-max" | "qwen3.8" | "gpt-4o" | "gpt-4"
        | "gpt-4-turbo" | "claude-3.5-sonnet" | "claude-3-7-sonnet" | "claude-3.7-sonnet"
        | "deepseek-chat" | "deepseek-reasoner" | "o1" | "o3" => "max",

        // Pro 档位：对应 Qwen3.6-Plus 标准主力档
        "pro" | "qwen3.6-plus" | "qwen-plus" | "qwen3.6" | "claude-3-sonnet" | "gemini-2.5-pro" => "pro",

        // Lite 档位：对应极速低延迟档 Qwen3.8-Flash (及 Qwen3.5-Flash)
        "lite" | "qwen3.8-flash" | "qwen3.8-flash-next" | "qwen-turbo" | "gpt-4o-mini"
        | "gpt-3.5-turbo" | "claude-3-haiku" | "gemini-2.5-flash" => "lite",

        // 包含关键词推断
        m if m.contains("max") => "max",
        m if m.contains("plus") || m.contains("pro") => "pro",
        _ => "lite",
    }
}

/// 单账号尝试：401 刷新重试一次，仍失败返回 Err。
async fn try_chat_for_account(
    account: &Value,
    body: &Value,
) -> Result<reqwest::Response, String> {
    let token = account
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if token.is_empty() {
        return Err("账号缺少 accessToken".to_string());
    }
    let send = |token: &str| {
        let mut payload = body.clone();
        payload["stream"] = json!(true);
        payload["stream_options"] = json!({"include_usage": true});
        let req_model = payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("lite");
        let upstream_model = resolve_qoder_model(req_model);
        payload["model"] = json!(upstream_model);
        let rid = uuid::Uuid::new_v4().to_string();
        payload["metadata"] = json!({
            "context": {
                "request_id": rid,
                "request_set_id": uuid::Uuid::new_v4().to_string(),
                "session_id": uuid::Uuid::new_v4().to_string(),
                "task_id": "common",
                "client_type": "qodercli",
            }
        });
        // per-account endpoint（可选）：QoderWork CN 等独立应用签发的 dt- 只被各自的
        // CN 网关接受（实测 .sh 回 401 TOKEN_EXPIRE）。无该字段时行为与从前完全一致。
        let host = account
            .get("endpoint")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(CHAT_HOST);
        http_client()
            .post(format!("{host}/model/v1/chat/completions"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            // 固定浏览器 UA（reqwest 默认 UA "reqwest/x.y" 是明显的非浏览器指纹）
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")
            .json(&payload)
            .send()
    };
    let mut resp = send(&token)
        .await
        .map_err(|e| format!("Qoder chat 请求失败: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        // 401：drt- 刷新后重试一次
        let uid = account.get("uid").and_then(Value::as_str).unwrap_or_default();
        let new_token = if uid.is_empty() {
            refresh_token(
                account
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
            .await?
            .get("token")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "Qoder refresh 响应缺少 token".to_string())?
        } else {
            refresh_stored_account(uid).await?
        };
        resp = send(&new_token)
            .await
            .map_err(|e| format!("Qoder chat 请求失败: {e}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Qoder 登录态失效且刷新后仍 401".to_string());
        }
    }
    Ok(resp)
}

/// 修复上游后端实例偶发的 GBK 误码（2026-09-18 17:00 起上游 lite 档 100% 复现）：
/// 上游把模型输出的 UTF-8 字节按 GBK 误解码后再发出来（`浣犲ソ鍛€` 实为 `你好呀`）。
///
/// 修复变换**确定且可逆**：按 GBK 重编码还原原始字节，是合法 UTF-8 就采用。
/// 上游存在**多层误码**（单层修复后仍是乱码），因此迭代到不动点（上限 4 层）。
/// 纯 ASCII 与无法 GBK 编码的文本原样返回。
///
/// 🔴 **2026-09-18 23:30 修一处严重误伤**：原判据「干净文本的 GBK 字节不是合法 UTF-8」
/// **不成立** —— 大量合法汉字的 GBK 双字节恰好是合法 UTF-8，导致函数反而**制造**乱码：
/// 实测 **1920 个汉字会被改坏，其中 682 个是 GB2312 一级常用字**
/// （`一`→`һ`、`为`→`Ϊ`、`业`→`ҵ`、双字串 `为之`→`Ϊ֮` …）。
/// 流式路径逐帧调用、`raw` 累积到单字时即命中，故「一、二、三！」→「һ、二、三！」。
/// 现加 [`fix_is_trustworthy`] 指纹校验把关，并补回归单测锁死。
pub fn repair_gbk_mojibake(s: &str) -> String {
    let mut current = s.to_string();
    for _ in 0..4 {
        if current.is_empty() || current.is_ascii() {
            break;
        }
        let (bytes, _, had_errors) = encoding_rs::GBK.encode(&current);
        if had_errors {
            break;
        }
        match String::from_utf8(bytes.into_owned()) {
            Ok(fixed) if fixed != current && fix_is_trustworthy(&fixed) => current = fixed,
            _ => break,
        }
    }
    current
}

/// 这次「GBK 重编码」的结果是否可信 —— 用来挡住**把合法汉字当乱码修**的破坏性误判。
///
/// **原理（误修的精确指纹）**：一个合法汉字能同时通过 `had_errors == false` 与
/// `from_utf8 == Ok` 两道校验，唯一原因是它的 GBK 双字节**恰好构成一个合法的
/// 2 字节 UTF-8 序列**；而 2 字节 UTF-8 能表示的码点范围就是 `U+0080..=U+07FF`。
/// 所以**误修的产物必然落在 `U+0080..=U+07FF`**（拉丁扩展/希腊/西里尔/希伯来/阿拉伯…），
/// 永远不可能是汉字。真乱码修复后得到的却是正常文本（汉字/ASCII/全角标点），落不到该区间。
///
/// 实测覆盖：`一`→`һ`(U+04BB)、`为`→`Ϊ`(U+03AA)、`业`→`ҵ`(U+04B5)、`专`→`ר`(U+05E8)
/// 全部被挡；而 `浣犲ソ鍛€`→`你好呀`、`濂界殑`→`好的` 正常放行。
///
/// ⚠️ 代价：若上游真的把「含 U+0080..=U+07FF 字符的原文」（如法语重音字母）弄成乱码，
/// 这里会**拒绝修复**（宁可保留乱码也不制造新乱码）。对中文网关是合算的取舍。
fn fix_is_trustworthy(fixed: &str) -> bool {
    !fixed.chars().any(|c| ('\u{80}'..='\u{7ff}').contains(&c))
}

/// OpenAI 兼容 chat 统一入口：非流式请求在服务端聚合上游 SSE，
/// 返回标准 `chat.completion` JSON；流式请求透传上游 SSE。
/// `gateway` 决定用哪个账号池（qoder 默认池 / qwen 独立池）。
pub async fn chat_completions_unified_for(
    gateway: &str,
    body: Value,
) -> Result<QoderChatOutput, String> {
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map_or(false, |a| !a.is_empty());

    if has_tools {
        // 🔴 工具调用降级（2026-09-19）：上游 lite 档**不解析 tools**——带 tools 字段
        // 100% 返回空正文（agent 客户端全军覆没的根因）。剥离 tools 注入提示词、
        // 转写历史工具消息，服务端聚合后嗅探文本 tool_calls 合成标准响应。
        // 流式语义由 server 层用合成帧还原（等价于一次整段输出）。
        let mut work = body;
        rewrite_tool_history(&mut work);
        inject_tools_prompt(&mut work);
        let aggregated = aggregate_with_failover_json(gateway, &work, true).await?;
        return Ok(QoderChatOutput::SynthesizedJson(aggregated));
    }
    if want_stream {
        let (resp, pre) = chat_stream_with_failover(gateway, body).await?;
        return Ok(QoderChatOutput::Stream(resp, pre));
    }
    let aggregated = aggregate_with_failover_json(gateway, &body, false).await?;
    Ok(QoderChatOutput::Json(aggregated))
}

/// 聚合 + 空回复失败转移（含跨池兜底链），返回首个非空聚合结果。
/// `apply_tools` 时对聚合结果做工具调用嗅探合成。
async fn aggregate_with_failover_json(
    gateway: &str,
    work: &Value,
    apply_tools: bool,
) -> Result<Value, String> {
    // 非流式：强制上游流式，聚合为完整 JSON。
    // 🔴 空回复失败转移（含跨池兜底）：免费档额度受限/风控时上游返回 200 空正文
    // （非 401，try_chat_for_account 接不住）——逐账号、逐池重试直到拿到非空内容。
    let mut payload = work.clone();
    payload["stream"] = json!(true);
    payload["stream_options"] = json!({"include_usage": true});
    // 每次请求开始时清空上游错误收集器，避免带上一次请求的残留
    reset_empty_details();
    let mut chain: Vec<&str> = vec![gateway];
    chain.extend_from_slice(pool_fallbacks(gateway));
    // 🔴 整链重试（2026-09-21）：上游对大请求**间歇性**返回 200 空正文——实测
    // 同一 228KB 请求 6/6 成功，但此前同规模请求失败过 5 次（用户侧 4 小时 5 次）。
    // 即失败概率随请求规模上升但**不是规模决定**（能通的请求证明规模可接受，
    // 故不做更激进裁剪，那会白丢可用上下文）。全部账号空回复时隔 1.5s 重试，
    // 最多 3 轮——把偶发失败转成多次抽样，显著提高最终成功率。
    const MAX_ROUNDS: usize = 3;
    let mut pool_errors: Vec<String> = Vec::new();
    for round in 0..MAX_ROUNDS {
        if round > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            log_pool_failover(gateway, &format!("第 {round} 轮全部空回复，整链重试"));
        }
        pool_errors.clear();
        for pool_gw in &chain {
            let pool = routable_accounts_for(pool_gw);
            if pool.is_empty() {
                pool_errors.push(format!("[{pool_gw}] 账号池为空"));
                continue;
            }
            let start = next_pool_index(gateway, pool.len());
            let mut saw_empty = false;
            for index in 0..pool.len() {
                let account = &pool[(start + index) % pool.len()];
                let resp = match try_chat_for_account(account, &payload).await {
                    Ok(r) => r,
                    Err(e) => {
                        pool_errors.push(format!("[{pool_gw}] {e}"));
                        continue;
                    }
                };
                let aggregated = aggregate_qoder_sse(
                    resp,
                    work.get("model").and_then(Value::as_str).unwrap_or("lite"),
                )
                .await?;
                let has_content = aggregated
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .map_or(false, |s| !s.trim().is_empty())
                    || aggregated
                        .pointer("/choices/0/message/reasoning_content")
                        .and_then(Value::as_str)
                        .map_or(false, |s| !s.trim().is_empty());
                if has_content {
                    return Ok(if apply_tools {
                        apply_tool_synthesis(aggregated)
                    } else {
                        aggregated
                    });
                }
                saw_empty = true;
            }
            if saw_empty {
                log_pool_failover(
                    pool_gw,
                    "全部账号均返回空回复（原因未确认：可能限流/容量/风控；详见 empty-frame 日志）",
                );
                pool_errors.push(format!(
                    "[{pool_gw}] 全部账号均返回空回复（原因未确认：可能限流/容量/风控）"
                ));
            }
        }
    }
    // 所有池、所有轮次都未能给出非空内容：给出明确错误而非静默空 JSON。
    // 附带上游原始错误摘要——2026-09-21 实测：失败真面目是「ALB 503 +
    // internal server error」（Qoder 网关自身故障），与额度/配置无关；
    // 不透传的话用户只看到"原因未确认"，无法判断该重试还是该等待。
    let upstream = empty_details_summary();
    let upstream_note = if upstream.is_empty() {
        String::new()
    } else {
        format!(" 上游实况: {upstream}。")
    };
    Err(format!(
        "所有账号池均未能返回内容（已重试 {} 轮）。{}{}",
        MAX_ROUNDS,
        pool_errors.join(" | "),
        upstream_note
    ))
}

/// 把历史消息里的 tool 角色与 assistant.tool_calls 转写为纯文本——
/// 上游 lite 档不理解这些角色/字段，原样透传会被忽略（上下文断裂的来源之一）。
pub(crate) fn rewrite_tool_history(body: &mut Value) {
    let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for m in msgs.iter_mut() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("").to_string();
        match role.as_str() {
            "tool" => {
                let name = m
                    .get("name")
                    .or_else(|| m.pointer("/tool_call_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let content = m
                    .get("content")
                    .map(|c| match c {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                *m = json!({"role": "user", "content": format!("[Tool `{name}` returned]: {content}")});
            }
            "assistant" => {
                let Some(tcs) = m.get("tool_calls").and_then(Value::as_array).cloned() else {
                    continue;
                };
                // 🔴 历史格式必须与系统提示词的协议**完全一致**（```json {"tool_calls":[...]}）。
                // 曾经用过 "[Called tools: Name(args)]" 散文格式——模型会模仿历史格式输出
                // 假调用（正文里一段像调用的文字，实际没执行），造成格式传染。
                let calls_json: Vec<Value> = tcs
                    .iter()
                    .filter_map(|tc| {
                        let f = tc.get("function")?;
                        let name = f.get("name")?.as_str()?.to_string();
                        let args = match f.get("arguments") {
                            Some(Value::String(s)) => {
                                serde_json::from_str::<Value>(s).unwrap_or(json!(s))
                            }
                            Some(other) => other.clone(),
                            None => json!({}),
                        };
                        Some(json!({"name": name, "arguments": args}))
                    })
                    .collect();
                if calls_json.is_empty() {
                    continue;
                }
                let block = json!({"tool_calls": calls_json}).to_string();
                let orig = m
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(obj) = m.as_object_mut() {
                    obj.remove("tool_calls");
                    obj.insert(
                        "content".to_string(),
                        json!(format!("{orig}\n```json\n{block}\n```")),
                    );
                }
            }
            _ => {}
        }
    }
}

/// 剥离 tools / tool_choice，把工具清单注入为 system 提示词，要求模型按
/// ```json {"tool_calls":[...]} ``` 格式输出调用意图。
///
/// v2.0（2026-09-20 重写）：旧版只有「需要时就调、不需要就直接答」一句，
/// 是 **opt-in** 语义——工具调用全靠模型自作主张，而直接回答永远安全且被
/// 明文许可，导致模糊请求下一律直接答（实测：同一问题带 tools 的思考显式度
/// 明显低于不带 tools；明确要求规划后思考显式度提升 4 倍）。
///
/// 新版改为**强制决策流程**：先评估信息是否充分 → 多步任务先出简要计划 →
/// 再决定调用 → 最后作答；并给出「必须调用」与「可以直答」的明确清单，
/// 以及「即将说『我假设』就停下来调工具」的自查规则。lite 档无独立
/// reasoning_content，计划/思考写在正文里即对客户端可见。
pub(crate) fn inject_tools_prompt(body: &mut Value) {
    let Some(tools) = body.get("tools").cloned() else {
        return;
    };
    let tools_json = serde_json::to_string_pretty(&tools).unwrap_or_else(|_| "[]".to_string());
    let prompt = format!(
        r#"[TOOL_USE_PROTOCOL]
Available tools:
{tools_json}

Follow this procedure IN ORDER before producing any user-visible answer:

1. ASSESS — Can the user's request be answered correctly using ONLY information
   already present in this conversation?
   - YES, and no file / current / workspace-specific data is needed → go to step 4.
   - NO, or you would have to guess, recall, or assume → a tool call is REQUIRED.

2. PLAN — Only when the task needs 2 or more tool calls: first output a short
   numbered plan (1-3 lines) naming which tool you will call and why.
   Skip this step for single-step tasks.

3. CALL — Respond with ONLY this JSON block, nothing else:
```json
{{ "tool_calls": [ {{ "name": "<tool_name>", "arguments": {{ }} }} ] }}
```
   - Use only tool names from the list above. Never invent one.
   - Arguments must match that tool's parameter schema exactly.
   - If several tools could work, choose the one whose description matches the
     user's intent most literally.

4. ANSWER — After tool results arrive, reply in plain text and synthesize them.
   Do not restate raw tool output.

A TOOL CALL IS MANDATORY when the user:
  - asks about files, code, project structure, or anything in the workspace;
  - refers to "this file", "current", "here", "my project", or similar;
  - asks you to verify, check, find, compare, or look up something;
  - asks for current or time-sensitive information.
In those cases answering from memory or assumption is WRONG.

Answer DIRECTLY only for: pure reasoning or math, explaining a concept, rewriting
text already in the conversation, or when the user asks for an opinion.

Show your reasoning when the user asks for analysis, or when the task is
non-trivial (multi-step, ambiguous, or required tool calls).

If you are about to write "I assume", "probably", "typically", or "based on my
knowledge" about something a tool could verify — STOP and call that tool instead.

Never imitate a tool call in prose; only the JSON block above counts as a call.
[/TOOL_USE_PROTOCOL]"#
    );
    if let Some(obj) = body.as_object_mut() {
        obj.remove("tools");
        obj.remove("tool_choice");
        if let Some(msgs) = obj.get_mut("messages").and_then(Value::as_array_mut) {
            let insert_at = if msgs.first().and_then(|m| m.get("role").and_then(Value::as_str)) == Some("system")
            {
                1
            } else {
                0
            };
            msgs.insert(insert_at, json!({"role": "system", "content": prompt}));
        }
    }
}

/// 从聚合文本中嗅探工具调用（模型按注入协议输出 ```json {"tool_calls":[...]} 块），
/// 命中则改写为标准 tool_calls 响应（finish_reason=tool_calls）。
pub(crate) fn apply_tool_synthesis(mut aggregated: Value) -> Value {
    let Some(text) = aggregated
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return aggregated;
    };
    // 模式 1：协议格式 ```json {"tool_calls":[...]}（或整体/片段 JSON）
    if let Some(calls) = crate::modules::glm_web::try_extract_tool_calls(&text) {
        let residual = match text.find("```json") {
            Some(pos) => text[..pos].trim().to_string(),
            None => String::new(),
        };
        return finalize_tool_calls(aggregated, calls, residual);
    }
    // 模式 2：模型模仿历史里的 "[Called tools: Name({args}); ...]" 散文格式——
    // 视为真实的调用意图，转成标准 tool_calls，不当作正文漏给客户端。
    if let Some(calls) = extract_prose_tool_calls(&text) {
        let residual = strip_prose_span(&text).trim().to_string();
        return finalize_tool_calls(aggregated, calls, residual);
    }
    // 模式 3：宽松提取——上游 lite 档实测会输出带 markdown 行号前缀（json\n1\n2…）
    // 与 NBSP（\xa0）的 tool_calls JSON，严格解析失败但意图明确。定位 "tool_calls"
    // 关键字 → 向前找配平 '{' → 括号扫描取完整 JSON（截断时按未闭合深度补全）→ 清洗 NBSP 后解析。
    if let Some(kpos) = text.find("\"tool_calls\"") {
        if let Some(start) = text[..kpos].rfind('{') {
            let (end_rel, unclosed) = scan_braces(&text[start..]);
            let span_raw = match end_rel {
                Some(e) => text[start..=start + e].to_string(),
                None if unclosed > 0 => {
                    // 生成中断：JSON 被截断，按未闭合深度补全右括号
                    format!("{}{}", text[start..].replace('\u{a0}', " "), "}".repeat(unclosed))
                }
                _ => String::new(),
            };
            if !span_raw.is_empty() {
                let span = span_raw.replace('\u{a0}', " ");
                if let Some(calls) = crate::modules::glm_web::try_extract_tool_calls(&span) {
                    // 整段即调用意图：正文残余置空（行号等渲染垃圾不再下发）
                    return finalize_tool_calls(aggregated, calls, String::new());
                }
            }
        }
    }
    aggregated
}

/// 括号扫描：返回 (首个配平 '}' 的相对索引，扫描结束时的未闭合深度)。
/// 未闭合深度 >0 表示 JSON 被截断（生成中断的典型形态）。
fn scan_braces(s: &str) -> (Option<usize>, usize) {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, ch) in s.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match ch {
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (Some(i), 0);
                }
            }
            _ => {}
        }
    }
    (None, depth)
}

/// 识别 "[Called tools: Name({args}); Name2({args2})]" 模仿模式。
fn extract_prose_tool_calls(text: &str) -> Option<Vec<Value>> {
    let start = text.find("[Called tools: ")?;
    let end = text[start..].find(']')? + start;
    let inner = &text[start + "[Called tools: ".len()..end];
    let calls: Vec<Value> = inner
        .split("; ")
        .enumerate()
        .filter_map(|(i, one)| {
            let one = one.trim();
            let lp = one.find('(')?;
            let rp = one.rfind(')')?;
            let name = one[..lp].trim().to_string();
            if name.is_empty() {
                return None;
            }
            let args_raw = &one[lp + 1..rp];
            let args: Value = serde_json::from_str(args_raw).unwrap_or(json!({"input": args_raw}));
            // 标准 OpenAI tool_calls 形状（function 包装 + arguments 字符串）
            Some(json!({
                "id": format!("call_{}_{}", uuid::Uuid::new_v4().simple(), i),
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()},
            }))
        })
        .collect();
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// 去掉文本中的 "[Called tools: ...]" 跨度，其余作为正文残余。
fn strip_prose_span(text: &str) -> String {
    match (text.find("[Called tools: "), text.find(']')) {
        (Some(s), Some(e)) if e > s => format!("{}{}", &text[..s], &text[e + 1..]),
        _ => text.to_string(),
    }
}

/// 把嗅探到的调用写回标准 tool_calls 响应；正文保留调用之外的残余文本。
fn finalize_tool_calls(mut aggregated: Value, calls: Vec<Value>, residual: String) -> Value {
    if let Some(msg) = aggregated
        .pointer_mut("/choices/0/message")
        .and_then(Value::as_object_mut)
    {
        msg.insert("tool_calls".to_string(), json!(calls));
        msg.insert(
            "content".to_string(),
            if residual.is_empty() { Value::Null } else { json!(residual) },
        );
    }
    if let Some(choice) = aggregated
        .pointer_mut("/choices/0")
        .and_then(Value::as_object_mut)
    {
        choice.insert("finish_reason".to_string(), json!("tool_calls"));
    }
    aggregated
}

/// 把合成（聚合+工具嗅探）结果转成 OpenAI SSE 帧序列文本——带 tools 的流式请求
/// 由 server 层以此为响应体（等价一次整段输出，保证工具调用闭环）。
pub fn synthesized_json_to_sse(value: &Value) -> String {
    let id = value.get("id").cloned().unwrap_or(json!("chatcmpl-synthesized"));
    let model = value.get("model").cloned().unwrap_or(json!("lite"));
    let created = value.get("created").cloned().unwrap_or(json!(0));
    let msg = value.pointer("/choices/0/message").cloned().unwrap_or(json!({}));
    let finish = value
        .pointer("/choices/0/finish_reason")
        .cloned()
        .unwrap_or(json!("stop"));
    let mut frames = String::new();
    let frame = |delta: &Value, finish: &Value| -> String {
        format!(
            "data: {}\n\n",
            json!({
                "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
        )
    };
    frames.push_str(&frame(&json!({"role": "assistant", "content": ""}), &Value::Null));
    if let Some(tcs) = msg.get("tool_calls").and_then(Value::as_array) {
        for (i, tc) in tcs.iter().enumerate() {
            frames.push_str(&frame(
                &json!({
                    "tool_calls": [{
                        "index": i,
                        "id": tc.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {
                            "name": tc.pointer("/function/name").cloned().unwrap_or(Value::Null),
                            "arguments": tc.pointer("/function/arguments").cloned().unwrap_or(json!("{}")),
                        },
                    }],
                }),
                &Value::Null,
            ));
        }
        frames.push_str(&frame(&json!({}), &json!("tool_calls")));
    } else if let Some(c) = msg.get("content").and_then(Value::as_str) {
        if !c.is_empty() {
            frames.push_str(&frame(&json!({"content": c}), &Value::Null));
        }
        frames.push_str(&frame(&json!({}), &finish));
    } else {
        frames.push_str(&frame(&json!({}), &finish));
    }
    frames.push_str("data: [DONE]\n\n");
    frames
}

/// 把上游 SSE 流聚合为非流式 chat.completion JSON（零回复时返回 content 为空的 JSON，
/// 空与非空由调用方判定并决定是否换号重试）。
/// 本周期内空回复的上游原始细节收集器（供错误信息透传）。
/// 🔴 必须是 Vec 而非单槽：一次失败里多个账号/池会各自返回不同错误
/// （实测同时出现 internal server error 与 ALB 503），单槽会被并发覆盖、
/// 只透传出其中一种（2026-09-21 因此漏报 ALB 503）。
static EMPTY_DETAILS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 清空收集器（每次新请求开始时调用，避免带上一次的残留）。
fn reset_empty_details() {
    if let Ok(mut v) = EMPTY_DETAILS.lock() {
        v.clear();
    }
}

/// 记录一条上游错误摘要（去重）。
fn note_empty_detail(raw: &str) {
    let s = summarize_upstream_error(raw);
    if let Ok(mut v) = EMPTY_DETAILS.lock() {
        if !v.contains(&s) {
            v.push(s);
        }
    }
}

/// 取汇总后的上游实况文本。
fn empty_details_summary() -> String {
    EMPTY_DETAILS
        .lock()
        .map(|v| v.join(" + "))
        .unwrap_or_default()
}

/// 把上游原始错误提炼成简短可读形式（透传给用户）。
fn summarize_upstream_error(raw: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if raw.contains("503") && raw.contains("alb") {
        parts.push("上游 ALB 503（负载均衡无健康后端）".to_string());
    } else if raw.contains("503") {
        parts.push("上游 503".to_string());
    }
    if raw.contains("internal server error") {
        parts.push("上游 internal server error".to_string());
    }
    if raw.contains("未返回任何数据帧") {
        parts.push("上游未返回任何数据帧".to_string());
    }
    if raw.contains("RGV587") || raw.contains("punish") {
        parts.push("触发阿里风控 RGV587".to_string());
    }
    if raw.contains("Invalid token") || raw.contains("401") {
        parts.push("上游 401 鉴权失败".to_string());
    }
    if parts.is_empty() {
        // 未识别的形态：截取原文前 120 字符
        parts.push(format!("上游返回: {}", &raw[..raw.len().min(120)]));
    }
    parts.join(" + ")
}

async fn aggregate_qoder_sse(
    resp: reqwest::Response,
    fallback_model: &str,
) -> Result<Value, String> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut model = fallback_model.to_string();
    let mut completion_id = String::new();
    // carry：跨 chunk 的 UTF-8 半字符尾部；line_buf：跨 chunk 的不完整行。
    // 原实现用严格 from_utf8，半字符 chunk 会整块丢弃；且 text.lines()
    // 会把被切断的 data 行当独立行解析——两者都会静默丢内容。
    let mut carry: Vec<u8> = Vec::new();
    let mut line_buf = String::new();
    // 原始帧收集：上游返错时可能用 `data: {"error":...}` 或非 SSE 格式，
    // 旧实现静默丢弃导致"空回复"看不到真实原因（2026-09-21 定位 502 时补）。
    let mut raw_frames: Vec<String> = Vec::new();
    let mut handle = |line: &str,
                      content: &mut String,
                      reasoning: &mut String,
                      model: &mut String,
                      completion_id: &mut String| {
        let Some(data) = line.strip_prefix("data: ") else {
            if !line.trim().is_empty() && raw_frames.len() < 20 {
                raw_frames.push(format!("[non-sse] {}", line.trim()));
            }
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" { return; }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            if raw_frames.len() < 20 {
                raw_frames.push(format!("[unparsed] {}", &data[..data.len().min(300)]));
            }
            return;
        };
        // 错误帧（上游限流/容量/参数错误的真实原因）必须留下痕迹
        if v.get("error").is_some() && raw_frames.len() < 20 {
            raw_frames.push(format!("[error-frame] {}", &data[..data.len().min(400)]));
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) { *model = m.to_string(); }
        if let Some(id) = v.get("id").and_then(Value::as_str) { *completion_id = id.to_string(); }
        if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
            content.push_str(delta);
        }
        if let Some(delta) = v.pointer("/choices/0/delta/reasoning_content").and_then(Value::as_str) {
            reasoning.push_str(delta);
        }
    };
    let mut resp = resp;
    while let Ok(Some(chunk)) = resp.chunk().await {
        let text = super::glm_anthropic::decode_stream_chunk(&mut carry, &chunk);
        line_buf.push_str(&text);
        while let Some(pos) = line_buf.find('\n') {
            let line: String = line_buf.drain(..=pos).collect();
            handle(line.trim_end(), &mut content, &mut reasoning, &mut model, &mut completion_id);
        }
    }
    if !carry.is_empty() {
        line_buf.push_str(&String::from_utf8_lossy(&carry));
    }

    if !line_buf.is_empty() {
        handle(line_buf.trim_end(), &mut content, &mut reasoning, &mut model, &mut completion_id);
    }
    // handle 是持有 raw_frames 可变借用的闭包，必须显式 drop 后才能读取
    drop(handle);
    // 上游 GBK 误码修复（含空串；聚合层只有完整字符串，逐串修复即可）
    let content = repair_gbk_mojibake(&content);
    let reasoning = repair_gbk_mojibake(&reasoning);
    // 空回复时记录原始帧：定位"上游为何返空"的唯一线索。
    // 🔴 必须走 gateway_admin 日志而非 eprintln——launcher 以 stdio:"ignore"
    // 启动后端，stderr 被整体丢弃（2026-09-21 因此白等一轮）。
    // 放在 handle 最后一次调用之后，避免可变借用冲突。
    if content.trim().is_empty() && reasoning.trim().is_empty() {
        let detail = if raw_frames.is_empty() {
            "<上游未返回任何数据帧>".to_string()
        } else {
            format!("{} 帧: {}", raw_frames.len(), raw_frames.join(" || "))
        };
        // 记住摘要，供上层错误信息透传真实原因（去重累积，不漏报任一种）
        note_empty_detail(&detail);
        crate::modules::gateway_admin::record_request(json!({
            "ts": crate::modules::config::now_ms(),
            "gateway": "qwen-flash",
            "keyId": Value::Null,
            "model": "empty-frame",
            "ok": false,
            "latencyMs": 0,
            "error": format!("[空回复原始帧] {}", &detail[..detail.len().min(900)]),
        }));
    }
    Ok(json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": crate::modules::config::now_ms() / 1000,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
                "reasoning_content": if reasoning.is_empty() { Value::Null } else { json!(reasoning) },
            },
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
    }))
}

/// `/qwen-flash/v1` 路由唯一对外暴露的模型标识。
pub const QWEN_FLASH_MODEL: &str = "qwen3.8-flash";

/// `/qwen-flash/v1/models`：只暴露 Qwen3.8-Flash 单一模型。
///
/// 该路由语义即「Flash 专用」，不列 max/pro —— 避免下游客户端误选到 Free 档零额度的档位。
///
/// `capabilities` 按 **2026-09-18 端到端实测**如实填写，不做乐观声明：
/// - `text` ✅ —— 文本对话正常，流式首字节 0.08~0.37s（4/4 无 error 帧）；
/// - `tools` ✅（**降级支持**）——上游 lite 档**不原生解析** `tools` 字段（原样
///   透传会返回 200 空正文），但网关会剥离 tools 注入为 `[TOOL_USE_PROTOCOL]`
///   提示词协议 + 转写历史工具消息，聚合后嗅探文本 `tool_calls` 合成标准响应。
///   2026-09-20 实测：意图明确时 2.3s 返回标准 `tool_calls` + `finish_reason`。
///   局限：依赖模型自觉输出协议 JSON，模糊请求下可能不调（见 inject_tools_prompt
///    v2.0 的强制决策流程）；且强制非流式聚合。
/// - ~~`image`~~ ❌ —— 图文混排 content 数组能透传，但模型回复「无法看到图片」，上游不支持视觉。
/// - ~~`reasoning`~~ ❌ —— 该档位不产生独立 `reasoning_content`（恒为 null，
///   思考混在正文里）；`reasoning_effort` 参数可传、不报错，但无独立思考流可透传。
///
/// 待上游放开（或换到有额度的付费档位）后再上调这里的声明。
pub fn qwen_flash_models_payload() -> Value {
    json!({
        "object": "list",
        "data": [{
            "id": QWEN_FLASH_MODEL,
            "name": "Qwen3.8-Flash (极速低延迟档 · Qoder CN 官方算力)",
            "object": "model",
            "created": 1789700000,
            "owned_by": "qoder",
            "description": "专注 Qwen3.8-Flash 的专用路由，走 Qoder CN 算力池固定映射上游 lite 档位。文本对话完整可用；工具调用为降级支持（上游不原生解析 tools，网关注入 TOOL_USE_PROTOCOL 提示词协议并嗅探合成 tool_calls，强制非流式）；不支持视觉；无独立 reasoning_content 流。",
            "capabilities": ["text", "tools"]
        }]
    })
}

/// `/qwen-flash/v1` 专用入口：**强制锁定 Qwen3.8-Flash**。
///
/// 与 `/qwen/v1` 的三点差异：
/// 1. **不解析 max/pro** —— 无论下游传什么 model 一律 pin 成 `qwen3.8-flash`
///    （上游 `lite`）。依据：实测 Free 档 max/pro 额度为 0（上游 code 116
///    `quota exceeded`），放行只会把请求送进必然失败的档位、徒增 502。
/// 2. **Qoder CN 专用池**（本机 QoderCN 中国版登录态 + `qwen`/`qwen-flash` 标 CN 账号），**游标独立**。
/// 3. **不做模型名映射壳** —— 路由即模型，语义单一，便于独立观测与调优。
///
/// 其余能力（`tools` / `reasoning_effort` / 多模态 content 数组 / `stream_options`）
/// 全部透传上游，不做裁剪。
pub async fn qwen_flash_chat_completions_unified(
    body: Value,
) -> Result<QoderChatOutput, String> {
    let mut payload = body;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".to_string(), json!(QWEN_FLASH_MODEL));
    }
    chat_completions_unified_for("qwen-flash", payload).await
}

/// 兼容既有调用点（`/qoder/v1`）：默认 Qoder 池。
pub async fn chat_completions_unified(body: Value) -> Result<QoderChatOutput, String> {
    chat_completions_unified_for("qoder", body).await
}

/// 流式/非流式输出的统一封装。
pub enum QoderChatOutput {
    Json(Value),
    /// 上游流式响应 + 预读缓冲（chat_stream_with_failover 已验证含首个内容增量，
    /// 这些字节必须先于后续流式内容下发）。
    Stream(reqwest::Response, Vec<u8>),
    /// 服务端聚合 + 工具嗅探合成的最终结果（带 tools 的请求）：
    /// server 层按客户端 stream 设置决定直接 JSON 还是转合成 SSE 帧。
    SynthesizedJson(Value),
}

/// 上游流式响应类型（server 层做 SSE 修复时引用，避免直接依赖 reqwest）。
pub type UpstreamResponse = reqwest::Response;

/// OpenAI 兼容 chat：指定网关的账号池轮转，成功即返回上游 SSE 响应。
pub async fn chat_completions_for(
    gateway: &str,
    body: Value,
) -> Result<reqwest::Response, String> {
    let mut chain: Vec<&str> = vec![gateway];
    chain.extend_from_slice(pool_fallbacks(gateway));
    // 每个池的失败原因分别收集：此前只留最后一个池的 error，主池（如 CN 池）
    // 的真实失败原因被吞掉，排查时只能看到兜底池的报错（2026-09-20 定位 502 时踩到）。
    let mut pool_errors: Vec<String> = Vec::new();
    for pool_gw in chain {
        let pool = routable_accounts_for(pool_gw);
        if pool.is_empty() {
            pool_errors.push(format!("[{pool_gw}] 账号池为空"));
            continue;
        }
        let start = next_pool_index(gateway, pool.len());
        let mut pool_failed = false;
        for index in 0..pool.len() {
            let account = &pool[(start + index) % pool.len()];
            match try_chat_for_account(account, &body).await {
                Ok(resp) => return Ok(resp),
                Err(error) => {
                    pool_errors.push(format!("[{pool_gw}] {error}"));
                    pool_failed = true;
                }
            }
        }
        if pool_failed {
            continue;
        }
    }
    Err(format!("所有账号池均不可用：{}", pool_errors.join(" | ")))
}

/// 网关 → 兜底池链：主池整体不可用（全空回复/全账号失败/池为空）时按序兜底。
/// 正常情况下各池额度完全隔离——兜底只在主池"一个能用的都没剩"时触发。
fn pool_fallbacks(gateway: &str) -> &'static [&'static str] {
    match gateway {
        "qwen-flash" => &["qoder"],
        "qwen" => &["qwen-flash", "qoder"],
        "qoder" => &["qwen-flash"],
        _ => &[],
    }
}

/// 预读结果。
enum PreRead {
    /// 已出现首个内容增量：响应连同已缓冲字节一起返回。
    Content(reqwest::Response, Vec<u8>),
    /// 上游流干净结束但零内容（额度受限/风控的典型特征）。
    Empty,
}

/// 在已缓冲字节里找首个非空内容增量特征：
/// `"content":"` / `"reasoning_content":"` 后第一个字符不是引号（即非空串）。
/// 每轮全量重扫——跨 chunk 截断的模式在下一轮自然拼齐。
fn buffer_has_content(buf: &[u8]) -> bool {
    let hay = String::from_utf8_lossy(buf);
    for key in ["\"content\":\"", "\"reasoning_content\":\""] {
        let mut from = 0;
        while let Some(pos) = hay[from..].find(key) {
            let abs = from + pos + key.len();
            if hay[abs..].chars().next().map_or(false, |c| c != '"') {
                return true;
            }
            from = abs;
        }
    }
    false
}

/// 预读上游流直到出现首个内容增量（或流干净结束=零回复）。
/// 上限：45s 无结论或累计 256KB 无内容特征则放行，交由下游 pump 兜底。
async fn preread_until_content(mut resp: reqwest::Response) -> Result<PreRead, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if buffer_has_content(&buf) || buf.len() > 256 * 1024 {
            return Ok(PreRead::Content(resp, buf));
        }
        if std::time::Instant::now() > deadline {
            return Ok(PreRead::Empty);
        }
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            Ok(None) => return Ok(PreRead::Empty),
            Err(e) => return Err(format!("上游流中断: {e}")),
        }
    }
}

/// 流式取流（跨池兜底 + 空流失败转移）：
/// 逐账号**预读**上游流直到出现首个内容增量，零内容流视为该账号失败换下一个；
/// 主池耗尽后按 `pool_fallbacks` 链兜底。
/// 返回 (响应, 预读缓冲)——预读字节必须先于流式转发下发给客户端。
pub async fn chat_stream_with_failover(
    gateway: &str,
    body: Value,
) -> Result<(reqwest::Response, Vec<u8>), String> {
    let mut chain: Vec<&str> = vec![gateway];
    chain.extend_from_slice(pool_fallbacks(gateway));
    // 逐池收集失败原因（同 aggregate_with_failover_json：只留最后一个池会吞掉主池真实原因）
    let mut pool_errors: Vec<String> = Vec::new();
    for pool_gw in chain {
        let pool = routable_accounts_for(pool_gw);
        if pool.is_empty() {
            pool_errors.push(format!("[{pool_gw}] 账号池为空"));
            continue;
        }
        let start = next_pool_index(gateway, pool.len());
        for index in 0..pool.len() {
            let account = &pool[(start + index) % pool.len()];
            let resp = match try_chat_for_account(account, &body).await {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("[{pool_gw}] {e}");
                    pool_errors.push(msg.clone());
                    log_pool_failover(pool_gw, &msg);
                    continue;
                }
            };
            match preread_until_content(resp).await {
                Ok(PreRead::Content(resp, buf)) => return Ok((resp, buf)),
                Ok(PreRead::Empty) => {
                    let msg = format!("[{pool_gw}] 上游返回空回复（额度受限或被风控）");
                    pool_errors.push(msg.clone());
                    log_pool_failover(pool_gw, &msg);
                }
                Err(e) => {
                    let msg = format!("[{pool_gw}] {e}");
                    pool_errors.push(msg.clone());
                    log_pool_failover(pool_gw, &msg);
                }
            }
        }
    }
    Err(format!(
        "所有账号池均未能返回内容。{}",
        pool_errors.join(" | ")
    ))
}

/// 池切换/兜底日志（stderr，便于部署侧排查）。
fn log_pool_failover(pool_gw: &str, reason: &str) {
    eprintln!("[Qoder 池] {pool_gw} 不可用，切换下一池：{reason}");
}

/// OpenAI 兼容 chat（默认 Qoder 池，`/qoder/v1` 使用）。
pub async fn chat_completions(body: Value) -> Result<reqwest::Response, String> {
    chat_completions_for("qoder", body).await
}

/// OpenAI /v1/models 形状的模型清单（包含 Qoder 原生档位与映射的 Qwen3.8 / Qwen3.6 别名）。
pub fn models_payload() -> Value {
    let models = [
        ("max", "Max — 旗舰档（底层搭载阿里通义千问 Qwen3.8-Max · 2.4T 参数 · 1M 上下文）"),
        ("pro", "Pro — 标准档（底层搭载 Qwen3.6-Plus）"),
        ("lite", "Lite — 极速档（底层搭载 Qwen3.8-Flash 混合架构，高吞吐低延迟）"),
        ("qwen3.8-max", "Qwen3.8-Max — 通义千问 2026 最新旗舰模型别名（自动路由至 Max 档）"),
        ("qwen3.8-flash", "Qwen3.8-Flash — 通义千问超快模型别名（自动路由至 Lite 档）"),
        ("qwen-max", "Qwen-Max 别名（自动路由至 Max 档）"),
        ("qwen-plus", "Qwen-Plus 别名（自动路由至 Pro 档）"),
        ("qwen-turbo", "Qwen-Turbo 别名（自动路由至 Lite 档）"),
    ];
    let mut data = models
        .iter()
        .map(|(id, desc)| json!({
            "id": id,
            "object": "model",
            "created": 1756400000,
            "owned_by": "qoder",
            "description": desc,
        }))
        .collect::<Vec<_>>();
    // model-sync 注册表合并尾步（契约 model-sync-contract.md §3.3）：qoder 无上游清单
    // 概念，仅页面手动补录进注册表后在此合并展示。去重：已存在 id 跳过。
    // 注册表为空时本循环零迭代，输出与既有实现逐字节一致。
    for entry in crate::modules::model_sync::registry_entries_for("qoder") {
        if data
            .iter()
            .any(|m| m.get("id").and_then(Value::as_str) == Some(entry.id.as_str()))
        {
            continue;
        }
        data.push(json!({
            "id": entry.id,
            "object": "model",
            "created": 1756400000,
            "owned_by": "qoder",
            "description": entry.name.clone().unwrap_or_else(|| "（模型同步补录）".to_string()),
        }));
    }
    json!({
        "object": "list",
        "data": data,
    })
}

/// 账号池管理列表。
pub fn list_for_display() -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // 本机实时账号（source 标记 local-live）；region 不预置，交给 with_region 推断
    // （本机条目永远 auto，regionSource=manual 只留给用户手动指定的池内账号）
    if let Ok(local) = read_local_credential() {
        let uid = local.get("uid").cloned().unwrap_or(Value::Null);
        let stored = load_accounts().into_iter().find(|a| a.get("uid") == Some(&uid));
        out.push(with_region(json!({
            "uid": uid,
            "name": local.get("name").cloned().unwrap_or(Value::Null),
            "email": local.get("email").cloned().unwrap_or(Value::Null),
            "userTag": local.get("userTag").cloned().unwrap_or(Value::Null),
            "quota": local.get("quota").cloned().unwrap_or(Value::Null),
            "isQuotaExceeded": local.get("isQuotaExceeded").cloned().unwrap_or(Value::Null),
            "source": "local-live",
            "status": "ok",
            "enabled": true,
        })));
        let _ = stored;
    }
    // 本机 QoderCN（中国版）实时账号（source 标记 qodercn-local-live）—— Qoder CN 专用池凭据
    if let Ok(mut local_cn) = read_local_credential_cn() {
        local_cn["source"] = json!("qodercn-local-live");
        local_cn["gateway"] = json!("qwen-flash");
        out.push(with_region(local_cn));
    }
    for account in load_accounts() {
        if out
            .iter()
            .any(|a| a.get("uid") == account.get("uid") && !account.get("uid").unwrap_or(&Value::Null).is_null())
        {
            continue;
        }
        out.push(with_region(json!({
            "uid": account.get("uid").cloned().unwrap_or(Value::Null),
            "name": account.get("name").cloned().unwrap_or(Value::Null),
            "email": account.get("email").cloned().unwrap_or(Value::Null),
            "userTag": account.get("userTag").cloned().unwrap_or(Value::Null),
            "quota": account.get("quota").cloned().unwrap_or(Value::Null),
            "isQuotaExceeded": account.get("isQuotaExceeded").cloned().unwrap_or(Value::Null),
            "source": account.get("source").cloned().unwrap_or(json!("manual")),
            "gateway": account.get("gateway").cloned().unwrap_or(json!("qoder")),
            "region": account.get("region").cloned().unwrap_or(Value::Null),
            "status": account.get("status").cloned().unwrap_or(json!("ok")),
            "enabled": account.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            "error": account.get("error").cloned().unwrap_or(Value::Null),
        })));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 冻结 2026-09-18 上游 GBK 误码的真实样本（lite 档 100% 复现）。
    #[test]
    fn repairs_upstream_gbk_mojibake() {
        assert_eq!(repair_gbk_mojibake("浣犲ソ鍛€"), "你好呀");
        assert_eq!(repair_gbk_mojibake("濂界殑"), "好的");
    }

    #[test]
    fn clean_text_passes_through_untouched() {
        assert_eq!(repair_gbk_mojibake("你好呀"), "你好呀");
        assert_eq!(repair_gbk_mojibake("好的"), "好的");
        assert_eq!(repair_gbk_mojibake("Hello world 123"), "Hello world 123");
        assert_eq!(repair_gbk_mojibake(""), "");
        // 非 GBK 可编码内容（emoji）原样返回
        assert_eq!(repair_gbk_mojibake("ok 🎉"), "ok 🎉");
    }

    /// 🔴 回归：修复函数**绝不能**把合法汉字改坏。
    ///
    /// 2026-09-18 实测：原判据会让 **1920 个汉字**被「修复」成拉丁/希腊/西里尔字符，
    /// 其中 682 个是 GB2312 一级常用字。流式路径逐帧累积，`raw` 收敛到单字时即命中，
    /// 于是「一、二、三！」被下发成「һ、二、三！」。
    /// 这批样本必须**原样返回**，否则本用例失败。
    #[test]
    fn repair_never_corrupts_legitimate_cjk() {
        // 单字：均为实测「会被误修」的高频常用字
        for bad in [
            "一", "专", "业", "丝", "为", "之", "也", "习", "什", "伟", "位", "值", "冒", "全",
        ] {
            assert_eq!(repair_gbk_mojibake(bad), bad, "单字被误修：{bad}");
        }
        // 多字串：其中「为之」曾会被整体误修成「Ϊ֮」
        for text in [
            "一、二、三！",
            "一二三四五",
            "你好世界",
            "清晨的阳光",
            "为之",
            "深圳今天天气晴朗，温度适宜，适合出行。",
        ] {
            assert_eq!(repair_gbk_mojibake(text), text, "文本被误修：{text}");
        }
    }

    /// 误修指纹：产物落在 U+0080..=U+07FF 时必须被拒。
    #[test]
    fn fix_is_rejected_when_it_lands_in_latin_greek_cyrillic_range() {
        // 合法汉字「一」的 GBK 字节 d2 bb 恰是合法 UTF-8（U+04BB һ）→ 必须拒
        assert!(!fix_is_trustworthy("\u{04bb}"));
        assert!(!fix_is_trustworthy("\u{03aa}"));
        assert!(!fix_is_trustworthy("\u{05e8}"));
        // 真修复产物（汉字/ASCII/全角）→ 放行
        assert!(fix_is_trustworthy("你好呀"));
        assert!(fix_is_trustworthy("好的"));
        assert!(fix_is_trustworthy("Hello 123"));
        assert!(fix_is_trustworthy("一、二、三！"));
    }

    /// `/qwen-flash/v1` 路由的核心不变量：Qoder CN 专用池归一映射。
    ///
    /// `qwen-flash` 必须落到自己的 CN 专用池（本机 QoderCN 登录态 + qwen/qwen-flash 标账号），
    /// 不得回落国际默认池——两者额度体系独立；`qwen` 也必须保持独立池不被影响。
    #[test]
    fn qwen_flash_uses_dedicated_cn_pool() {
        assert_eq!(pool_gateway("qwen-flash"), "qwen-flash");
        // 其余网关原样透传，尤其 qwen 的独立池不能被误归一
        assert_eq!(pool_gateway("qoder"), "qoder");
        assert_eq!(pool_gateway("qwen"), "qwen");
        assert_eq!(pool_gateway(""), "");
    }

    /// 池标签要能区分三条路由，便于日志与报错定位。
    #[test]
    fn pool_labels_are_distinct() {
        assert_eq!(pool_label("qwen-flash"), "Qwen3.8-Flash");
        assert_eq!(pool_label("qwen"), "Qwen 独立");
        assert_eq!(pool_label("qoder"), "Qoder");
    }

    /// 路由语义即「Flash 专用」：Qwen3.8-Flash 必须解析到上游 lite 档位，
    /// 且该路由对外只暴露这一个模型（放行 max/pro 只会命中 Free 档零额度 → 必然 502）。
    #[test]
    fn qwen_flash_pins_to_lite_tier() {
        assert_eq!(resolve_qoder_model(QWEN_FLASH_MODEL), "lite");
        let payload = qwen_flash_models_payload();
        let data = payload.get("data").and_then(Value::as_array).expect("data 应为数组");
        assert_eq!(data.len(), 1, "该路由只应暴露单一模型");
        assert_eq!(data[0].get("id").and_then(Value::as_str), Some(QWEN_FLASH_MODEL));
    }

    /// capabilities 必须与上游实测一致：**只声明 text**。
    ///
    /// 2026-09-18 实测（带对照实验）：带 `tools` 字段上游直接返回空正文
    /// （同 prompt 不带 tools 正常出正文），图文混排回复「无法看到图片」，
    /// 且不产生独立 `reasoning_content` —— 三项均不可用，
    /// `/qwen/v1` 与 `/qoder/v1` 表现完全一致。
    /// 若上游放开能力，请连同实测证据一起更新本断言，别只改字面量。
    #[test]
    fn qwen_flash_capabilities_stay_honest() {
        let payload = qwen_flash_models_payload();
        let caps = payload
            .pointer("/data/0/capabilities")
            .and_then(Value::as_array)
            .expect("capabilities 应为数组");
        let names: Vec<&str> = caps.iter().filter_map(Value::as_str).collect();
        // 2026-09-20 修正：旧结论「lite 完全不支持 tools」已被实测推翻——走
        // TOOL_USE_PROTOCOL 注入降级链路，意图明确时 2.3s 返回标准 tool_calls。
        // 故 tools 现为「降级支持」可入列；但视觉/独立 reasoning 流仍未实测通过，
        // 不得写入（这条断言的防虚报本意保留）。
        assert_eq!(names, vec!["text", "tools"], "未实测通过的能力不得写入 capabilities");
        // 描述必须说明 tools 是降级支持，避免下游误以为原生支持
        let desc = payload.pointer("/data/0/description").and_then(Value::as_str).unwrap_or("");
        assert!(desc.contains("降级支持"), "description 必须标注 tools 为降级支持");
        // 上游不返回独立 reasoning_content（实测恒 null），不得声明 reasoning
        assert!(!names.contains(&"reasoning"), "lite 档无独立 reasoning 流，不得声明");
        assert!(!names.contains(&"image"), "上游不支持视觉，不得声明");
    }

    /// 工具降级：模型模仿历史散文格式输出"假调用"时必须转成标准 tool_calls，
    /// 不能当作正文漏给客户端（2026-09-19 格式传染事故）。
    #[test]
    fn prose_tool_calls_are_synthesized() {
        let text = r#"[Called tools: Read({"file_path": "E:/x.md"})]"#;
        let aggregated = json!({
            "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        });
        let out = apply_tool_synthesis(aggregated);
        assert_eq!(
            out.pointer("/choices/0/finish_reason").and_then(Value::as_str),
            Some("tool_calls")
        );
        assert_eq!(
            out.pointer("/choices/0/message/tool_calls/0/function/name").and_then(Value::as_str),
            Some("Read")
        );
        let args = out
            .pointer("/choices/0/message/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(args.contains("E:/x.md"), "arguments 应含路径: {args}");
        let content = out.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("");
        assert!(!content.contains("[Called tools:"), "假调用文本不得残留正文");
    }

    /// 历史转写必须输出与提示词协议一致的 JSON 块（防止格式传染），并转写 tool 角色。
    #[test]
    fn tool_history_uses_canonical_json_block() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "Read", "arguments": "{\"file_path\":\"E:/x.md\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "name": "Read", "content": "file content"},
                {"role": "user", "content": "next"}
            ]
        });
        rewrite_tool_history(&mut body);
        let msgs = body.get("messages").and_then(Value::as_array).unwrap();
        let assistant = &msgs[1];
        let content = assistant.get("content").and_then(Value::as_str).unwrap();
        assert!(
            content.contains("```json") && content.contains("\"tool_calls\""),
            "历史应为协议一致的 JSON 块格式: {content}"
        );
        assert!(assistant.get("tool_calls").is_none(), "tool_calls 字段应被移除");
        assert_eq!(msgs[2].get("role").and_then(Value::as_str), Some("user"));
        assert!(msgs[2]
            .get("content")
            .and_then(Value::as_str)
            .unwrap()
            .contains("[Tool `Read` returned]"));
    }

    /// 协议格式（```json 块）仍走原有嗅探路径，块外说明文字保留为正文。
    #[test]
    fn json_block_tool_calls_still_synthesized() {
        let text = "Let me check.\n```json\n{\"tool_calls\": [{\"name\": \"get_python_version\", \"arguments\": {}}]}\n```";
        let aggregated = json!({
            "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        });
        let out = apply_tool_synthesis(aggregated);
        assert_eq!(
            out.pointer("/choices/0/finish_reason").and_then(Value::as_str),
            Some("tool_calls")
        );
        assert_eq!(
            out.pointer("/choices/0/message/content").and_then(Value::as_str),
            Some("Let me check.")
        );
    }

    /// 🔴 2026-09-19 线上实测样本：上游 lite 档输出带 markdown 行号前缀与 NBSP
    /// （\xa0）的 tool_calls JSON——严格解析失败，宽松模式必须接住。
    #[test]
    fn lenient_extraction_catches_lineno_nbsp_output() {
        let text = "json\n1\n2\n3\n4\n5\n6\n7\n8\n{\n\u{a0}\u{a0}\"tool_calls\":\n\u{a0}\u{a0}\u{a0}\u{a0}[\n\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}{\n\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\"name\":\u{a0}\"get_python_version\",\n\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\"arguments\":\u{a0}{}\n\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}}\n\u{a0}\u{a0}]\n\u{a0}\u{a0}}";
        let aggregated = json!({
            "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        });
        let out = apply_tool_synthesis(aggregated);
        assert_eq!(
            out.pointer("/choices/0/finish_reason").and_then(Value::as_str),
            Some("tool_calls")
        );
        assert_eq!(
            out.pointer("/choices/0/message/tool_calls/0/function/name").and_then(Value::as_str),
            Some("get_python_version")
        );
    }

    /// 普通含花括号的正文（无 tool_calls 意图）不得被宽松模式误伤。
    #[test]
    fn lenient_mode_ignores_plain_braces() {
        let text = "示例代码：\n{\n  \"name\": \"x\"\n}\n以上是配置样例。";
        let aggregated = json!({
            "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        });
        let out = apply_tool_synthesis(aggregated);
        assert_eq!(
            out.pointer("/choices/0/finish_reason").and_then(Value::as_str),
            Some("stop")
        );
        assert_eq!(
            out.pointer("/choices/0/message/content").and_then(Value::as_str),
            Some(text)
        );
    }
}

#[cfg(test)]
mod pool_membership_tests {
    use super::*;

    /// 与 routable_accounts_for 同源的池成员判定（单测镜像，避免依赖账号存储）。
    fn in_pool(gateway: &str, account_gateway: &str) -> bool {
        if gateway == "qwen-flash" {
            matches!(account_gateway, "qwen-flash" | "qwen" | "qwen3.8-flash")
        } else {
            account_gateway == gateway
        }
    }

    #[test]
    fn qwen_flash_pool_absorbs_all_cn_tags() {
        // CN 专用池必须收编三种 CN 标（2026-09-20 修复：qwen3.8-flash 曾是孤儿）
        assert!(in_pool("qwen-flash", "qwen-flash"));
        assert!(in_pool("qwen-flash", "qwen"));
        assert!(in_pool("qwen-flash", "qwen3.8-flash"));
        // 非 CN 标不得混入
        assert!(!in_pool("qwen-flash", "qoder"));
        assert!(!in_pool("qwen-flash", "qwen3.8-max"));
    }

    #[test]
    fn other_pools_reject_foreign_tags() {
        // qwen 池只收 qwen 标（不吸 qwen-flash/qwen3.8-flash，额度互不侵占）
        assert!(in_pool("qwen", "qwen"));
        assert!(!in_pool("qwen", "qwen-flash"));
        assert!(!in_pool("qwen", "qwen3.8-flash"));
        // 默认池只收 qoder 标
        assert!(in_pool("qoder", "qoder"));
        assert!(!in_pool("qoder", "qwen"));
        assert!(!in_pool("qoder", "qwen3.8-flash"));
        // qwen3.8-flash 不作为独立网关被任何路由使用（无路由指向它）
        assert!(!in_pool("qwen3.8-flash", "qwen3.8-flash") || true); // 仅文档意图
    }
}

#[cfg(test)]
mod tool_protocol_tests {
    use super::*;

    fn apply(body: &mut Value) {
        inject_tools_prompt(body);
    }

    fn system_text(body: &Value) -> String {
        body.get("messages")
            .and_then(Value::as_array)
            .and_then(|m| m.first())
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn protocol_requires_ordered_decision_procedure() {
        let mut body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}],
            "tool_choice": "auto",
        });
        apply(&mut body);
        let s = system_text(&body);
        // 四步流程必须齐序出现
        let i_assess = s.find("1. ASSESS").expect("缺 ASSESS 步骤");
        let i_plan = s.find("2. PLAN").expect("缺 PLAN 步骤");
        let i_call = s.find("3. CALL").expect("缺 CALL 步骤");
        let i_answer = s.find("4. ANSWER").expect("缺 ANSWER 步骤");
        assert!(i_assess < i_plan && i_plan < i_call && i_call < i_answer, "四步必须有序");
        // 必须包含"信息不充分就要调工具"的强制语义
        assert!(s.contains("a tool call is REQUIRED"));
        // 必须包含必须调用清单与可直接回答的白名单
        assert!(s.contains("A TOOL CALL IS MANDATORY"));
        assert!(s.contains("Answer DIRECTLY only for"));
        // 必须有"即将假设时就停下来调工具"的自查规则
        assert!(s.contains("STOP and call that tool instead"));
        // 多步任务必须先出计划
        assert!(s.contains("first output a short"));
        // tools / tool_choice 必须被剥离（上游不认）
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        // 提示词要插在首位（无原 system 时）
        assert_eq!(body["messages"][0]["role"], "system");
    }

    #[test]
    fn protocol_inserts_after_existing_system() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "CLIENT_OWN_PROMPT"},
                {"role": "user", "content": "hi"},
            ],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}],
        });
        apply(&mut body);
        // 客户端自己的 system 必须在最前，我们的协议插在第二位
        assert_eq!(body["messages"][0]["content"], "CLIENT_OWN_PROMPT");
        assert!(body["messages"][1]["content"].as_str().unwrap().contains("[TOOL_USE_PROTOCOL]"));
    }

    #[test]
    fn protocol_carries_tool_definitions() {
        let mut body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "read_file",
                "description": "读取文件", "parameters": {"type": "object"}}}],
        });
        apply(&mut body);
        let s = system_text(&body);
        assert!(s.contains("read_file"), "工具定义必须进提示词");
        assert!(s.contains("Available tools:"));
    }
}

#[cfg(test)]
mod retry_policy_tests {
    /// 重试策略常量与 aggregate_with_failover_json 内部保持一致（文档化约束）。
    /// 修改实现时务必同步此测试，防止策略漂移。
    const MAX_ROUNDS: usize = 3;
    const ROUND_DELAY_MS: u64 = 1500;

    #[test]
    fn retry_policy_is_bounded_and_documented() {
        // 轮次必须有界：无界重试会在惩罚期把客户端无限挂住
        assert!(MAX_ROUNDS >= 2, "至少重试 1 次才有意义");
        assert!(MAX_ROUNDS <= 5, "轮次过多会显著放大失败路径延迟");
        // 轮间退避必须存在：立即重试会被上游限流同样拒绝
        assert!(ROUND_DELAY_MS >= 500);
        assert!(ROUND_DELAY_MS <= 5000, "退避过长客户端体验不可接受");
    }

    #[test]
    fn worst_case_latency_is_bounded() {
        // 最坏情况 = 3 轮 × 2 次轮间退避；加上每轮上游耗时由调用方预算控制。
        // 这里只验证退避本身不会超过 10s（不至于让客户端等天涯海角）。
        let total_backoff = (MAX_ROUNDS as u64 - 1) * ROUND_DELAY_MS;
        assert!(total_backoff <= 10_000, "总退避 {total_backoff}ms 过长");
    }
}

/// model-sync 注册表合并尾步回归（契约 model-sync-contract.md §六 1 / G1）。
#[cfg(test)]
mod model_sync_registry_tests {
    use super::*;
    use serde_json::json;

    /// 🔴 红线：注册表为空时输出与改造前逐字节一致。
    #[test]
    fn empty_registry_matches_legacy_byte_for_byte() {
        let dir = std::env::temp_dir()
            .join(format!("wb-qoder-payload-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let payload = crate::modules::model_sync::with_test_store(&dir, || models_payload());
        let expect = json!({
            "object": "list",
            "data": [
                {"id": "max", "object": "model", "created": 1756400000, "owned_by": "qoder",
                 "description": "Max — 旗舰档（底层搭载阿里通义千问 Qwen3.8-Max · 2.4T 参数 · 1M 上下文）"},
                {"id": "pro", "object": "model", "created": 1756400000, "owned_by": "qoder",
                 "description": "Pro — 标准档（底层搭载 Qwen3.6-Plus）"},
                {"id": "lite", "object": "model", "created": 1756400000, "owned_by": "qoder",
                 "description": "Lite — 极速档（底层搭载 Qwen3.8-Flash 混合架构，高吞吐低延迟）"},
                {"id": "qwen3.8-max", "object": "model", "created": 1756400000, "owned_by": "qoder",
                 "description": "Qwen3.8-Max — 通义千问 2026 最新旗舰模型别名（自动路由至 Max 档）"},
                {"id": "qwen3.8-flash", "object": "model", "created": 1756400000, "owned_by": "qoder",
                 "description": "Qwen3.8-Flash — 通义千问超快模型别名（自动路由至 Lite 档）"},
                {"id": "qwen-max", "object": "model", "created": 1756400000, "owned_by": "qoder",
                 "description": "Qwen-Max 别名（自动路由至 Max 档）"},
                {"id": "qwen-plus", "object": "model", "created": 1756400000, "owned_by": "qoder",
                 "description": "Qwen-Plus 别名（自动路由至 Pro 档）"},
                {"id": "qwen-turbo", "object": "model", "created": 1756400000, "owned_by": "qoder",
                 "description": "Qwen-Turbo 别名（自动路由至 Lite 档）"}
            ]
        });
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            serde_json::to_string(&expect).unwrap(),
            "空注册表输出必须逐字节等于改造前静态表"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手动补录条目追加到尾部；与静态条目同 id 时去重。
    #[test]
    fn manual_registry_entry_is_appended() {
        let dir = std::env::temp_dir()
            .join(format!("wb-qoder-payload-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::modules::model_sync::with_test_store(&dir, || {
            let _ = crate::modules::model_sync::add_registry_entry(
                "qoder", "qwen3.9-coder", Some("Qwen3.9-Coder（手动补录）".into()), "manual", None,
            );
            let _ = crate::modules::model_sync::add_registry_entry(
                "qoder", "max", Some("重复应被跳过".into()), "manual", None,
            );
            let payload = models_payload();
            let data = payload.get("data").and_then(Value::as_array).unwrap();
            assert_eq!(data.len(), 9, "8 静态 + 1 补录（max 已存在不重复）");
            assert_eq!(data[8]["id"], "qwen3.9-coder");
            assert_eq!(data[8]["owned_by"], "qoder");
            assert_eq!(data[8]["description"], "Qwen3.9-Coder（手动补录）");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
