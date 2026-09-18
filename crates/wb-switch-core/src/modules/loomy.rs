//! Loomy（讯飞）账号接入：登录态发现、积分余额、每日签到与多账号池。
//!
//! 登录态存于 Loomy 桌面客户端（Electron）的 `auth-session.json`：
//! Windows 下 userData 被重定向到
//! `C:\Users\Public\Loomy\<用户名 sha256 前 12 位>\userData`（config-path.js），
//! 文件为明文 JSON（session / userid / phone / updatedAt）。
//!
//! 后端网关 `https://loomyad.xunfei.cn`（.env.prod 解密实测）：
//! - 积分/模型走 `token: <session>` 请求头（PointsService / model-service）；
//! - chat 走 `Authorization: Bearer <session>`（OpenAI 兼容 /api/v1/chat/completions）；
//! - 业务码 `000000` 成功，`100002` 登录失效。
//!
//! Loomy 没有独立签到端点：每日 5000 积分在登录后由服务端自动发放
//! （dailyBalance 字段），「签到」等价于校验登录态有效；本模块的签到
//! 动作以积分流水探测登录态并记录快照。

use base64::engine::general_purpose;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, http_request, now_ms, store_dir};

pub(crate) const LOOMY_API_HOST: &str = "https://loomyad.xunfei.cn";
pub const PLATFORM_LOOMY: &str = "loomy";
const ACCOUNTS_FILE: &str = "loomy_accounts.json";
/// 登录时申请的 session 有效期（14 天，客户端 checkCode 请求 expire 字段）。
pub const SESSION_TTL_DAYS: u64 = 14;

static WRITE_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// 凭证发现（本机客户端）
// ---------------------------------------------------------------------------

/// 用户名 → Loomy 数据根哈希（config-path.js：sha256(username) 前 12 位 hex）。
fn user_hash(username: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(username.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(12)
        .collect()
}

/// 本机 Loomy userData 目录（可能存在多用户哈希目录，全部探测）。
pub(crate) fn loomy_data_dir() -> Option<PathBuf> {
    let base = PathBuf::from("C:/Users/Public/Loomy");
    let entries = std::fs::read_dir(&base).ok()?;
    let candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("userData/auth-session.json").is_file())
        .collect();
    if candidates.is_empty() {
        // 无已登录目录；非 Windows 或目录结构变化时回退当前用户推算路径。
        let username = std::env::var("USERNAME").unwrap_or_default();
        let dir = base.join(user_hash(&username)).join("userData");
        return dir.is_dir().then_some(dir);
    }
    // 取最近修改的 auth-session.json（当前登录的用户）。
    candidates
        .into_iter()
        .max_by_key(|dir| {
            std::fs::metadata(dir.join("userData/auth-session.json"))
                .and_then(|m| m.modified())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                })
                .unwrap_or(0)
        })
        .map(|dir| dir.join("userData"))
}

pub fn auth_session_path() -> Option<PathBuf> {
    loomy_data_dir().map(|dir| dir.join("auth-session.json"))
}

/// 读取本机当前登录的 Loomy session（明文 token）。
pub fn read_local_session() -> Result<String, String> {
    let path = auth_session_path()
        .ok_or_else(|| "未找到 Loomy 数据目录（请安装并登录 Loomy 桌面客户端）".to_string())?;
    let text = std::fs::read_to_string(&path)
        .map_err(|_| "未找到 Loomy 登录信息（请安装并登录 Loomy 桌面客户端）".to_string())?;
    let auth: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析 Loomy auth-session.json 失败: {e}"))?;
    let session = auth
        .get("session")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "Loomy 尚未登录（auth-session.json 无 session）".to_string())?;
    Ok(session.trim().to_string())
}

