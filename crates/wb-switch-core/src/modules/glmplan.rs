//! GLM Plan 网关（1.4.0 新通道）：把 GLM Coding Plan 订阅账号池暴露为
//! OpenAI / Anthropic 兼容接口，参照 workbuddy-pool 区域契约做国内版/国际版
//! 双区分类，并内置请求规范化精确缓存（Exact Cache）与官方 CLI 身份拟真。
//!
//! 设计参照 ZCode-Hub（Bun 单文件号池网关，2026-09-28 拆解）+ 本工程 glm_proxy
//! 的账号池模式：
//! - 双 provider：bigmodel（open.bigmodel.cn，国内版）/ zai（api.z.ai，国际版），
//!   各带 OpenAI coding 端点（/api/coding/paas/v4）与 Anthropic 端点（/api/anthropic）；
//! - 账号 = 该 provider 的 API Key，按区轮询 + 冷却 + 启用开关；
//! - 精确缓存：规范化请求体（model+messages+tools+采样参数）sha256 作键，
//!   仅非流式响应入缓存，TTL/容量可配；
//! - 身份拟真（可关）：Referer/Origin 用官方 CLI 域名，Anthropic metadata.user_id
//!   按官方 CLI 规则伪造。
//!
//! 上游（2026-09-28 自 ZCode-Hub 内嵌源码还原）：
//!   ZAI:       https://api.z.ai/api/coding/paas/v4        + https://api.z.ai/api/anthropic
//!   BIGMODEL:  https://open.bigmodel.cn/api/coding/paas/v4 + https://open.bigmodel.cn/api/anthropic
//!   Referer:   https://zcode.z.ai（官方 Coding CLI 来源）

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 常量与 provider
// ---------------------------------------------------------------------------

pub const PROVIDER_BIGMODEL: &str = "bigmodel";
pub const PROVIDER_ZAI: &str = "zai";
pub const DEFAULT_MODEL: &str = "glm-4.6";
pub const DEFAULT_MODELS: [&str; 2] = ["glm-4.6", "glm-5.3-flash"];

/// 上游基址与区域归属（区域词表沿用 glm_proxy：domestic/international）。
pub fn provider_info(provider: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match provider {
        PROVIDER_BIGMODEL => Some((
            "https://open.bigmodel.cn/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/anthropic",
            "domestic",
        )),
        PROVIDER_ZAI => Some((
            "https://api.z.ai/api/coding/paas/v4",
            "https://api.z.ai/api/anthropic",
            "international",
        )),
        _ => None,
    }
}

pub fn provider_label(provider: &str) -> &'static str {
    match provider {
        PROVIDER_BIGMODEL => "国内版",
        PROVIDER_ZAI => "国际版",
        _ => "未知",
    }
}

/// 归一化区域入参：cn→domestic，intl→international（沿用 glm_proxy 口径）。
pub fn normalize_region(region: Option<&str>) -> Option<&'static str> {
    match region {
        None | Some("all") => None,
        Some("cn") | Some("domestic") | Some("china") => Some("domestic"),
        Some("intl") | Some("international") | Some("overseas") => Some("international"),
        _ => None,
    }
}

