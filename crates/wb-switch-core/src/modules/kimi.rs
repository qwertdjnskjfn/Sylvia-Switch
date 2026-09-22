//! Kimi 融合网关（移植自 chopper1026/kimi2api，Kimi Web 协议逆向封装）。
//!
//! 把多个 Kimi Web 账号（refresh token / access token）汇聚为 OpenAI 兼容
//! 接口（`/kimi/v1/chat/completions` + `/kimi/v1/models`），带账号池调度：
//! 每账号独立并发上限与最小请求间隔、429/5xx 冷却、401/403 标记异常、
//! refresh token 自动换 access token 并缓存。
//!
//! 协议要点（kimi2api 源码逆向，2026-09-15 核对）：
//! - chat：`POST https://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/Chat`，
//!   `Content-Type: application/connect+json`，body = connect 帧
//!   `0x00 + 4字节大端长度 + JSON`；响应为流式 connect 帧（同帧格式，
//!   `flag & 0x80` 的帧为压缩/结尾，跳过）；事件 JSON 里
//!   `block.think.content` 为推理增量、`block.text.content` 为正文增量、
//!   `done` 字段结束；`chat.id` / `message.id` 用于续写上下文；
//! - 模型目录：`POST /apiv2/kimi.gateway.config.v1.ConfigService/GetAvailableModels`
//!   （标准 JSON 接口：`Content-Type: application/json` + 空体 `{}`；
//!   **GET 会被判 405** —— 2026-09-16 实测）；
//! - 刷新：`GET /api/auth/token/refresh`（Bearer = refresh token）→
//!   `{access_token}`（JWT，exp 为有效期）；
//! - headers：固定伪装头（Chrome 131 / X-Msh-Platform: web）+
//!   `X-Msh-Device-Id`（19 位数字）/ `X-Msh-Session-Id`。
//!
//! 账号池落盘 `~/.wb-switch/kimi_accounts.json`；本服务不存对话历史，
//! 每次请求按 OpenAI messages 全量格式化为单条 user 消息（与 kimi2api 一致）。

use base64::engine::general_purpose;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};

const KIMI_BASE: &str = "https://www.kimi.com";
const KIMI_CHAT_PATH: &str = "/apiv2/kimi.gateway.chat.v1.ChatService/Chat";
const KIMI_MODELS_PATH: &str = "/apiv2/kimi.gateway.config.v1.ConfigService/GetAvailableModels";
const KIMI_REFRESH_PATH: &str = "/api/auth/token/refresh";
const KIMI_SCENARIO_DEFAULT: &str = "SCENARIO_K2D5";
/// 模型目录缓存。
const MODEL_CATALOG_TTL_MS: i64 = 5 * 60 * 1000;
/// 默认调度参数（与 kimi2api .env 默认一致）。
const DEFAULT_MAX_CONCURRENCY: usize = 2;
const DEFAULT_MIN_INTERVAL_MS: u64 = 500;
/// 5xx/网络错误冷却；429 用 Retry-After（缺省 60s）。
const COOLDOWN_5XX_MS: i64 = 30_000;
const COOLDOWN_429_MS: i64 = 60_000;

static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 上游「空闲读」上限（秒）：连续这么久收不到任何字节才判定断流。
///
/// 🔴 聊天端点是 SSE 流：原先请求级 `.timeout(120s)` 算的是「连接 + 响应体
/// 读完」的**总时长**，长回答会被掐断，且 reqwest 只回
/// `error decoding response body`（看起来像上游断流）。
/// 改为 client 层 `read_timeout`：只在连续无数据时触发，读到数据即重置。
const UPSTREAM_READ_IDLE_SECS: u64 = 120;

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy() // 国内上游直连，不继承系统代理
            .connect_timeout(std::time::Duration::from_secs(20))
            .read_timeout(std::time::Duration::from_secs(UPSTREAM_READ_IDLE_SECS))
            .build()
            .expect("failed to build kimi client")
    })
}

/// 凭据校验专用客户端：比业务请求更短的超时。
///
/// 2026-09-16 实测：业务客户端 `connect_timeout=20s` 会让一次网络抖动
/// 把「同步 token」整整卡 21 秒才报错。校验是交互路径（扩展点一下要等
/// 结果），这里压到 8s 连接 / 20s 总超时，配合重试，把最坏耗时控制在
/// 约 25 秒内，同时不再误报成「凭据失效」。
fn verify_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(8))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("failed to build kimi verify client")
    })
}

/// 校验时的网络重试次数（含首次）。
const VERIFY_ATTEMPTS: u32 = 3;

/// 传输层错误的退避间隔（毫秒）。
fn retry_backoff_ms(attempt: u32) -> u64 {
    400 * u64::from(attempt) + 400
}

/// Kimi 上游错误分类。
///
/// 🔴 **关键区分：传输层失败（DNS/TCP/TLS/超时）不等于凭据失效。**
/// 2026-09-16 实测：一次瞬时网络抖动（`error sending request`，21s）被
/// `verify_account` 直接报成「验证失败」，用户看到的是「凭据全都不行」，
/// 而真实原因是网络瞬时不可达 —— 归因错了就会去重新登录，白折腾。
///
/// 另外，旧代码把**任何**非 2xx 都当成「token 已失效」并落盘 `status=invalid`，
/// 而扩展的巡检（`sweepInvalid`）会据此**删除账号** —— 一个 405/500 就能
/// 把好账号清掉。现在只有 401/403 允许落盘标记失效。
#[derive(Debug, Clone)]
pub enum KimiUpstreamError {
    /// 传输层失败（DNS/TCP/TLS/超时）——凭据未必失效，可重试。
    Transport(String),
    /// 上游明确拒绝（401/403）——凭据确实失效。
    Unauthorized(String),
    /// 其他上游错误（5xx / 429 / 协议 / 解析）。
    Upstream(String),
    /// 本地问题（账号不存在、参数缺失）。
    Local(String),
}

impl KimiUpstreamError {
    /// 机器可读的错误类别，供前端分级展示。
    pub fn kind(&self) -> &'static str {
        match self {
            KimiUpstreamError::Transport(_) => "upstream_unreachable",
            KimiUpstreamError::Unauthorized(_) => "credential_invalid",
            KimiUpstreamError::Upstream(_) => "upstream_error",
            KimiUpstreamError::Local(_) => "local_error",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            KimiUpstreamError::Transport(m)
            | KimiUpstreamError::Unauthorized(m)
            | KimiUpstreamError::Upstream(m)
            | KimiUpstreamError::Local(m) => m,
        }
    }

    /// 是否值得让用户直接重试（网络类错误）。
    pub fn retryable(&self) -> bool {
        matches!(self, KimiUpstreamError::Transport(_))
    }

    /// 映射到对外 HTTP 状态码。
    pub fn http_status(&self) -> reqwest::StatusCode {
        match self {
            KimiUpstreamError::Transport(_) => reqwest::StatusCode::SERVICE_UNAVAILABLE,
            KimiUpstreamError::Unauthorized(_) => reqwest::StatusCode::UNAUTHORIZED,
            KimiUpstreamError::Upstream(_) => reqwest::StatusCode::BAD_GATEWAY,
            KimiUpstreamError::Local(_) => reqwest::StatusCode::NOT_FOUND,
        }
    }

    /// 结构化 JSON，前端可按 `code` 分流文案。
    pub fn to_value(&self) -> Value {
        json!({
            "ok": false,
            "code": self.kind(),
            "error": self.message(),
            "retryable": self.retryable(),
        })
    }
}

/// 让旧的 `Result<_, String>` 调用点继续用 `?`。
impl From<KimiUpstreamError> for String {
    fn from(e: KimiUpstreamError) -> String {
        e.message().to_string()
    }
}

/// 由 HTTP 状态码判定错误类别：只有 401/403 算「凭据失效」。
fn classify_status(status: reqwest::StatusCode, msg: String) -> KimiUpstreamError {
    if matches!(status.as_u16(), 401 | 403) {
        KimiUpstreamError::Unauthorized(msg)
    } else {
        KimiUpstreamError::Upstream(msg)
    }
}

