//! 常量、路径与通用工具函数（对照 server.py 常量区与工具区）

use chrono::{Local, TimeZone};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

pub const WORKBUDDY_API_ENDPOINT: &str = "https://www.codebuddy.cn";
/// WorkBuddy API 基址·国际版（契约 §10/§11：domain 含 workbuddy.ai 的账号走此域）。
pub const WORKBUDDY_API_ENDPOINT_INTL: &str = "https://www.workbuddy.ai";
pub const WORKBUDDY_API_PREFIX: &str = "/v2/plugin";
pub const WORKBUDDY_PLATFORM: &str = "workbuddy";

/// 按账号 domain 选 API 基址：含 workbuddy.ai → 国际版，其余 → 国服。
/// 此前 refresh/checkin/travel 对所有账号硬打国服端点，国际账号刷新天然
/// invalid_grant（keycloak realm 不同），被误标「需重新登录」
/// （2026-09-25 用户实测发现并定位）。
pub fn workbuddy_api_endpoint_for(account: &Value) -> &'static str {
    let domain = account
        .get("domain")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if domain.contains("workbuddy.ai") {
        WORKBUDDY_API_ENDPOINT_INTL
    } else {
        WORKBUDDY_API_ENDPOINT
    }
}

/// 国际版上游按 Origin/Referer 判定来源区域，跨区域发送会被拒（与池网关
/// `apply_common` 同口径）；国服现状不带这些头也能通，保持不变。
pub fn apply_region_headers(account: &Value, headers: &mut HashMap<String, String>) {
    if workbuddy_api_endpoint_for(account) == WORKBUDDY_API_ENDPOINT_INTL {
        headers.insert("Origin".to_string(), WORKBUDDY_API_ENDPOINT_INTL.to_string());
        headers.insert(
            "Referer".to_string(),
            format!("{WORKBUDDY_API_ENDPOINT_INTL}/"),
        );
        headers.insert("X-Auth-Refresh-Source".to_string(), "workbuddy".to_string());
    }
}

pub const OAUTH_TIMEOUT_SECONDS: i64 = 600;

pub const CHECKIN_API_PREFIX: &str = "/v2/billing/meter";
pub const CHECKIN_LOG_KEEP_DAYS: i64 = 30;
pub const CHECKIN_LOG_MAX_RECORDS: usize = 500;

/// 派猫猫旅行接口前缀（成长中心，非 /v2/plugin 体系，直接挂在 API 域名下）。
pub const TRAVEL_API_PREFIX: &str = "/activity/growth/buddy/travel";

static CHECKIN_LOG_WRITE_LOCK: Mutex<()> = Mutex::new(());
static TRAVEL_CACHE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Serialize travel-cache read-modify-write across depart and claim cycles.
pub fn with_travel_cache_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = TRAVEL_CACHE_WRITE_LOCK.lock().unwrap();
    f()
}

/// 官网套餐页桌面 Chrome UA（plans-usage 捕获）。
pub const DEFAULT_HTTP_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// 1.4.1：各模型家族的最长上下文宣称值（全网关统一口径，供模型清单序列化
/// 时覆盖/补齐上游目录缺失值）。匹配按小写包含；未识别的家族返回 None
/// （保留上游原值，不虚构）。
pub fn max_context_window(model: &str) -> Option<u64> {
    let m = model.to_ascii_lowercase();
    // 精确优先：GLM-4.6（Coding Plan）官方 200k，先于 glm 泛匹配
    if m.contains("glm-4.6") {
        return Some(200_000);
    }
    if m.contains("step-5") {
        return Some(1_024_000);
    }
    if m.contains("step") {
        return Some(262_144);
    }
    if m.contains("grok") {
        return Some(500_000);
    }
    if m.contains("minimax") {
        return Some(1_000_000);
    }
    if m.contains("deepseek") || m.contains("hy3") || m.contains("hunyuan") {
        return Some(256_000);
    }
    if m.contains("kimi") || m.contains("moonshot") {
        return Some(256_000);
    }
    // GLM 家族（glm-5.x / glm-4.x / auto / auto-fast / spark）目录实锤 1M
    if m.contains("glm") || m.contains("auto") || m.contains("spark") {
        return Some(1_048_576);
    }
    if m.contains("qwen") {
        return Some(1_048_576);
    }
    None
}

pub fn store_dir() -> PathBuf {
    home_dir().join(".wb-switch")
}

pub fn accounts_file() -> PathBuf {
    store_dir().join("accounts.json")
}

pub fn backup_dir() -> PathBuf {
    store_dir().join("backups")
}

pub fn checkin_config_file() -> PathBuf {
    store_dir().join("auto_checkin_config.json")
}