fn provider_of_region(region: &str) -> Option<&'static str> {
    match region {
        "domestic" => Some(PROVIDER_BIGMODEL),
        "international" => Some(PROVIDER_ZAI),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 配置与账号存储
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PlanAccount {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub api_key: String,
    pub enabled: bool,
    pub added_at: u64,
    pub last_error: Option<String>,
    pub cooldown_until_ms: u64,
}

#[derive(Clone, Debug)]
pub struct PlanConfig {
    pub enabled: bool,
    pub api_key: String,
    pub default_provider: String,
    pub default_model: String,
    pub models: Vec<String>,
    pub cache_enabled: bool,
    pub cache_ttl_ms: u64,
    pub cache_max_entries: usize,
    pub spoof: bool,
    pub accounts: Vec<PlanAccount>,
}

pub fn config_path() -> std::path::PathBuf {
    crate::modules::config::store_dir().join("glmplan_config.json")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn gen_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

pub fn gen_api_key() -> String {
    format!("sk-glmplan-{}", uuid::Uuid::new_v4())
}

pub fn load_config() -> PlanConfig {
    let default = || PlanConfig {
        enabled: false,
        api_key: gen_api_key(),
        default_provider: PROVIDER_BIGMODEL.to_string(),
        default_model: DEFAULT_MODEL.to_string(),
        models: DEFAULT_MODELS.iter().map(|s| s.to_string()).collect(),
        cache_enabled: true,
        cache_ttl_ms: 24 * 60 * 60 * 1000,
        cache_max_entries: 1000,
        spoof: true,
        accounts: Vec::new(),
    };
    let text = match std::fs::read_to_string(config_path()) {
        Ok(t) => t,
        Err(_) => {
            // 首次使用：持久化默认配置（含生成的 api_key），否则每次 load 都会
            // 重新随机生成密钥，导致鉴权永不对不上且用户无从得知。
            let cfg = default();
            save_config(&cfg);
            return cfg;
        }
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return default();
    };
    let mut cfg = default();
    cfg.enabled = v.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    if let Some(k) = v.get("api_key").and_then(Value::as_str) {
        if !k.is_empty() {
            cfg.api_key = k.to_string();
        }
    }
    if let Some(p) = v.get("default_provider").and_then(Value::as_str) {
        if provider_info(p).is_some() {
            cfg.default_provider = p.to_string();
        }
    }
    if let Some(m) = v.get("default_model").and_then(Value::as_str) {
        if !m.is_empty() {
            cfg.default_model = m.to_string();
        }
    }
    if let Some(list) = v.get("models").and_then(Value::as_array) {
        let models: Vec<String> = list
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if !models.is_empty() {
            cfg.models = models;
        }
    }
    cfg.cache_enabled = v.get("cache_enabled").and_then(Value::as_bool).unwrap_or(true);
    cfg.cache_ttl_ms = v
        .get("cache_ttl_ms")
        .and_then(Value::as_u64)
        .unwrap_or(cfg.cache_ttl_ms);
    cfg.cache_max_entries = v
        .get("cache_max_entries")
        .and_then(Value::as_u64)
        .unwrap_or(cfg.cache_max_entries as u64) as usize;
    cfg.spoof = v.get("spoof").and_then(Value::as_bool).unwrap_or(true);
    if let Some(list) = v.get("accounts").and_then(Value::as_array) {
        cfg.accounts = list
            .iter()
            .filter_map(|a| {
                let provider = a.get("provider").and_then(Value::as_str)?;
                provider_info(provider)?;
                Some(PlanAccount {
                    id: a
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(&gen_id())
                        .to_string(),
                    label: a
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("未命名")
                        .to_string(),
                    provider: provider.to_string(),
                    api_key: a
                        .get("api_key")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    enabled: a.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                    added_at: a.get("added_at").and_then(Value::as_u64).unwrap_or(0),
                    last_error: a
                        .get("last_error")
                        .and_then(Value::as_str)
                        .map(String::from),
                    cooldown_until_ms: a
                        .get("cooldown_until_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                })
            })
            .collect();
    }
    cfg
}

pub fn save_config(cfg: &PlanConfig) {
    let accounts: Vec<Value> = cfg
        .accounts
        .iter()
        .map(|a| {
            json!({
                "id": a.id, "label": a.label, "provider": a.provider,
                "api_key": a.api_key, "enabled": a.enabled,
                "added_at": a.added_at, "last_error": a.last_error,
                "cooldown_until_ms": a.cooldown_until_ms,
            })
        })
        .collect();
    let v = json!({
        "enabled": cfg.enabled,
        "api_key": cfg.api_key,
        "default_provider": cfg.default_provider,
        "default_model": cfg.default_model,
        "models": cfg.models,
        "cache_enabled": cfg.cache_enabled,
        "cache_ttl_ms": cfg.cache_ttl_ms,
        "cache_max_entries": cfg.cache_max_entries,
        "spoof": cfg.spoof,
        "accounts": accounts,
        "updated_at": now_ms(),
    });
    if let Some(dir) = config_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(config_path(), serde_json::to_string_pretty(&v).unwrap_or_default());
}

// ---------------------------------------------------------------------------
// 模型清单（区域分组，参照 workbuddy-pool 区域契约）
// ---------------------------------------------------------------------------

/// 模型清单：cfg.models 按 provider 归区（bigmodel→domestic，zai→international），
/// 每条带 region 字段，顶层 regions 元信息为各组条数；region 过滤口径同 glm_proxy。
pub fn models_payload_filtered(cfg: &PlanConfig, region: Option<&str>) -> Value {
    let want = normalize_region(region);
    let mut data = Vec::new();
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for model in &cfg.models {
        let Some((_, _, region_str)) = provider_of_region_info(cfg, model) else {
            continue;
        };
        if let Some(w) = want {
            if region_str != w {
                continue;
            }
        }
        *counts.entry(region_str).or_insert(0) += 1;
        let mut entry = json!({
            "id": model,
            "object": "model",
            "created": 0,
            "owned_by": format!("glmplan:{}", model_provider(cfg, model)),
            "region": region_str,
        });
        if let Some(cw) = crate::modules::config::max_context_window(model) {
            entry["contextWindow"] = json!(cw);
        }
        data.push(entry);
    }
    let mut regions = serde_json::Map::new();
    for (name, cnt) in [("domestic", cn_count(&counts)), ("international", intl_count(&counts))] {
        if want.map(|w| w == name).unwrap_or(true) {
            regions.insert(name.to_string(), json!({ "models": cnt }));
        }
    }
    json!({"object": "list", "data": data, "regions": Value::Object(regions)})
}

// 模型 → provider 的归属：默认模型按 default_provider 归区；条目级 provider
// 由模型名前缀显式指定（如 "zai:glm-4.6" / "bigmodel:glm-4.6"）。
fn model_provider(cfg: &PlanConfig, model: &str) -> &'static str {
    match provider_of_model(cfg, model) {
        PROVIDER_ZAI => PROVIDER_ZAI,
        _ => PROVIDER_BIGMODEL,
    }
}

fn provider_of_region_info(cfg: &PlanConfig, model: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let provider = provider_of_model(cfg, model);
    let info = provider_info(provider)?;
    Some((provider, info.0, info.2))
}

fn provider_of_model(cfg: &PlanConfig, model: &str) -> &'static str {
    if let Some((prefix, _)) = model.split_once(':') {
        if provider_info(prefix).is_some() {
            return match prefix {
                PROVIDER_ZAI => PROVIDER_ZAI,
                _ => PROVIDER_BIGMODEL,
            };
        }
    }
    // default_provider 只接受两个合法值（load_config 已校验），此处映射回静态串
    if cfg.default_provider == PROVIDER_ZAI {
        PROVIDER_ZAI
    } else {
        PROVIDER_BIGMODEL
    }
}

fn cn_count(counts: &HashMap<&'static str, usize>) -> usize {
    counts.get("domestic").copied().unwrap_or(0)
}

fn intl_count(counts: &HashMap<&'static str, usize>) -> usize {
    counts.get("international").copied().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 精确缓存（Exact Cache）：规范化请求 → sha256 键 → 响应体
// ---------------------------------------------------------------------------

pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
}

struct CacheEntry {
    body: Value,
    expire_at: Instant,
}

static CACHE: Mutex<Option<HashMap<String, CacheEntry>>> = Mutex::new(None);
static CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
static CACHE_MISSES: AtomicUsize = AtomicUsize::new(0);

/// 规范化缓存键：只取影响输出的确定性字段，键序稳定（递归排序 object 键）。
pub fn canonical_cache_key(provider: &str, body: &Value) -> String {
    let parts = json!({
        "provider": provider,
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "messages": body.get("messages").cloned().unwrap_or(Value::Null),
        "tools": body.get("tools").cloned().unwrap_or(Value::Null),
        "tool_choice": body.get("tool_choice").cloned().unwrap_or(Value::Null),
        "temperature": body.get("temperature").cloned().unwrap_or(Value::Null),
        "top_p": body.get("top_p").cloned().unwrap_or(Value::Null),
        "max_tokens": body.get("max_tokens").cloned().unwrap_or(Value::Null),
        "response_format": body.get("response_format").cloned().unwrap_or(Value::Null),
    });
    let canonical = canonicalize_value(&parts);
    let digest = sha256_hex(canonical.as_bytes());
    format!("glmplan:{provider}:{digest}")
}

/// 递归排序 object 键，保证同语义请求得到同一键。
fn canonicalize_value(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .filter_map(|k| map.get(k.as_str()).map(|v| format!("{k}:{}", canonicalize_value(v))))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonicalize_value).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    // 轻量摘要：工程内已有 sha2 依赖则直接用；此处用可用实现兜底。
    // （wb-switch-core 依赖 sha2 —— 见 Cargo.toml；若无则此函数会编译失败并提示）
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn cache_get(key: &str) -> Option<Value> {
    let mut guard = CACHE.lock().ok()?;
    let map = guard.as_mut()?;
    let entry = map.get(key)?;
    if entry.expire_at < Instant::now() {
        map.remove(key);
        return None;
    }
    Some(entry.body.clone())
}

fn cache_put(cfg: &PlanConfig, key: String, body: Value) {
    let Ok(mut guard) = CACHE.lock() else { return };
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= cfg.cache_max_entries {
        // 简单过期清理：先删过期，仍超限则移除最早过期项
        let now = Instant::now();
        map.retain(|_, e| e.expire_at > now);
        if map.len() >= cfg.cache_max_entries {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.expire_at)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
    }
    map.insert(
        key,
        CacheEntry {
            body,
            expire_at: Instant::now() + Duration::from_millis(cfg.cache_ttl_ms),
        },
    );
}

pub fn cache_stats() -> CacheStats {
    let hits = CACHE_HITS.load(Ordering::Relaxed) as u64;
    let misses = CACHE_MISSES.load(Ordering::Relaxed) as u64;
    let entries = CACHE
        .lock()
        .map(|g| g.as_ref().map(|m| m.len()).unwrap_or(0))
        .unwrap_or(0);
    CacheStats { hits, misses, entries }
}

// ---------------------------------------------------------------------------
// 上游调用（轮询 + 拟真 + 缓存）
// ---------------------------------------------------------------------------

fn upstream_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .user_agent("GLM-Coding-CLI/1.0 (zcode)")
            .timeout(Duration::from_secs(300))
            .build()
            .expect("failed to build glmplan client")
    })
}

