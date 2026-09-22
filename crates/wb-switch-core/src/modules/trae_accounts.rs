//! TRAE Work CN 多账号池。
//!
//! TRAE 客户端同一时间只保留一个登录账号（storage.json 单登录态）。要给
//! 多个 TRAE 账号签到：先把客户端当前登录态"快照"进池（含 access/refresh
//! token 与设备 ECDSA 密钥对），再切换客户端登录下一个账号、再次导入。
//! 池内续期用快照里的 refresh token + 设备私钥走官方
//! `/trae/api/v3/oauth/ExchangeToken`，不依赖客户端当前登录态。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::platform_checkin::{
    get_trae_credits_with, get_trae_status_with, perform_trae_checkin_with, trae_exchange_token,
    trae_local_snapshot, TraeCredential,
};

const ACCOUNTS_FILE: &str = "trae_accounts.json";
/// TRAE 默认 API host（快照里正常带有 host，此处仅兜底）。
const TRAE_DEFAULT_HOST: &str = "https://api.trae.cn";

static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn accounts_path() -> PathBuf {
    store_dir().join(ACCOUNTS_FILE)
}

pub fn load_accounts() -> Vec<Value> {
    std::fs::read_to_string(accounts_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// 按 id 取池条目（一键切换用）。
pub fn find_stored(id: &str) -> Result<Value, String> {
    load_accounts()
        .into_iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("账号不存在: {id}"))
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

/// 池条目的 access token 是否已到期（到期条目退出签到列表，需重新导入）。
pub fn entry_expired(account: &Value) -> bool {
    account
        .get("expiredAt")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt < chrono::Local::now())
        .unwrap_or(false)
}

/// 本机客户端当前登录账号是否与池条目同一身份（同账号由客户端实时代管）。
pub fn hidden_by_local_login(account: &Value) -> bool {
    let Some(local_uid) = crate::modules::platform_checkin::trae_local_user_id() else {
        return false;
    };
    account.get("userId").and_then(Value::as_str) == Some(local_uid.as_str())
}

/// 把本机客户端当前登录的账号作为快照加入账号池（幂等：同账号更新凭证）。
pub fn import_local_snapshot() -> Result<Value, String> {
    let snapshot = trae_local_snapshot()?;
    let user_id = snapshot
        .get("userId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "TRAE 登录态缺少 userId".to_string())?
        .to_string();
    let username = snapshot
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let id = format!("trae_work_cn:{user_id}");
    let label = if username.is_empty() {
        "本机账号".to_string()
    } else {
        username
    };

    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let now = now_ms();
    let entry = json!({
        "id": id,
        "label": label,
        "userId": user_id,
        "accessToken": snapshot.get("accessToken").cloned().unwrap_or(Value::Null),
        "refreshToken": snapshot.get("refreshToken").cloned().unwrap_or(Value::Null),
        "expiredAt": snapshot.get("expiredAt").cloned().unwrap_or(Value::Null),
        "refreshExpiredAt": snapshot.get("refreshExpiredAt").cloned().unwrap_or(Value::Null),
        "host": snapshot.get("host").cloned().unwrap_or(json!(TRAE_DEFAULT_HOST)),
        "deviceId": snapshot.get("deviceId").cloned().unwrap_or(Value::Null),
        "machineId": snapshot.get("machineId").cloned().unwrap_or(Value::Null),
        "clientVersion": snapshot.get("clientVersion").cloned().unwrap_or(Value::Null),
        "keypairs": snapshot.get("keypairs").cloned().unwrap_or(json!([])),
        "status": "ok",
        "error": Value::Null,
        "updatedAt": now,
        "addedAt": now,
    });
    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
    {
        // 幂等更新：刷新凭证与设备资料，保留 addedAt。
        for field in [
            "accessToken",
            "refreshToken",
            "expiredAt",
            "refreshExpiredAt",
            "host",
            "deviceId",
            "machineId",
            "clientVersion",
            "keypairs",
        ] {
            existing[field] = entry[field].clone();
        }
        existing["label"] = json!(label);
        existing["status"] = json!("ok");
        existing["error"] = Value::Null;
        existing["updatedAt"] = json!(now);
        let updated = existing.clone();
        save_accounts(&accounts)?;
        return Ok(updated);
    }
    accounts.push(entry.clone());
    save_accounts(&accounts)?;
    Ok(entry)
}

pub fn remove_account(id: &str) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.get("id").and_then(Value::as_str) != Some(id));
    if accounts.len() == before {
        return false;
    }
    save_accounts(&accounts).is_ok()
}