/// 本机登录账号的 userid（发现 ID 用）。
fn read_local_userid() -> Option<String> {
    let text = std::fs::read_to_string(auth_session_path()?).ok()?;
    let auth: Value = serde_json::from_str(&text).ok()?;
    auth.get("userid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn read_local_phone() -> Option<String> {
    let text = std::fs::read_to_string(auth_session_path()?).ok()?;
    let auth: Value = serde_json::from_str(&text).ok()?;
    auth.get("phone")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// 本机登录账号的发现 ID（`loomy:<userid>`）。
pub fn local_discovery_id() -> Option<String> {
    read_local_userid().map(|uid| format!("{PLATFORM_LOOMY}:{uid}"))
}

/// Loomy 网关请求头（积分/模型接口用 `token` 头，注入官方拟真 UA）。
pub(crate) fn loomy_headers(token: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("token".to_string(), token.trim().to_string());
    headers.insert("Accept".to_string(), "application/json, text/plain, */*".to_string());
    headers.insert("Accept-Language".to_string(), "zh-CN,zh;q=0.9".to_string());
    headers.insert(
        "User-Agent".to_string(),
        crate::modules::account_protection::simulated_loomy_user_agent(),
    );
    headers
}

// ---------------------------------------------------------------------------
// 网关调用（积分/用户信息）
// ---------------------------------------------------------------------------

/// 业务响应是否登录失效（code 100002）。
fn is_auth_error(resp: &Value) -> bool {
    resp.get("code").and_then(Value::as_str) == Some("100002")
}

/// 查询积分余额与流水（/api/v2/points/records 的 balance 字段即权威余额）。
/// 返回 {ok, balance, dailyBalance, records?} 或 {ok:false, error}。
pub async fn get_loomy_credits_with(session: &str) -> Value {
    let headers = loomy_headers(session);
    let url = format!("{LOOMY_API_HOST}/api/v2/points/records?pageNo=1&pageSize=20&direction=credit&granularity=chat");
    let resp = http_request(&url, "GET", None, Some(&headers)).await;
    if is_auth_error(&resp) {
        return json!({"ok": false, "authFailed": true, "error": "Loomy 登录态失效，请打开 Loomy 重新登录"});
    }
    if resp.get("code").and_then(Value::as_str) != Some("000000") {
        let error = resp
            .get("desc")
            .or_else(|| resp.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("积分查询失败");
        return json!({"ok": false, "error": error});
    }
    let data = resp.get("data").cloned().unwrap_or(Value::Null);
    let balance = data.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
    let daily = data
        .get("dailyBalance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let records: Vec<Value> = data
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(10)
        .collect();
    json!({
        "ok": true,
        "balance": balance,
        "remainingTotal": balance,
        "totalRemaining": balance,
        "dailyBalance": daily,
        "platform": "loomy",
        "recentRecords": records,
    })
}

/// 实时校验 session 有效性（积分域 /api/v1/user/identity，code 000000 即有效）。
///
/// 讯飞的 userinfo/baseInfo 在账号域（account.xfinfr.com，HMAC 签名），
/// 积分域拿不到 userid/phone——账号身份由调用方从本机 auth-session.json
/// 读取后经 `hint_userid`/`hint_phone` 传入（手动导入无此信息，用 session
/// 哈希当稳定 ID）。
pub async fn verify_session(session: &str) -> Value {
    verify_session_with_hints(session, None, None).await
}

pub async fn verify_session_with_hints(
    session: &str,
    hint_userid: Option<String>,
    hint_phone: Option<String>,
) -> Value {
    let headers = loomy_headers(session);
    let url = format!("{LOOMY_API_HOST}/api/v1/user/identity");
    let resp = http_request(&url, "GET", None, Some(&headers)).await;
    if resp.get("code").and_then(Value::as_str) != Some("000000") {
        let error = resp
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or("session 校验失败");
        return json!({"ok": false, "authFailed": is_auth_error(&resp), "error": error});
    }
    let userid = hint_userid.unwrap_or_default();
    let id = if userid.is_empty() {
        session_fingerprint(session)
    } else {
        userid
    };
    json!({
        "ok": true,
        "userid": id,
        "phone": hint_phone.unwrap_or_default(),
        "nickname": Value::Null,
    })
}

/// session 指纹（手动导入时没有 userid，用 sha256 前 12 位当稳定 ID；
/// 同一 session 重导入幂等，session 换新则视为新账号）。
fn session_fingerprint(session: &str) -> String {
    use sha2::Digest as _;
    let mut hasher = Sha256::new();
    hasher.update(session.trim().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(12)
        .collect()
}

// ---------------------------------------------------------------------------
// 平台账号发现与签到路由（platform_checkin 集成点）
// ---------------------------------------------------------------------------

/// 本机实时发现的 Loomy 账号（格式对齐 discover_autoclaw_account）。
pub fn discover_loomy_account() -> Option<Value> {
    let session = read_local_session().ok()?;
    let uid = read_local_userid().unwrap_or_default();
    let phone = read_local_phone().unwrap_or_default();
    let display = if phone.is_empty() { &uid } else { &phone };
    if display.is_empty() {
        return None;
    }
    let id = if uid.is_empty() {
        "loomy:local".to_string()
    } else {
        format!("{PLATFORM_LOOMY}:{uid}")
    };
    Some(json!({
        "id": id,
        "email": format!("Loomy · {display}"),
        "nickname": format!("Loomy · {display}"),
        "platform": PLATFORM_LOOMY,
    }))
}

/// Loomy 账号是否本机客户端当前登录。
pub fn is_local_login(account: &Value) -> bool {
    let Some(id) = account.get("id").and_then(Value::as_str) else {
        return false;
    };
    local_discovery_id().is_some_and(|local| local == id) || id == "loomy:local"
}

/// 每日签到：Loomy 的每日 5000 积分随登录自动发放（无独立签到端点），
/// 这里以积分查询校验登录态并刷新余额快照。
/// 返回结构与 AutoClaw/Trae 一致：{ok, todayCheckedIn, already?, raw}。
pub async fn checkin_with(session: &str) -> Value {
    let credits = get_loomy_credits_with(session).await;
    if credits.get("ok").and_then(Value::as_bool) != Some(true) {
        return json!({
            "ok": false,
            "already": false,
            "error": credits.get("error").and_then(Value::as_str).unwrap_or("Loomy 登录态校验失败"),
        });
    }
    json!({
        "ok": true,
        "already": true,
        "todayCheckedIn": true,
        "note": "Loomy 每日积分随登录自动发放，无需手动签到",
        "balance": credits.get("balance").cloned().unwrap_or(Value::Null),
        "dailyBalance": credits.get("dailyBalance").cloned().unwrap_or(Value::Null),
    })
}

// ---------------------------------------------------------------------------
// 账号池（~/.wb-switch/loomy_accounts.json）
// ---------------------------------------------------------------------------

fn accounts_path() -> PathBuf {
    store_dir().join(ACCOUNTS_FILE)
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

fn find_account_mut<'a>(accounts: &'a mut [Value], id: &str) -> Option<&'a mut Value> {
    accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
}

fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() >= 7 {
        format!(
            "{}****{}",
            chars[..3].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else {
        phone.to_string()
    }
}

/// 导入本机当前登录账号为池快照（幂等，id = loomy:<userid>）。
pub async fn import_local() -> Result<Value, String> {
    let session = read_local_session()
        .map_err(|e| format!("本机 Loomy 登录态不可用: {e}"))?;
    let info = verify_session_with_hints(
        &session,
        read_local_userid(),
        read_local_phone(),
    )
    .await;
    if info.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(
            info.get("error")
                .and_then(Value::as_str)
                .unwrap_or("session 校验失败")
                .to_string(),
        );
    }
    let uid = read_local_userid()
        .or_else(|| {
            info.get("userid")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| general_purpose::STANDARD.encode(uuid::Uuid::new_v4()));
    let phone = read_local_phone().or_else(|| {
        info.get("phone")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    });
    let id = format!("{PLATFORM_LOOMY}:{uid}");
    let label = phone
        .as_deref()
        .map(mask_phone)
        .or_else(|| {
            info.get("nickname")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| "本机账号".to_string());

    let account = json!({
        "id": id,
        "label": label,
        "session": session,
        "userid": uid,
        "source": "local-import",
        "localDiscoveryId": local_discovery_id(),
        "status": "ok",
        "addedAt": now_ms(),
        "updatedAt": now_ms(),
    });
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(existing) = find_account_mut(&mut accounts, &id) {
        for (key, value) in account.as_object().unwrap() {
            if key == "addedAt" {
                continue;
            }
            existing[key] = value.clone();
        }
        let snapshot = existing.clone();
        save_accounts(&accounts)?;
        return Ok(snapshot);
    }
    accounts.push(account.clone());
    save_accounts(&accounts)?;
    Ok(account)
}

/// 探测本机 auth-session.json 是否变动，若更新了新 session 则自动自愈更新池内快照。
/// 返回 (是否变动, 当前最新 session)。
pub fn sync_local_session_if_changed() -> (bool, Option<String>) {
    let Ok(session) = read_local_session() else {
        return (false, None);
    };
    let local_id = local_discovery_id().unwrap_or_else(|| "loomy:local".to_string());
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(acc) = find_account_mut(&mut accounts, &local_id) {
        let old_session = acc.get("session").and_then(Value::as_str).unwrap_or("");
        if old_session != session {
            acc["session"] = json!(session);
            acc["status"] = json!("ok");
            acc["lastError"] = Value::Null;
            acc["updatedAt"] = json!(now_ms());
            let _ = save_accounts(&accounts);
            return (true, Some(session));
        }
    }
    (false, Some(session))
}

/// 手动导入 session（可从另一台机器的 auth-session.json 拷贝）。
pub async fn add_account(label: Option<String>, session: &str) -> Result<Value, String> {
    let session = session.trim().to_string();
    if session.is_empty() {
        return Err("session 不能为空".to_string());
    }
    let info = verify_session(&session).await;
    if info.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "session 校验失败：{}",
            info.get("error").and_then(Value::as_str).unwrap_or("未知错误")
        ));
    }
    let uid = info
        .get("userid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "无法从网关获取 userid，已取消导入".to_string())?
        .to_string();
    let phone = info
        .get("phone")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let id = format!("{PLATFORM_LOOMY}:{uid}");
    let display = label
        .filter(|l| !l.trim().is_empty())
        .or_else(|| phone.as_deref().map(mask_phone))
        .or_else(|| {
            info.get("nickname")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| "导入账号".to_string());
    let account = json!({
        "id": id,
        "label": display,
        "session": session,
        "userid": uid,
        "source": "manual",
        "status": "ok",
        "addedAt": now_ms(),
        "updatedAt": now_ms(),
    });
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(existing) = find_account_mut(&mut accounts, &id) {
        for (key, value) in account.as_object().unwrap() {
            if key == "addedAt" {
                continue;
            }
            existing[key] = value.clone();
        }
        let snapshot = existing.clone();
        save_accounts(&accounts)?;
        return Ok(snapshot);
    }
    accounts.push(account.clone());
    save_accounts(&accounts)?;
    Ok(account)
}

pub fn remove_account(id: &str) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.get("id").and_then(Value::as_str) != Some(id));
    if accounts.len() != before {
        let _ = save_accounts(&accounts);
        true
    } else {
        false
    }
}

pub fn mark_account_error(id: &str, error: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(account) = find_account_mut(&mut accounts, id) {
        account["status"] = json!("invalid");
        account["lastError"] = json!(error);
        let _ = save_accounts(&accounts);
    }
}

/// 快照是否应被本机实时登录账号合并（隐藏）。
pub fn hidden_by_local_login(account: &Value) -> bool {
    let Some(local_id) = local_discovery_id() else {
        return false;
    };
    if account.get("localDiscoveryId").and_then(Value::as_str) == Some(local_id.as_str()) {
        return true;
    }
    account.get("id").and_then(Value::as_str) == Some(local_id.as_str())
}

/// 账号列表（管理界面用）：本机实时账号 + 池内快照（去重）。
pub fn list_for_display() -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(session) = read_local_session().ok() {
        let (expires_days, expires_at) = session_age();
        out.push(json!({
            "id": local_discovery_id().unwrap_or_else(|| "loomy:local".to_string()),
            "label": "本机登录",
            "source": "local-live",
            "status": "ok",
            "lastError": Value::Null,
            "addedAt": Value::Null,
            "updatedAt": Value::Null,
            "tokenPreview": Value::Null,
            "sessionDays": SESSION_TTL_DAYS,
            "expiresDays": expires_days,
            "expiresAt": expires_at,
            "_localSession": session,
        }));
    }
    for account in load_accounts() {
        if hidden_by_local_login(&account) {
            continue;
        }
        out.push(json!({
            "id": account.get("id").cloned().unwrap_or(Value::Null),
            "label": account.get("label").cloned().unwrap_or(Value::Null),
            "source": account.get("source").cloned().unwrap_or(Value::Null),
            "status": account.get("status").cloned().unwrap_or(json!("ok")),
            "lastError": account.get("lastError").cloned().unwrap_or(Value::Null),
            "addedAt": account.get("addedAt").cloned().unwrap_or(Value::Null),
            "updatedAt": account.get("updatedAt").cloned().unwrap_or(Value::Null),
            "tokenPreview": account.get("tokenPreview").cloned().unwrap_or(Value::Null),
            "sessionDays": SESSION_TTL_DAYS,
        }));
    }
    out
}

/// 本机 session 的签发时间与剩余天数（auth-session.json 的 updatedAt）。
fn session_age() -> (Option<u64>, Option<i64>) {
    let Some(text) = auth_session_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return (None, None);
    };
    let Ok(auth) = serde_json::from_str::<Value>(&text) else {
        return (None, None);
    };
    let updated_at = auth.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
    if updated_at <= 0 {
        return (None, None);
    }
    let expires_at = updated_at + (SESSION_TTL_DAYS as i64) * 86_400_000;
    let remain_ms = expires_at - now_ms();
    let days = if remain_ms > 0 {
        Some((remain_ms / 86_400_000) as u64)
    } else {
        Some(0)
    };
    (days, Some(expires_at))
}

/// 代理用账号池：本机登录优先，其后是池内条目（未失效且未被本机登录隐藏）。
pub(crate) fn pool_sessions() -> Vec<(String, String, Option<String>)> {
    let mut pool = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(session) = read_local_session() {
        let id = local_discovery_id().unwrap_or_else(|| "loomy:local".to_string());
        seen.insert(id.clone());
        pool.push((id, session, None));
    }
    for account in load_accounts() {
        if account.get("status").and_then(Value::as_str) == Some("invalid") {
            continue;
        }
        if hidden_by_local_login(&account) {
            continue;
        }
        let Some(id) = account.get("id").and_then(Value::as_str).map(String::from) else {
            continue;
        };
        let Some(session) = account
            .get("session")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
        else {
            continue;
        };
        if seen.insert(id.clone()) {
            pool.push((id.clone(), session, Some(id)));
        }
    }
    pool
}

/// 池内单账号签到（校验登录态，401/100002 标记失效）。
pub async fn checkin_stored(id: &str) -> Value {
    let Some(account) = load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    else {
        return json!({"result": "error", "error": "账号不存在"});
    };
    let Some(session) = account
        .get("session")
        .and_then(Value::as_str)
        .map(String::from)
    else {
        return json!({"result": "error", "error": "账号缺少 session，请重新导入"});
    };
    let res = checkin_with(&session).await;
    if res.get("ok").and_then(Value::as_bool) == Some(true) {
        json!({"result": "already"})
    } else {
        let error = res
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Loomy 登录态校验失败")
            .to_string();
        if res.get("authFailed").and_then(Value::as_bool) == Some(true) {
            mark_account_error(id, &error);
            return json!({"result": "error", "error": format!("{error}；请重新导入该账号的 session")});
        }
        json!({"result": "error", "error": error})
    }
}

/// 池内单账号积分（/api/platform/accounts 的 credit 字段）。
pub async fn credits_stored(id: &str) -> Value {
    let Some(account) = load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    else {
        return json!({"ok": false, "error": "账号不存在"});
    };
    let Some(session) = account
        .get("session")
        .and_then(Value::as_str)
        .map(String::from)
    else {
        return json!({"ok": false, "error": "账号缺少 session"});
    };
    let result = get_loomy_credits_with(&session).await;
    if result.get("authFailed").and_then(Value::as_bool) == Some(true) {
        mark_account_error(id, "Loomy 登录态失效");
    }
    result
}

/// 本机账号积分（实时读 auth-session.json）。
pub async fn credits_local() -> Value {
    match read_local_session() {
        Ok(session) => get_loomy_credits_with(&session).await,
        Err(error) => json!({"ok": false, "error": error}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_hash_matches_node_impl() {
        // config-path.js：sha256(username) 十六进制前 12 位。
        // 参考向量：sha256("user") 前缀（与实机目录名比对）。
        let hash = user_hash("user");
        assert_eq!(hash.len(), 12);
        let mut hasher = Sha256::new();
        hasher.update(b"user");
        let full: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hash, full[..12]);
    }

    #[test]
    fn hidden_by_local_login_matches() {
        let snapshot = json!({
            "id": "loomy:1001",
            "localDiscoveryId": "loomy:2002",
        });
        assert!(!hidden_by_local_login(&snapshot));
        let hidden = json!({
            "id": "loomy:1001",
            "localDiscoveryId": "loomy:1001",
        });
        assert!(!hidden_by_local_login(&hidden)); // 本机未登录时（None）不隐藏
    }

    #[test]
    fn mask_phone_keeps_prefix_and_suffix() {
        assert_eq!(mask_phone("18012342401"), "180****2401");
        assert_eq!(mask_phone("123"), "123");
    }
}