fn spoof_headers(cfg: &PlanConfig, provider: &str) -> Vec<(String, String)> {
    if !cfg.spoof {
        return Vec::new();
    }
    let referer = match provider {
        PROVIDER_ZAI => "https://zcode.z.ai",
        _ => "https://open.bigmodel.cn",
    };
    vec![
        ("Referer".to_string(), referer.to_string()),
        ("Origin".to_string(), referer.to_string()),
        ("X-Source-Title".to_string(), "cli".to_string()),
    ]
}

/// 轮询游标（每 provider 独立）。
fn pool_cursor(provider: &str) -> usize {
    static CURSORS: Mutex<Option<HashMap<String, AtomicUsize>>> = Mutex::new(None);
    let Ok(mut guard) = CURSORS.lock() else { return 0 };
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(provider.to_string())
        .or_insert_with(|| AtomicUsize::new(0))
        .fetch_add(1, Ordering::Relaxed)
}

fn pick_account<'a>(cfg: &'a PlanConfig, provider: &str) -> Option<&'a PlanAccount> {
    let now = now_ms();
    let usable: Vec<&PlanAccount> = cfg
        .accounts
        .iter()
        .filter(|a| a.provider == provider && a.enabled && a.cooldown_until_ms < now)
        .collect();
    if usable.is_empty() {
        return None;
    }
    let start = pool_cursor(provider) % usable.len();
    Some(usable[start])
}