// ---------------------------------------------------------------------------
// 账号池（~/.wb-switch/kimi_accounts.json）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct KimiAccount {
    pub id: String,
    pub name: String,
    pub raw_token: String,
    pub enabled: bool,
    pub max_concurrency: usize,
    pub min_interval_ms: u64,
    pub device_id: String,
    /// refresh token 换出的 access token 缓存。
    pub cached_access_token: String,
    /// 缓存 token 的过期时间（秒级 epoch，来自 JWT exp）。
    pub access_expires_at: i64,
    /// 冷却截止（毫秒）。
    pub cooldown_until: i64,
    pub status: String,
    pub error: String,
}

fn accounts_path() -> PathBuf {
    store_dir().join("kimi_accounts.json")
}

fn load_raw() -> Vec<Value> {
    std::fs::read_to_string(accounts_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn save_raw(accounts: &[Value]) -> Result<(), String> {
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

fn account_from_json(v: &Value) -> KimiAccount {
    KimiAccount {
        id: v.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
        name: v.get("name").and_then(Value::as_str).unwrap_or("Kimi").to_string(),
        raw_token: v.get("raw_token").and_then(Value::as_str).unwrap_or_default().to_string(),
        enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        max_concurrency: v
            .get("max_concurrency")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|n| *n >= 1)
            .unwrap_or(DEFAULT_MAX_CONCURRENCY),
        min_interval_ms: v
            .get("min_interval_seconds")
            .and_then(Value::as_f64)
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(DEFAULT_MIN_INTERVAL_MS),
        device_id: v
            .get("device_id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(new_device_id),
        cached_access_token: v
            .get("cached_access_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        access_expires_at: v
            .get("access_expires_at")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        cooldown_until: 0,
        status: v.get("status").and_then(Value::as_str).unwrap_or("ok").to_string(),
        error: v.get("error").and_then(Value::as_str).unwrap_or_default().to_string(),
    }
}

fn account_to_json(a: &KimiAccount) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "raw_token": a.raw_token,
        "enabled": a.enabled,
        "max_concurrency": a.max_concurrency,
        "min_interval_seconds": a.min_interval_ms as f64 / 1000.0,
        "device_id": a.device_id,
        "cached_access_token": a.cached_access_token,
        "access_expires_at": a.access_expires_at,
        "status": a.status,
        "error": a.error,
    })
}

pub(crate) fn load_accounts() -> Vec<KimiAccount> {
    load_raw().iter().map(account_from_json).collect()
}

fn persist_account(updated: &KimiAccount) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut raw = load_raw();
    if let Some(slot) = raw
        .iter_mut()
        .find(|v| v.get("id").and_then(Value::as_str) == Some(updated.id.as_str()))
    {
        *slot = account_to_json(updated);
    }
    let _ = save_raw(&raw);
}

/// 新增账号（refresh token 或 JWT access token）。
///
/// 去重规则（按优先级）：
/// 1. **同一 Kimi 用户（JWT `sub` 相同）** → 覆盖更新其 `raw_token`，不新增条目。
///    因为 refresh token 每次刷新都会轮换，只有 `sub` 才是稳定身份。
/// 2. 完全相同的 token 字符串 → 直接报「已在账号池中」。
pub fn add_account(
    name: Option<&str>,
    raw_token: &str,
    device_id: Option<&str>,
) -> Result<Value, String> {
    let token = raw_token.trim();
    if token.len() < 20 {
        return Err("token 内容过短，请粘贴完整的 refresh token 或 access token".to_string());
    }
    // 设备 id 优先级：调用方显式传入 > token 自带 claims > 本地生成
    let resolved_device_id = device_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| jwt_device_id(token))
        .unwrap_or_else(new_device_id);

    let _guard = WRITE_LOCK.lock().unwrap();
    let mut raw = load_raw();

    // ① 同一用户（sub 相同）→ 覆盖更新，避免轮换产生重复账号
    let incoming_sub = jwt_subject(token);
    if let Some(sub) = incoming_sub.as_deref() {
        if let Some(slot) = raw.iter_mut().find(|v| {
            v.get("raw_token")
                .and_then(Value::as_str)
                .and_then(jwt_subject)
                .as_deref()
                == Some(sub)
        }) {
            let existing = slot
                .get("raw_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let same_token = existing == token;
            // 🔴 防回退：refresh token 每次刷新都会轮换，浏览器里存的可能是**更旧的一代**。
            // 若把旧值写回，宽限期一过账号就废了 —— 只在来者不更旧时才覆盖。
            let incoming_iat = jwt_issued_at(token);
            let existing_iat = jwt_issued_at(&existing);
            let stale = !same_token
                && incoming_iat > 0
                && existing_iat > 0
                && incoming_iat < existing_iat;
            // 🔴 防降级（2026-09-16 补）：access token 只有十几分钟寿命，refresh token 是长期的。
            // 旧代码只比 iat，而 access token 的 iat 通常**更新**（上游每次刷新都会签发一个），
            // 于是「浏览器里抓到的 access token」会把池里的 refresh token 冲掉 ——
            // 十几分钟后账号必然失效，且再也无法自动续期（access token 没有 refresh 能力）。
            // 同一用户下 refresh token 的优先级永远更高。
            let downgrade = !same_token
                && !stale
                && detect_token_type(token) == "jwt"
                && detect_token_type(&existing) == "refresh";
            let reason = if same_token {
                "already-synced"
            } else if stale {
                "stale-ignored"
            } else if downgrade {
                // 保留 raw_token（refresh）；顺手把这个更新的 access token 存进缓存，
                // 既拿到了新鲜凭据，又没丢掉续期能力。
                let incoming_exp = jwt_expires_at(token);
                let cached_exp = slot
                    .get("access_expires_at")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if incoming_exp > cached_exp {
                    slot["cached_access_token"] = json!(token);
                    slot["access_expires_at"] = json!(incoming_exp);
                }
                "downgrade-ignored"
            } else {
                if !same_token {
                    slot["raw_token"] = json!(token);
                }
                "token-rotated"
            };
            if let Some(n) = name {
                slot["name"] = json!(n);
            }
            // 设备 id 只在原先为空时补齐，避免覆盖已生效的值
            let has_device = slot
                .get("device_id")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !has_device {
                slot["device_id"] = json!(resolved_device_id);
            }
            let entry = slot.clone();
            save_raw(&raw)?;
            return Ok(json!({
                "updated": reason == "token-rotated",
                "reason": reason,
                "account": entry,
            }));
        }
    }

    // ② 完全相同的 token 字符串 → 视为已存在
    if raw
        .iter()
        .any(|v| v.get("raw_token").and_then(Value::as_str) == Some(token))
    {
        return Err("该 token 已在账号池中".to_string());
    }

    let id = uuid::Uuid::new_v4().simple().to_string();
    let display = name
        .map(String::from)
        .unwrap_or_else(|| format!("Kimi {}", raw.len()));
    let entry = account_to_json(&KimiAccount {
        id: id.clone(),
        name: display,
        raw_token: token.to_string(),
        enabled: true,
        max_concurrency: DEFAULT_MAX_CONCURRENCY,
        min_interval_ms: DEFAULT_MIN_INTERVAL_MS,
        device_id: resolved_device_id,
        cached_access_token: String::new(),
        access_expires_at: 0,
        cooldown_until: 0,
        status: "ok".to_string(),
        error: String::new(),
    });
    raw.push(entry.clone());
    save_raw(&raw)?;
    Ok(json!({
        "updated": false,
        "reason": "added",
        "account": entry,
    }))
}

pub fn remove_account(id: &str) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut raw = load_raw();
    let before = raw.len();
    raw.retain(|v| v.get("id").and_then(Value::as_str) != Some(id));
    if raw.len() == before {
        return false;
    }
    save_raw(&raw).is_ok()
}

pub fn set_account_enabled(id: &str, enabled: bool) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut raw = load_raw();
    let mut changed = false;
    for v in raw.iter_mut() {
        if v.get("id").and_then(Value::as_str) == Some(id) {
            v["enabled"] = json!(enabled);
            changed = true;
        }
    }
    if changed {
        let _ = save_raw(&raw);
    }
    changed
}