pub fn checkin_logs_file() -> PathBuf {
    store_dir().join("auto_checkin_logs.json")
}

pub fn travel_config_file() -> PathBuf {
    store_dir().join("auto_travel_config.json")
}

pub fn travel_cache_file() -> PathBuf {
    store_dir().join("travel_cache.json")
}

pub fn credit_usage_snapshots_file() -> PathBuf {
    store_dir().join("credit_usage_snapshots.json")
}

pub fn official_usage_cache_file() -> PathBuf {
    store_dir().join("official_usage_cache.json")
}

pub fn workbuddy_exe_cache_file() -> PathBuf {
    store_dir().join("workbuddy_exe.json")
}

/// 「近期到期」窗口配置文件（2026-10-01：此前是硬编码常量，改配置要重新构建）。
pub fn expiring_soon_config_file() -> PathBuf {
    store_dir().join("expiring_soon_config.json")
}

fn parse_workbuddy_exe_cache_json(text: &str) -> Option<PathBuf> {
    let v: Value = serde_json::from_str(text).ok()?;
    let exe = v.get("exe")?.as_str()?.trim();
    if exe.is_empty() {
        None
    } else {
        Some(PathBuf::from(exe))
    }
}

/// 读取上次成功解析到的 WorkBuddy.exe；损坏或空文件视为无缓存。
pub fn load_workbuddy_exe_cache() -> Option<PathBuf> {
    let f = workbuddy_exe_cache_file();
    if !f.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&f).ok()?;
    parse_workbuddy_exe_cache_json(&text)
}

/// 记住已存在的 WorkBuddy.exe，供下次未运行时启动。
pub fn save_workbuddy_exe_cache(exe: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(store_dir())?;
    let content =
        serde_json::to_string_pretty(&json!({ "exe": exe.to_string_lossy() })).unwrap_or_default();
    atomic_write(&workbuddy_exe_cache_file(), &content)
}

pub fn clear_workbuddy_exe_cache() {
    let _ = std::fs::remove_file(workbuddy_exe_cache_file());
}

// ---------------------------------------------------------------------------
// 签到配置 / 日志（对照 server.py load/save_checkin_config / load/save/add_checkin_log）
// ---------------------------------------------------------------------------

/// 默认签到配置。旧时间窗口字段仅为配置文件兼容保留，调度不再读取。
pub fn default_checkin_config() -> Value {
    json!({
        "enabled": true,
        "start_hour": 6,
        "end_hour": 12,
        "keepalive_days": 0,
        "lazy_refresh_hours": 24,
    })
}

fn merge_checkin_config(input: &Value) -> Value {
    let mut merged = default_checkin_config();
    let Some(map) = input.as_object() else {
        return merged;
    };
    if let Some(enabled) = map.get("enabled").and_then(Value::as_bool) {
        merged["enabled"] = json!(enabled);
    }
    for key in [
        "start_hour",
        "end_hour",
        "keepalive_days",
        "lazy_refresh_hours",
    ] {
        if let Some(value) = map.get(key).and_then(Value::as_i64) {
            merged[key] = json!(value);
        }
    }
    merged
}

/// 读取签到配置（缺失/损坏时合并默认值）。
pub fn load_checkin_config() -> Value {
    let f = checkin_config_file();
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                return merge_checkin_config(&value);
            }
        }
    }
    default_checkin_config()
}

/// 保存签到配置（只保留已知字段）。
pub fn save_checkin_config(cfg: &Value) -> std::io::Result<()> {
    let merged = merge_checkin_config(cfg);
    std::fs::create_dir_all(store_dir())?;
    let content = serde_json::to_string_pretty(&merged).unwrap_or_default();
    atomic_write(&checkin_config_file(), &content)
}

// ---------------------------------------------------------------------------
// 「近期到期」窗口（2026-10-01 由硬编码常量改为可配置）
//
// 背景：原为 `EXPIRING_SOON_DAYS = 7`，导致 10/31 到期（29 天）的包不亮；
// 放宽到 30 后仍可能不合不同用户的预期 → 改为配置化，改完即时生效、无需重建。
// ---------------------------------------------------------------------------

/// 「近期到期」窗口的**默认**天数（2026-10-01 由 7 放宽到 30）。
pub const DEFAULT_EXPIRING_SOON_DAYS: i64 = 30;

fn default_expiring_soon_config() -> Value {
    json!({"days": DEFAULT_EXPIRING_SOON_DAYS})
}

/// 规范化「近期到期」窗口配置（暴露为 pub 便于单测直接校验夹取逻辑）。
pub fn merge_expiring_soon_config(input: &Value) -> Value {
    let mut merged = default_expiring_soon_config();
    let Some(map) = input.as_object() else {
        return merged;
    };
    if let Some(days) = map.get("days").and_then(Value::as_i64) {
        // 1..=365：0 会让「近期到期」永不亮，过大则失去提醒意义。
        merged["days"] = json!(days.clamp(1, 365));
    }
    merged
}