/// 手动添加账号的输入（T4：/trae-workbuddy 页账号池「手动添加」）。
/// 与 import_local_snapshot 的区别：不带设备密钥对与 userId，凭据由用户粘贴；
/// 续期（ExchangeToken 需要设备私钥）不可用，但签到/状态/额度在 expiredAt 前可用。
pub struct ManualAccountInput {
    pub label: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub device_id: Option<String>,
    pub machine_id: Option<String>,
}

/// 手动添加池条目（幂等：同 access token 更新凭据，保留 addedAt）。
pub fn add_manual_account(input: ManualAccountInput) -> Result<Value, String> {
    let access = input.access_token.trim().to_string();
    if access.is_empty() {
        return Err("accessToken 不能为空".to_string());
    }
    // id 派生：token 稳定哈希，同 token 重复添加即更新而非重复条目。
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(access.as_bytes());
    let hash = hasher.finalize();
    let hash_hex: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let id = format!("trae_work_cn:manual:{hash_hex}");

    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let now = now_ms();
    let label = input
        .label
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "手动导入账号".to_string());
    let mut entry = json!({
        "id": id,
        "label": label,
        "userId": "",
        "accessToken": access,
        "refreshToken": input.refresh_token.clone().unwrap_or_default(),
        "expiredAt": Value::Null,
        "refreshExpiredAt": Value::Null,
        "host": TRAE_DEFAULT_HOST,
        "deviceId": input.device_id.clone().unwrap_or_default(),
        "machineId": input.machine_id.clone().unwrap_or_default(),
        "clientVersion": Value::Null,
        "keypairs": json!([]),
        "status": "ok",
        "error": Value::Null,
        "manual": true,
        "updatedAt": now,
        "addedAt": now,
    });
    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
    {
        for field in [
            "label",
            "accessToken",
            "refreshToken",
            "deviceId",
            "machineId",
            "manual",
        ] {
            existing[field] = entry[field].clone();
        }
        existing["status"] = json!("ok");
        existing["error"] = Value::Null;
        existing["updatedAt"] = json!(now);
        let updated = existing.clone();
        save_accounts(&accounts)?;
        return Ok(updated);
    }
    accounts.push(entry.clone());
    save_accounts(&accounts)?;
    entry["addedAt"] = json!(now);
    Ok(entry)
}