pub fn update_account_limits(id: &str, max_concurrency: usize, min_interval_ms: u64) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut raw = load_raw();
    let mut changed = false;
    for v in raw.iter_mut() {
        if v.get("id").and_then(Value::as_str) == Some(id) {
            v["max_concurrency"] = json!(max_concurrency.max(1));
            v["min_interval_seconds"] = json!(min_interval_ms as f64 / 1000.0);
            changed = true;
        }
    }
    if changed {
        let _ = save_raw(&raw);
    }
    changed
}

fn new_device_id() -> String {
    // 19 位数字（与 kimi2api generate_device_id 一致的量级）
    (7_000_000_000_000_000_000u64
        + (now_ms() as u64 % 1_000_000_000_000_000_000))
    .to_string()
}

// ---------------------------------------------------------------------------
// token：类型识别 / JWT exp / refresh
// ---------------------------------------------------------------------------

fn parse_jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| general_purpose::URL_SAFE.decode(parts[1]))
        .ok()?;
    serde_json::from_slice(&payload).ok()
}

/// refresh token（非 JWT）或 access token（kimi JWT）。
fn detect_token_type(token: &str) -> &'static str {
    if token.starts_with("eyJ") && token.split('.').count() == 3 {
        if let Some(payload) = parse_jwt_payload(token) {
            if payload.get("app_id").and_then(Value::as_str) == Some("kimi")
                && payload.get("typ").and_then(Value::as_str) == Some("access")
            {
                return "jwt";
            }
        }
    }
    "refresh"
}

fn jwt_expires_at(token: &str) -> i64 {
    parse_jwt_payload(token)
        .and_then(|p| p.get("exp").and_then(Value::as_i64))
        .unwrap_or(0)
}

fn jwt_issued_at(token: &str) -> i64 {
    parse_jwt_payload(token)
        .and_then(|p| p.get("iat").and_then(Value::as_i64))
        .unwrap_or(0)
}

/// JWT 里的账号身份（`sub`）。**refresh token 会轮换，token 值不能当身份用**，
/// 同一个 Kimi 用户必须靠 `sub` 归并，否则每轮换一次就多出一个重复账号。
pub fn jwt_subject(token: &str) -> Option<String> {
    parse_jwt_payload(token)
        .and_then(|p| p.get("sub").and_then(Value::as_str).map(String::from))
}

/// JWT 里的 `device_id`（与浏览器同源，写回上游可降低风控概率）。
fn jwt_device_id(token: &str) -> Option<String> {
    parse_jwt_payload(token)
        .and_then(|p| p.get("device_id").and_then(Value::as_str).map(String::from))
}

/// 一次 refresh 调用的结果：新 access token +（可能被轮换的）新 refresh token。
pub struct RefreshOutcome {
    pub access_token: String,
    /// Kimi 每次刷新都会**轮换 refresh token**；非空表示本次拿到了新的一代。
    pub rotated_refresh_token: Option<String>,
}

/// 用 refresh token 换 access token（GET /api/auth/token/refresh）。
///
/// 🔴 **Kimi 每次刷新都会轮换 refresh token**：响应体同时返回 `access_token` 与
/// 新的 `refresh_token`。旧 refresh token 只有短暂宽限期，若只取 access_token
/// 而不把新 refresh token 写回账号池，本地凭据就会「一代代老化」，几天后必然失效。
/// 因此这里把新 refresh token 一并带出，由调用方写回 `account.raw_token`。
async fn exchange_access_token(
    account: &KimiAccount,
) -> Result<RefreshOutcome, KimiUpstreamError> {
    let resp = http_client()
        .get(format!("{KIMI_BASE}{KIMI_REFRESH_PATH}"))
        .header("Authorization", format!("Bearer {}", account.raw_token))
        .header("X-Msh-Platform", "web")
        .header("X-Msh-Device-Id", &account.device_id)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .send()
        .await
        .map_err(|e| KimiUpstreamError::Transport(format!("Kimi refresh 请求失败: {e}")))?;
    if resp.status() != reqwest::StatusCode::OK {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let msg = format!(
            "Kimi refresh 失败（{status}）：{}",
            text.chars().take(120).collect::<String>()
        );
        return Err(classify_status(status, msg));
    }
    let value: Value = resp.json().await.map_err(|e| {
        KimiUpstreamError::Upstream(format!("Kimi refresh 响应解析失败: {e}"))
    })?;
    let access_token = value
        .get("access_token")
        .or_else(|| value.get("token"))
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| {
            KimiUpstreamError::Upstream("Kimi refresh 响应缺少 access_token".to_string())
        })?;
    // 轮换的新 refresh token（与旧值相同则视为未轮换）
    let rotated_refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| s.len() >= 20 && *s != account.raw_token)
        .map(String::from);
    Ok(RefreshOutcome {
        access_token,
        rotated_refresh_token,
    })
}

/// 把 refresh 结果写回账号（含**轮换后的 refresh token**），返回可用的 access token。
fn apply_refresh(account: &mut KimiAccount, outcome: RefreshOutcome) -> String {
    account.cached_access_token = outcome.access_token.clone();
    account.access_expires_at = jwt_expires_at(&outcome.access_token);
    if let Some(rotated) = outcome.rotated_refresh_token {
        account.raw_token = rotated;
    }
    account.status = "ok".to_string();
    account.error.clear();
    persist_account(account);
    outcome.access_token
}

/// 拿到可用 access token：缓存未过期直接用；否则刷新并写回池。
async fn access_token_for(account: &mut KimiAccount) -> Result<String, String> {
    let now_secs = now_ms() / 1000;
    if !account.cached_access_token.is_empty()
        && account.access_expires_at > now_secs + 300
    {
        return Ok(account.cached_access_token.clone());
    }
    let token_type = detect_token_type(&account.raw_token);
    if token_type == "jwt" {
        // 直接用 access token（上游仍可能提前吊销）
        if account.access_expires_at == 0 {
            account.access_expires_at = jwt_expires_at(&account.raw_token);
        }
        return Ok(account.raw_token.clone());
    }
    let outcome = exchange_access_token(account).await?;
    Ok(apply_refresh(account, outcome))
}

/// 强制刷新（忽略本地缓存）。用于上游**提前吊销** access token（未到 exp 就 401/403）后的重试，
/// 避免「续期中断」直接变成永久失败。
async fn access_token_for_force(account: &mut KimiAccount) -> Result<String, String> {
    if detect_token_type(&account.raw_token) == "jwt" {
        // 账号本身就是 access token，没有 refresh 能力
        return Err("当前账号为 access token，无法自动刷新".to_string());
    }
    let outcome = exchange_access_token(account).await?;
    Ok(apply_refresh(account, outcome))
}

// ---------------------------------------------------------------------------
// 模型目录（GetAvailableModels + 后缀别名）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct KimiModelSpec {
    pub id: String,
    pub display_name: String,
    pub scenario: String,
    pub thinking: bool,
    pub supports_web_search: bool,
    pub force_web_search: bool,
    pub kimi_plus_id: String,
    pub agent_mode: String,
}

fn model_version_slug(display_name: &str, scenario: &str) -> String {
    // displayName 里抓 "K 2.6" 之类
    if let Some(pos) = display_name.to_uppercase().find('K') {
        let rest = &display_name[pos + 1..];
        let version: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if !version.is_empty() {
            return format!("k{}", version);
        }
    }
    match scenario {
        "SCENARIO_K2" => "k2".to_string(),
        "SCENARIO_K2D5" => "k2.6".to_string(),
        other => other.to_lowercase().replace("scenario_", "").replace('_', "-"),
    }
}

fn model_suffix(spec: &KimiModelSpec) -> String {
    let name = spec.display_name.to_lowercase();
    if spec.agent_mode == "TYPE_ULTRA" || name.contains("swarm") {
        "agent-swarm".to_string()
    } else if spec.scenario == "SCENARIO_OK_COMPUTER"
        || !spec.kimi_plus_id.is_empty()
        || name.contains("agent")
    {
        "agent".to_string()
    } else if spec.thinking {
        "thinking".to_string()
    } else {
        String::new()
    }
}

