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
//! 账号池落盘 `~/.wb-switch/qoder_accounts.json`，按 uid 去重。

use base64::engine::general_purpose;
use base64::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};

const OPENAPI_HOST: &str = "https://openapi.qoder.sh";
const CHAT_HOST: &str = "https://api2-v2.qoder.sh";
/// device flow 的 client_id（qoderclicn@1.1.16 混淆常量解出）。
const DEVICE_CLIENT_ID: &str = "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb";
/// 轮询间隔 / 总超时（与客户端常量一致）。
const POLL_INTERVAL_MS: u64 = 1000;
const POLL_TIMEOUT_MS: u64 = 300_000;

static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder().build().expect("failed to build qoder client"))
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
        for field in ["accessToken", "refreshToken", "expireTime", "email", "name", "userTag", "plan"] {
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

/// 解密本机 Qoder IDE 的登录态（dt-/drt- token + 用户信息）。
pub fn read_local_credential() -> Result<Value, String> {
    use crate::modules::vscode_cn_inject::{decrypt_windows_gcm_v10, dpapi_decrypt};

    let user_dir = qoder_user_data_dir()
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
pub fn import_manual(uid: &str, token: &str, refresh_token: &str) -> Result<Value, String> {
    if !token.starts_with("dt-") {
        return Err("access token 应为 dt- 前缀（device token）".to_string());
    }
    let entry = json!({
        "uid": if uid.is_empty() { Value::Null } else { json!(uid) },
        "accessToken": token,
        "refreshToken": if refresh_token.is_empty() { Value::Null } else { json!(refresh_token) },
        "source": "manual",
    });
    upsert_account(entry)
}

/// 导入本机 Qoder 登录态到账号池（幂等）。
pub fn import_local() -> Result<Value, String> {
    let credential = read_local_credential()?;
    upsert_account(credential)
}

// ---------------------------------------------------------------------------
// PKCE device flow（添加账号：浏览器授权 → 轮询凭据）
// ---------------------------------------------------------------------------

/// 生成 device flow 会话：授权 URL 给用户打开，nonce 留给轮询。
pub fn device_flow_start() -> Result<Value, String> {
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

/// 参与路由的账号（enabled != false 且有 token），本机登录账号优先。
fn routable_accounts() -> Vec<Value> {
    let mut pool: Vec<Value> = Vec::new();
    // 本机实时登录态优先（最新鲜）
    if let Ok(local) = read_local_credential() {
        pool.push(local);
    }
    for account in load_accounts() {
        if account.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if account.get("status").and_then(Value::as_str) == Some("invalid") {
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

static POOL_CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
        http_client()
            .post(format!("{CHAT_HOST}/model/v1/chat/completions"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
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

/// OpenAI 兼容 chat 统一入口：非流式请求在服务端聚合上游 SSE，
/// 返回标准 `chat.completion` JSON；流式请求透传上游 SSE。
pub async fn chat_completions_unified(body: Value) -> Result<QoderChatOutput, String> {
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if want_stream {
        let resp = chat_completions(body).await?;
        return Ok(QoderChatOutput::Stream(resp));
    }
    // 非流式：强制上游流式，聚合为完整 JSON
    let mut payload = body.clone();
    payload["stream"] = json!(true);
    payload["stream_options"] = json!({"include_usage": true});
    let mut resp = chat_completions(payload).await?;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut model = body.get("model").and_then(Value::as_str).unwrap_or("lite").to_string();
    let mut completion_id = String::new();
    // carry：跨 chunk 的 UTF-8 半字符尾部；line_buf：跨 chunk 的不完整行。
    // 原实现用严格 from_utf8，半字符 chunk 会整块丢弃；且 text.lines()
    // 会把被切断的 data 行当独立行解析——两者都会静默丢内容。
    let mut carry: Vec<u8> = Vec::new();
    let mut line_buf = String::new();
    let mut handle = |line: &str,
                      content: &mut String,
                      reasoning: &mut String,
                      model: &mut String,
                      completion_id: &mut String| {
        let Some(data) = line.strip_prefix("data: ") else { return };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" { return; }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        if let Some(m) = v.get("model").and_then(Value::as_str) { *model = m.to_string(); }
        if let Some(id) = v.get("id").and_then(Value::as_str) { *completion_id = id.to_string(); }
        if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
            content.push_str(delta);
        }
        if let Some(delta) = v.pointer("/choices/0/delta/reasoning_content").and_then(Value::as_str) {
            reasoning.push_str(delta);
        }
    };
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
    Ok(QoderChatOutput::Json(json!({
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
    })))
}

/// 流式/非流式输出的统一封装。
pub enum QoderChatOutput {
    Json(Value),
    Stream(reqwest::Response),
}

/// OpenAI 兼容 chat：账号池轮转，成功即返回上游 SSE 响应。
pub async fn chat_completions(body: Value) -> Result<reqwest::Response, String> {
    let pool = routable_accounts();
    if pool.is_empty() {
        return Err("Qoder 账号池为空：请导入本机登录态或添加账号".to_string());
    }
    let start = POOL_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % pool.len();
    let mut last_error = String::new();
    for index in 0..pool.len() {
        let account = &pool[(start + index) % pool.len()];
        match try_chat_for_account(account, &body).await {
            Ok(resp) => return Ok(resp),
            Err(error) => last_error = error,
        }
    }
    Err(format!("全部 {} 个 Qoder 账号均不可用：{}", pool.len(), last_error))
}

/// OpenAI /v1/models 形状的模型清单（Qoder 已知档位）。
pub fn models_payload() -> Value {
    let models = [
        ("lite", "Lite — 默认快速档"),
        ("pro", "Pro — 标准档"),
        ("max", "Max — 最强档"),
    ];
    json!({
        "object": "list",
        "data": models
            .iter()
            .map(|(id, desc)| json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "qoder",
                "description": desc,
            }))
            .collect::<Vec<_>>(),
    })
}

/// 账号池管理列表。
pub fn list_for_display() -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // 本机实时账号（source 标记 local-live）
    if let Ok(local) = read_local_credential() {
        let uid = local.get("uid").cloned().unwrap_or(Value::Null);
        let stored = load_accounts().into_iter().find(|a| a.get("uid") == Some(&uid));
        out.push(json!({
            "uid": uid,
            "name": local.get("name").cloned().unwrap_or(Value::Null),
            "email": local.get("email").cloned().unwrap_or(Value::Null),
            "userTag": local.get("userTag").cloned().unwrap_or(Value::Null),
            "quota": local.get("quota").cloned().unwrap_or(Value::Null),
            "isQuotaExceeded": local.get("isQuotaExceeded").cloned().unwrap_or(Value::Null),
            "source": "local-live",
            "status": "ok",
            "enabled": true,
        }));
        let _ = stored;
    }
    for account in load_accounts() {
        if out
            .iter()
            .any(|a| a.get("uid") == account.get("uid") && !account.get("uid").unwrap_or(&Value::Null).is_null())
        {
            continue;
        }
        out.push(json!({
            "uid": account.get("uid").cloned().unwrap_or(Value::Null),
            "name": account.get("name").cloned().unwrap_or(Value::Null),
            "email": account.get("email").cloned().unwrap_or(Value::Null),
            "userTag": account.get("userTag").cloned().unwrap_or(Value::Null),
            "quota": account.get("quota").cloned().unwrap_or(Value::Null),
            "isQuotaExceeded": account.get("isQuotaExceeded").cloned().unwrap_or(Value::Null),
            "source": account.get("source").cloned().unwrap_or(json!("manual")),
            "status": account.get("status").cloned().unwrap_or(json!("ok")),
            "enabled": account.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            "error": account.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
    out
}