fn cred_from_entry(entry: &Value) -> Result<TraeCredential, String> {
    let token = entry
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "账号缺少 accessToken，请重新导入快照".to_string())?;
    let mut device_id = entry
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if device_id.is_empty() || device_id.contains('-') {
        if let Some(aha) = crate::modules::platform_checkin::resolve_system_aha_device_id() {
            device_id = aha;
        }
    }
    Ok(TraeCredential {
        token: token.to_string(),
        device_id,
        machine_id: entry
            .get("machineId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        host: entry
            .get("host")
            .and_then(Value::as_str)
            .filter(|h| h.starts_with("http"))
            .unwrap_or(TRAE_DEFAULT_HOST)
            .to_string(),
        username: entry
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// 池条目续期：遍历快照里的设备密钥对尝试 ExchangeToken，成功即写回池。
/// 刷新令牌失效类错误会把条目标记为 invalid（从签到列表移除，需重新导入）。
pub async fn refresh_stored_account(id: &str) -> Result<String, String> {
    let mut accounts = load_accounts();
    let Some(entry) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Err("账号池条目不存在".to_string());
    };
    let host = entry
        .get("host")
        .and_then(Value::as_str)
        .filter(|h| h.starts_with("http"))
        .unwrap_or(TRAE_DEFAULT_HOST)
        .to_string();
    let old_token = entry
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let refresh_token = entry
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if refresh_token.is_empty() {
        return Err("账号缺少 refreshToken，请重新导入快照".to_string());
    }
    let device_id = entry
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let machine_id = entry
        .get("machineId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let client_version = entry
        .get("clientVersion")
        .and_then(Value::as_str)
        .unwrap_or("1.0.0")
        .to_string();
    let keypairs: Vec<(String, String)> = entry
        .get("keypairs")
        .and_then(Value::as_array)
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    let private_key = pair.get("privateKeyPEM").and_then(Value::as_str)?;
                    let public_key = pair.get("publicKeyPEM").and_then(Value::as_str)?;
                    Some((private_key.to_string(), public_key.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    if keypairs.is_empty() {
        return Err("账号缺少设备密钥对，请重新导入快照".to_string());
    }

    let mut last_error = String::new();
    'keypairs: for (private_key_pem, public_key_pem) in &keypairs {
        let mut net_attempt = 0u32;
        loop {
            match trae_exchange_token(
                &host,
                &old_token,
                &refresh_token,
                &device_id,
                &machine_id,
                private_key_pem,
                public_key_pem,
                &client_version,
            )
            .await
            {
                Ok(out) => {
                    let token = out.token.clone();
                    entry["accessToken"] = json!(out.token);
                    if let Some(new_refresh) = out.refresh_token {
                        entry["refreshToken"] = json!(new_refresh);
                    }
                    if let Some(expired_at) = out.expired_at {
                        entry["expiredAt"] = json!(expired_at);
                    }
                    if let Some(refresh_expired_at) = out.refresh_expired_at {
                        entry["refreshExpiredAt"] = json!(refresh_expired_at);
                    }
                    entry["status"] = json!("ok");
                    entry["error"] = Value::Null;
                    entry["updatedAt"] = json!(now_ms());
                    save_accounts(&accounts)?;
                    return Ok(token);
                }
                Err(error) => {
                    last_error = error;
                    // F1：网络类错误（传输层，http_request 统一映射 code=-1）指数退避
                    // 重试，每个密钥对最多 3 次；服务端明确拒鉴权（如 20403）不重试，
                    // 直接走下方「不判死」提示。
                    if last_error.contains("code=-1") && net_attempt < 2 {
                        net_attempt += 1;
                        let backoff = 800u64 * (1u64 << net_attempt);
                        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                        continue;
                    }
                    continue 'keypairs;
                }
            }
        }
    }
    // 注意：续期失败不判死条目——TRAE 的刷新令牌随客户端会话绑定（账号
    // 从客户端切换/重新登录后即被服务端拒绝），但 access token 在 expiredAt
    // 前仍可直接签到；条目到期后由 entry_expired 剔除。
    if last_error.is_empty() {
        last_error = "所有设备密钥对均续期失败".to_string();
    }
    if last_error.contains("刷新令牌已失效") {
        let expired_at = entry
            .get("expiredAt")
            .and_then(Value::as_str)
            .map(|t| t.chars().take(10).collect::<String>())
            .unwrap_or_else(|| "未知".to_string());
        last_error = format!(
            "TRAE 刷新令牌已随客户端切换账号失效（20403）。该快照登录态在 {expired_at} 前仍可直接签到；到期后请切换客户端登录该账号并重新导入"
        );
    }
    // F1：失败仅记录 error（status 保持 ok，不判死），供池列表与保活巡检可见。
    entry["error"] = json!(last_error);
    entry["updatedAt"] = json!(now_ms());
    let _ = save_accounts(&accounts);
    Err(last_error)
}

/// F3：上游限流特征码命中时把账号置入冷却熔断（account_protection，
/// 以 trae_work_cn: 前缀的条目 id 为键，与 loomy:/autoclaw 隔离）。
pub fn note_trae_rate_limit(entry: &Value, err: &str) {
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() || !id.starts_with("trae_work_cn:") {
        return;
    }
    let is_rate = err.contains("429")
        || err.contains("太多")
        || err.contains("稍后再试")
        || err.contains("请稍后")
        || err.contains("繁忙")
        || err.contains("稍后重试")
        || err.contains("限流")
        || err.contains("rate limit")
        || err.contains("RateLimit");
    if !is_rate {
        return;
    }
    let cfg = crate::modules::account_protection::load_protection_config();
    let label = entry.get("label").and_then(Value::as_str).unwrap_or(id);
    crate::modules::account_protection::mark_account_cooling(
        id,
        label,
        &format!("TRAE 上游限流：{err}"),
        cfg.cooldown_minutes,
    );
}

/// 池条目签到状态查询：用自身快照凭证，失败时自动续期重试一次。
pub async fn status_stored(store_id: &str) -> Value {
    let Some(entry) = load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(store_id))
    else {
        return json!({"ok": false, "error": "账号池条目不存在"});
    };
    let Ok(cred) = cred_from_entry(&entry) else {
        return json!({"ok": false, "error": "账号缺少 accessToken，请重新导入快照"});
    };
    let mut status = get_trae_status_with(&cred).await;
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        if let Ok(new_token) = refresh_stored_account(store_id).await {
            let mut refreshed = cred;
            refreshed.token = new_token;
            status = get_trae_status_with(&refreshed).await;
        }
    }
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        if let Some(err) = status.get("error").and_then(Value::as_str) {
            note_trae_rate_limit(&entry, err);
        }
    }
    status
}

/// 池条目每日签到领取：先查状态（含续期），未签到则领取，领取遇登录态
/// 失效再续期重试一次。
pub async fn claim_stored(store_id: &str) -> Value {
    let Some(entry) = load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(store_id))
    else {
        return json!({"result": "error", "error": "账号池条目不存在"});
    };
    let Ok(mut cred) = cred_from_entry(&entry) else {
        return json!({"result": "error", "error": "账号缺少 accessToken，请重新导入快照"});
    };
    let status = status_stored(store_id).await;
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        if let Some(err) = status.get("error").and_then(Value::as_str) {
            note_trae_rate_limit(&entry, err);
        }
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
    let mut res = perform_trae_checkin_with(&cred).await;
    let error = res.get("error").and_then(Value::as_str).unwrap_or("");
    if res.get("ok").and_then(Value::as_bool) != Some(true)
        && (error.contains("登录") || error.contains("token") || error.contains("Token"))
    {
        if let Ok(new_token) = refresh_stored_account(store_id).await {
            cred.token = new_token;
            res = perform_trae_checkin_with(&cred).await;
        }
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
                .unwrap_or("TRAE Work CN 签到失败"),
        })
    }
}