fn spec_from_raw(raw: &Value) -> KimiModelSpec {
    let scenario = raw
        .get("scenario")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let display_name = raw
        .get("displayName")
        .or_else(|| raw.get("display_name"))
        .and_then(Value::as_str)
        .unwrap_or(&scenario)
        .to_string();
    // thinking：现行响应用 reasoningEffortOptions / defaultReasoningEffort 表达。
    // 若档位里含 NONE，说明该模型默认可不思考（如 Instant/k2d6）；否则视为思考模型。
    let has_none_effort = raw
        .get("reasoningEffortOptions")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .any(|o| o.get("effort").and_then(Value::as_str) == Some("REASONING_EFFORT_NONE"))
        })
        .unwrap_or(false);
    let thinking = raw.get("thinking").and_then(Value::as_bool).unwrap_or_else(|| {
        !has_none_effort && raw.get("reasoningEffortOptions").is_some()
    });
    let kimi_plus_id = raw
        .get("kimiPlusId")
        .or_else(|| raw.get("kimi_plus_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let agent_mode = raw
        .get("agentMode")
        .or_else(|| raw.get("agent_mode"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut spec = KimiModelSpec {
        id: String::new(),
        display_name,
        scenario: scenario.clone(),
        thinking,
        supports_web_search: scenario == "SCENARIO_K2D5",
        force_web_search: false,
        kimi_plus_id,
        agent_mode,
    };
    let version = model_version_slug(&spec.display_name, &scenario);
    let suffix = model_suffix(&spec);
    // 现行响应直接提供规范 key（网页端使用的模型标识，如 k3 / k3-agent-ultra / k2d6），
    // 有则优先采用，避免从 displayName 猜测版本号（"Instant" 这类名字猜不出来）。
    let key = raw.get("key").and_then(Value::as_str).unwrap_or_default();
    spec.id = if !key.is_empty() {
        format!("kimi-{key}")
    } else if suffix.is_empty() {
        format!("kimi-{version}")
    } else {
        format!("kimi-{version}-{suffix}")
    };
    spec
}

fn with_search_aliases(mut models: Vec<KimiModelSpec>) -> Vec<KimiModelSpec> {
    let existing: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
    let mut aliases = Vec::new();
    for model in &models {
        if !model.supports_web_search || model.force_web_search || model.id.ends_with("-search") {
            continue;
        }
        let alias_id = format!("{}-search", model.id);
        if existing.contains(&alias_id) {
            continue;
        }
        aliases.push(KimiModelSpec {
            id: alias_id,
            display_name: format!("{} Search", model.display_name),
            force_web_search: true,
            ..model.clone()
        });
    }
    models.extend(aliases);
    models
}

static MODEL_CATALOG: Mutex<Option<(i64, Vec<KimiModelSpec>, String)>> = Mutex::new(None);

/// 拉取模型目录（缓存 5 分钟；无账号时回退静态表）。
pub async fn model_catalog() -> Result<(Vec<KimiModelSpec>, String), String> {
    if let Ok(guard) = MODEL_CATALOG.lock() {
        if let Some((ts, models, default_id)) = guard.as_ref() {
            if now_ms() - ts < MODEL_CATALOG_TTL_MS {
                return Ok((models.clone(), default_id.clone()));
            }
        }
    }
    // 需要一个可用 token（顺带取该账号的 device_id，与 chat 调用保持一致）
    let mut accounts = load_accounts();
    let mut access = String::new();
    let mut device_id = String::new();
    for account in accounts.iter_mut() {
        if !account.enabled {
            continue;
        }
        if let Ok(token) = access_token_for(account).await {
            access = token;
            device_id = account.device_id.clone();
            break;
        }
    }
    if access.is_empty() {
        return Err("Kimi 账号池为空或全部不可用，无法拉取模型目录".to_string());
    }

    // GetAvailableModels 是**标准 JSON 接口**（不是 connect 帧接口）：
    //   POST + Content-Type: application/json + 空 JSON 体  →  纯 JSON 响应
    // 实测对照（2026-09-16）：
    //   GET                              → 405 Method Not Allowed
    //   application/connect+json + 帧体  → 415 Unsupported Media Type
    //   application/json + 帧体          → 400 unmarshal 失败
    //   application/json + {}            → 200 ✅
    // 冷启动时（服务刚拉起、网络/代理解析尚未就绪）首次出站请求容易出现
    // `error sending request`，若不做重试会把瞬时故障直接显示到界面上。
    // 这里做 3 次短退避重试，覆盖启动瞬间的网络抖动。
    let mut last_err = String::new();
    let mut fetched: Option<reqwest::Response> = None;
    for attempt in 0..3u32 {
        match http_client()
            .post(format!("{KIMI_BASE}{KIMI_MODELS_PATH}"))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {access}"))
            .header("X-Msh-Platform", "web")
            .header("X-Msh-Device-Id", &device_id)
            .body("{}")
            .send()
            .await
        {
            Ok(r) => {
                fetched = Some(r);
                break;
            }
            Err(e) => {
                last_err = e.to_string();
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        400 * u64::from(attempt) + 400,
                    ))
                    .await;
                }
            }
        }
    }
    let resp = fetched.ok_or_else(|| format!("Kimi 模型目录请求失败: {last_err}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Kimi 模型目录失败（{status}）：{}", text.chars().take(120).collect::<String>()));
    }
    // 响应为纯 JSON；同时兼容旧版 connect 帧格式（取首个可解析为 JSON 的帧）
    let payload = parse_connect_frames(text.as_bytes())
        .into_iter()
        .find(|p| serde_json::from_str::<Value>(p).is_ok())
        .unwrap_or_else(|| text.clone());
    let value: Value = serde_json::from_str(&payload).map_err(|e| {
        format!(
            "Kimi 模型目录解析失败: {e}：{}",
            text.chars().take(120).collect::<String>()
        )
    })?;
    // 现行响应结构：{ availableModels: [...], defaultScenario: {...} }
    let raw_models = value
        .get("availableModels")
        .or_else(|| value.pointer("/modelConfig/models"))
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut specs: Vec<KimiModelSpec> = raw_models
        .iter()
        .map(spec_from_raw)
        .filter(|spec| !spec.id.is_empty())
        .collect();
    // 去重
    let mut seen = std::collections::HashSet::new();
    specs.retain(|spec| seen.insert(spec.id.clone()));
    let default_scenario = value
        .get("defaultScenario")
        .cloned()
        .or_else(|| value.pointer("/modelConfig/defaultScenario").cloned())
        .unwrap_or(json!({}));
    let default_id = default_default_model_id(&specs, &default_scenario);
    let specs = with_search_aliases(specs);
    if let Ok(mut guard) = MODEL_CATALOG.lock() {
        *guard = Some((now_ms(), specs.clone(), default_id.clone()));
    }
    Ok((specs, default_id))
}

fn default_default_model_id(specs: &[KimiModelSpec], default_scenario: &Value) -> String {
    let scenario = default_scenario.get("scenario").and_then(Value::as_str).unwrap_or_default();
    let thinking = default_scenario.get("thinking").is_some();
    for spec in specs {
        if spec.scenario == scenario && spec.thinking == thinking && !spec.id.ends_with("-search") {
            return spec.id.clone();
        }
    }
    specs
        .iter()
        .find(|spec| spec.scenario == KIMI_SCENARIO_DEFAULT && !spec.thinking)
        .map(|spec| spec.id.clone())
        .or_else(|| specs.first().map(|spec| spec.id.clone()))
        .unwrap_or_else(|| "kimi-k2d6".to_string())
}

/// OpenAI /v1/models 形状。
pub async fn models_payload() -> Result<Value, String> {
    let (specs, default_id) = model_catalog().await?;
    let data: Vec<Value> = specs
        .iter()
        .map(|spec| {
            json!({
                "id": spec.id,
                "object": "model",
                "created": 0,
                "owned_by": "kimi",
                "display_name": spec.display_name,
                "thinking": spec.thinking,
                "web_search": spec.force_web_search || spec.supports_web_search,
            })
        })
        .collect();
    Ok(json!({"object": "list", "data": data, "default": default_id}))
}

// ---------------------------------------------------------------------------
// 调度（每账号并发上限 + 最小间隔 + 冷却 + 最少 in_flight 优先）
// ---------------------------------------------------------------------------