/// 读取「近期到期」窗口配置（缺失/损坏时合并默认值）。
pub fn load_expiring_soon_config() -> Value {
    let f = expiring_soon_config_file();
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                return merge_expiring_soon_config(&value);
            }
        }
    }
    default_expiring_soon_config()
}

/// 保存「近期到期」窗口配置（只保留已知字段）。
pub fn save_expiring_soon_config(cfg: &Value) -> std::io::Result<()> {
    let merged = merge_expiring_soon_config(cfg);
    std::fs::create_dir_all(store_dir())?;
    let content = serde_json::to_string_pretty(&merged).unwrap_or_default();
    atomic_write(&expiring_soon_config_file(), &content)
}

/// 「近期到期」窗口天数（`unified_accounts` / `credits` 读这里，不再用常量）。
pub fn expiring_soon_days() -> i64 {
    load_expiring_soon_config()
        .get("days")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_EXPIRING_SOON_DAYS)
}

/// 读取签到日志。
pub fn load_checkin_logs() -> Vec<Value> {
    let f = checkin_logs_file();
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&text) {
                return arr;
            }
        }
    }
    vec![]
}

fn save_checkin_logs_unlocked(logs: &[Value]) -> std::io::Result<()> {
    let kept = normalize_checkin_logs(logs, now_ms());
    std::fs::create_dir_all(store_dir())?;
    let content = serde_json::to_string_pretty(&kept).unwrap_or_default();
    atomic_write(&checkin_logs_file(), &content)
}

/// 保存签到日志（30 天过滤 + 保留最近 500 条，保持插入顺序）。
pub fn save_checkin_logs(logs: &[Value]) -> std::io::Result<()> {
    let _guard = CHECKIN_LOG_WRITE_LOCK.lock().unwrap();
    save_checkin_logs_unlocked(logs)
}

fn checkin_log_local_date(ts_ms: i64) -> Option<String> {
    Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .map(|date| date.format("%Y-%m-%d").to_string())
}

fn legacy_checkin_identity(entry: &Value) -> Option<String> {
    if let Some(account_id) = entry
        .get("accountId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(format!("account:{account_id}"));
    }

    // Old log rows predate accountId and only carried the display identity in
    // `email`. Keep this fallback namespaced so it can never merge with a
    // stable local account ID that happens to have the same text.
    entry
        .get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|email| format!("legacy:{email}"))
}

/// Apply the persisted check-in log contract without changing the source file.
///
/// `success` and `error` entries retain their full multiplicity. Only repeated
/// legacy `already` rows are reduced to the latest timestamp for one account
/// and local calendar date.
fn normalize_checkin_logs(logs: &[Value], at_ms: i64) -> Vec<Value> {
    let cutoff = at_ms.saturating_sub(CHECKIN_LOG_KEEP_DAYS * 24 * 3600 * 1000);
    let retained: Vec<(usize, i64, &Value)> = logs
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let ts = norm_ts(entry.get("ts"))?;
            (ts >= cutoff).then_some((index, ts, entry))
        })
        .collect();

    let mut dedupable_already_indices = HashSet::new();
    let mut latest_already = HashMap::<(String, String), (i64, usize)>::new();
    for (index, ts, entry) in &retained {
        if entry.get("result").and_then(Value::as_str) != Some("already") {
            continue;
        }
        let Some(identity) = legacy_checkin_identity(entry) else {
            continue;
        };
        let Some(date) = checkin_log_local_date(*ts) else {
            continue;
        };
        dedupable_already_indices.insert(*index);
        let candidate = (*ts, *index);
        latest_already
            .entry((identity, date))
            .and_modify(|current| {
                if candidate >= *current {
                    *current = candidate;
                }
            })
            .or_insert(candidate);
    }

    let winning_already_indices: HashSet<usize> = latest_already
        .into_values()
        .map(|(_, index)| index)
        .collect();
    let mut normalized: Vec<Value> = retained
        .into_iter()
        .filter(|(index, _, entry)| {
            entry.get("result").and_then(Value::as_str) != Some("already")
                || !dedupable_already_indices.contains(index)
                || winning_already_indices.contains(index)
        })
        .map(|(_, _, entry)| entry.clone())
        .collect();

    if normalized.len() > CHECKIN_LOG_MAX_RECORDS {
        normalized.drain(..normalized.len() - CHECKIN_LOG_MAX_RECORDS);
    }
    normalized
}