fn mark_account_error(cfg: &mut PlanConfig, account_id: &str, error: &str, cooldown_ms: u64) {
    if let Some(a) = cfg.accounts.iter_mut().find(|a| a.id == account_id) {
        a.last_error = Some(error.to_string());
        a.cooldown_until_ms = now_ms() + cooldown_ms;
    }
    save_config(cfg);
}

fn error_text(status: u16, text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(msg) = v
            .pointer("/error/message")
            .or_else(|| v.get("message"))
            .and_then(Value::as_str)
        {
            return format!("上游 {status}: {msg}");
        }
    }
    format!("上游 {status}: {}", text.chars().take(200).collect::<String>())
}

/// OpenAI 兼容 chat/completions（含精确缓存；流式不缓存）。
pub async fn chat_completions(mut body: Value) -> Result<Value, (u16, String)> {
    let mut cfg = load_config();
    if !cfg.enabled {
        return Err((503, "GLM Plan 网关未启用".to_string()));
    }
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(cfg.default_model.as_str())
        .to_string();
    // 显式区域：body.region / body.provider > 模型名前缀 > 默认 provider
    let region_hint = normalize_region(body.get("region").and_then(Value::as_str));
    let provider = region_hint
        .and_then(provider_of_region)
        .unwrap_or_else(|| provider_of_model(&cfg, &requested))
        .to_string();
    body["model"] = json!(requested
        .split_once(':')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or(requested.clone()));

    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let key = canonical_cache_key(&provider, &body);
    if cfg.cache_enabled && !streaming {
        if let Some(cached) = cache_get(&key) {
            CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            return Ok(cached);
        }
        CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }

    let Some((openai_base, _, _)) = provider_info(&provider) else {
        return Err((400, format!("未知 provider: {provider}")));
    };
    let Some(account) = pick_account(&cfg, &provider) else {
        return Err((
            503,
            format!(
                "{}账号池无可用账号：请先在 GLM Plan 页添加并启用 {} 的 API Key",
                provider_label(&provider),
                provider_label(&provider)
            ),
        ));
    };
    let account_id = account.id.clone();
    let api_key = account.api_key.clone();

    let url = format!("{openai_base}/chat/completions");
    let mut req = upstream_client()
        .post(&url)
        .bearer_auth(&api_key)
        .json(&body);
    for (k, v) in spoof_headers(&cfg, &provider) {
        req = req.header(k, v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (502u16, format!("上游请求失败: {e}")))?;
    let status = resp.status().as_u16();

    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        let msg = error_text(status, &text);
        if status == 401 || status == 403 {
            mark_account_error(&mut cfg, &account_id, &msg, 10 * 60 * 1000);
        }
        return Err((status, msg));
    }

    let payload: Value = resp
        .json()
        .await
        .map_err(|e| (502u16, format!("上游响应解析失败: {e}")))?;

    if cfg.cache_enabled && !streaming {
        cache_put(&cfg, key, payload.clone());
    }
    Ok(payload)
}