struct AccountRuntime {
    account: KimiAccount,
    in_flight: usize,
}

fn runtimes_lock() -> &'static Mutex<HashMap<String, (usize, i64)>> {
    // id -> (in_flight, next_request_at monotonic-ish ms)
    static RUNTIMES: std::sync::OnceLock<Mutex<HashMap<String, (usize, i64)>>> = std::sync::OnceLock::new();
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 选账号：启用、健康、未冷却、并发未满；in_flight 最少优先，同分轮询。
async fn acquire_account() -> Result<(KimiAccount, std::sync::Arc<tokio::sync::OwnedSemaphorePermit>), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(QUEUE_WAIT_SECS);
    loop {
        // 锁只包住"决策 + in_flight 占位"，绝不能跨 await（std MutexGuard 非 Send）
        let picked = {
            let accounts = load_accounts();
            let mut runtimes = runtimes_lock().lock().unwrap();
            let now = now_ms();
            let mut candidates: Vec<(String, usize)> = Vec::new(); // (id, in_flight)
            for account in &accounts {
                if !account.enabled || account.status != "ok" {
                    continue;
                }
                if account.cooldown_until > now {
                    continue;
                }
                let state = runtimes.get(&account.id).cloned().unwrap_or((0, 0));
                if state.0 >= account.max_concurrency {
                    continue;
                }
                candidates.push((account.id.clone(), state.0));
            }
            // in_flight 最少优先（稳定排序保持池顺序 = 轮询）
            let mut candidates = candidates;
            candidates.sort_by_key(|(_, in_flight)| *in_flight);
            candidates
                .first()
                .and_then(|(id, in_flight)| {
                    accounts
                        .iter()
                        .find(|a| a.id == *id)
                        .map(|account| (account.clone(), *in_flight))
                })
                .map(|(mut account, in_flight)| {
                    runtimes.insert(account.id.clone(), (in_flight + 1, now));
                    account
                })
        };
        if let Some(account) = picked {
            // 每账号信号量（按 max_concurrency），拿不到说明被并发抢占，重试
            let sem = account_semaphore(&account.id, account.max_concurrency);
            let permit = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sem.clone().acquire_owned(),
            )
            .await
            {
                Ok(Ok(permit)) => permit,
                _ => {
                    release_in_flight(&account.id);
                    continue;
                }
            };
            // 最小请求间隔
            enforce_min_interval(&account.id, account.min_interval_ms).await;
            return Ok((account, std::sync::Arc::new(permit)));
        }
        if std::time::Instant::now() >= deadline {
            return Err("Kimi 账号池繁忙：没有可用账号（全部冷却/占满/禁用），请稍后重试".to_string());
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

fn release_in_flight(id: &str) {
    if let Ok(mut runtimes) = runtimes_lock().lock() {
        if let Some((in_flight, next)) = runtimes.get_mut(id) {
            *in_flight = in_flight.saturating_sub(1);
            let _ = next;
        }
    }
}

fn account_semaphore(id: &str, max_concurrency: usize) -> std::sync::Arc<tokio::sync::Semaphore> {
    static SEMS: std::sync::OnceLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Semaphore>>>> =
        std::sync::OnceLock::new();
    let mut sems = SEMS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    sems.entry(id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrency.max(1))))
        .clone()
}

async fn enforce_min_interval(id: &str, min_interval_ms: u64) {
    if min_interval_ms == 0 {
        return;
    }
    let wait = {
        let mut runtimes = runtimes_lock().lock().unwrap();
        let entry = runtimes.entry(id.to_string()).or_insert((0, 0));
        let now = now_ms();
        let start_at = entry.1.max(now);
        entry.1 = start_at + min_interval_ms as i64;
        (start_at - now).max(0) as u64
    };
    if wait > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    }
}

const QUEUE_WAIT_SECS: u64 = 60;

fn mark_failure(id: &str, status: u16, retry_after_ms: i64) {
    let cooldown = match status {
        429 => retry_after_ms.max(COOLDOWN_429_MS),
        401 | 403 => 0, // 标记异常而非冷却
        _ => COOLDOWN_5XX_MS,
    };
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut raw = load_raw();
    for v in raw.iter_mut() {
        if v.get("id").and_then(Value::as_str) == Some(id) {
            match status {
                401 | 403 => {
                    v["status"] = json!("invalid");
                    v["error"] = json!(format!("上游返回 {status}，token 可能已被吊销，请刷新或更换"));
                }
                _ => {
                    v["error"] = json!(format!("上游 {status}，冷却 {}s", cooldown / 1000));
                }
            }
        }
    }
    let _ = save_raw(&raw);
    if cooldown > 0 {
        if let Ok(mut runtimes) = runtimes_lock().lock() {
            runtimes.insert(id.to_string(), (0, now_ms() + cooldown));
        }
    }
    release_in_flight(id);
}

fn mark_success(id: &str) {
    release_in_flight(id);
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut raw = load_raw();
    for v in raw.iter_mut() {
        if v.get("id").and_then(Value::as_str) == Some(id) {
            v["status"] = json!("ok");
            v["error"] = Value::Null;
        }
    }
    let _ = save_raw(&raw);
}

// ---------------------------------------------------------------------------
// Kimi Web chat 协议（connect 帧）
// ---------------------------------------------------------------------------