fn compact_checkin_logs_at(path: &Path, at_ms: i64) -> std::io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path)?;
    let Ok(Value::Array(logs)) = serde_json::from_str::<Value>(&text) else {
        // Preserve unreadable user data rather than replacing it with an empty
        // file. Normal log loading keeps its existing tolerant behavior.
        return Ok(false);
    };
    let normalized = normalize_checkin_logs(&logs, at_ms);
    if normalized == logs {
        return Ok(false);
    }
    let content = serde_json::to_string_pretty(&normalized).unwrap_or_default();
    atomic_write(path, &content)?;
    Ok(true)
}

/// Compact legacy persisted check-in logs once during host startup.
///
/// Returns `true` only when the file was rewritten. Loading logs remains a
/// read-only operation; both hosts invoke this explicit migration before their
/// first automatic verification cycle.
pub fn compact_checkin_logs() -> std::io::Result<bool> {
    let _guard = CHECKIN_LOG_WRITE_LOCK.lock().unwrap();
    compact_checkin_logs_at(&checkin_logs_file(), now_ms())
}

/// 追加一条签到日志。
pub fn add_checkin_log(entry: &Value) {
    // Account-scoped check-in coordination permits unrelated accounts to run
    // concurrently. Serialize the file read-modify-write so neither entry is lost.
    let _guard = CHECKIN_LOG_WRITE_LOCK.lock().unwrap();
    let mut logs = load_checkin_logs();
    logs.push(entry.clone());
    let _ = save_checkin_logs_unlocked(&logs);
}

// ---------------------------------------------------------------------------
// 派猫猫旅行配置 / 缓存
// ---------------------------------------------------------------------------

/// 默认自动旅行配置。
pub fn default_travel_config() -> Value {
    json!({ "enabled": true })
}

/// 读取自动旅行配置（缺失/损坏时合并默认值）。
pub fn load_travel_config() -> Value {
    let mut cfg = default_travel_config();
    let f = travel_config_file();
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
                if let Some(enabled) = map.get("enabled").and_then(Value::as_bool) {
                    cfg["enabled"] = json!(enabled);
                }
            }
        }
    }
    cfg
}

/// 保存自动旅行配置（只保留已知字段）。
pub fn save_travel_config(cfg: &Value) -> std::io::Result<()> {
    let mut merged = default_travel_config();
    if let Some(enabled) = cfg.get("enabled").and_then(Value::as_bool) {
        merged["enabled"] = json!(enabled);
    }
    std::fs::create_dir_all(store_dir())?;
    let content = serde_json::to_string_pretty(&merged).unwrap_or_default();
    atomic_write(&travel_config_file(), &content)
}

/// 读取旅行缓存（`{ date, completed, results: { accountId: {...} } }`）。
pub fn load_travel_cache() -> Value {
    let f = travel_cache_file();
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
                return Value::Object(map);
            }
        }
    }
    json!({})
}

/// 保存旅行缓存。
pub fn save_travel_cache(cache: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(store_dir())?;
    let content = serde_json::to_string_pretty(cache).unwrap_or_default();
    atomic_write(&travel_cache_file(), &content)
}

// ---------------------------------------------------------------------------
// 并发运行标志（替代 Python threading.Lock，Send 安全可跨 await）
// ---------------------------------------------------------------------------

/// RAII 运行标志：进入临界区置 true，Drop 时复位。
pub struct RunFlagGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> RunFlagGuard<'a> {
    /// 尝试获取标志；已被占用返回 None。
    pub fn try_acquire(flag: &'a AtomicBool) -> Option<Self> {
        if flag.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(Self { flag })
        }
    }
}