/// 池条目额度/权益包查询（失败自动续期重试一次）。
pub async fn credits_stored(store_id: &str) -> Value {
    let Some(entry) = load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(store_id))
    else {
        return json!({"ok": false, "error": "账号池条目不存在"});
    };
    let Ok(cred) = cred_from_entry(&entry) else {
        return json!({"ok": false, "error": "账号缺少 accessToken，请重新导入快照"});
    };
    let mut credits = get_trae_credits_with(&cred).await;
    if credits.get("ok").and_then(Value::as_bool) != Some(true) {
        if let Ok(new_token) = refresh_stored_account(store_id).await {
            let mut refreshed = cred;
            refreshed.token = new_token;
            credits = get_trae_credits_with(&refreshed).await;
        }
    }
    if credits.get("ok").and_then(Value::as_bool) != Some(true) {
        if let Some(err) = credits.get("error").and_then(Value::as_str) {
            note_trae_rate_limit(&entry, err);
        }
    }
    credits
}

/// 账号池管理列表（含本机登录中标记）。
pub fn list_for_display() -> Vec<Value> {
    load_accounts()
        .into_iter()
        .map(|account| {
            let token_preview = account
                .get("accessToken")
                .and_then(Value::as_str)
                .map(|access| {
                    format!(
                        "{}…({})",
                        &access[..access.len().min(10)],
                        access.len()
                    )
                });
            json!({
                "id": account.get("id").cloned().unwrap_or(Value::Null),
                "label": account.get("label").cloned().unwrap_or(Value::Null),
                "userId": account.get("userId").cloned().unwrap_or(Value::Null),
                "status": account.get("status").cloned().unwrap_or(json!("ok")),
                "error": account.get("error").cloned().unwrap_or(Value::Null),
                "expiredAt": account.get("expiredAt").cloned().unwrap_or(Value::Null),
                "expired": entry_expired(&account),
                "tokenPreview": token_preview,
                "updatedAt": account.get("updatedAt").cloned().unwrap_or(Value::Null),
                "localLogin": hidden_by_local_login(&account),
            })
        })
        .collect()
}