fn fake_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Accept", "*/*".to_string()),
        ("Accept-Language", "zh-CN,zh;q=0.9,en-US;q=0.8,en;q=0.7".to_string()),
        ("Cache-Control", "no-cache".to_string()),
        ("Pragma", "no-cache".to_string()),
        ("Origin", KIMI_BASE.to_string()),
        ("R-Timezone", "Asia/Shanghai".to_string()),
        (
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".to_string(),
        ),
        ("X-Msh-Platform", "web".to_string()),
    ]
}

/// OpenAI messages → kimi2api 同款格式化（system 前置 + role:text 行）。
fn format_messages(messages: &[Value]) -> String {
    let mut system_lines: Vec<String> = Vec::new();
    let mut body_lines: Vec<String> = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| {
                    if b.is_string() {
                        b.as_str().map(String::from)
                    } else {
                        b.get("text").and_then(Value::as_str).map(String::from)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        match role {
            "system" | "developer" => system_lines.push(text),
            "assistant" => body_lines.push(format!("assistant:{text}")),
            "tool" => body_lines.push(format!("user:{text}")),
            other => body_lines.push(format!("{other}:{text}")),
        }
    }
    let mut lines: Vec<String> = system_lines
        .iter()
        .map(|line| format!("system:{line}"))
        .collect();
    lines.extend(body_lines);
    lines.join("\n")
}

/// build_chat_payload（与 kimi2api `_build_chat_payload` 一致）。
fn build_chat_payload(
    spec: &KimiModelSpec,
    messages: &[Value],
    enable_web_search: bool,
) -> Result<Vec<u8>, String> {
    let content = format_messages(messages);
    if content.is_empty() {
        return Err("messages 不能为空".to_string());
    }
    let payload = json!({
        "scenario": spec.scenario,
        "tools": if enable_web_search { vec![json!({"type": "TOOL_TYPE_SEARCH", "search": {}})] } else { vec![] },
        "message": {
            "role": "user",
            "blocks": [{"message_id": "", "text": {"content": content}}],
            "scenario": spec.scenario,
        },
        "options": {"thinking": spec.thinking},
        "kimiplusId": spec.kimi_plus_id,
        "agentMode": spec.agent_mode,
    });
    let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    // connect 帧：0x00 + 4 字节大端长度
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.push(0x00);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// 解析 connect 帧流 → JSON 事件文本列表（跳过 flag & 0x80 帧）。
fn parse_connect_frames(bytes: &[u8]) -> Vec<String> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    while offset + 5 <= bytes.len() {
        let flag = bytes[offset];
        let length = u32::from_be_bytes([
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
            bytes[offset + 4],
        ]) as usize;
        let frame_end = offset + 5 + length;
        if frame_end > bytes.len() {
            break;
        }
        let payload = &bytes[offset + 5..frame_end];
        offset = frame_end;
        if flag & 0x80 != 0 {
            continue;
        }
        if let Ok(text) = std::str::from_utf8(payload) {
            let text = text.trim().to_string();
            if !text.is_empty() {
                events.push(text);
            }
        }
    }
    events
}

/// 上游错误码 → 便于用户理解的提示。
///
/// 🔴 背景（2026-09-16 实测）：上游在**鉴权通过、HTTP 200** 的情况下，会在流里
/// 回一帧 `{"error":{"code":...,"debug":{...}}}` 表示业务失败（如免费账号用 K3 会
/// 得到 `resource_exhausted` / `REASON_SERVER_OVERLOADED_FOR_FREE_USER`）。
/// 旧实现只解析 `block`/`done`，把这一帧静默丢弃 → 对外表现为「HTTP 200 + 空内容」，
/// 用户完全无从判断是模型不支持、额度不足还是协议出错。
/// 零内容报错时最多附带几条原始帧、每条截断多少字符。
/// 目的：让「账号可能无该模型权限或上游限流」这种**猜测**能自证，
/// 而不是把排查成本推给用户（2026-09-16 用户问「这是什么意思」就是被这条文案坑的）。
const RAW_TAIL_MAX: usize = 6;
const RAW_FRAME_MAX_CHARS: usize = 240;

/// 生成「最近上游原始帧」后缀。帧里不含凭据（只是上游返回的正文/心跳/错误帧）。
fn raw_tail_suffix(frames: &[String]) -> String {
    if frames.is_empty() {
        return "。上游连一帧都没发（TCP/流建立后即结束，通常是上游侧直接拒绝或超时）".to_string();
    }
    format!("。最近上游原始帧：{}", frames.join(" ⏎ "))
}

fn describe_upstream_error(code: &str, debug: Option<&Value>) -> String {
    let reason = debug
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let localized = debug
        .and_then(|d| d.pointer("/localizedMessage/message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let need_upgrade = debug
        .and_then(|d| d.pointer("/paywall/needUpgrade"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match reason {
        "REASON_SERVER_OVERLOADED_FOR_FREE_USER" => {
            "该模型当前仅对付费会员开放（当前账号为免费用户），请改用 Instant 系列模型，或升级会员".to_string()
        }
        _ => {
            if need_upgrade {
                format!("上游拒绝：{code}（需升级会员）。{localized}")
            } else if !localized.is_empty() {
                format!("上游拒绝：{code}（{localized}）")
            } else if !reason.is_empty() {
                format!("上游拒绝：{code}（{reason}）")
            } else {
                format!("上游拒绝：{code}")
            }
        }
    }
}

/// 从 connect 帧事件里解析上游业务错误；非错误事件返回 `None`。
///
/// 错误帧形如：
/// `{"error":{"code":"resource_exhausted","details":[{"value":"<base64 protobuf>"}],"debug":{...}}}`
/// `details[].value` 是 base64 的 protobuf，直接丢弃会把唯一的人类可读信息（会员等级文案）丢掉；
/// 这里做一次宽松解码，抽出可打印片段作为兜底文案。
fn upstream_error_of(event: &Value) -> Option<String> {
    let error = event.get("error")?;
    if error.is_null() {
        return None;
    }
    if let Some(code) = error.as_str() {
        return Some(format!("上游拒绝：{code}"));
    }
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    let debug = error.get("debug");
    let mut message = describe_upstream_error(code, debug);
    // details[].value → base64 protobuf：抽可打印 ASCII 作为附加信息（失败则忽略）
    if let Some(details) = error.get("details").and_then(Value::as_array) {
        if let Some(value) = details
            .iter()
            .find_map(|d| d.get("value").and_then(Value::as_str))
        {
            let padded = match value.len() % 4 {
                0 => value.to_string(),
                n => format!("{value}{}", "=".repeat(4 - n)),
            };
            if let Ok(decoded) = general_purpose::STANDARD
                .decode(&padded)
                .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(value))
            {
                let readable: String = decoded
                    .iter()
                    .map(|b| if (0x20..0x7f).contains(b) { *b as char } else { ' ' })
                    .collect();
                let readable = readable.split_whitespace().collect::<Vec<_>>().join(" ");
                // 只保留足够长、确实像句子的片段，避免塞入乱码
                if readable.len() > 24 && !message.contains(&readable[..readable.len().min(24)]) {
                    message.push_str(&format!("（原始提示：{}）", readable.chars().take(200).collect::<String>()));
                }
            }
        }
    }
    Some(message)
}

/// 事件 → (正文增量, 推理增量, 是否结束, chat_id, assistant_message_id)。
fn extract_delta(event: &Value) -> (Option<String>, Option<String>, bool, Option<String>, Option<String>) {
    if event.get("heartbeat").is_some() {
        return (None, None, false, None, None);
    }
    let chat_id = event
        .pointer("/chat/id")
        .and_then(Value::as_str)
        .map(String::from);
    let message_id = event
        .pointer("/message/id")
        .and_then(Value::as_str)
        .filter(|_| {
            event.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
        })
        .map(String::from);
    let done = event.get("done").is_some();
    // 阶段判定：multiStage stages 或 text.flags
    let mut phase_thinking = false;
    if let Some(stages) = event.pointer("/block/multiStage/stages").and_then(Value::as_array) {
        if let Some(first) = stages.first() {
            if first.get("name").and_then(Value::as_str) == Some("STAGE_NAME_THINKING") {
                phase_thinking = first.get("status").and_then(Value::as_str) != Some("completed");
            }
        }
    }
    if event.pointer("/block/text/flags").and_then(Value::as_str) == Some("thinking") {
        phase_thinking = true;
    }
    if let Some(think) = event.pointer("/block/think/content").and_then(Value::as_str) {
        return (None, Some(think.to_string()), done, chat_id, message_id);
    }
    if let Some(text) = event.pointer("/block/text/content").and_then(Value::as_str) {
        if phase_thinking {
            return (None, Some(text.to_string()), done, chat_id, message_id);
        }
        return (Some(text.to_string()), None, done, chat_id, message_id);
    }
    (None, None, done, chat_id, message_id)
}

// ---------------------------------------------------------------------------
// OpenAI 兼容 chat（账号池调度 + 流式转换）
// ---------------------------------------------------------------------------

/// 解析模型 id → spec（thinking/search/agent 语义从 id 后缀推导）。
async fn resolve_model_spec(model: &str) -> Result<KimiModelSpec, String> {
    let (specs, default_id) = model_catalog().await?;
    let normalized = model.trim().to_lowercase();
    let target = if normalized.is_empty() { default_id.as_str() } else { normalized.as_str() };
    specs
        .iter()
        .find(|spec| spec.id == target)
        .cloned()
        .ok_or_else(|| {
            format!(
                "未知模型 {target:?}，可用模型: {}",
                specs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")
            )
        })
}

pub struct KimiUpstreamStream {
    pub rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, String>>,
}

/// chat completions 统一入口（流式/非流式）。
///
/// 返回类型立即装箱（`Pin<Box<dyn Future>>`）：统一 fn 的 native future
/// 推断复杂到让 rustc 1.98 在 server crate 的 Handler 检查里触发 ICE
/// （"unexpectedly panicked"），装箱把类型固定为必然 Send 的 trait object。
#[allow(clippy::type_complexity)]
pub fn chat_completions(
    body: Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TabbitLikeOutput, String>> + Send>> {
    Box::pin(async move {
        if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
            chat_completions_impl(body, true).await
        } else {
            chat_completions_impl(body, false).await
        }
    })
}

async fn chat_completions_impl(body: Value, stream: bool) -> Result<TabbitLikeOutput, String> {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let spec = resolve_model_spec(&model).await?;
    let enable_search = spec.force_web_search
        || body.get("web_search_options").is_some()
        || body
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools.iter().any(|t| {
                    t.get("type").and_then(Value::as_str)
                        .map(|ty| ty.contains("web_search"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
        || ["enable_web_search", "enable_web", "web_search", "search"]
            .iter()
            .any(|key| body.get(key).and_then(Value::as_bool) == Some(true));
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let frame = build_chat_payload(&spec, &messages, enable_search)?;

    // 调度：选账号 + 拿 access token（含刷新）
    let (mut account, _permit) = acquire_account().await?;
    let access = access_token_for(&mut account).await.map_err(|e| {
        mark_failure(&account.id, 401, 0);
        e
    })?;

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Content-Type", "application/connect+json".parse().unwrap());
    headers.insert("Authorization", format!("Bearer {access}").parse().unwrap());
    headers.insert("X-Msh-Platform", "web".parse().unwrap());
    headers.insert("X-Msh-Device-Id", account.device_id.parse().unwrap());
    for (name, value) in fake_headers() {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }

    let mut resp = http_client()
        .post(format!("{KIMI_BASE}{KIMI_CHAT_PATH}"))
        .headers(headers.clone())
        .body(frame.clone())
        .send()
        .await
        .map_err(|e| {
            mark_failure(&account.id, 502, 0);
            format!("Kimi chat 请求失败: {e}")
        })?;

    // 🔴 access token 可能被上游**提前吊销**（未到 exp 就 401/403）。
    // 此时清掉缓存强刷一次并重试，把「续期中断」挡在永久失败之前。
    if matches!(resp.status().as_u16(), 401 | 403)
        && detect_token_type(&account.raw_token) == "refresh"
    {
        account.cached_access_token.clear();
        account.access_expires_at = 0;
        if let Ok(fresh) = access_token_for_force(&mut account).await {
            if let Ok(value) = format!("Bearer {fresh}").parse() {
                headers.insert("Authorization", value);
            }
            resp = http_client()
                .post(format!("{KIMI_BASE}{KIMI_CHAT_PATH}"))
                .headers(headers)
                .body(frame)
                .send()
                .await
                .map_err(|e| {
                    mark_failure(&account.id, 502, 0);
                    format!("Kimi chat 重试请求失败: {e}")
                })?;
        }
    }
    let status = resp.status().as_u16();
    if status == 429 || status >= 500 {
        let retry_after = resp
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .map(|secs| secs * 1000)
            .unwrap_or(0);
        mark_failure(&account.id, status, retry_after);
        return Err(format!("Kimi 上游返回 {status}（账号已冷却，可重试）"));
    }
    if status == 401 || status == 403 {
        mark_failure(&account.id, status, 0);
        return Err(format!("Kimi 上游返回 {status}：token 被拒绝，账号已标记异常"));
    }
    if !(200..300).contains(&status) {
        mark_failure(&account.id, status, 0);
        return Err(format!("Kimi 上游返回 {status}"));
    }

    if stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, String>>(16);
        let account_id = account.id.clone();
        let model_id = spec.id.clone();
        tokio::spawn(async move {
            pump_stream(resp, model_id, account_id, tx).await;
        });
        Ok(TabbitLikeOutput::Stream(KimiUpstreamStream { rx }))
    } else {
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Kimi 响应读取失败: {e}"))?;
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut chat_id = String::new();
        let mut upstream_error: Option<String> = None;
        let mut raw_tail: Vec<String> = Vec::new();
        for event_text in parse_connect_frames(&bytes) {
            if raw_tail.len() < RAW_TAIL_MAX {
                raw_tail.push(event_text.chars().take(RAW_FRAME_MAX_CHARS).collect::<String>());
            }
            let Ok(event) = serde_json::from_str::<Value>(&event_text) else { continue };
            if let Some(err) = upstream_error_of(&event) {
                // 上游业务错误（HTTP 200 但语义失败）：必须显式抛出，
                // 否则会退化成「成功 + 空内容」，前端无法区分原因。
                upstream_error = Some(err);
                continue;
            }
            let (c, r, _, cid, _) = extract_delta(&event);
            if let Some(cid) = cid {
                chat_id = cid;
            }
            if let Some(c) = c {
                content.push_str(&c);
            }
            if let Some(r) = r {
                reasoning.push_str(&r);
            }
        }
        if let Some(err) = upstream_error {
            mark_failure(&account.id, 0, 0);
            return Err(format!("Kimi {err}"));
        }
        if content.is_empty() && reasoning.is_empty() {
            // 既没有正文也没有推理：宁可报错，也不要返回一个看似成功却空白的响应
            mark_failure(&account.id, 0, 0);
            return Err(format!(
                "Kimi 上游未返回任何内容（模型 {}）：账号可能无该模型权限或上游限流{}",
                spec.id,
                raw_tail_suffix(&raw_tail)
            ));
        }
        mark_success(&account.id);
        Ok(TabbitLikeOutput::Json(json!({
            "id": if chat_id.is_empty() { format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()) } else { chat_id },
            "object": "chat.completion",
            "created": now_ms() / 1000,
            "model": spec.id,
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
}

/// 流式泵：上游 connect 帧流 → OpenAI chunk SSE（含推理增量）。
async fn pump_stream(
    mut resp: reqwest::Response,
    model_id: String,
    account_id: String,
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, String>>,
) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk_id = String::new();
    let mut started = false;
    // 是否真正向客户端送出过正文/推理；用于把「零内容即结束」识别为上游异常。
    let mut emitted = false;
    // 最近几条上游原始帧（截断、不含凭据）。零内容报错时附上，
    // 把「账号可能无该模型权限或上游限流」这类**猜测**变成可自证的证据。
    let mut raw_tail: Vec<String> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                buffer.extend_from_slice(&chunk);
                let events = parse_connect_frames(&buffer);
                let consumed = consumed_connect_bytes(&buffer);
                if consumed > 0 {
                    buffer.drain(..consumed);
                }
                for event_text in events {
                    if raw_tail.len() < RAW_TAIL_MAX {
                        raw_tail.push(event_text.chars().take(RAW_FRAME_MAX_CHARS).collect::<String>());
                    }
                    let Ok(event) = serde_json::from_str::<Value>(&event_text) else { continue };
                    // 🔴 上游业务错误帧（HTTP 200）：必须以 error 结束流，
                    // 否则客户端只会看到「立刻 stop 且零内容」，无从判断原因。
                    if let Some(err) = upstream_error_of(&event) {
                        let _ = tx
                            .send(Err(format!("Kimi {err}")))
                            .await;
                        mark_failure(&account_id, 0, 0);
                        return;
                    }
                    let (content, reasoning, done, chat_id, _message_id) = extract_delta(&event);
                    if let Some(id) = chat_id {
                        chunk_id = id;
                    }
                    let mut frames = Vec::new();
                    if !started {
                        started = true;
                        if chunk_id.is_empty() {
                            chunk_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
                        }
                        frames.push(json!({
                            "id": chunk_id, "object": "chat.completion.chunk", "model": model_id,
                            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": Value::Null}],
                        }));
                    }
                    if let Some(reasoning) = reasoning {
                        emitted = true;
                        frames.push(json!({
                            "id": chunk_id, "object": "chat.completion.chunk", "model": model_id,
                            "choices": [{"index": 0, "delta": {"reasoning_content": reasoning}, "finish_reason": Value::Null}],
                        }));
                    }
                    if let Some(content) = content {
                        emitted = true;
                        frames.push(json!({
                            "id": chunk_id, "object": "chat.completion.chunk", "model": model_id,
                            "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": Value::Null}],
                        }));
                    }
                    for frame in frames {
                        if tx.send(Ok(bytes::Bytes::from(format!("data: {frame}\n\n")))).await.is_err() {
                            mark_success(&account_id);
                            return;
                        }
                    }
                    if done {
                        if !emitted {
                            // 上游只发了心跳就结束：不能伪装成正常 stop，
                            // 否则客户端拿到空回答却以为调用成功。
                            let _ = tx
                                .send(Err(format!(
                                    "Kimi 上游未返回任何内容（模型 {model_id}）：账号可能无该模型权限或上游限流{}",
                                    raw_tail_suffix(&raw_tail)
                                )))
                                .await;
                            mark_failure(&account_id, 0, 0);
                            return;
                        }
                        let finish = json!({
                            "id": chunk_id, "object": "chat.completion.chunk", "model": model_id,
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                        });
                        let _ = tx.send(Ok(bytes::Bytes::from(format!("data: {finish}\n\n")))).await;
                        let _ = tx.send(Ok(bytes::Bytes::from("data: [DONE]\n\n"))).await;
                        mark_success(&account_id);
                        return;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tx.send(Err(format!("Kimi 流中断: {e}"))).await;
                mark_failure(&account_id, 599, 0);
                return;
            }
        }
    }
    if started && emitted {
        let finish = json!({
            "id": chunk_id, "object": "chat.completion.chunk", "model": model_id,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        });
        let _ = tx.send(Ok(bytes::Bytes::from(format!("data: {finish}\n\n")))).await;
        let _ = tx.send(Ok(bytes::Bytes::from("data: [DONE]\n\n"))).await;
        mark_success(&account_id);
    } else {
        // 流已结束却没有任何正文/推理：显式报错，避免「成功但空白」。
        let _ = tx
            .send(Err(format!(
                "Kimi 上游未返回任何内容（模型 {model_id}）：账号可能无该模型权限或上游限流{}",
                raw_tail_suffix(&raw_tail)
            )))
            .await;
        mark_failure(&account_id, 0, 0);
    }
}

/// 已完整消费的 connect 帧字节数（供流式缓冲推进）。
fn consumed_connect_bytes(buffer: &[u8]) -> usize {
    let mut offset = 0usize;
    while offset + 5 <= buffer.len() {
        let length = u32::from_be_bytes([
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
            buffer[offset + 4],
        ]) as usize;
        let frame_end = offset + 5 + length;
        if frame_end > buffer.len() {
            break;
        }
        offset = frame_end;
    }
    offset
}

/// 流式/非流式输出的统一封装（与 glm_proxy::GlmChatCompletion 同构）。
pub enum TabbitLikeOutput {
    Json(Value),
    Stream(KimiUpstreamStream),
}

/// 账号池管理列表（token 脱敏）。
pub fn list_for_display() -> Vec<Value> {
    load_accounts()
        .iter()
        .map(|account| {
            let token_preview = format!(
                "{}…({})",
                &account.raw_token[..account.raw_token.len().min(8)],
                account.raw_token.len()
            );
            let cooldown_left = (account.cooldown_until - now_ms()).max(0) / 1000;
            let refresh_exp = jwt_expires_at(&account.raw_token);
            json!({
                "id": account.id,
                "name": account.name,
                "tokenType": detect_token_type(&account.raw_token),
                "tokenPreview": token_preview,
                "subjectId": jwt_subject(&account.raw_token),
                "deviceId": account.device_id,
                "enabled": account.enabled,
                "maxConcurrency": account.max_concurrency,
                "minIntervalSeconds": account.min_interval_ms as f64 / 1000.0,
                "hasCachedAccess": !account.cached_access_token.is_empty(),
                "accessExpiresAt": if account.access_expires_at > 0 { json!(account.access_expires_at * 1000) } else { Value::Null },
                "refreshExpiresAt": if refresh_exp > 0 { json!(refresh_exp * 1000) } else { Value::Null },
                "status": account.status,
                "error": account.error,
                "cooldownSeconds": cooldown_left,
            })
        })
        .collect()
}

/// 🔴 只有上游**明确拒绝**（401/403）才允许标记失效并落盘。
///
/// 旧代码把任何非 2xx（含 405 / 5xx / 429）都写成 `status="invalid"`，
/// 而扩展的 `sweepInvalid` 巡检正是按这个字段**删除账号** ——
/// 一次上游 500 或方法用错就能把好账号清掉。这里收紧成白名单。
fn mark_invalid(account: &mut KimiAccount, reason: &str) {
    account.status = "invalid".to_string();
    account.error = reason.to_string();
    persist_account(account);
}

/// 带传输层重试的 refresh 换取（校验路径专用）。
async fn exchange_access_token_retry(
    account: &KimiAccount,
) -> Result<RefreshOutcome, KimiUpstreamError> {
    let mut last: Option<KimiUpstreamError> = None;
    for attempt in 0..VERIFY_ATTEMPTS {
        match exchange_access_token(account).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let retryable = matches!(e, KimiUpstreamError::Transport(_));
                last = Some(e);
                if !retryable || attempt + 1 >= VERIFY_ATTEMPTS {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(retry_backoff_ms(attempt)))
                    .await;
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        KimiUpstreamError::Transport("Kimi refresh 未知错误".to_string())
    }))
}

/// 带传输层重试的模型目录探活（校验路径专用）。
///
/// 🔴 **必须是 POST + `application/json` + `{}`**：该端点是标准 JSON 接口，
/// 用 GET 会被上游判 405（2026-09-16 实测：`GET` → 405，`POST {}` → 200）。
/// 旧代码用 GET，即使网络正常也会失败，并进一步把好 token 标成 invalid。
async fn probe_models_with_retry(
    account: &KimiAccount,
) -> Result<reqwest::Response, KimiUpstreamError> {
    let mut last: Option<KimiUpstreamError> = None;
    for attempt in 0..VERIFY_ATTEMPTS {
        match verify_client()
            .post(format!("{KIMI_BASE}{KIMI_MODELS_PATH}"))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", account.raw_token))
            .header("X-Msh-Platform", "web")
            .header("X-Msh-Device-Id", &account.device_id)
            .body("{}")
            .send()
            .await
        {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last = Some(KimiUpstreamError::Transport(format!("验证请求失败: {e}")));
                if attempt + 1 >= VERIFY_ATTEMPTS {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(retry_backoff_ms(attempt)))
                    .await;
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        KimiUpstreamError::Transport("验证请求失败: 未知错误".to_string())
    }))
}

/// 手动验证账号。
///
/// - `refresh` token → 换取一次 access（换取成功即通过），顺带把**轮换后的**
///   refresh token 写回账号池；
/// - `access` token → 探活模型目录（POST，见 `probe_models_with_retry`）。
///
/// 失败语义（2026-09-16 修订）：
/// - 网络不可达 → `Transport`（**不**标记 invalid，可重试）；
/// - 401/403 → `Unauthorized`（标记 invalid 并落盘，等待扩展巡检回收）；
/// - 其他 → `Upstream`（**不**标记 invalid）。
pub async fn verify_account(id: &str) -> Result<Value, KimiUpstreamError> {
    let mut accounts = load_accounts();
    let Some(account) = accounts.iter_mut().find(|a| a.id == id) else {
        return Err(KimiUpstreamError::Local("账号不存在".to_string()));
    };
    let token_type = detect_token_type(&account.raw_token);
    if token_type == "refresh" {
        let outcome = exchange_access_token_retry(account).await?;
        let rotated = outcome.rotated_refresh_token.is_some();
        apply_refresh(account, outcome);
        Ok(json!({
            "ok": true,
            "tokenType": "refresh",
            "refreshed": true,
            "refreshRotated": rotated,
            "expiresAt": account.access_expires_at * 1000,
        }))
    } else {
        // access token：先本地看 exp，过期就不必打上游了（快且不误导）
        let exp = jwt_expires_at(&account.raw_token);
        let now_secs = now_ms() / 1000;
        if exp > 0 && exp <= now_secs {
            let msg = format!(
                "access token 已过期（exp 已过 {} 秒），请在浏览器重新登录 kimi.com 后再同步",
                now_secs - exp
            );
            mark_invalid(account, &msg);
            return Err(KimiUpstreamError::Unauthorized(msg));
        }
        let resp = probe_models_with_retry(account).await?;
        let status = resp.status();
        if status.is_success() {
            // 探活成功：顺带把状态恢复（上游提前吊销后重登的场景）
            if account.status != "ok" {
                account.status = "ok".to_string();
                account.error.clear();
                persist_account(account);
            }
            Ok(json!({
                "ok": true,
                "tokenType": "jwt",
                "valid": true,
                "expiresAt": exp * 1000,
            }))
        } else {
            let text = resp.text().await.unwrap_or_default();
            let snippet = text.chars().take(160).collect::<String>();
            let msg = if snippet.trim().is_empty() {
                format!("上游返回 {status}（无响应体），token 可能已失效")
            } else {
                format!("上游返回 {status}：{snippet}")
            };
            let err = classify_status(status, msg);
            if matches!(err, KimiUpstreamError::Unauthorized(_)) {
                mark_invalid(account, err.message());
            }
            Err(err)
        }
    }
}
