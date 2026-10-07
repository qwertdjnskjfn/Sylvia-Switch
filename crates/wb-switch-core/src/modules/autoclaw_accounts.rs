//! AutoClaw 多账号池：多账号持久化存储、按账号刷新登录态、签到与代理共用。
//!
//! 账号来源：
//! - 手动导入：粘贴 access/refresh token（其余机器登录的 AutoClaw、抓包等）；
//! - 本机快照：把当前 `auth.json` 登录的账号复制一份存入池中，之后即使桌面
//!   端切换登录，池里的账号仍可独立签到/走代理。
//!
//! 兼容性：本机 `auth.json` 当前登录账号仍由 platform_checkin 自动发现
//! （动态跟随桌面端登录），与池内账号按 uid 去重后合并，互不干扰。
//! token 以明文存放于 `~/.wb-switch/autoclaw_accounts.json`（用户目录内，
//! 与其他本地配置一致），请勿把该文件分享给他人。
//!
//! 区域隔离 (v1.0+): 
//! - 国际版账号 (international): OAuth/手动导入/抓包获取
//! - 国内版账号 (domestic): 从本机 AutoClaw 桌面端导入
//! - 两个账号池完全隔离，互不共享 API 访问权限

use base64::engine::general_purpose;
use base64::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, http_request, now_ms, store_dir};
use crate::modules::platform_checkin::{
    autoclaw_data_dir, autoclaw_decrypt_token, autoclaw_headers, read_autoclaw_token,
};

const ACCOUNTS_FILE: &str = "autoclaw_accounts.json";

static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 账号区域枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountRegion {
    #[serde(rename = "international")]
    International,  // 国际版（海外）
    
    #[serde(rename = "domestic")]
    Domestic,       // 国内版（中国大陆）
}

impl std::fmt::Display for AccountRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountRegion::International => write!(f, "🌐 国际"),
            AccountRegion::Domestic => write!(f, "🇨🇳 国内"),
        }
    }
}

impl AccountRegion {
    pub fn as_str(&self) -> &'static str {
        match self {
            AccountRegion::International => "international",
            AccountRegion::Domestic => "domestic",
        }
    }

    /// 该区域账号使用的 AutoClaw 网关 host（2026-09-18 逆向国际版客户端
    /// app.asar + 实测：两区签到/续期/积分/模型代理路径同构，仅 host 不同；
    /// 国际账号调国内网关返回 code=410000「用户未登录」，绝不可跨区调用）。
    /// 国内 = `autoglm-acceleration-api.zhipuai.cn`；国际 = `autoglm-api.autoglm.ai`。
    pub fn api_host(self) -> &'static str {
        match self {
            AccountRegion::Domestic => "https://autoglm-acceleration-api.zhipuai.cn",
            AccountRegion::International => "https://autoglm-api.autoglm.ai",
        }
    }

    /// 模型代理 chat completions 上游 URL（两区路径结构相同，仅 host 不同；
    /// 国际版 2026-09-18 实测 200）。仅供 glm_proxy 使用。
    pub fn chat_upstream_url(self) -> String {
        format!(
            "{}/autoclaw-proxy/proxy/autoclaw/chat/completions",
            self.api_host()
        )
    }
    
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "international" | "intl" | "overseas" | "global" => Some(AccountRegion::International),
            "domestic" | "cn" | "china" | "local" => Some(AccountRegion::Domestic),
            _ => None,
        }
    }
    
    /// 根据账号来源设置默认区域
    pub fn default_for_source(source: &AccountSource) -> Self {
        match source {
            AccountSource::LocalSnapshot => AccountRegion::Domestic,
            _ => AccountRegion::International,
        }
    }
}

/// 账号来源枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountSource {
    #[serde(rename = "oauth")]
    OAuth,                    // OAuth 扫码
    
    #[serde(rename = "manual_import")]
    ManualImport,             // 手动导入 Token
    
    #[serde(rename = "packet_capture")]
    PacketCapture,            // 抓包获取
    
    #[serde(rename = "local_snapshot")]
    LocalSnapshot,            // 本机桌面端快照
    
    #[serde(rename = "imported")]
    Imported,                 // 从其他设备导入
}

impl AccountSource {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "oauth" | "scan" | "qr" => Some(AccountSource::OAuth),
            "manual" | "paste" | "import" => Some(AccountSource::ManualImport),
            "capture" | "packets" => Some(AccountSource::PacketCapture),
            "local" | "snapshot" | "desktop" => Some(AccountSource::LocalSnapshot),
            "imported" | "migrated" => Some(AccountSource::Imported),
            _ => None,
        }
    }
    
    pub fn as_str(&self) -> &'static str {
        match self {
            AccountSource::OAuth => "oauth",
            AccountSource::ManualImport => "manual_import",
            AccountSource::PacketCapture => "packet_capture",
            AccountSource::LocalSnapshot => "local_snapshot",
            AccountSource::Imported => "imported",
        }
    }
}