/// 流式 chat：直连上游透传字节流（不缓存，账号轮询/拟真同非流式）。
pub async fn chat_stream(mut body: Value) -> Result<reqwest::Response, (u16, String)> {
    let cfg = load_config();
    if !cfg.enabled {
        return Err((503, "GLM Plan 网关未启用".to_string()));
    }
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(cfg.default_model.as_str())
        .to_string();
    let region_hint = normalize_region(body.get("region").and_then(Value::as_str));
    let provider = region_hint
        .and_then(provider_of_region)
        .unwrap_or_else(|| provider_of_model(&cfg, &requested))
        .to_string();
    let Some((openai_base, _, _)) = provider_info(&provider) else {
        return Err((400, format!("未知 provider: {provider}")));
    };
    let Some(account) = pick_account(&cfg, &provider) else {
        return Err((
            503,
            format!("{}账号池无可用账号", provider_label(&provider)),
        ));
    };
    body["model"] = json!(requested
        .split_once(':')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or(requested));
    body["stream"] = json!(true);
    let url = format!("{openai_base}/chat/completions");
    let mut req = upstream_client()
        .post(&url)
        .bearer_auth(&account.api_key)
        .json(&body);
    for (k, v) in spoof_headers(&cfg, &provider) {
        req = req.header(k, v);
    }
    resp_req(req).await
}