impl Drop for RunFlagGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 时间
// ---------------------------------------------------------------------------

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn utc_iso() -> String {
    // 对照 Python utc_iso：%Y-%m-%dT%H-%M-%S + "Z"
    format!("{}Z", chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S"))
}

// ---------------------------------------------------------------------------
// 文件
// ---------------------------------------------------------------------------

/// 原子写文件（临时文件 + rename），对照 Python atomic_write。
///
/// 🔴 2026-10-09：Windows 上 `rename` 覆盖已存在文件会被**杀毒/索引器短暂独占锁**挡住，
/// 报 `os error 5 拒绝访问`。原实现一次失败就放弃 ⇒ **静默丢掉整次写入**
/// （实测单测并行时必现；生产表现为「配置/账本偶尔没保存」，且只有一行
/// `[atomic] rename FAILED` 打印，极难归因）。
/// 修法：rename 失败按 10/20/40/80/160ms 退避重试 5 次；
/// 全部失败才报错，并在错误里带上目标路径，便于定位。
pub fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    if let Err(e) = std::fs::write(&tmp, content) {
        eprintln!("[atomic] write tmp FAILED: {}: {e}", path.display());
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let mut last: Option<std::io::Error> = None;
    for i in 0..5u32 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(10 << i));
            }
        }
    }
    let e = last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "rename failed without error")
    });
    eprintln!("[atomic] rename FAILED (5 retries): {}: {e}", path.display());
    let _ = std::fs::remove_file(&tmp);
    Err(std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

// ---------------------------------------------------------------------------
// 时间戳归一化
// ---------------------------------------------------------------------------

/// 把秒/毫秒/字符串时间戳统一为毫秒；无效返回 None。对照 server.py `_norm_ts`。
pub fn norm_ts(v: Option<&Value>) -> Option<i64> {
    let mut ts: i64 = match v {
        Some(Value::String(s)) => s.trim().parse::<f64>().ok()? as i64,
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?,
        _ => return None,
    };
    if ts < 10_000_000_000 {
        ts *= 1000; // 秒 → 毫秒
    }
    Some(ts)
}

// ---------------------------------------------------------------------------
// HTTP 客户端（对照 Python http_request）
// ---------------------------------------------------------------------------

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(DEFAULT_HTTP_USER_AGENT)
}

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        http_client_builder()
            .build()
            .expect("failed to build reqwest client")
    })
}

/// 通用 HTTP 请求，返回解析后的 JSON。
///
/// 行为对齐 Python 版：
/// - 2xx：解析 body 为 JSON；
/// - HTTP 错误：body 可解析则返回其 JSON，否则 `{"code": <status>, "message": <body 前 500 字符>}`；
/// - 网络错误：`{"code": -1, "message": <原因>}`。
pub async fn http_request(
    url: &str,
    method: &str,
    body: Option<Value>,
    headers: Option<&HashMap<String, String>>,
) -> Value {
    http_request_with_proxy(url, method, body, headers, None).await
}

/// 通用 HTTP 请求，可为单次请求显式指定 HTTP/HTTPS 代理。
pub async fn http_request_with_proxy(
    url: &str,
    method: &str,
    body: Option<Value>,
    headers: Option<&HashMap<String, String>>,
    proxy: Option<&str>,
) -> Value {
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let client = match proxy.map(str::trim).filter(|value| !value.is_empty()) {
        Some(proxy) => match http_client_builder()
            .proxy(match reqwest::Proxy::all(proxy) {
                Ok(proxy) => proxy,
                Err(e) => return json!({"code": -1, "message": format!("代理地址无效: {e}")}),
            })
            .build()
        {
            Ok(client) => client,
            Err(e) => return json!({"code": -1, "message": format!("代理客户端创建失败: {e}")}),
        },
        None => http_client().clone(),
    };
    let mut req = client.request(method, url);
    req = req.header("Content-Type", "application/json");
    if let Some(h) = headers {
        for (k, v) in h {
            req = req.header(k, v);
        }
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or_else(|_| sanitize_non_json_error(status.as_u16(), &text))
            }
        }
        Err(e) => json!({"code": -1, "message": e.to_string()}),
    }
}

/// 非 JSON 错误响应的错误整形（纯函数，便于单测）。
///
/// 🔴 HTML 页面（如上游网关层 nginx 的 `401 Authorization Required`）绝不透传原文：
/// 原文进 `message` 后会被前端当文案渲染，界面上直接糊一段 HTML 源码
/// （2026-09-25 用户实测：国际版账号 token 失效时 www.workbuddy.ai 网关层回
/// nginx 401 HTML，原文上屏账号页）。转为「状态码 + <title> 摘要」的干净文案，
/// 并保留数字状态码与 401/unauthorized 等关键词，使既有的登录态失效分类
/// （travel 的 is_unauthorized 等）照常命中。非 HTML 的纯文本错误保持原文截断。
pub fn sanitize_non_json_error(status: u16, text: &str) -> Value {
    let lowered = text.to_ascii_lowercase();
    let looks_html = lowered.contains("<html") || lowered.contains("<!doctype html");
    if !looks_html {
        return json!({
            "code": status,
            "message": text.chars().take(500).collect::<String>(),
        });
    }
    let title = extract_html_title(text)
        .map(|t| format!("：{t}"))
        .unwrap_or_default();
    json!({
        "code": status,
        "message": format!(
            "上游返回 HTTP {status}（HTML 页面{title}）——若为 401 多为登录态失效，请刷新凭证或重新登录"
        ),
    })
}