pub(crate) fn accounts_path() -> PathBuf {
    store_dir().join(autoclaw_accounts_file_name())
}

fn autoclaw_accounts_file_name() -> &'static str {
    ACCOUNTS_FILE
}

pub fn load_accounts() -> Vec<Value> {
    std::fs::read_to_string(accounts_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("accounts").and_then(Value::as_array).cloned())     
        .unwrap_or_default()
        .into_iter()
        .map(|mut account| {
            // 缺 region 的历史数据按身份/来源推断（手机号账号=国内、
            // 邮箱账号=国际），不能无脑默认国际。
            if account.get("region").is_none() {
                account["region"] = json!(stored_region(&account).as_str());
            }
            account
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
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
    
    // 确保每个账号都有 region、source、is_manual_specified 字段
    let enriched_accounts: Vec<Value> = accounts.iter().map(|acc| {
        let mut acc = acc.clone();
        if acc.get("region").is_none() {
            acc["region"] = json!("international");
        }
        if acc.get("source").is_none() {
            acc["source"] = json!("imported");
        }
        if acc.get("is_manual_specified").is_none() {
            acc["is_manual_specified"] = json!(false);
        }
        acc
    }).collect();
    
    let content = serde_json::to_string_pretty(&json!({
        "accounts": enriched_accounts,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

fn find_account_mut<'a>(
    accounts: &'a mut [Value],
    id: &str,
) -> Option<&'a mut Value> {
    accounts.iter_mut().find(|a| a.get("id").and_then(Value::as_str) == Some(id))
}

/// 解析 access token（JWT）里的 user_id，作为账号稳定 ID。
/// 服务端对手机号/邮箱账号返回数字 uid，字符串解析需兼容数字类型。
fn user_id_from_access_token(access_token: &str) -> Option<String> {
    let raw = access_token.strip_prefix("Bearer ").unwrap_or(access_token);
    let middle = raw.split('.').nth(1)?;
    let payload = general_purpose::URL_SAFE_NO_PAD
        .decode(middle)
        .or_else(|_| general_purpose::URL_SAFE.decode(middle))
        .ok()?;
    let value: Value = serde_json::from_slice(&payload).ok()?;
    value
        .get("user_id")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
}

/// 解析 access token（JWT）里的 jti（登录身份：国际账号 = 邮箱，国内账号 = 手机号）。
fn jti_from_access_token(access_token: &str) -> Option<String> {
    let raw = access_token.strip_prefix("Bearer ").unwrap_or(access_token);
    let middle = raw.split('.').nth(1)?;
    let payload = general_purpose::URL_SAFE_NO_PAD
        .decode(middle)
        .or_else(|_| general_purpose::URL_SAFE.decode(middle))
        .ok()?;
    let value: Value = serde_json::from_slice(&payload).ok()?;
    value
        .get("jti")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 从登录身份特征推断区域：含 `@`（邮箱注册，如国际版 Gmail 账号）→ 国际；
/// 纯数字（国内手机号）→ 国内；无法识别 → None。
pub fn infer_region_from_identity(identity: &str) -> Option<AccountRegion> {
    if identity.contains('@') {
        Some(AccountRegion::International)
    } else if !identity.is_empty() && identity.chars().all(|c| c.is_ascii_digit()) {
        Some(AccountRegion::Domestic)
    } else {
        None
    }
}

/// 账号条目的区域：显式 `region` 字段优先（含手动指定）；缺失时按 JWT jti
/// 推断，再按来源兜底（本机桌面端导入 = 国内，其余 = 国际）。
pub(crate) fn stored_region(account: &Value) -> AccountRegion {    if let Some(r) = account
        .get("region")
        .and_then(Value::as_str)
        .and_then(AccountRegion::from_str)
    {
        return r;
    }
    let access = account.get("accessToken").and_then(Value::as_str).unwrap_or("");
    if let Some(jti) = jti_from_access_token(access) {
        if let Some(r) = infer_region_from_identity(&jti) {
            return r;
        }
    }
    match account.get("source").and_then(Value::as_str) {
        Some("local_snapshot") | Some("local_import") | Some("local-import") => AccountRegion::Domestic,
        _ => AccountRegion::International,
    }
}

/// 本机 auth.json 当前登录账号的区域：优先按 JWT jti 判断，其次按
/// userInfo 的邮箱/手机号（国际版 = 邮箱注册无手机号）；均不可得默认国内。
pub fn local_autoclaw_region() -> AccountRegion {
    if let Ok(token) = read_autoclaw_token() {
        if let Some(jti) = jti_from_access_token(token.trim_start_matches("Bearer ")) {
            if let Some(r) = infer_region_from_identity(&jti) {
                return r;
            }
        }
    }
    if let Some(dir) = autoclaw_data_dir() {
        if let Ok(text) = std::fs::read_to_string(dir.join("auth.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                let email = v.pointer("/userInfo/email").and_then(Value::as_str).unwrap_or("");
                let phone = v
                    .pointer("/userInfo/user_phone")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !email.is_empty() && phone.is_empty() {
                    return AccountRegion::International;
                }
                if !phone.is_empty() {
                    return AccountRegion::Domestic;
                }
            }
        }
    }
    AccountRegion::Domestic
}

/// 解析 access token（JWT）里的过期时间戳（exp，秒级 UNIX 时间戳）。
pub fn jwt_exp_from_access_token(access_token: &str) -> Option<i64> {
    let raw = access_token.strip_prefix("Bearer ").unwrap_or(access_token);
    let middle = raw.split('.').nth(1)?;
    let payload = general_purpose::URL_SAFE_NO_PAD
        .decode(middle)
        .or_else(|_| general_purpose::URL_SAFE.decode(middle))
        .ok()?;
    let value: Value = serde_json::from_slice(&payload).ok()?;
    value.get("exp").and_then(Value::as_i64)
}

/// 调 user-profile 补齐账号信息（uid + 脱敏手机号），失败不影响导入。
/// 必须用账号所属区域的网关：跨区网关对该账号返回 410000。
async fn profile_info_region(
    access_token: &str,
    device_id: &str,
    region: AccountRegion,
) -> Option<(String, String)> {
    let bearer = if access_token.starts_with("Bearer ") {
        access_token.to_string()
    } else {
        format!("Bearer {access_token}")
    };
    let headers = autoclaw_headers(Some(&bearer));
    let body = json!({
        "source_id": "autoclaw",
        "device_id": device_id,
    });
    let resp = http_request(
        &format!("{}/userapi/v1/user-profile", region.api_host()),
        "POST",
        Some(body),
        Some(&headers),
    )
    .await;
    if resp.get("code").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let uid = resp
        .pointer("/data/user_id")
        .and_then(Value::as_str)
        .map(String::from)?;
    let phone = resp
        .pointer("/data/user_phone")
        .or_else(|| resp.pointer("/data/phone"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((uid, phone))
}

/// 本机登录账号的"发现 ID"（`autoclaw:<auth.json userInfo.user_id>`）。
///
/// 这是本机账号的稳定离线身份（与 platform_checkin::discover_autoclaw_account
/// 及既有签到日志一致）；导入快照时记录该 ID，用于"本机登录中隐藏同账号
/// 快照"的合并判定。
pub fn local_discovery_id() -> Option<String> {
    let auth_path = crate::modules::platform_checkin::autoclaw_data_dir()?;
    let text = std::fs::read_to_string(auth_path.join("auth.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let uid = value
        .pointer("/userInfo/user_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(format!("autoclaw:{uid}"))
}

/// 快照是否应被本机实时登录账号合并（隐藏）：
/// 快照记录的 localDiscoveryId 与当前本机发现 ID 一致，或快照 ID 中的
/// uid 与本机发现 ID 的 uid 相同（兼容早期无该字段的快照）。
pub(crate) fn hidden_by_local_login(account: &Value) -> bool {
    hidden_by_local_login_with(account, local_discovery_id())
}

fn hidden_by_local_login_with(account: &Value, local_discovery: Option<String>) -> bool {
    let Some(local_id) = local_discovery else {
        return false;
    };
    if account.get("localDiscoveryId").and_then(Value::as_str) == Some(local_id.as_str()) {
        return true;
    }
    let Some(id) = account.get("id").and_then(Value::as_str) else {
        return false;
    };
    id == local_id.as_str()
}

/// 把本机 `auth.json` 当前登录的账号作为快照加入账号池。
/// 已存在同 uid 账号时返回该账号（幂等），并补记 localDiscoveryId。
pub async fn import_local() -> Result<Value, String> {
    let token = read_autoclaw_token().map_err(|e| format!("本机 AutoClaw 登录态不可用: {e}"))?;
    let legacy = crate::modules::platform_checkin::autoclaw_data_dir()
        .ok_or_else(|| "无法定位 AutoClaw 数据目录".to_string())?
        .join("auth.json");
    // 旧布局解析（可能只剩 deviceId 空壳——新版 AutoClaw 清掉了 token 字段）：
    // deviceId 仍有效（设备指纹没变），refresh 则按有无分别处理。
    let (legacy_refresh, legacy_device_id) = std::fs::read_to_string(&legacy)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| {
            (
                v.get("refreshToken")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                v.get("deviceId")
                    .and_then(Value::as_str)
                    .map(String::from),
            )
        })
        .unwrap_or((None, None));
    // 旧布局：auth.json 的 enc refreshToken → 解密；新版布局：旧文件只剩
    // deviceId 空壳（AutoClaw 更新后登录态迁到 account-credentials.enc），
    // refresh 从 enc 明文 JSON 取——「auth.json 缺少 refreshToken」由此修复。
    let refresh_token = match legacy_refresh {
        Some(enc) if !enc.is_empty() => autoclaw_decrypt_token(&enc)?,
        _ => {
            #[cfg(target_os = "windows")]
            {
                let (_, refresh, _) =
                    crate::modules::platform_checkin::autoclaw_official_credentials()
                        .map_err(|e| format!("auth.json 缺少 refreshToken 且新版凭据不可用: {e}"))?;
                if refresh.is_empty() {
                    return Err("新版 AutoClaw 凭据里没有 refreshToken：请打开 AutoClaw 客户端确认登录后重试".to_string());
                }
                refresh
            }
            #[cfg(not(target_os = "windows"))]
            {
                return Err("auth.json 缺少 refreshToken".to_string());
            }
        }
    };
    let refresh_token = refresh_token.trim_start_matches("Bearer ").to_string();
    let device_id = legacy_device_id.unwrap_or_default();

    let (uid, phone) = profile_info_region(&token, &device_id, local_autoclaw_region())
        .await
        .map(|(uid, phone)| (Some(uid), Some(phone)))
        .unwrap_or((None, None));
    let jwt_uid = user_id_from_access_token(&token);
    let id = format!(
        "autoclaw:{}",
        uid.clone()
            .or(jwt_uid)
            .unwrap_or_else(|| general_purpose::STANDARD.encode(uuid::Uuid::new_v4()))
    );
    let label = phone.unwrap_or_else(|| "本机账号".to_string());

    let local_discovery = local_discovery_id();
    let account = json!({
        "id": id,
        "label": label,
        "accessToken": token.trim_start_matches("Bearer ").to_string(),
        "refreshToken": refresh_token.trim_start_matches("Bearer ").to_string(),
        "deviceId": device_id,
        "source": "local_snapshot",
        // 本机快照不再硬编码国内：按当前登录账号身份推断（本机登录国际
        // 账号时快照同样是国际版），推断不出才回退国内。
        "region": local_autoclaw_region().as_str(),
        "localDiscoveryId": local_discovery.clone().map(|v| json!(v)).unwrap_or(Value::Null),
        "status": "ok",
        "addedAt": now_ms(),
        "updatedAt": now_ms(),
    });

    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(existing) = find_account_mut(&mut accounts, &id) {
        // 幂等：同 uid 已在池中，则刷新快照。
        for (key, value) in account.as_object().unwrap() {
            if key == "addedAt" {
                continue;
            }
            existing[key] = value.clone();
        }
        existing["source"] = json!(existing
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("local-import"));
        if let Some(ld) = local_discovery {
            existing["localDiscoveryId"] = json!(ld);
        }
        let snapshot = existing.clone();
        save_accounts(&accounts)?;
        return Ok(snapshot);
    }
    accounts.push(account.clone());
    save_accounts(&accounts)?;
    Ok(account)
}

/// 手动导入账号（access + refresh token 必填）。
/// 区域优先级：调用方显式指定 > JWT jti 身份推断 > 国际版（用户规则：
/// OAuth/手动导入/抓包默认国际；token 是手机号账号时自动纠正为国内）。
pub async fn add_account(
    label: Option<String>,
    access_token: &str,
    refresh_token: &str,
    device_id: Option<String>,
) -> Result<Value, String> {
    add_account_v2(label, access_token, refresh_token, device_id, None, AccountSource::ManualImport).await
}

/// 构造 intl 条目的 intlMeta 附加字段（契约 docs/autoclaw-intl-pool-contract.md §2.1）：
/// - sourceEmail：JWT jti（注册邮箱，幂等去重键）；
/// - tokenExpireAt：accessToken JWT exp（秒级）；
/// - registeredVia：入池来源标记（OAuth 管线 = "oauth-pipeline"）；
/// - deviceIsolated：该号是否独立虚拟 deviceId。默认 false（当前首号绑真
///   设备 = 契约 §4 已知风险；是否虚拟化由注册管线掌握，M3 接入时补传）。
///
/// 国内条目不构造该对象（调用方按 region 判定），国内路径零改动。
fn build_intl_meta(access_token: &str, source: AccountSource) -> Value {
    json!({
        "sourceEmail": jti_from_access_token(access_token).unwrap_or_default(),
        "registeredVia": if source == AccountSource::OAuth { "oauth-pipeline" } else { source.as_str() },
        "tokenExpireAt": jwt_exp_from_access_token(access_token),
        "deviceIsolated": false,
    })
}

/// intl 号入库成功后同步初始化轮换池状态（契约 §3.1：PoolState.accounts
/// 初始化 state=active → events「join」）。国内条目直接跳过，零改动。
fn join_intl_pool(account: &Value) {
    let Some(id) = account.get("id").and_then(Value::as_str) else {
        return;
    };
    if account.get("region").and_then(Value::as_str) != Some("international") {
        return;
    }
    let source_email = account
        .pointer("/intlMeta/sourceEmail")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let token_expire_at = account.pointer("/intlMeta/tokenExpireAt").and_then(Value::as_i64);
    let device_isolated = account
        .pointer("/intlMeta/deviceIsolated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    crate::modules::autoclaw_intl_pool::pool_manager()
        .join(id, &source_email, token_expire_at, device_isolated);
}

/// 带区域和来源元数据的账号导入（region 传 None 时按身份自动归类）。
pub async fn add_account_v2(
    label: Option<String>,
    access_token: &str,
    refresh_token: &str,
    device_id: Option<String>,
    region: Option<AccountRegion>,
    source: AccountSource,
) -> Result<Value, String> {
    let access_token = access_token.trim().trim_start_matches("Bearer ").to_string();
    let refresh_token = refresh_token.trim().trim_start_matches("Bearer ").to_string();
    if access_token.is_empty() || refresh_token.is_empty() {
        return Err("access token 与 refresh token 均不能为空".to_string());
    }
    // 先定区域（决定去哪个网关验证 token）：显式指定 > 身份推断 > 国际。
    let inferred = jti_from_access_token(&access_token)
        .as_deref()
        .and_then(infer_region_from_identity);
    let region = region.or(inferred).unwrap_or(AccountRegion::International);
    let device_id = device_id.unwrap_or_default();
    let (uid, phone) = profile_info_region(&access_token, &device_id, region)
        .await
        .map(|(uid, phone)| (Some(uid), Some(phone)))
        .unwrap_or((user_id_from_access_token(&access_token), None));
    if uid.is_none() {
        return Err("无法从 token 解析账号身份（token 可能无效），已取消导入".to_string());
    }
    let id = format!("autoclaw:{}", uid.unwrap());
    let display = label
        .filter(|l| !l.trim().is_empty())
        .or(phone)
        .unwrap_or_else(|| "导入账号".to_string());

    // 契约 §2.1：intl 条目补 intlMeta；国内条目不写（零改动）。
    let intl_meta = (region == AccountRegion::International)
        .then(|| build_intl_meta(&access_token, source));

    let account = json!({
        "id": id,
        "label": display,
        "accessToken": access_token,
        "refreshToken": refresh_token,
        "deviceId": device_id,
        "source": source.as_str(),
        "region": region.as_str(),
        "is_manual_specified": false,
        "status": "ok",
        "addedAt": now_ms(),
        "updatedAt": now_ms(),
    });

    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(existing) = find_account_mut(&mut accounts, &id) {
        // intlMeta 字段级合并：重复入池=更新令牌（契约 §3.1），既有
        // deviceIsolated/welcomePoints 等保留，其余键以本次为准。
        if let Some(meta) = &intl_meta {
            let mut merged = existing.get("intlMeta").cloned().unwrap_or(json!({}));
            if let (Some(dst), Some(src)) = (merged.as_object_mut(), meta.as_object()) {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
            existing["intlMeta"] = merged;
        }
        for (key, value) in account.as_object().unwrap() {
            if key == "addedAt" {
                continue;
            }
            existing[key] = value.clone();
        }
        let snapshot = existing.clone();
        save_accounts(&accounts)?;
        join_intl_pool(&snapshot);
        return Ok(snapshot);
    }
    let mut account = account;
    if let Some(meta) = &intl_meta {
        account["intlMeta"] = meta.clone();
    }
    accounts.push(account.clone());
    save_accounts(&accounts)?;
    join_intl_pool(&account);
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

fn update_stored_tokens(id: &str, access_token: &str, refresh_token: &str) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let Some(account) = find_account_mut(&mut accounts, id) else {
        return Err("账号不存在".to_string());
    };
    account["accessToken"] =
        json!(access_token.trim_start_matches("Bearer ").to_string());
    account["refreshToken"] =
        json!(refresh_token.trim_start_matches("Bearer ").to_string());
    account["updatedAt"] = json!(now_ms());
    account["status"] = json!("ok");
    account["lastError"] = Value::Null;
    // intl 条目同步刷新 intlMeta.tokenExpireAt（契约 §2.1：随 accessToken
    // JWT exp 更新，供轮换池的 token 到期过滤/续期扫描使用）。
    if account.get("intlMeta").is_some() {
        if let Some(exp) = jwt_exp_from_access_token(access_token) {
            account["intlMeta"]["tokenExpireAt"] = json!(exp);
        }
    }
    save_accounts(&accounts)
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

/// 用账号自身的 refresh token 换新登录态并写回账号池。
pub async fn refresh_stored_account(id: &str) -> Result<String, String> {
    let account = {
        let accounts = load_accounts();
        accounts
            .iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
            .cloned()
            .ok_or_else(|| "账号不存在".to_string())?
    };
    let refresh_token = account
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let device_id = account
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // 网络错误（code == -1）按瞬时故障退避重试；仅服务端明确拒绝鉴权时
    // 才标记失效——瞬时网络抖动不应永久废掉账号。
    // 续期网关按账号所属区域路由（跨区网关会报 410000）。
    let region = stored_region(&account);
    let mut last_error = String::from("未知错误");
    let mut new_access = String::new();
    let mut attempt = 0u32;
    loop {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(800 * (1 << attempt))).await;
        }
        let resp = http_request(
            &format!("{}/userapi/v1/refresh", region.api_host()),
            "POST",
            Some(json!({
                "source_id": "autoclaw",
                "device_id": device_id,
                "refresh_token": refresh_token,
            })),
            Some(&autoclaw_headers(None)),
        )
        .await;
        let code = resp.get("code").and_then(Value::as_i64);
        if code == Some(0) {
            new_access = resp
                .pointer("/data/access_token")
                .or_else(|| resp.pointer("/data/token"))
                .and_then(Value::as_str)
                .ok_or_else(|| "刷新响应缺少 access_token".to_string())?
                .to_string();
            break;
        }
        let msg = resp
            .get("msg")
            .or_else(|| resp.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("刷新被拒绝")
            .to_string();
        if code == Some(-1) || msg.contains("error sending request") {
            // 网络错误：瞬时故障，重试且不标记失效。
            last_error = msg;
        } else {
            let lower = msg.to_ascii_lowercase();
            let auth_related = ["token", "令牌", "过期", "失效", "invalid", "expired", "unauthorized"]
                .iter()
                .any(|k| lower.contains(k));
            if auth_related {
                // refresh token 确认无效：需要重新导入，标记失效。
                mark_account_error(id, &msg);
                return Err(format!("刷新令牌已失效：{msg}"));
            }
            last_error = msg;
        }
        attempt += 1;
        if attempt >= 3 {
            return Err(format!(
                "刷新暂时不可用（{last_error}）；账号未被标记失效，稍后将自动重试"
            ));
        }
    }
    update_stored_tokens(id, &new_access, &refresh_token)?;
    Ok(new_access.trim_start_matches("Bearer ").to_string())
}

/// 尝试复活此前被标记失效的账号（每个失效账号续期一次）。
/// 供每日签到循环调用：瞬时网络故障导致的"失效"会在后续循环自动恢复。
pub async fn recover_invalid_accounts() -> usize {
    let invalid: Vec<String> = load_accounts()
        .iter()
        .filter(|a| a.get("status").and_then(Value::as_str) == Some("invalid"))
        .filter_map(|a| a.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    let mut recovered = 0usize;
    for id in invalid {
        if refresh_stored_account(&id).await.is_ok() {
            recovered += 1;
        }
    }
    recovered
}

/// 账号列表（管理界面用）：本机实时登录账号 + 池内快照。
///
/// 合并规则与操作顺序无关：本机当前登录的账号永远显示为「本机登录」，
/// 与其同一身份（localDiscoveryId 匹配）的快照隐藏——因此
/// 「先导入再换登录」与「先换登录再导入」得到的列表完全一致。
pub fn list_for_display() -> Vec<Value> {
    let mut out = Vec::new();
    // 1) 本机实时账号（离线读取 auth.json，无需网络）。
    // 区域按当前登录账号身份动态推断——本机登录国际账号（如 Gmail）时
    // 必须显示国际版，不能再硬编码国内。
    if let Some(entry) = crate::modules::platform_checkin::discover_autoclaw_account() {
        out.push(json!({
            "id": entry.get("id").cloned().unwrap_or(Value::Null),
            "label": entry.get("email").cloned().unwrap_or(Value::Null),
            "source": "local-live",
            "region": local_autoclaw_region().as_str(),
            "status": "ok",
            "lastError": Value::Null,
            "addedAt": Value::Null,
            "updatedAt": Value::Null,
            "tokenPreview": Value::Null,
        }));
    }
    // 2) 池内快照：与本机实时账号同身份的隐藏。
    for account in load_accounts() {
        if hidden_by_local_login(&account) {
            continue;
        }
        out.push(json!({
            "id": account.get("id").cloned().unwrap_or(Value::Null),
            "label": account.get("label").cloned().unwrap_or(Value::Null),
            "source": account.get("source").cloned().unwrap_or(Value::Null),
            "region": account.get("region").cloned().unwrap_or(json!("international")),
            "is_manual_specified": account.get("is_manual_specified").cloned().unwrap_or(json!(false)),
            "status": account.get("status").cloned().unwrap_or(json!("ok")),
            "lastError": account.get("lastError").cloned().unwrap_or(Value::Null),
            "addedAt": account.get("addedAt").cloned().unwrap_or(Value::Null),
            "updatedAt": account.get("updatedAt").cloned().unwrap_or(Value::Null),
            "tokenPreview": account.get("tokenPreview").cloned().unwrap_or(Value::Null),
        }));
    }
    // 1.5.0：附带账号保护层冷却状态（封禁 410004 / 限流落库后，账号管理与
    // 渠道健康的冷却徽标都从这里取）
    for entry in out.iter_mut() {
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            entry["isCooling"] = json!(crate::modules::account_protection::is_account_cooling(id));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 账号池（GLM 代理轮询 + 批量签到共用）
// ---------------------------------------------------------------------------

/// 代理/签到用的单个账号凭证。`store_id` 为 None 表示本机 auth.json 账号
/// （刷新走 platform_checkin），Some(id) 表示账号池条目（刷新写回存储）。
#[derive(Clone, Debug)]
pub(crate) struct PoolAccount {
    pub id: String,
    pub label: String,
    pub access_token: String,
    pub refresh_token: String,
    pub device_id: String,
    pub store_id: Option<String>,
    /// 账号所属区域：签到/续期/代理必须路由到该区域的网关。
    pub region: AccountRegion,
}

impl PoolAccount {
    /// 是否本机 auth.json 当前登录账号（`store_id` 为 None 即本机账号，
    /// 池内快照为 Some(id)）。代理选号策略用。
    pub(crate) fn is_local(&self) -> bool {
        self.store_id.is_none()
    }

    pub(crate) fn bearer(&self) -> String {
        format!("Bearer {}", self.access_token.trim_start_matches("Bearer "))
    }

    /// 401 时刷新该账号登录态，返回新 access token。
    pub(crate) async fn refresh(&self) -> Result<String, String> {
        match &self.store_id {
            Some(id) => refresh_stored_account(id).await,
            None => {
                crate::modules::platform_checkin::autoclaw_refresh_access_token().await
            }
        }
    }

    pub(crate) fn mark_invalid(&self, error: &str) {
        if let Some(id) = &self.store_id {
            mark_account_error(id, error);
        }
    }
}

/// 账号池：本机 auth.json 登录账号优先，其后是账号池条目；
/// 按访问 token 的 user_id 去重（同一账号不重复参与轮询）。
/// 每个条目携带所属区域，签到/代理按区域过滤与路由。
pub(crate) fn pool_accounts() -> Vec<PoolAccount> {
    let mut pool: Vec<PoolAccount> = Vec::new();
    let mut seen_user_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(local) = local_pool_account() {
        if let Some(uid) = user_id_from_access_token(&local.access_token) {
            seen_user_ids.insert(uid);
        }
        pool.push(local);
    }
    for account in load_accounts() {
        if account.get("status").and_then(Value::as_str) == Some("invalid") {
            continue;
        }
        // 本机登录中：与实时账号同身份的快照不参与池（登录态切换后自动回归）。
        if hidden_by_local_login(&account) {
            continue;
        }
        let access_token = account
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if access_token.is_empty() {
            continue;
        }
        let uid = user_id_from_access_token(&access_token);
        if let Some(uid) = &uid {
            if !seen_user_ids.insert(uid.clone()) {
                continue; // 与本机登录/更早条目为同一账号。
            }
        }
        pool.push(PoolAccount {
            id: account
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            label: account
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("账号")
                .to_string(),
            access_token,
            refresh_token: account
                .get("refreshToken")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            device_id: account
                .get("deviceId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            store_id: account
                .get("id")
                .and_then(Value::as_str)
                .map(String::from),
            region: stored_region(&account),
        });
    }
    pool
}

/// 按区域过滤账号池（None = 全部）。签到与代理的区域隔离入口。
pub(crate) fn pool_accounts_filtered(region: Option<AccountRegion>) -> Vec<PoolAccount> {
    pool_accounts()
        .into_iter()
        .filter(|a| region.map_or(true, |r| a.region == r))
        .collect()
}

/// 按访问 token 匹配账号区域（本机/池内精确比对；JWT 可解析时按 jti 优先）。
/// 供钱包/流水查询按账号路由网关；无法匹配时回退国内（向后兼容旧数据）。
pub fn region_for_token(token: &str) -> AccountRegion {
    let raw = token.trim().trim_start_matches("Bearer ");
    if let Some(jti) = jti_from_access_token(raw) {
        if let Some(r) = infer_region_from_identity(&jti) {
            return r;
        }
    }
    for account in load_accounts() {
        let at = account.get("accessToken").and_then(Value::as_str).unwrap_or("");
        if at.trim_start_matches("Bearer ") == raw {
            return stored_region(&account);
        }
    }
    if let Some(local) = local_pool_account() {
        if local.access_token.trim_start_matches("Bearer ") == raw {
            return local.region;
        }
    }
    AccountRegion::Domestic
}

fn local_pool_account() -> Option<PoolAccount> {
    let access_token = read_autoclaw_token().ok()?;
    let device_id = autoclaw_data_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join("auth.json")).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v.get("deviceId")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_default();
    Some(PoolAccount {
        id: local_discovery_id().unwrap_or_else(|| "autoclaw:local".to_string()),
        label: "本机登录".to_string(),
        access_token: access_token.trim_start_matches("Bearer ").to_string(),
        refresh_token: String::new(),
        device_id,
        store_id: None,
        region: local_autoclaw_region(),
    })
}

/// 池内单账号签到：查状态 → 未签到则领取；401 刷新重试一次。
pub async fn checkin_stored(id: &str) -> Value {
    let Some(account) = load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    else {
        return json!({"result": "error", "error": "账号不存在"});
    };
    let mut access_token = account
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        return json!({"result": "error", "error": "账号缺少 token"});
    }
    let invalid_hint = "请重新导入该账号的 token";

    // 状态/签到请求按账号区域路由：get_autoclaw_status_with 内部按 token
    // 自动解析区域网关（region_for_token）。
    let mut status = crate::modules::platform_checkin::get_autoclaw_status_with(&access_token).await;
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        // 登录态可能过期：续期后重试一次。
        let refreshed = refresh_stored_account(id).await;
        match refreshed {
            Ok(new_token) => {
                access_token = new_token;
                status = crate::modules::platform_checkin::get_autoclaw_status_with(&access_token)
                    .await;
            }
            Err(e) => {
                return json!({"result": "error", "error": format!("登录态续期失败：{e}；{invalid_hint}")});
            }
        }
    }
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
    let res = crate::modules::platform_checkin::perform_autoclaw_checkin_with(&access_token).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_by_local_login_matches_discovery_id_and_uid() {
        let snapshot = json!({
            "id": "autoclaw:eb5b1479",
            "localDiscoveryId": "autoclaw:1daf28d6",
        });
        // 本机登录的是同一账号（发现 ID 一致）→ 隐藏。
        assert!(super::hidden_by_local_login_with(
            &snapshot,
            Some("autoclaw:1daf28d6".to_string())
        ));
        // 本机登录的是另一账号 → 不隐藏。
        assert!(!super::hidden_by_local_login_with(
            &snapshot,
            Some("autoclaw:other".to_string())
        ));
        // 本机未登录 → 不隐藏（快照继续参与签到/代理）。
        assert!(!super::hidden_by_local_login_with(&snapshot, None));
        // 快照 ID 本身等于本机发现 ID（早期无 localDiscoveryId 字段的快照）→ 隐藏。
        let legacy = json!({"id": "autoclaw:1daf28d6"});
        assert!(super::hidden_by_local_login_with(
            &legacy,
            Some("autoclaw:1daf28d6".to_string())
        ));
    }

    #[test]
    fn jwt_user_id_extraction() {
        // header.payload.signature 构造（payload 含 user_id）。
        let payload = general_purpose::URL_SAFE_NO_PAD.encode(br#"{"user_id":"560128"}"#);
        let token = format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig");
        assert_eq!(user_id_from_access_token(&token), Some("560128".to_string()));
        assert_eq!(user_id_from_access_token("not-a-jwt"), None);
    }
}

// ---------------------------------------------------------------------------
// 区域隔离功能 (v1.0+)
// ---------------------------------------------------------------------------

/// 手动指定账号归属区域（防误判；写入 is_manual_specified 后自动推断不再覆盖）。
pub fn set_account_region(id: &str, region: AccountRegion) -> Result<Value, String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let snapshot = {
        let Some(account) = find_account_mut(&mut accounts, id) else {
            return Err("账号不存在".to_string());
        };
        account["region"] = json!(region.as_str());
        account["is_manual_specified"] = json!(true);
        account["updatedAt"] = json!(now_ms());
        account.clone()
    };
    save_accounts(&accounts)?;
    Ok(snapshot)
}

/// 迁移旧数据（无 region 字段）：按 JWT jti 身份推断（邮箱=国际、
/// 手机号=国内），推断不出再按来源兜底（本机桌面端导入=国内）。
pub fn migrate_accounts_data() -> Result<usize, String> {
    let accounts = load_accounts();
    let mut migrated_count = 0;

    let migrated_accounts: Vec<Value> = accounts.into_iter().map(|mut acc| {
        if acc.get("region").is_none() {
            acc["region"] = json!(stored_region(&acc).as_str());
            if acc.get("is_manual_specified").is_none() {
                acc["is_manual_specified"] = json!(false);
            }
            migrated_count += 1;
        }
        acc
    }).collect();

    if migrated_count > 0 {
        save_accounts(&migrated_accounts)?;
        eprintln!("成功迁移 {migrated_count} 个账号到新版本格式（添加 region 字段）");
    }

    Ok(migrated_count)
}

/// 根据来源设置默认区域
pub fn set_default_region_by_source(source: &AccountSource) -> AccountRegion {
    AccountRegion::default_for_source(source)
}