async fn resp_req(req: reqwest::RequestBuilder) -> Result<reqwest::Response, (u16, String)> {
    let resp = req.send().await.map_err(|e| (502u16, format!("上游请求失败: {e}")))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        return Err((status, error_text(status, &text)));
    }
    Ok(resp)
}

/// Anthropic 兼容 /v1/messages（透传 + 拟真 metadata.user_id；不缓存）。
pub async fn anthropic_messages(body: Value) -> Result<(u16, String), (u16, String)> {
    let mut cfg = load_config();
    if !cfg.enabled {
        return Err((503, "GLM Plan 网关未启用".to_string()));
    }
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(cfg.default_model.as_str())
        .to_string();
    let region_hint = normalize_region(body.get("region").and_then(Value::as_str));
    let provider = region_hint
        .and_then(provider_of_region)
        .unwrap_or_else(|| provider_of_model(&cfg, &requested))
        .to_string();
    let Some((_, anthropic_base, _)) = provider_info(&provider) else {
        return Err((400, format!("未知 provider: {provider}")));
    };
    let Some(account) = pick_account(&cfg, &provider) else {
        return Err((
            503,
            format!("{}账号池无可用账号", provider_label(&provider)),
        ));
    };
    let account_id = account.id.clone();
    let api_key = account.api_key.clone();

    let mut body = body;
    body["model"] = json!(requested
        .split_once(':')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or(requested));
    if cfg.spoof {
        // 官方 CLI 的 metadata.user_id 形如 "_{deviceMid}_user_{accountId}"；
        // 以账号 id 摘要 + 随机会话伪造，保持会话内稳定。
        let session = body
            .pointer("/metadata/user_id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| {
                format!(
                    "_{}_user_{}",
                    &uuid::Uuid::new_v4().simple().to_string()[..16],
                    &account_id
                )
            });
        body["metadata"] = json!({ "user_id": session });
    }

    let url = format!("{anthropic_base}/v1/messages");
    let mut req = upstream_client()
        .post(&url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    for (k, v) in spoof_headers(&cfg, &provider) {
        req = req.header(k, v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (502u16, format!("上游请求失败: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let msg = error_text(status, &text);
        if status == 401 || status == 403 {
            mark_account_error(&mut cfg, &account_id, &msg, 10 * 60 * 1000);
        }
        return Err((status, msg));
    }
    Ok((status, text))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> PlanConfig {
        PlanConfig {
            enabled: true,
            api_key: "sk-test".into(),
            default_provider: PROVIDER_BIGMODEL.into(),
            default_model: DEFAULT_MODEL.into(),
            models: vec!["glm-4.6".into(), "zai:glm-4.6".into()],
            cache_enabled: true,
            cache_ttl_ms: 1000,
            cache_max_entries: 16,
            spoof: true,
            accounts: vec![
                PlanAccount {
                    id: "a1".into(),
                    label: "国内一号".into(),
                    provider: PROVIDER_BIGMODEL.into(),
                    api_key: "k1".into(),
                    enabled: true,
                    added_at: 0,
                    last_error: None,
                    cooldown_until_ms: 0,
                },
                PlanAccount {
                    id: "a2".into(),
                    label: "国际一号".into(),
                    provider: PROVIDER_ZAI.into(),
                    api_key: "k2".into(),
                    enabled: true,
                    added_at: 0,
                    last_error: None,
                    cooldown_until_ms: 0,
                },
            ],
        }
    }

    #[test]
    fn provider_info_maps_regions() {
        let (_, _, big) = provider_info(PROVIDER_BIGMODEL).unwrap();
        let (_, _, zai) = provider_info(PROVIDER_ZAI).unwrap();
        assert_eq!(big, "domestic");
        assert_eq!(zai, "international");
        assert!(provider_info("unknown").is_none());
    }

    #[test]
    fn normalize_region_aliases() {
        assert_eq!(normalize_region(Some("cn")), Some("domestic"));
        assert_eq!(normalize_region(Some("intl")), Some("international"));
        assert_eq!(normalize_region(Some("all")), None);
        assert_eq!(normalize_region(None), None);
        assert_eq!(normalize_region(Some("bogus")), None);
    }

    #[test]
    fn models_payload_groups_by_provider_region() {
        let cfg = test_cfg();
        let payload = models_payload_filtered(&cfg, None);
        let regions = &payload["regions"];
        assert_eq!(regions["domestic"]["models"], 1);
        assert_eq!(regions["international"]["models"], 1);
        let data = payload["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["region"], "domestic");
        assert_eq!(data[1]["region"], "international");
        // 过滤：只要国内
        let cn = models_payload_filtered(&cfg, Some("cn"));
        assert_eq!(cn["data"].as_array().unwrap().len(), 1);
        assert!(cn["regions"].get("international").is_none());
    }

    #[test]
    fn canonical_cache_key_is_stable_and_order_insensitive() {
        let a = json!({"model":"glm-4.6","messages":[{"role":"user","content":"hi"}],"temperature":0.7});
        let mut b = a.clone();
        // 键序不同、语义相同
        b = json!({"temperature":0.7,"messages":[{"role":"user","content":"hi"}],"model":"glm-4.6"});
        let k1 = canonical_cache_key("bigmodel", &a);
        let k2 = canonical_cache_key("bigmodel", &b);
        assert_eq!(k1, k2, "同语义请求必须同键");
        // provider 不同 → 键不同
        assert_ne!(k1, canonical_cache_key("zai", &a));
        // 内容不同 → 键不同
        let mut c = a.clone();
        c["temperature"] = json!(0.8);
        assert_ne!(k1, canonical_cache_key("bigmodel", &c));
    }

    #[test]
    fn pick_account_respects_provider_and_cooldown() {
        let mut cfg = test_cfg();
        assert_eq!(pick_account(&cfg, PROVIDER_BIGMODEL).unwrap().id, "a1");
        assert_eq!(pick_account(&cfg, PROVIDER_ZAI).unwrap().id, "a2");
        cfg.accounts[0].cooldown_until_ms = now_ms() + 600_000;
        assert!(pick_account(&cfg, PROVIDER_BIGMODEL).is_none(), "冷却中的账号不可选");
    }

    #[test]
    fn error_text_prefers_upstream_message() {
        assert_eq!(
            error_text(403, r#"{"error":{"message":"The account has been banned"}}"#),
            "上游 403: The account has been banned"
        );
        assert_eq!(error_text(403, r#"{"message":"账号已被封禁"}"#), "上游 403: 账号已被封禁");
        assert_eq!(error_text(403, "plain"), "上游 403: plain");
    }
}