/// 提取 HTML <title> 文本（大小写不敏感；无/空 → None）。
fn extract_html_title(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = lower[start..].find("</title>")? + start;
    let title = text[start..end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// 通用 HTTP 请求，返回原始响应（状态码 + 响应头 + 响应体），可选是否跟随重定向。
///
/// 供需要读取响应头（如 302 的 `Location`）或自行处理非 JSON 响应的场景使用；
/// 其余场景优先用 [`http_request_with_proxy`]。失败（网络错误 / 代理配置错误）
/// 返回 `(0, HashMap::new(), 错误信息)`，由调用方根据 status 判断。
pub async fn http_request_raw(
    url: &str,
    method: &str,
    body: Option<Value>,
    headers: Option<&HashMap<String, String>>,
    proxy: Option<&str>,
    follow_redirects: bool,
) -> (u16, HashMap<String, String>, String) {
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let client = match proxy.map(str::trim).filter(|value| !value.is_empty()) {
        Some(proxy) => {
            let mut builder = http_client_builder().proxy(match reqwest::Proxy::all(proxy) {
                Ok(proxy) => proxy,
                Err(e) => return (0, HashMap::new(), format!("代理地址无效: {e}")),
            });
            if !follow_redirects {
                builder = builder.redirect(reqwest::redirect::Policy::none());
            }
            match builder.build() {
                Ok(client) => client,
                Err(e) => return (0, HashMap::new(), format!("代理客户端创建失败: {e}")),
            }
        }
        None => {
            if follow_redirects {
                http_client().clone()
            } else {
                match http_client_builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                {
                    Ok(client) => client,
                    Err(e) => return (0, HashMap::new(), format!("客户端创建失败: {e}")),
                }
            }
        }
    };
    let mut req = client.request(method, url);
    req = req.header("Content-Type", "application/json");
    if let Some(h) = headers {
        for (k, v) in h {
            req = req.header(k, v);
        }
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut resp_headers = HashMap::new();
            for (k, v) in resp.headers() {
                if let Ok(vs) = v.to_str() {
                    resp_headers.insert(k.as_str().to_string(), vs.to_string());
                }
            }
            let text = resp.text().await.unwrap_or_default();
            (status, resp_headers, text)
        }
        Err(e) => (0, HashMap::new(), e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_html_401_is_clean_and_classifiable() {
        // 2026-09-25 用户实测形态：国际版上游 nginx 的 401 HTML 页面。
        let raw = "<html>\r\n<head><title>401 Authorization Required</title></head>\r\n\
                   <body>\r\n<center><h1>401 Authorization Required</h1></center>\r\n</body>\r\n</html>";
        let v = sanitize_non_json_error(401, raw);
        assert_eq!(v.get("code").and_then(Value::as_u64), Some(401));
        let msg = v.get("message").and_then(Value::as_str).unwrap();
        assert!(!msg.contains("<html"), "message 不得包含 HTML 标记: {msg}");
        assert!(!msg.contains("<title>"), "message 不得包含 HTML 标记: {msg}");
        assert!(msg.contains("HTTP 401"), "保留状态码供分类: {msg}");
        assert!(
            msg.contains("401 Authorization Required"),
            "保留 <title> 摘要供诊断: {msg}"
        );
        // 既有登录态分类（关键词 401/unauthorized）仍能命中。
        assert!(msg.to_lowercase().contains("401"));
    }

    #[test]
    fn sanitize_plain_text_error_keeps_raw_truncated() {
        let v = sanitize_non_json_error(502, "plain upstream error");
        assert_eq!(v.get("code").and_then(Value::as_u64), Some(502));
        assert_eq!(
            v.get("message").and_then(Value::as_str),
            Some("plain upstream error")
        );
    }

    #[test]
    fn sanitize_html_without_title_has_no_suffix() {
        let v = sanitize_non_json_error(503, "<html><body>maintenance</body></html>");
        let msg = v.get("message").and_then(Value::as_str).unwrap();
        assert!(msg.contains("HTTP 503"));
        assert!(!msg.contains("<"), "message 不得包含 HTML 标记: {msg}");
    }

    #[test]
    fn workbuddy_api_endpoint_follows_account_domain() {
        // 国际账号（domain .ai）→ workbuddy.ai；国服/缺省 → codebuddy.cn。
        let intl = json!({"domain": "www.workbuddy.ai"});
        let cn = json!({"domain": "www.workbuddy.cn"});
        let none = json!({});
        assert_eq!(
            workbuddy_api_endpoint_for(&intl),
            WORKBUDDY_API_ENDPOINT_INTL
        );
        assert_eq!(workbuddy_api_endpoint_for(&cn), WORKBUDDY_API_ENDPOINT);
        assert_eq!(workbuddy_api_endpoint_for(&none), WORKBUDDY_API_ENDPOINT);
        // 国际账号附带区域头（Origin/Referer/刷新来源），国服不带。
        let mut headers = HashMap::new();
        apply_region_headers(&cn, &mut headers);
        assert!(headers.is_empty());
        apply_region_headers(&intl, &mut headers);
        assert_eq!(
            headers.get("Origin").map(String::as_str),
            Some(WORKBUDDY_API_ENDPOINT_INTL)
        );
        assert_eq!(headers.get("X-Auth-Refresh-Source").map(String::as_str), Some("workbuddy"));
    }

    fn local_timestamp_ms(year: i32, month: u32, day: u32, hour: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .expect("test timestamp must be unambiguous")
            .timestamp_millis()
    }

    #[test]
    fn auto_checkin_defaults_enabled_and_preserves_legacy_fields() {
        let cfg = default_checkin_config();
        assert_eq!(cfg.get("enabled").and_then(Value::as_bool), Some(true));
        assert_eq!(cfg.get("start_hour").and_then(Value::as_i64), Some(6));
        assert_eq!(cfg.get("end_hour").and_then(Value::as_i64), Some(12));
    }

    #[test]
    fn auto_checkin_explicit_false_wins_and_invalid_value_uses_default() {
        let disabled = merge_checkin_config(&json!({"enabled": false, "keepalive_days": 7}));
        assert_eq!(
            disabled.get("enabled").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            disabled.get("keepalive_days").and_then(Value::as_i64),
            Some(7)
        );

        let corrupt = merge_checkin_config(&json!({"enabled": "no", "lazy_refresh_hours": null}));
        assert_eq!(corrupt.get("enabled").and_then(Value::as_bool), Some(true));
        assert_eq!(
            corrupt.get("lazy_refresh_hours").and_then(Value::as_i64),
            Some(24)
        );
    }

    #[test]
    fn checkin_log_normalization_keeps_latest_already_per_identity_and_local_date() {
        let day = local_timestamp_ms(2026, 8, 20, 12);
        let logs = vec![
            json!({"accountId": "a", "email": "same", "result": "already", "ts": day + 1, "marker": "a-old"}),
            json!({"accountId": "b", "email": "same", "result": "already", "ts": day + 2, "marker": "b"}),
            json!({"accountId": "a", "email": "same", "result": "already", "ts": day + 3, "marker": "a-new"}),
            json!({"email": "legacy@example.com", "result": "already", "ts": day + 4, "marker": "legacy-old"}),
            json!({"email": "legacy@example.com", "result": "already", "ts": day + 5, "marker": "legacy-new"}),
            json!({"result": "already", "ts": day + 6, "marker": "no-identity"}),
        ];

        let normalized = normalize_checkin_logs(&logs, day + 10);
        let markers: Vec<&str> = normalized
            .iter()
            .filter_map(|entry| entry.get("marker").and_then(Value::as_str))
            .collect();

        assert_eq!(markers, vec!["b", "a-new", "legacy-new", "no-identity"]);
    }

    #[test]
    fn checkin_log_identity_namespaces_stable_ids_and_legacy_email_fallbacks() {
        let day = local_timestamp_ms(2026, 8, 20, 12);
        let logs = vec![
            json!({"accountId": "same@example.com", "email": "display", "result": "already", "ts": day + 1, "marker": "stable-old"}),
            json!({"email": "same@example.com", "result": "already", "ts": day + 2, "marker": "legacy-old"}),
            json!({"accountId": "same@example.com", "email": "display", "result": "already", "ts": day + 3, "marker": "stable-new"}),
            json!({"email": "same@example.com", "result": "already", "ts": day + 4, "marker": "legacy-new"}),
        ];

        let normalized = normalize_checkin_logs(&logs, day + 10);
        let markers: Vec<&str> = normalized
            .iter()
            .filter_map(|entry| entry.get("marker").and_then(Value::as_str))
            .collect();

        assert_eq!(markers, vec!["stable-new", "legacy-new"]);
    }

    #[test]
    fn checkin_log_normalization_keeps_already_for_separate_dates() {
        let first_day = local_timestamp_ms(2026, 8, 19, 12);
        let second_day = local_timestamp_ms(2026, 8, 20, 12);
        let logs = vec![
            json!({"accountId": "a", "result": "already", "ts": first_day}),
            json!({"accountId": "a", "result": "already", "ts": second_day}),
        ];

        assert_eq!(normalize_checkin_logs(&logs, second_day).len(), 2);
    }

    #[test]
    fn checkin_log_normalization_preserves_success_and_error_multiplicity() {
        let day = local_timestamp_ms(2026, 8, 20, 12);
        let logs = vec![
            json!({"accountId": "a", "result": "success", "ts": day + 1}),
            json!({"accountId": "a", "result": "success", "ts": day + 2}),
            json!({"accountId": "a", "result": "error", "ts": day + 3}),
            json!({"accountId": "a", "result": "error", "ts": day + 4}),
        ];

        assert_eq!(normalize_checkin_logs(&logs, day + 10), logs);
    }

    #[test]
    fn checkin_log_normalization_applies_retention_and_record_cap() {
        let now = local_timestamp_ms(2026, 8, 20, 12);
        let cutoff = now - CHECKIN_LOG_KEEP_DAYS * 24 * 3600 * 1000;
        let mut logs = vec![json!({
            "accountId": "old",
            "result": "success",
            "ts": cutoff - 1,
            "marker": -1,
        })];
        logs.extend((0..505).map(|marker| {
            json!({
                "accountId": "a",
                "result": "success",
                "ts": now,
                "marker": marker,
            })
        }));

        let normalized = normalize_checkin_logs(&logs, now);
        assert_eq!(normalized.len(), CHECKIN_LOG_MAX_RECORDS);
        assert_eq!(normalized[0]["marker"], 5);
        assert_eq!(normalized.last().unwrap()["marker"], 504);
    }

    #[test]
    fn checkin_log_normalization_deduplicates_before_taking_final_500() {
        let now = local_timestamp_ms(2026, 8, 20, 12);
        let mut logs = vec![
            json!({"accountId": "duplicate", "result": "already", "ts": now - 2, "marker": "duplicate-old"}),
            json!({"accountId": "duplicate", "result": "already", "ts": now - 1, "marker": "duplicate-new"}),
        ];
        logs.extend((0..500).map(|marker| {
            json!({
                "accountId": "a",
                "result": "success",
                "ts": now,
                "marker": marker,
            })
        }));

        let normalized = normalize_checkin_logs(&logs, now);
        assert_eq!(normalized.len(), CHECKIN_LOG_MAX_RECORDS);
        assert_eq!(normalized[0]["marker"], 0);
        assert_eq!(normalized.last().unwrap()["marker"], 499);
        assert!(normalized
            .iter()
            .all(|entry| entry["marker"] != "duplicate-old"));
    }

    #[test]
    fn checkin_log_normalization_is_idempotent() {
        let day = local_timestamp_ms(2026, 8, 20, 12);
        let logs = vec![
            json!({"accountId": "a", "result": "already", "ts": day + 1}),
            json!({"accountId": "a", "result": "already", "ts": day + 2}),
            json!({"accountId": "a", "result": "success", "ts": day + 3}),
        ];

        let once = normalize_checkin_logs(&logs, day + 10);
        assert_eq!(normalize_checkin_logs(&once, day + 10), once);
    }

    #[test]
    fn persisted_checkin_log_compaction_writes_only_when_changed() {
        let day = local_timestamp_ms(2026, 8, 20, 12);
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-checkin-log-compaction-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("logs.json");
        let logs = json!([
            {"accountId": "a", "result": "already", "ts": day + 1},
            {"accountId": "a", "result": "already", "ts": day + 2}
        ]);
        std::fs::write(&path, serde_json::to_string_pretty(&logs).unwrap()).unwrap();

        assert!(compact_checkin_logs_at(&path, day + 10).unwrap());
        let after_first = std::fs::read_to_string(&path).unwrap();
        assert!(!compact_checkin_logs_at(&path, day + 10).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);

        let missing = dir.join("missing.json");
        assert!(!compact_checkin_logs_at(&missing, day + 10).unwrap());
        assert!(!missing.exists());

        let corrupt = dir.join("corrupt.json");
        std::fs::write(&corrupt, "not-json").unwrap();
        assert!(!compact_checkin_logs_at(&corrupt, day + 10).unwrap());
        assert_eq!(std::fs::read_to_string(&corrupt).unwrap(), "not-json");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parse_workbuddy_exe_cache_json_reads_exe() {
        let path = parse_workbuddy_exe_cache_json(
            r#"{ "exe": "D:\\Users\\Zhou\\AppData\\Local\\Programs\\WorkBuddy\\WorkBuddy.exe" }"#,
        )
        .expect("valid cache");
        assert_eq!(
            path.to_string_lossy(),
            r"D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe"
        );
    }

    #[test]
    fn parse_workbuddy_exe_cache_json_ignores_corrupt_and_empty() {
        assert!(parse_workbuddy_exe_cache_json("not-json").is_none());
        assert!(parse_workbuddy_exe_cache_json(r#"{ "exe": "  " }"#).is_none());
        assert!(parse_workbuddy_exe_cache_json("{}").is_none());
    }

    #[test]
    fn default_http_user_agent_matches_official_chrome_desktop() {
        assert_eq!(
            DEFAULT_HTTP_USER_AGENT,
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36"
        );
        let _ = http_client_builder();
    }
}
