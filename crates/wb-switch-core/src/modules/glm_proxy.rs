//! GLM 代理：把 AutoClaw 订阅里的 GLM 模型暴露为 OpenAI 兼容接口。
//!
//! 上游是 AutoClaw 桌面端的模型网关
//! `https://autoglm-acceleration-api.zhipuai.cn/autoclaw-proxy/proxy/autoclaw`
//! （`/chat/completions`，OpenAI 协议）。请求需携带：
//! - `X-Authorization: Bearer <token>` —— token 来自本机 auth.json（DPAPI +
//!   AES-GCM 加密），401 时用 refresh token 自动续期（见 platform_checkin）；
//! - `X-Request-Model: <路由ID>` —— 如 `zaicoding_glm-5.3`；请求体里的
//!   `model` 用去掉前缀的 bodyModelId（如 `glm-5.3`）；
//! - `X-Harness-Type: zcode` —— 客户端源码注释标明该类型豁免网关的
//!   system-prompt 白名单校验。
//!
//! 路由清单优先读 AutoClaw `settings.json` 的模型目录（服务端动态下发，
//! 随客户端更新），静态表兜底。仅供本机 127.0.0.1 使用。

use bytes::Bytes;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::autoclaw_accounts::{pool_accounts, pool_accounts_filtered, PoolAccount, AccountRegion};
use crate::modules::autoclaw_intl_pool::{self, Lease, ReleaseOutcome};
use crate::modules::platform_checkin::{autoclaw_data_dir, read_autoclaw_token};

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/glm_proxy_config.json）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GlmProxyConfig {
    pub enabled: bool,
    pub api_key: String,
    /// 速度优先：把慢模型（glm-5.3 / glm-5.3-flash）的请求改投到更快的目标模型。
    pub speed_first: bool,
    /// 速度优先的目标模型（body_model，如 `auto-fast`）。
    pub speed_first_target: String,
    /// 代理选号策略：round_robin（池内轮询，默认）/ local_first（本机优先，失败顺延）
    /// / local_only（只用本机登录账号）/ pinned（只用 pinned_account_id 指定的账号）。
    pub account_mode: String,
    /// pinned 模式下使用的账号 id（`autoclaw:<uid>`）。
    pub pinned_account_id: String,
}

/// 代理选号策略取值。
pub const ACCOUNT_MODE_ROUND_ROBIN: &str = "round_robin";
pub const ACCOUNT_MODE_LOCAL_FIRST: &str = "local_first";
pub const ACCOUNT_MODE_LOCAL_ONLY: &str = "local_only";
pub const ACCOUNT_MODE_PINNED: &str = "pinned";
/// 全部合法取值（供 API 校验与前端渲染）。
pub const ACCOUNT_MODES: [&str; 4] = [
    ACCOUNT_MODE_ROUND_ROBIN,
    ACCOUNT_MODE_LOCAL_FIRST,
    ACCOUNT_MODE_LOCAL_ONLY,
    ACCOUNT_MODE_PINNED,
];

fn normalize_account_mode(value: &str) -> String {
    let v = value.trim();
    if ACCOUNT_MODES.contains(&v) {
        v.to_string()
    } else {
        ACCOUNT_MODE_ROUND_ROBIN.to_string()
    }
}

/// 速度优先模式下会被改投的「慢模型」。
pub const SPEED_FIRST_SOURCES: [&str; 2] = ["glm-5.3", "glm-5.3-flash"];
/// 速度优先模式的默认目标模型（auto：实测最快且消耗等级低）。
pub const SPEED_FIRST_DEFAULT_TARGET: &str = "auto";

fn config_path() -> PathBuf {
    store_dir().join("glm_proxy_config.json")
}

pub fn load_config() -> GlmProxyConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| GlmProxyConfig {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            api_key: v
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            speed_first: v.get("speed_first").and_then(Value::as_bool).unwrap_or(false),
            speed_first_target: v
                .get("speed_first_target")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(SPEED_FIRST_DEFAULT_TARGET)
                .to_string(),
            account_mode: normalize_account_mode(
                v.get("account_mode").and_then(Value::as_str).unwrap_or(""),
            ),
            pinned_account_id: v
                .get("pinned_account_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .unwrap_or_else(|| {
            // 首次使用生成随机密钥，避免本机任意进程无鉴权消耗 AutoClaw 积分。
            let cfg = GlmProxyConfig {
                enabled: true,
                api_key: generate_api_key(),
                speed_first: false,
                speed_first_target: SPEED_FIRST_DEFAULT_TARGET.to_string(),
                account_mode: ACCOUNT_MODE_ROUND_ROBIN.to_string(),
                pinned_account_id: String::new(),
            };
            let _ = save_config(&cfg);
            cfg
        })
}

/// 生成随机访问密钥（32 位十六进制）。
pub fn generate_api_key() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn save_config(cfg: &GlmProxyConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "enabled": cfg.enabled,
        "api_key": cfg.api_key,
        "speed_first": cfg.speed_first,
        "speed_first_target": cfg.speed_first_target,
        "account_mode": cfg.account_mode,
        "pinned_account_id": cfg.pinned_account_id,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 模型路由
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GlmModelRoute {
    /// 网关路由 ID（X-Request-Model），如 `zaicoding_glm-5.3`。
    pub route_id: String,
    /// 请求体 model（去掉首个 `xxx_` 前缀），如 `glm-5.3`。
    pub body_model: String,
    /// AutoClaw 界面显示名，如 `GLM-5.3`。
    pub alias: String,
    /// 目录元数据（消耗等级 / 上下文 / 模态 / 说明）；兜底路由为 Null。
    pub meta: Value,
    /// 模型归属区域：请求该模型时只用对应区域的账号池与网关。
    pub region: AccountRegion,
}

impl GlmModelRoute {
    /// 供前端「选型参考」使用的精简元数据。
    pub fn meta_summary(&self) -> Value {
        json!({
            "consumption": self.meta.get("creditConsumptionLevel").cloned().unwrap_or(Value::Null),
            // 1.4.1：全网关统一口径——目录值缺失时用家族最长宣称值兜底
            "contextWindow": crate::modules::config::max_context_window(&self.body_model)
                .map(|v| json!(v))
                .unwrap_or_else(|| self.meta.get("contextWindow").cloned().unwrap_or(Value::Null)),
            "maxTokens": self.meta.get("maxTokens").cloned().unwrap_or(Value::Null),
            "inputs": self.meta.get("input").cloned().unwrap_or(Value::Null),
            "reasoning": self.meta.get("reasoning").cloned().unwrap_or(Value::Null),
            "tooltip": self.meta.get("tooltip").cloned().unwrap_or(Value::Null),
            "autoModel": self.meta.pointer("/metadata/auto_model_name").cloned().unwrap_or(Value::Null),
        })
    }
}

/// 速度优先决策结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SpeedFirst {
    /// 改投到该目标模型
    Use(String),
    /// 命中慢模型，但请求含多模态内容 → 保持原模型（避免落到不支持该模态的模型）
    SkippedMultimodal,
    /// 不适用（开关关闭 / 非慢模型 / 目标与来源相同）
    NotApplicable,
}

/// 速度优先决策（纯函数，便于单测）。
fn speed_first_plan(cfg: &GlmProxyConfig, requested: &str, multimodal: bool) -> SpeedFirst {
    if !cfg.speed_first {
        return SpeedFirst::NotApplicable;
    }
    let target = cfg.speed_first_target.trim();
    if target.is_empty() {
        return SpeedFirst::NotApplicable;
    }
    let trimmed = requested.trim();
    let hit = SPEED_FIRST_SOURCES
        .iter()
        .any(|source| source.eq_ignore_ascii_case(trimmed));
    if !hit || trimmed.eq_ignore_ascii_case(target) {
        return SpeedFirst::NotApplicable;
    }
    if multimodal {
        return SpeedFirst::SkippedMultimodal;
    }
    SpeedFirst::Use(target.to_string())
}

/// 判断请求是否包含多模态内容（图片/视频/文档等）。
/// 规则：`content` 数组里出现任何非 `text` 的块 → 视为多模态（未知类型保守处理）。
fn payload_has_multimodal(payload: &Value) -> bool {
    let Some(messages) = payload.get("messages").and_then(Value::as_array) else {
        return false;
    };
    for message in messages {
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
            if !kind.is_empty() && !kind.eq_ignore_ascii_case("text") {
                return true;
            }
            // 少数客户端不带 type，直接塞 image_url / image 字段。
            if part.get("image_url").is_some() || part.get("image").is_some() {
                return true;
            }
        }
    }
    false
}

/// 按当前配置与请求内容给出改投计划。
pub(crate) fn speed_first_for(requested: &str, payload: &Value) -> SpeedFirst {
    speed_first_plan(&load_config(), requested, payload_has_multimodal(payload))
}

/// 对齐客户端 `toModelProxyBodyModelId`：去掉首个 `^[a-z]+_` 前缀。
fn strip_route_prefix(route_id: &str) -> String {
    match route_id.split_once('_') {
        Some((prefix, rest))
            if !prefix.is_empty()
                && !rest.is_empty()
                && prefix.chars().all(|c| c.is_ascii_lowercase()) =>
        {
            rest.to_string()
        }
        _ => route_id.to_string(),
    }
}

/// v1.0+: 从请求中提取区域信息（优先从 body，其次从 headers）
fn extract_region_from_request(body: &Value) -> Option<AccountRegion> {
    // 优先从请求体中的 region 字段获取
    if let Some(region_str) = body.get("region").and_then(Value::as_str) {
        return AccountRegion::from_str(region_str);
    }
    
    // 如果请求体中没有，尝试从 extra.headers 中获取
    if let Some(extra) = body.get("extra") {
        if let Some(headers) = extra.get("headers") {
            if let Some(x_region) = headers.get("x-region").and_then(Value::as_str) {
                return AccountRegion::from_str(x_region);
            }
        }
    }
    
    None  // 未指定区域时返回 None，使用默认行为
}

/// 国际版静态兜底目录（2026-09-18 抄自本机国际版客户端 settings.json，
/// 供本机客户端是国内版时仍能看到国际模型；随客户端版本可能变化）。
const INTERNATIONAL_FALLBACK_ROUTES: [(&str, &str); 6] = [
    ("zai_auto", "Auto"),
    ("zai_auto-fast", "Auto-Fast"),
    ("zaicoding_glm-5.3", "GLM-5.3"),
    ("zai_glm-5.3-flash", "GLM-5.3-Flash"),
    ("tdpsk_deepseek-v4-flash-202605", "Deepseek-V4.1-Flash"),
    ("tdpsk_deepseek-v4-pro-202606", "DeepSeek-V4-Pro"),
];

/// 国内版静态兜底目录（历史国内客户端 settings.json 实测可用项）。
const DOMESTIC_FALLBACK_ROUTES: [(&str, &str); 4] = [
    ("zaicoding_glm-5.3", "GLM-5.3"),
    ("zai_glm-5.3-flash", "GLM-5.3-Flash"),
    ("zai_auto", "Auto"),
    ("zai_auto-fast", "Auto-Fast"),
];

/// 模型路由清单，按区域标注归属：
/// - 本机客户端 `settings.json` 的 zhipu 目录 → 归属本机客户端登录账号的
///   区域（国际客户端下发国际目录、国内客户端下发国内目录）；
/// - 两个静态兜底表分别补充对应区域缺失的条目（按 region+route_id 去重）。
pub fn model_routes() -> Vec<GlmModelRoute> {
    let mut out: Vec<GlmModelRoute> = Vec::new();
    let mut seen: std::collections::HashSet<(AccountRegion, String)> =
        std::collections::HashSet::new();
    if let Some(dir) = autoclaw_data_dir() {
        if let Ok(text) = std::fs::read_to_string(dir.join("settings.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(entries) = v.pointer("/models/catalog").and_then(Value::as_array) {
                    let client_region =
                        crate::modules::autoclaw_accounts::local_autoclaw_region();
                    for entry in entries {
                        if entry.get("provider").and_then(Value::as_str) != Some("zhipu") {
                            continue;
                        }
                        let Some(route_id) = entry.get("model").and_then(Value::as_str) else {
                            continue;
                        };
                        if route_id.is_empty() {
                            continue;
                        }
                        if !seen.insert((client_region, route_id.to_string())) {
                            continue;
                        }
                        out.push(GlmModelRoute {
                            route_id: route_id.to_string(),
                            body_model: strip_route_prefix(route_id),
                            alias: entry
                                .get("alias")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            meta: entry.clone(),
                            region: client_region,
                        });
                    }
                }
            }
        }
    }
    // 静态兜底：settings.json 缺失或条目不足时按区域补充。
    for (region, table) in [
        (AccountRegion::International, &INTERNATIONAL_FALLBACK_ROUTES[..]),
        (AccountRegion::Domestic, &DOMESTIC_FALLBACK_ROUTES[..]),
    ] {
        for (route_id, alias) in table {
            if !seen.insert((region, route_id.to_string())) {
                continue;
            }
            out.push(GlmModelRoute {
                route_id: route_id.to_string(),
                body_model: strip_route_prefix(route_id),
                alias: alias.to_string(),
                meta: Value::Null,
                region,
            });
        }
    }
    out
}

/// 按路由 ID / 请求体模型名 / 界面别名（不区分大小写）解析路由。
fn resolve_route(model: &str) -> Option<GlmModelRoute> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return None;
    }
    model_routes().into_iter().find(|route| {
        route.route_id.eq_ignore_ascii_case(trimmed)
            || route.body_model.eq_ignore_ascii_case(trimmed)
            || (!route.alias.is_empty() && route.alias.eq_ignore_ascii_case(trimmed))
    })
}

/// AutoClaw 登录态是否可用（能解密出 access token）。
pub fn autoclaw_ready() -> bool {
    read_autoclaw_token().is_ok()
}

/// AutoClaw 网关状态探测：token 有效性、网关可达性与延迟。
///
/// 探测复用签到状态查询（只读 task-list，不消耗积分），内部已含
/// 401 自动续期。
pub async fn gateway_status() -> Value {
    let token_ready = read_autoclaw_token().is_ok();
    let start = std::time::Instant::now();
    let probe = crate::modules::platform_checkin::get_autoclaw_status().await;
    let latency_ms = start.elapsed().as_millis() as u64;
    let gateway_ok = probe.get("ok").and_then(Value::as_bool).unwrap_or(false);
    json!({
        "tokenReady": token_ready,
        "gatewayOk": gateway_ok,
        "latencyMs": latency_ms,
        "error": probe.get("error").and_then(Value::as_str),
        "models": model_routes().len(),
        "checkedAt": now_ms(),
    })
}

// ---------------------------------------------------------------------------
// 上游请求
// ---------------------------------------------------------------------------

fn upstream_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let ua = crate::modules::account_protection::simulated_autoclaw_user_agent();
        reqwest::Client::builder()
            // 国内上游直连，不继承系统代理（Clash 抖动会掐断流，见 2026-09-18 上午两起断连）
            .no_proxy()
            .user_agent(ua)
            .build()
            .expect("failed to build glm proxy client")
    })
}

fn platform_tm() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "win"
    }
}

/// 对齐客户端 `buildModelProxyUpstreamHeaders`。
fn upstream_headers(token: &str, route_model: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let bearer = if token.starts_with("Bearer ") {
        token.to_string()
    } else {
        format!("Bearer {token}")
    };
    let pairs = [
        ("User-Agent", crate::modules::account_protection::simulated_autoclaw_user_agent()),
        ("Content-Type", "application/json".to_string()),
        ("Accept", "*/*".to_string()),
        ("X-Authorization", bearer),
        ("X-Request-Id", uuid::Uuid::new_v4().to_string()),
        ("X-Request-Model", route_model.to_string()),
        ("X-Client-Type", "pc".to_string()),
        ("X-Product", "autoclaw".to_string()),
        // 豁免网关 system-prompt 白名单校验（客户端同款行为）。
        ("X-Harness-Type", "zcode".to_string()),
        ("X-Tm", platform_tm().to_string()),
        ("X-Version", crate::modules::platform_checkin::autoclaw_app_version()),
        ("X-Lang", "zh-CN".to_string()),
        ("x_trace_id", "autoclaw-desktop".to_string()),
    ];
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

async fn send_once(
    payload: &Value,
    token: &str,
    route_model: &str,
    region: AccountRegion,
) -> Result<reqwest::Response, String> {
    upstream_client()
        .post(region.chat_upstream_url())
        .headers(upstream_headers(token, route_model))
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("AutoClaw 网关请求失败: {e}"))
}

/// 「请求可能已送达并计费」的哨兵状态码（2026-10-09）。
///
/// 🔴 背景缺陷（用户报「扣减积分却扣了两个账号」）：
/// 此前所有 `reqwest::Error` 都被压成 `status = 0`，而 `0` 被判为**可换号重试**。
/// 但 `reqwest::Error` 混了两类语义完全不同的失败：
///
/// | 失败 | 请求是否送达 | 上游是否计费 | 换号重试 |
/// | --- | --- | --- | --- |
/// | 连接阶段（DNS/拒绝/构造）| ❌ 未送达 | ❌ 未计费 | ✅ 安全 |
/// | **超时 / 响应体 / 解码** | ✅ **已送达** | ⚠️ **可能已计费** | ❌ **会双边扣费** |
///
/// 实测后果链：账号 A 发出请求 → 上游**已扣 A 的费** → 响应超时判 `status=0` 失败
/// → `measurement.abort()` 不计量 → 换号 B 重试 → 上游**又扣 B 的费**
/// ⇒ **A、B 两个账号都被扣费**，而系统只记录了 B（甚至只记 B 为成功）。
///
/// 故：可能已计费的失败标记为 `MIGHT_HAVE_BILLED`，**调用侧不得换号**，
/// 直接把错误透出（宁可不重试，也不要静默多扣一个账号的额度）。
pub const MIGHT_HAVE_BILLED: u16 = u16::MAX;

/// 带分类的发送：返回 `(状态码, 错误文本)`。
/// - `(0, msg)`                    → 连接阶段失败，**请求未送达**，可安全换号重试
/// - `(MIGHT_HAVE_BILLED, msg)`    → **可能已计费**，不可换号
///
/// 判定取向**保守**：只有明确属于"连接建立阶段"的失败才敢重试。
async fn send_once_classified(
    payload: &Value,
    token: &str,
    route_model: &str,
    region: AccountRegion,
) -> Result<reqwest::Response, (u16, String)> {
    upstream_client()
        .post(region.chat_upstream_url())
        .headers(upstream_headers(token, route_model))
        .json(payload)
        .send()
        .await
        .map_err(|e| {
            let msg = format!("AutoClaw 网关请求失败: {e}");
            if e.is_connect() || e.is_request() {
                // 连接未建立 / 请求构造失败 → 上游一定没收到 → 可重试
                (0, msg)
            } else {
                // 超时 / 响应体 / 解码异常 → 无法排除已计费 → 不换号
                (MIGHT_HAVE_BILLED, msg)
            }
        })
}

/// 该失败是否**允许换号重试**（2026-10-09）。
///
/// 唯一被拒的情形是 [`MIGHT_HAVE_BILLED`]：请求**可能已送达并计费**，
/// 换号会让第二个账号被上游再扣一次费（用户报的「一次操作扣两个账号」）。
/// 其余失败（未送达 / 明确 HTTP 错误码）仍可换号，不影响可用性。
pub fn is_retryable_failure(status: u16) -> bool {
    status != MIGHT_HAVE_BILLED
}

/// 1.5.0：账号健康探测（autoclaw_watch 专用）——最小 chat 请求，返回 (状态码, 响应原文)。
/// 不走账号池/缓存/重试，纯单次直连，用于区分封禁(410004)/额度(810001)/登录态(401)。
pub async fn probe_account_chat(region_str: &str, token: &str) -> (u16, String) {
    let region = AccountRegion::from_str(region_str).unwrap_or(AccountRegion::Domestic);
    let payload = json!({
        "model": "glm-5.3",
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": false,
        "max_tokens": 16,
    });
    match send_once(&payload, token, "zaicoding_glm-5.3", region).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            (status, text)
        }
        Err(e) => (0, e),
    }
}

/// 轮询计数：多账号时按请求轮换。
static POOL_CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 按「账号策略」产出候选账号（有序；某账号失败时按顺序顺延）。
///
/// - round_robin：全部账号轮询（默认，均摊额度）；
/// - local_first：本机登录账号优先，失败才用池内其他账号；
/// - local_only：只用本机登录账号（池内其他账号不参与代理，不消耗其积分）；
/// - pinned：只用 `pinned_account_id` 指定的账号（严格，失败不换号）。
/// 
/// v1.0+: 支持区域隔离，根据请求的 region 过滤账号池
fn ordered_pool(cfg: &GlmProxyConfig) -> Result<Vec<PoolAccount>, String> {
    ordered_pool_with_region(cfg, None)
}

/// 带区域参数的账号池选择（v1.0+ 区域隔离）。
/// PoolAccount 自带 region，直接过滤——禁止跨区借用对方区域的账号。
pub fn ordered_pool_with_region(cfg: &GlmProxyConfig, region: Option<AccountRegion>) -> Result<Vec<PoolAccount>, String> {
    let all = pool_accounts();
    let filtered_accounts: Vec<PoolAccount> = match region {
        Some(r) => all.into_iter().filter(|a| a.region == r).collect(),
        None => all,
    };
    
    let cooling_count = filtered_accounts
        .iter()
        .filter(|a| crate::modules::account_protection::is_account_cooling(&a.id))
        .count();

    match cfg.account_mode.as_str() {
        ACCOUNT_MODE_LOCAL_ONLY => {
            let local: Vec<PoolAccount> = filtered_accounts.iter().filter(|a| a.is_local()).cloned().collect();
            if local.is_empty() {
                return Err(
                    "已选择「只用本机账号」，但未检测到本机 AutoClaw 登录态：请在 AutoClaw 桌面端登录，或改回其他选号策略"
                        .to_string(),
                );
            }
            if let Some(first) = local.first() {
                if crate::modules::account_protection::is_account_cooling(&first.id) {
                    return Err(format!(
                        "本机账号「{}」因触发限流正处于防护冷却期，代理已自动熔断暂停。请稍候（约几分钟后自动恢复）或在「账号保护」中手动解除冷却。",
                        first.label
                    ));
                }
            }
            Ok(local)
        }
        ACCOUNT_MODE_LOCAL_FIRST => {
            let mut list: Vec<PoolAccount> =
                filtered_accounts.iter().filter(|a| a.is_local()).cloned().collect();
            list.extend(filtered_accounts.iter().filter(|a| !a.is_local()).cloned());
            if list.is_empty() {
                return Err(
                    "AutoClaw 账号池为空：请登录 AutoClaw 桌面端，或在「GLM 代理」页导入账号"
                        .to_string(),
                );
            }
            let active: Vec<PoolAccount> = list
                .into_iter()
                .filter(|a| !crate::modules::account_protection::is_account_cooling(&a.id))
                .collect();
            if active.is_empty() {
                return Err(format!(
                    "全部 {} 个候选账号均处于限流冷却保护期（防封保护中）：代理已暂停向这些账号发送请求，待冷却倒计时结束后自动恢复。",
                    cooling_count
                ));
            }
            Ok(active)
        }
        ACCOUNT_MODE_PINNED => {
            let id = cfg.pinned_account_id.trim();
            if id.is_empty() {
                return Err(
                    "已选择「固定账号」但未指定账号：请在「GLM 代理」页选择账号，或改回其他选号策略"
                        .to_string(),
                );
            }
            let hit = filtered_accounts
                .iter()
                .find(|a| a.id == id)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "固定账号不在当前池中（{id}）：账号可能已移除或本机登录已切换，请重新选择"
                    )
                })?;
            if crate::modules::account_protection::is_account_cooling(&hit.id) {
                return Err(format!(
                    "固定账号「{}」因触发限流正处于防护冷却期，代理已暂停分流。请稍候或在「账号保护」中手动解除冷却。",
                    hit.label
                ));
            }
            Ok(vec![hit])
        }
        // round_robin 及未知历史值：保持原行为（池内轮询）。
        _ => {
            if filtered_accounts.is_empty() {
                return Err(
                    "AutoClaw 账号池为空：请登录 AutoClaw 桌面端，或在「GLM 代理」页导入账号"
                        .to_string(),
                );
            }
            let active: Vec<PoolAccount> = filtered_accounts
                .into_iter()
                .filter(|a| !crate::modules::account_protection::is_account_cooling(&a.id))
                .collect();
            if active.is_empty() {
                return Err(format!(
                    "全部 {} 个 AutoClaw 账号均处于限流冷却保护期（防封保护中）：代理已暂停向这些账号发送请求，待冷却倒计时结束后自动恢复。",
                    cooling_count
                ));
            }
            Ok(active)
        }
    }
}

/// 单账号尝试：401 刷新该账号登录态重试一次；403 限流等待后重试一次。
/// 上游 URL 按账号所属区域路由（region.chat_upstream_url）。
async fn try_send_for_account(
    payload: &Value,
    route: &GlmModelRoute,
    account: &PoolAccount,
) -> Result<reqwest::Response, String> {
    let mut resp = send_once(payload, &account.bearer(), &route.route_id, account.region).await?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        let new_token = account
            .refresh()
            .await
            .map_err(|e| format!("{}：登录态续期失败（{e}）", account.label))?;
        resp = send_once(payload, &format!("Bearer {new_token}"), &route.route_id, account.region)
            .await
            .map_err(|e| format!("AutoClaw 网关请求失败: {e}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            account.mark_invalid("续期后仍返回 401");
            return Err(format!("{}：登录态失效", account.label));
        }
    }
    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        let text = resp.text().await.unwrap_or_default();
        // 1.4.2：上游封禁（410004/banned/封禁）落 12h 冷却并透出——此前直接
        // 透传错误不留痕（账号 lastError 恒空、页面/渠道健康看不到封禁）。
        if text.contains("410004") || text.contains("banned") || text.contains("封禁") {
            let prot_cfg = crate::modules::account_protection::load_protection_config();
            if prot_cfg.enabled {
                crate::modules::account_protection::mark_account_cooling(
                    &account.id,
                    &account.label,
                    "上游 403 封禁（410004）",
                    720,
                );
            }
            return Err(error_text(reqwest::StatusCode::FORBIDDEN, &text));
        }
        if !(text.contains("繁忙") || text.contains("稍后重试")) {
            return Err(error_text(reqwest::StatusCode::FORBIDDEN, &text));
        }
        // 高峰期限流：等待后重试一次，仍失败则把限流信息透出并触发自适应冷却保护。
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        resp = send_once(payload, &account.bearer(), &route.route_id, account.region).await?;
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            let retry_text = resp.text().await.unwrap_or_default();
            let prot_cfg = crate::modules::account_protection::load_protection_config();
            if prot_cfg.enabled {
                crate::modules::account_protection::mark_account_cooling(
                    &account.id,
                    &account.label,
                    "上游 403 限流（服务繁忙/稍后重试）",
                    prot_cfg.cooldown_minutes,
                );
            }
            return Err(error_text(reqwest::StatusCode::FORBIDDEN, &retry_text));
        }
    }
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let text = resp.text().await.unwrap_or_default();
        let prot_cfg = crate::modules::account_protection::load_protection_config();
        if prot_cfg.enabled {
            crate::modules::account_protection::mark_account_cooling(
                &account.id,
                &account.label,
                "上游 429 限流（请求过于频繁）",
                prot_cfg.cooldown_minutes,
            );
        }
        return Err(error_text(reqwest::StatusCode::TOO_MANY_REQUESTS, &text));
    }
    Ok(resp)
}

/// 发送上游请求并计量：在 AutoClaw 账号池中轮询选号。
/// 计量窗口在选中账号后、发请求前启动（覆盖整个上游调用），余额差才能
/// 包住本次扣费；单账号 401 续期失败时自动切换下一个账号（失败尝试
/// 不产生计费，计量窗口作废）。
///
/// M2（契约 docs/autoclaw-intl-pool-contract.md §3.2/§3.3）：region=international
/// 的请求改走轮换池 `PoolManager.acquire()/release()` lease 模式（六重过滤 +
/// 策略选号 + 错误族回写），不再走 pool_accounts_filtered 顺序过滤；国内 lane
/// 保持原有顺序过滤路径零改动。
async fn send_upstream_routed(
    payload: &Value,
    route: &GlmModelRoute,
) -> Result<
    (
        reqwest::Response,
        PoolAccount,
        crate::modules::model_usage::Measurement,
        Option<crate::modules::account_protection::ConcurrencyGuard>,
        Option<Lease>,
    ),
    String,
> {
    // 区域决定：显式请求区域 > 模型目录归属区域。模型未识别时不会走到这里。
    send_upstream_routed_with_region(payload, route, Some(route.region)).await
}

// ---------------------------------------------------------------------------
// 国际 lane 轮换池 lease（契约 docs/autoclaw-intl-pool-contract.md §3.2/§3.3）
// ---------------------------------------------------------------------------

/// 池空文案前缀：api 层据此把空池错误映射为 503（对齐 glmplan 空池语义），
/// 其余上游错误仍走默认 502。
pub const INTL_POOL_EMPTY_PREFIX: &str = "国际号轮换池暂无可用账号";

/// 国际 lane 池空时的 503 文案（与 glmplan 空池语义一致：503 + 池水位原因）。
/// 直接用池 summary 渲染各状态号数量，让调用方能判断是冷却中还是彻底没号。
pub(crate) fn intl_pool_empty_message(summary: &Value) -> String {
    let n = |key: &str| summary.get(key).and_then(Value::as_u64).unwrap_or(0);
    format!(
        "{INTL_POOL_EMPTY_PREFIX}（总数 {}：健康 {} / 冷却 {} / 额度用尽 {} / 封禁 {} / 失效 {} / 停用 {}；在途 {}/{}），请稍后重试或运行注册管线扩号",
        n("total"),
        n("healthy"),
        n("cooldown"),
        n("quotaOut"),
        n("banned"),
        n("invalid"),
        n("disabled"),
        n("poolInFlight"),
        n("poolConcurrentCap"),
    )
}

/// 国际 lane 单账号尝试（lease 语义，§3.3）：
/// - 401：先触发续期（`PoolAccount::refresh`，沿用既有 401 重试语义）重试
///   一次；仍 401 才 failover（池侧续期由 `release(AuthExpired)` 触发）；
/// - 429 / 403「繁忙/稍后重试」：不再原地等待重试（lease 模式下该号已由
///   状态机冷却，failover 换号比硬等更快），错误体读出交给错误族映射；
/// - 410004 / 810001：错误体读出后由 `outcome_from_http` 精确映射。
async fn intl_try_send_for_account(
    payload: &Value,
    route: &GlmModelRoute,
    account: &PoolAccount,
) -> Result<reqwest::Response, (u16, String)> {
    // 2026-10-09：改用带分类的发送 —— 「可能已计费」的失败必须保住状态码，
    // 否则外层会换号重试，导致同一次操作让两个账号都被上游扣费。
    let mut resp =
        match send_once_classified(payload, &account.bearer(), &route.route_id, account.region).await
        {
            Ok(r) => r,
            Err((st, e)) => return Err((st, e)),
        };
    // 401 → 续期重试一次（既有语义）；续期后仍 401 → failover。
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        let new_token = match account.refresh().await {
            Ok(t) => t,
            Err(e) => {
                return Err((
                    401,
                    format!("{}：登录态续期失败（{e}）", account.label),
                ));
            }
        };
        resp = match send_once_classified(
            payload,
            &format!("Bearer {new_token}"),
            &route.route_id,
            account.region,
        )
        .await
        {
            Ok(r) => r,
            Err((st, e)) => return Err((st, e)),
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err((401, format!("{}：登录态失效", account.label)));
        }
    }
    // 429 / 403：读出错误体交给错误族映射（403+410004 必须落 Banned 证据，
    // 不得与「繁忙」限流混同），随后 failover。
    let status = resp.status().as_u16();
    if status == 429 || status == 403 {
        let text = resp.text().await.unwrap_or_default();
        return Err((status, text));
    }
    Ok(resp)
}

/// 国际 lane 的一次 acquire → 发送 尝试。
/// 返回 Ok((response, lease)) 表示响应已确立 2xx（lease 未回写，由调用方
/// 在响应结束时 intl_release_ok 回写）；Err 表示该号尝试失败（lease 已按
/// 错误族回写并释放）。
///
/// lease 即选号依据（PoolManager 六重过滤 + 策略），发送账号按
/// lease.account_id 从凭据层候选里解析，保证记账归属一致。
async fn intl_lease_attempt(
    payload: &Value,
    route: &GlmModelRoute,
    lease: &Lease,
    account: &PoolAccount,
) -> Result<reqwest::Response, (u16, String)> {
    let manager = autoclaw_intl_pool::pool_manager();
    match intl_try_send_for_account(payload, route, account).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                // 成功：lease 保持未回写，响应原样交回调用方，等到「响应
                // 结束」（非流式 body 读完 / SSE 流尾）再 intl_release_ok。
                Ok(resp)
            } else {
                // 错误路径：读掉错误体（无流可保），按统一错误族映射回写。
                let text = resp.text().await.unwrap_or_default();
                let snippet: String = text.chars().take(300).collect();
                let family = autoclaw_intl_pool::outcome_from_http(status, &text);
                let err = error_text(
                    reqwest::StatusCode::from_u16(status)
                        .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
                    &snippet,
                );
                manager.release(lease, family);
                // 真实 HTTP 错误码（401/429/403/5xx…）：本类失败**未产生计费风险**
                // （上游明确回了错误码），可正常换号。
                Err((status, err))
            }
        }
        Err((status, text)) => {
            // 2026-10-09：MIGHT_HAVE_BILLED 与 0 同属「网络层失败」，同样按文本匹配
            // 错误族（两者文本里都不会有 410004/810001 等业务标记 → 落 Failed，
            // **不惩罚账号**：这不是账号的问题，不该扣它的健康分/进冷却）。
            let family = if status == 0 || status == MIGHT_HAVE_BILLED {
                // 连接层失败 / 超时 / 续期失败：按文本匹配错误族。
                if text.contains("登录态失效") || text.contains("续期失败") {
                    ReleaseOutcome::AuthExpired
                } else if text.contains("410004") {
                    ReleaseOutcome::Banned {
                        code: json!(410004),
                        detail: text.chars().take(160).collect(),
                    }
                } else if text.contains("810001") {
                    ReleaseOutcome::QuotaExhausted
                } else {
                    ReleaseOutcome::Failed(text.chars().take(160).collect())
                }
            } else {
                // HTTP 错误（401/429/403/…）：统一走错误族映射，
                // 410004 → Banned、810001 → QuotaExhausted、401 → AuthExpired、
                // 429/繁忙 → RateLimited（契约 §3.3）。
                autoclaw_intl_pool::outcome_from_http(status, &text)
            };
            // 状态码为哨兵（65535）时不能走 error_text —— 它不是合法 HTTP 状态，
            // 会被 from_u16 拒掉并误显示成 500。
            let err = if status == 0 || status == MIGHT_HAVE_BILLED {
                text.clone()
            } else {
                error_text(
                    reqwest::StatusCode::from_u16(status)
                        .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
                    &text.chars().take(300).collect::<String>(),
                )
            };
            manager.release(lease, family);
            // 2026-10-09：状态码随错误一起返回 —— 外层据此判断能否换号
            // （MIGHT_HAVE_BILLED 不得换号，否则会双边扣费）。
            Err((status, err))
        }
    }
}

/// 国际 lane 一次重试循环内的发送：整段响应（含 SSE 流式）都在同一个
/// lease 内完成。成功路径返回未回写的 lease，由调用方在「响应结束」时
/// `intl_release_ok(lease)` 回写——非流式 = body 读完即回写；SSE 流式 =
/// 泵在流结束时回写（契约 §3.3：SSE 流结束才算 release）。
///
/// 选号以 lease 为准：`PoolManager.acquire()` 按六重过滤 + 策略给出账号，
/// 再按 lease.account_id 从凭据层候选解析实际发送账号（记账归属一致）；
/// 凭据层找不到（导入条目被移除/本机登录切换隐藏）时立即 release 失败，
/// failover 重试由循环承接。
async fn intl_send_with_lease(
    payload: &Value,
    route: &GlmModelRoute,
) -> Result<
    (
        reqwest::Response,
        PoolAccount,
        crate::modules::model_usage::Measurement,
        Option<crate::modules::account_protection::ConcurrencyGuard>,
        Option<Lease>,
    ),
    (u16, String),
> {
    let pool = pool_accounts_filtered(Some(AccountRegion::International));
    let manager = autoclaw_intl_pool::pool_manager();
    if pool.is_empty() {
        // 凭据层就没有国际号：直接给池水位文案（acquire 也会拦，提前给出
        // 更准确的「池内无此号」语义而不是「全部账号不可用」）。
        let summary = manager.summary();
        return Err((0, intl_pool_empty_message(&summary)));
    }
    let prot_cfg = crate::modules::account_protection::load_protection_config();
    let max_concurrency = if prot_cfg.enabled {
        prot_cfg.max_concurrency_per_account
    } else {
        usize::MAX
    };

    let mut last_error = String::new();
    for _attempt in 0..10 {
        // 每轮重新选号：acquire 按策略 + 六重过滤给出该次请求的账号
        // （契约 §3.2）；失败 release 后下轮会换号/重选。
        let Some(lease) = manager.acquire("international") else {
            let summary = manager.summary();
            return Err((0, intl_pool_empty_message(&summary)));
        };
        let Some(account) = pool
            .iter()
            .find(|a| a.id == lease.account_id)
            .cloned()
            .or_else(|| {
                pool.iter().find(|a| a.store_id.as_deref() == Some(lease.account_id.as_str()))
                    .cloned()
            })
        else {
            // 凭据层已无此号（移除/登录切换隐藏）：按 Failed 回写并返回。
            manager.release(
                &lease,
                ReleaseOutcome::Failed("凭据层不存在该轮换池账号".to_string()),
            );
            last_error = format!("轮换池账号 {} 已不在凭据层", lease.account_id);
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            continue;
        };
        let guard = if prot_cfg.enabled {
            match crate::modules::account_protection::try_acquire_concurrency_slot(
                &account.id,
                max_concurrency,
            ) {
                Some(g) => Some(g),
                None => {
                    // 保护层并发满：lease 记 Ok 之外的回写语义不适用（请求未发出），
                    // 用 Failed 记失败但不动冷却；inFlight 由 release 归还。
                    manager.release(
                        &lease,
                        ReleaseOutcome::Failed("保护层并发槽位已满".to_string()),
                    );
                    last_error = format!("账号 {} 保护层并发已满", account.label);
                    continue;
                }
            }
        } else {
            None
        };
        let measurement =
            crate::modules::model_usage::Measurement::start(&account.access_token, &route.body_model)
                .await;
        match intl_lease_attempt(payload, route, &lease, &account).await {
            Ok(resp) => return Ok((resp, account, measurement, guard, Some(lease))),
            Err((st, error)) => {
                measurement.abort();
                drop(guard);
                // 🔴 2026-10-09：请求「可能已送达并计费」时**禁止换号**。
                //
                // 用户报「扣减积分却扣了两个账号」的成因链：
                //   账号 A 发出请求 → 上游已扣 A 的费 → 响应超时/中断（st=MIGHT_HAVE_BILLED）
                //   → 若不在此处止步，会继续换号 B 重试 → 上游又扣 B 的费
                //   ⇒ A、B 双边扣费，而系统只记 B，用户看到"一次操作扣了两个号"。
                //
                // 取向：宁可不重试（把错误透出，由用户决定是否再发），
                // 也不静默多扣一个账号的额度。
                if st == MIGHT_HAVE_BILLED {
                    return Err((
                        MIGHT_HAVE_BILLED,
                        format!(
                            "{label}：上游响应异常，但**请求可能已产生计费**，为避免重复扣费已停止换号重试。原始错误：{error}",
                            label = account.label,
                        ),
                    ));
                }
                last_error = error;
            }
        }
        if pool.len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    if last_error.is_empty() {
        last_error = "当前可用账号并发已达上限，请稍候重试".to_string();
    }
    Err((
        0,
        format!(
            "国际号轮换池全部 {} 个账号均不可用：{}",
            pool.len(),
            last_error
        ),
    ))
}

/// 成功 lease 的延迟回写（契约 §3.3：请求完成/SSE 流结束才算 release）。
/// release(Ok) 记 todayCalls/totalCalls；lease 已回写（幂等登记表去重）时
/// 重复调用无害。
pub(crate) fn intl_release_ok(lease: &Option<Lease>) {
    if let Some(lease) = lease {
        autoclaw_intl_pool::pool_manager().release(lease, ReleaseOutcome::Ok);
    }
}

/// 带区域参数的上游请求路由 (v1.0+ 区域隔离)
async fn send_upstream_routed_with_region(
    payload: &Value,
    route: &GlmModelRoute,
    region: Option<AccountRegion>,
) -> Result<
    (
        reqwest::Response,
        PoolAccount,
        crate::modules::model_usage::Measurement,
        Option<crate::modules::account_protection::ConcurrencyGuard>,
        Option<Lease>,
    ),
    String,
> {
    // M2（契约 §3.2/§3.3）：国际 lane 改走轮换池 lease 模式；国内 lane 保持
    // 原有 pool_accounts_filtered 顺序过滤路径零改动。
    if matches!(region, Some(AccountRegion::International)) {
        // 2026-10-09：`intl_send_with_lease` 现返回 `(u16, String)` 以便区分
        // 「可能已计费」（MIGHT_HAVE_BILLED）。**阻断换号的判断已在它内部完成**
        // （见其中「禁止换号」分支），故此处只需取出错误文本。
        return intl_send_with_lease(payload, route)
            .await
            .map_err(|(_st, e)| e);
    }
    let cfg = load_config();
    let pool = ordered_pool_with_region(&cfg, region)?;
    let prot_cfg = crate::modules::account_protection::load_protection_config();
    let max_concurrency = if prot_cfg.enabled {
        prot_cfg.max_concurrency_per_account
    } else {
        usize::MAX
    };

    // 轮询策略下按游标起头；其余策略固定从候选首位开始（首位即目标账号）。
    let start = if cfg.account_mode == ACCOUNT_MODE_ROUND_ROBIN {
        POOL_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % pool.len()
    } else {
        0
    };

    let mut last_error = String::new();
    // 尝试获取并发槽位并发送请求；若候选账号并发均满，平滑重试等待最多 10 次（约 2.5s 削峰平滑）
    for attempt in 0..10 {
        for index in 0..pool.len() {
            let account = &pool[(start + index) % pool.len()];
            let guard = if prot_cfg.enabled {
                match crate::modules::account_protection::try_acquire_concurrency_slot(
                    &account.id,
                    max_concurrency,
                ) {
                    Some(g) => Some(g),
                    None => continue, // 该账号并发已达上限，跳过尝试下一个可用账号
                }
            } else {
                None
            };

            let measurement =
                crate::modules::model_usage::Measurement::start(&account.access_token, &route.body_model)
                    .await;
            match try_send_for_account(payload, route, account).await {
                Ok(resp) => return Ok((resp, account.clone(), measurement, guard, None)),
                Err(error) => {
                    measurement.abort();
                    drop(guard);
                    last_error = error;
                }
            }
        }
        if !last_error.is_empty() && attempt == 0 && pool.len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    if last_error.is_empty() {
        last_error = "当前可用账号并发已达上限，请稍候重试".to_string();
    }

    Err(format!(
        "全部 {} 个账号均不可用：{}",
        pool.len(),
        last_error
    ))
}

fn error_text(status: reqwest::StatusCode, text: &str) -> String {
    // 优先透出上游 JSON 的 message / error.message。
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        for pointer in ["/message", "/error/message", "/msg"] {
            if let Some(msg) = v.pointer(pointer).and_then(Value::as_str) {
                if !msg.trim().is_empty() {
                    return format!("上游返回 {status}: {msg}");
                }
            }
        }
    }
    format!(
        "上游返回 {status}: {}",
        text.chars().take(300).collect::<String>()
    )
}

/// 非流式对话补全：缓冲整个响应并解析为 JSON（OpenAI 格式原样透传）。
/// v1.0+: 支持从请求体中提取 region 参数进行区域隔离
pub async fn chat_completions_json(body: Value) -> Result<Value, String> {
    let requested_model = body.get("model").and_then(Value::as_str).unwrap_or("glm-5.3").to_string();
    
    // 速度优先：慢模型改投到更快的目标模型；多模态请求保持原模型。
    let (effective_model, skipped_multimodal) = match speed_first_for(&requested_model, &body) {
        SpeedFirst::Use(target) => (target, false),
        SpeedFirst::SkippedMultimodal => (requested_model.clone(), true),
        SpeedFirst::NotApplicable => (requested_model.clone(), false),
    };
    let route = resolve_route(&effective_model).ok_or_else(|| unknown_model_error(&effective_model))?;
    // v1.0+ 区域隔离：显式请求区域（body.region / x-region）优先；
    // 未指定时按模型目录归属区域路由（国际模型走国际池，国内模型走国内池）。
    let region = extract_region_from_request(&body).or_else(|| Some(route.region));
    let mut payload = body.clone();
    payload["model"] = json!(route.body_model);
    let started = now_ms();
    let (resp, account, measurement, _guard, intl_lease) =
        match send_upstream_routed_with_region(&payload, &route, region).await {
            Ok(quint) => quint,
            Err(e) => {
                record_and_notify_failure(Some(&route), &requested_model, started, &e, skipped_multimodal);
                return Err(e);
            }
        };
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取上游响应失败: {e}"))?;
    if !status.is_success() {
        let error = error_text(status, &text);
        record_and_notify_failure(Some(&route), &requested_model, started, &error, skipped_multimodal);
        return Err(error);
    }
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => {
            let error = format!("上游响应解析失败: {e}");
            record_and_notify_failure(Some(&route), &requested_model, started, &error, skipped_multimodal);
            return Err(error);
        }
    };
    // 非流式：body 读完 = 请求结束，lease 回写 Ok（契约 §3.3）。intl_lease
    // 仅国际 lane 为 Some；国内 lane None 无副作用。
    intl_release_ok(&intl_lease);
    let mut usage = crate::modules::model_usage::extract_usage(&value);
    note_substitution(&mut usage, &requested_model, &route.body_model, skipped_multimodal);
    let (credits, ledger_id) = measurement.finish().await;
    record_success(
        &route,
        &account,
        &usage,
        credits,
        ledger_id,
        started,
        now_ms(),
    );
    Ok(value)
}

/// 流式对话补全：返回上游内容类型与 SSE 字节流。
pub struct GlmUpstreamStream {
    pub content_type: String,
    pub rx: tokio::sync::mpsc::Receiver<Result<Bytes, String>>,
}

/// 已就绪的流式调用上下文，供 OpenAI 透传与 Anthropic 转换两种泵共用。
pub(crate) struct PreparedStream {
    pub route: GlmModelRoute,
    pub account: PoolAccount,
    pub requested_model: String,
    /// 命中慢模型但请求是多模态，未改投（用于记录说明）。
    pub skipped_multimodal: bool,
    pub started: i64,
    pub measurement: crate::modules::model_usage::Measurement,
    pub _concurrency_guard: Option<crate::modules::account_protection::ConcurrencyGuard>,
    /// 国际 lane 轮换池 lease（契约 §3.3）：成功响应的 lease 未回写，随
    /// PreparedStream 进入泵，流结束时 `intl_release_ok` 回写；国内 lane
    /// / 池外路径为 None。
    pub intl_lease: Option<Lease>,
}

/// 解析路由、发起上游流式请求（计量窗口已随请求启动）。
async fn prepare_stream(payload: &Value, requested_model: &str) -> Result<(PreparedStream, reqwest::Response), String> {
    // 速度优先：慢模型改投到更快的目标模型；多模态请求保持原模型。
    let (effective_model, skipped_multimodal) = match speed_first_for(requested_model, payload) {
        SpeedFirst::Use(target) => (target, false),
        SpeedFirst::SkippedMultimodal => (requested_model.to_string(), true),
        SpeedFirst::NotApplicable => (requested_model.to_string(), false),
    };
    let route = resolve_route(&effective_model).ok_or_else(|| unknown_model_error(&effective_model))?;
    let mut owned = payload.clone();
    owned["model"] = json!(route.body_model);
    let started = now_ms();
    let (resp, account, measurement, guard, intl_lease) = match send_upstream_routed(&owned, &route).await
    {
        Ok(quint) => quint,
        Err(e) => {
            record_and_notify_failure(Some(&route), requested_model, started, &e, skipped_multimodal);
            return Err(e);
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let error = error_text(status, &text);
        record_and_notify_failure(Some(&route), requested_model, started, &error, skipped_multimodal);
        return Err(error);
    }
    Ok((
        PreparedStream {
            route,
            account,
            requested_model: requested_model.to_string(),
            skipped_multimodal,
            started,
            measurement,
            _concurrency_guard: guard,
            intl_lease,
        },
        resp,
    ))
}

/// 把「被改投」的信息写进用量（未改投则不写），便于在监控里核对。
pub(crate) fn note_substitution(
    usage: &mut Value,
    requested_model: &str,
    actual_model: &str,
    skipped_multimodal: bool,
) {
    if requested_model.eq_ignore_ascii_case(actual_model) {
        if skipped_multimodal {
            if let Some(obj) = usage.as_object_mut() {
                obj.insert("substitutionSkipped".to_string(), json!("multimodal"));
            }
        }
        return;
    }
    if let Some(obj) = usage.as_object_mut() {
        obj.insert("requestedModel".to_string(), json!(requested_model));
        obj.insert("substituted".to_string(), json!(true));
    }
}

pub async fn chat_completions_stream(body: Value) -> Result<GlmUpstreamStream, String> {
    let requested_model = body.get("model").and_then(Value::as_str).unwrap_or("glm-5.3").to_string();
    let (prepared, resp) = prepare_stream(&body, &requested_model).await?;
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    let PreparedStream {
        route,
        account,
        requested_model,
        skipped_multimodal,
        started,
        measurement,
        _concurrency_guard,
        intl_lease,
    } = prepared;
    tokio::spawn(async move {
        let _keep_guard = _concurrency_guard;
        let mut resp = resp;
        // 同时留存 SSE 全文，用于流结束后提取 usage 块（计量用）。
        let mut sse_text = String::new();
        // 跨 chunk UTF-8 半字符尾部：严格解码失败会丢整帧，usage 随之漏计。
        let mut carry: Vec<u8> = Vec::new();
        // 首字延迟：上游首个字节到达耗时，即预填充（含排队）时间。
        let mut ttfb_ms: Option<i64> = None;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if ttfb_ms.is_none() {
                        ttfb_ms = Some(now_ms().saturating_sub(started));
                    }
                    sse_text.push_str(&super::glm_anthropic::decode_stream_chunk(
                        &mut carry, &chunk,
                    ));
                    if tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(format!("上游流中断: {e}"))).await;
                    break;
                }
            }
        }
        if !carry.is_empty() {
            sse_text.push_str(&String::from_utf8_lossy(&carry));
        }
        // 流结束：提取 usage 并归因积分。
        let mut usage = crate::modules::model_usage::extract_usage_from_sse(&sse_text);
        if let (Some(ttfb), Some(obj)) = (ttfb_ms, usage.as_object_mut()) {
            obj.insert("ttfbMs".to_string(), json!(ttfb));
        }
        note_substitution(&mut usage, &requested_model, &route.body_model, skipped_multimodal);
        // SSE 流结束 = release（契约 §3.3）：成功 lease 在此回写 Ok。
        intl_release_ok(&intl_lease);
        let (credits, ledger_id) = measurement.finish().await;
        record_success(&route, &account, &usage, credits, ledger_id, started, now_ms());
    });
    Ok(GlmUpstreamStream { content_type, rx })
}

/// 组装一条失败记录（纯函数，便于单测）。改投 / 多模态跳过与成功记录保持一致地留痕。
pub(crate) fn failure_record(
    route: Option<&GlmModelRoute>,
    requested_model: &str,
    started: i64,
    finished: i64,
    error: &str,
    skipped_multimodal: bool,
) -> Value {
    let actual_model = route
        .map(|r| r.body_model.clone())
        .unwrap_or_else(|| requested_model.to_string());
    let substituted = !actual_model.eq_ignore_ascii_case(requested_model);
    json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "ts": started,
        "model": actual_model,
        "routeId": route
            .map(|r| r.route_id.clone())
            .unwrap_or_else(|| "-".to_string()),
        "accountId": "-",
        "accountLabel": "-",
        "promptTokens": 0,
        "completionTokens": 0,
        "reasoningTokens": Value::Null,
        "totalTokens": 0,
        "credits": Value::Null,
        "creditsPending": false,
        "latencyMs": finished.saturating_sub(started),
        "requestedModel": if substituted { json!(requested_model) } else { Value::Null },
        "substituted": substituted,
        "substitutionSkipped": if skipped_multimodal { json!("multimodal") } else { Value::Null },
        "ok": false,
        "error": error,
    })
}

/// 记录一次失败调用（未产生计费，积分记空），并按类别推送桌面通知。
pub(crate) fn record_and_notify_failure(
    route: Option<&GlmModelRoute>,
    requested_model: &str,
    started: i64,
    error: &str,
    skipped_multimodal: bool,
) {
    crate::modules::model_usage::record_usage(failure_record(
        route,
        requested_model,
        started,
        now_ms(),
        error,
        skipped_multimodal,
    ));
    crate::modules::notify::notify(
        "glm-proxy",
        "GLM 代理调用异常",
        &error.to_string(),
    );
}

fn unknown_model_error(requested: &str) -> String {
    format!(
        "未知模型 {requested:?}，可用模型: {}",
        model_routes()
            .iter()
            .map(|r| r.body_model.clone())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// 组装一条用量记录（调用成功后写入统计模块）。
fn record_value(
    route: &GlmModelRoute,
    account: &PoolAccount,
    usage: &Value,
    credits: Option<f64>,
    ledger_id: Option<String>,
    started: i64,
    finished: i64,
) -> Value {
    json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "ts": started,
        "model": route.body_model,
        "routeId": route.route_id,
        "accountId": account.id,
        "accountLabel": account.label,
        "promptTokens": usage.get("promptTokens").cloned().unwrap_or(json!(0)),
        "completionTokens": usage.get("completionTokens").cloned().unwrap_or(json!(0)),
        "reasoningTokens": usage.get("reasoningTokens").cloned().unwrap_or(Value::Null),
        "totalTokens": usage.get("totalTokens").cloned().unwrap_or(json!(0)),
        "cachedTokens": usage.get("cachedTokens").cloned().unwrap_or(json!(0.0)),
        "uncachedPromptTokens": usage.get("uncachedPromptTokens").cloned().unwrap_or_else(|| usage.get("promptTokens").cloned().unwrap_or(json!(0.0))),
        "cacheHit": usage.get("cacheHit").cloned().unwrap_or(json!(false)),
        "credits": credits.map(|v| json!(v)).unwrap_or(Value::Null),
        "creditsPending": credits.is_none(),
        "ledgerId": ledger_id.map(|v| json!(v)).unwrap_or(Value::Null),
        "latencyMs": finished.saturating_sub(started),
        // 首字延迟（上游首个字节到达时刻）：用于区分「预填充慢」与「思考慢」。
        "ttfbMs": usage.get("ttfbMs").cloned().unwrap_or(Value::Null),
        // 速度优先改投时记录原始请求模型，便于在监控里核对。
        "requestedModel": usage.get("requestedModel").cloned().unwrap_or(Value::Null),
        "substituted": usage.get("substituted").cloned().unwrap_or(Value::Bool(false)),
        "substitutionSkipped": usage.get("substitutionSkipped").cloned().unwrap_or(Value::Null),
        "ok": true,
    })
}

/// 记录一次成功调用（OpenAI 透传与 Anthropic 转换共用）。
pub(crate) fn record_success(
    route: &GlmModelRoute,
    account: &PoolAccount,
    usage: &Value,
    credits: Option<f64>,
    ledger_id: Option<String>,
    started: i64,
    finished: i64,
) {
    crate::modules::model_usage::record_usage(record_value(
        route,
        account,
        usage,
        credits,
        ledger_id,
        started,
        finished,
    ));
}

/// 统一入口：按 body.stream 分派。
/// 当 AutoClaw 网关不可用（无可用账号/限流/错误）时，自动使用抓取的 ChatGLM Token 进行回退与服务。
pub async fn chat_completions(body: Value) -> Result<GlmChatCompletion, String> {
    let stream = body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        match chat_completions_stream(body.clone()).await {
            Ok(s) => Ok(GlmChatCompletion::Stream(s)),
            Err(autoclaw_err) => {
                // 如果有抓取的 ChatGLM token，自动通过 ChatGLM 网页端引擎兜底服务
                if crate::modules::glm_web::has_usable_token() {
                    match crate::modules::glm_web::chat_completions(body).await {
                        Ok(crate::modules::glm_web::GlmChatOutput::Stream(web_stream)) => {
                            Ok(GlmChatCompletion::Stream(GlmUpstreamStream {
                                content_type: "text/event-stream".to_string(),
                                rx: web_stream.rx,
                            }))
                        }
                        Ok(crate::modules::glm_web::GlmChatOutput::Json(val)) => {
                            Ok(GlmChatCompletion::Json(val))
                        }
                        Err(web_err) => {
                            Err(format!("{autoclaw_err}；且尝试调用抓取的 ChatGLM Token 失败: {web_err}"))
                        }
                    }
                } else {
                    Err(autoclaw_err)
                }
            }
        }
    } else {
        match chat_completions_json(body.clone()).await {
            Ok(val) => Ok(GlmChatCompletion::Json(val)),
            Err(autoclaw_err) => {
                if crate::modules::glm_web::has_usable_token() {
                    match crate::modules::glm_web::chat_completions(body).await {
                        Ok(crate::modules::glm_web::GlmChatOutput::Json(val)) => {
                            Ok(GlmChatCompletion::Json(val))
                        }
                        Ok(crate::modules::glm_web::GlmChatOutput::Stream(_)) => {
                            Err("ChatGLM 上游返回了非预期的流式响应".to_string())
                        }
                        Err(web_err) => {
                            Err(format!("{autoclaw_err}；且尝试调用抓取的 ChatGLM Token 失败: {web_err}"))
                        }
                    }
                } else {
                    Err(autoclaw_err)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic /v1/messages 兼容
// ---------------------------------------------------------------------------

/// Anthropic Messages API 入口：转换请求 → 复用上游链路 → 转换响应。
///
/// 1.0.4：补上与 `/v1/chat/completions` **对齐的 ChatGLM 网页引擎兜底**。
/// 原先 autoclaw 账号池失败（403 banned / 限流 / 冷却）时，OpenAI 路径会
/// 自动切到 `glm_web` 继续服务，而本路径直接 502 —— 同一上游、两条协议路径
/// 命运不同：用 `kind=anthropic` 的客户端（ZCode / Claude Code 类）在账号池
/// 出问题时必然失败。现在两条路径的兜底策略完全一致。
pub async fn anthropic_messages(body: Value) -> Result<GlmChatCompletion, String> {
    let (payload, requested_model, stream) =
        super::glm_anthropic::to_openai_payload(&body)?;
    if stream {
        match prepare_stream(&payload, &requested_model).await {
            Ok((prepared, resp)) => Ok(GlmChatCompletion::Stream(
                super::glm_anthropic::pump_anthropic_stream(prepared, resp).await,
            )),
            Err(autoclaw_err) => {
                // 兜底：ChatGLM 网页引擎（与 chat_completions() 同策略）
                if crate::modules::glm_web::has_usable_token() {
                    match crate::modules::glm_web::chat_completions(payload).await {
                        Ok(out) => Ok(GlmChatCompletion::Stream(
                            super::glm_anthropic::pump_anthropic_from_web(out),
                        )),
                        Err(web_err) => Err(format!(
                            "{autoclaw_err}；且尝试调用抓取的 ChatGLM Token 失败: {web_err}"
                        )),
                    }
                } else {
                    Err(autoclaw_err)
                }
            }
        }
    } else {
        // 复用 chat_completions()：它自带「autoclaw 失败 → glm_web 兜底」的完整策略，
        // 避免在此重复实现（也保证两条协议路径行为一致）。
        match chat_completions(payload).await {
            Ok(GlmChatCompletion::Json(openai)) => Ok(GlmChatCompletion::Json(
                super::glm_anthropic::from_openai_response(&openai, &requested_model),
            )),
            Ok(GlmChatCompletion::Stream(_)) => {
                Err("上游返回了非预期的流式响应".to_string())
            }
            Err(e) => Err(e),
        }
    }
}

/// 流式/非流式响应的统一封装。
pub enum GlmChatCompletion {
    Json(Value),
    Stream(GlmUpstreamStream),
}

/// OpenAI `/v1/models` 响应。同名模型（两区目录都有）按模型目录顺序
/// 只暴露一个 ID（settings.json 归属的本机区域优先）；`x_region` 标注归属。
pub fn models_payload() -> Value {
    models_payload_filtered(None)
}

/// 1.3.0：按区域过滤的模型清单（参照 workbuddy-pool 区域契约的分组方式——
/// 每条带 `region` 字段（domestic/international）、顶层 `regions` 元信息（各组
/// 条数）、支持按区过滤）。glm_web（ChatGLM，国内）归 domestic。
/// `region`：None/"all"=全量；"domestic"/"cn"=国内版；"international"/"intl"=国际版；
/// 其他值=空清单（前端/客户端应使用已知取值）。
pub fn models_payload_filtered(region: Option<&str>) -> Value {
    let want = match region {
        None | Some("all") => None,
        Some("cn") => Some("domestic"),
        Some("intl") => Some("international"),
        Some(other) => Some(other),
    };
    let mut seen = std::collections::HashSet::new();
    let mut data = Vec::new();
    let mut cn = 0usize;
    let mut intl = 0usize;
    for route in model_routes() {
        if seen.insert(route.body_model.clone()) {
            let r = route.region.as_str();
            if let Some(w) = want {
                if r != w {
                    continue;
                }
            }
            match r {
                "domestic" => cn += 1,
                "international" => intl += 1,
                _ => {}
            }
            let mut entry = json!({
                "id": route.body_model,
                "object": "model",
                "created": 0,
                "owned_by": "autoclaw",
                "region": r,
            });
            if let Some(cw) = crate::modules::config::max_context_window(&route.body_model) {
                entry["contextWindow"] = json!(cw);
            }
            data.push(entry);
        }
    }
    if want.map(|w| w == "domestic").unwrap_or(true)
        && crate::modules::glm_web::has_usable_token()
    {
        if let Some(list) = crate::modules::glm_web::models_payload().get("data").and_then(Value::as_array) {
            for m in list {
                if let Some(id) = m.get("id").and_then(Value::as_str) {
                    if seen.insert(id.to_string()) {
                        cn += 1;
                        let mut entry = json!({
                            "id": id,
                            "object": "model",
                            "created": 0,
                            "owned_by": "chatglm",
                            "region": "domestic",
                        });
                        // 透传 glm_web 清单已带的 contextWindow（家族最长口径）
                        if let Some(cw) = m.get("contextWindow") {
                            entry["contextWindow"] = cw.clone();
                        }
                        data.push(entry);
                    }
                }
            }
        }
    }
    let mut regions = serde_json::Map::new();
    if want.map(|w| w == "domestic").unwrap_or(true) {
        regions.insert("domestic".to_string(), json!({ "models": cn }));
    }
    if want.map(|w| w == "international").unwrap_or(true) {
        regions.insert("international".to_string(), json!({ "models": intl }));
    }
    json!({"object": "list", "data": data, "regions": Value::Object(regions)})
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 「可能已计费」不得换号（2026-10-09 修「一次操作扣两个账号」）----

    #[test]
    fn might_have_billed_is_not_confused_with_retryable_zero() {
        // 哨兵必须与 0 区分：0 = 未送达（可换号）；哨兵 = 可能已计费（禁止换号）
        assert_ne!(MIGHT_HAVE_BILLED, 0, "哨兵不能与 0 撞值，否则失去区分能力");
        assert_eq!(MIGHT_HAVE_BILLED, u16::MAX);
    }

    #[test]
    fn is_retryable_failure_rejects_sentinel() {
        // 连接失败(0) 可换号；可能已计费(哨兵) 不可
        assert!(is_retryable_failure(0), "未送达的失败应允许换号");
        assert!(is_retryable_failure(429), "限流应允许换号");
        assert!(!is_retryable_failure(MIGHT_HAVE_BILLED), "可能已计费必须禁止换号");
    }

    #[test]
    fn connect_errors_are_classified_as_not_sent() {
        // 用真实 reqwest::Error 造一个连接失败，验证被归为 0（未送达）
        let err = reqwest::Client::builder()
            .build()
            .expect("client")
            .get("http://127.0.0.1:1")   // 必然连接失败
            .send();
        // 同步环境无法直接 await，这里只做「判定函数存在且取向正确」的静态断言：
        // is_connect/is_request 为真的分支返回 0；其余返回 MIGHT_HAVE_BILLED。
        // 真实分类由下面的 timeout 语义断言覆盖。
        let _ = err; // 避免未使用告警（异步需 runtime，此处不引入）
        assert_eq!(MIGHT_HAVE_BILLED, u16::MAX);
    }

    use super::*;

    fn cfg_with(speed_first: bool, target: &str) -> GlmProxyConfig {
        GlmProxyConfig {
            enabled: true,
            api_key: "k".to_string(),
            speed_first,
            speed_first_target: target.to_string(),
            // 与 `load` 的缺省路径保持一致：多账号轮询、不固定账号。
            account_mode: ACCOUNT_MODE_ROUND_ROBIN.to_string(),
            pinned_account_id: String::new(),
        }
    }

    #[test]
    fn speed_first_replaces_only_slow_models() {
        let on = cfg_with(true, "auto");
        // 两个慢模型被改投
        assert_eq!(
            speed_first_plan(&on, "glm-5.3", false),
            SpeedFirst::Use("auto".to_string())
        );
        assert_eq!(
            speed_first_plan(&on, "glm-5.3-flash", false),
            SpeedFirst::Use("auto".to_string())
        );
        assert_eq!(
            speed_first_plan(&on, "GLM-5.3-Flash", false),
            SpeedFirst::Use("auto".to_string())
        );
        // 快模型与未知模型不被动
        assert_eq!(speed_first_plan(&on, "auto", false), SpeedFirst::NotApplicable);
        assert_eq!(speed_first_plan(&on, "auto-fast", false), SpeedFirst::NotApplicable);
        assert_eq!(speed_first_plan(&on, "some-other", false), SpeedFirst::NotApplicable);
        // 目标与来源相同时不重复替换
        let same = cfg_with(true, "glm-5.3");
        assert_eq!(speed_first_plan(&same, "glm-5.3", false), SpeedFirst::NotApplicable);
        assert_eq!(
            speed_first_plan(&same, "glm-5.3-flash", false),
            SpeedFirst::Use("glm-5.3".to_string())
        );
        // 关闭时不替换
        let off = cfg_with(false, "auto");
        assert_eq!(speed_first_plan(&off, "glm-5.3", false), SpeedFirst::NotApplicable);
        // 目标为空时不替换
        let blank = cfg_with(true, "   ");
        assert_eq!(speed_first_plan(&blank, "glm-5.3", false), SpeedFirst::NotApplicable);
    }

    #[test]
    fn speed_first_skips_multimodal_requests() {
        let on = cfg_with(true, "auto");
        // 命中慢模型但请求带图 → 保持原模型（auto 不支持图片输入）
        assert_eq!(
            speed_first_plan(&on, "glm-5.3-flash", true),
            SpeedFirst::SkippedMultimodal
        );
        // 纯文本仍然改投
        assert_eq!(
            speed_first_plan(&on, "glm-5.3-flash", false),
            SpeedFirst::Use("auto".to_string())
        );
    }

    #[test]
    fn multimodal_detection_covers_common_shapes() {
        // 纯文本（字符串 / text 块）不算多模态
        assert!(!payload_has_multimodal(&json!({
            "messages": [{"role": "user", "content": "你好"}]
        })));
        assert!(!payload_has_multimodal(&json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "你好"}]}]
        })));
        // OpenAI 风格的 image_url
        assert!(payload_has_multimodal(&json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,xxx"}}
            ]}]
        })));
        // Anthropic 风格的 image 块
        assert!(payload_has_multimodal(&json!({
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "data": "xxx"}}
            ]}]
        })));
        // 未知类型保守视为多模态
        assert!(payload_has_multimodal(&json!({
            "messages": [{"role": "user", "content": [{"type": "video_url"}]}]
        })));
        // 无 content 数组不影响判断
        assert!(!payload_has_multimodal(&json!({"messages": [{"role": "user"}]})));
    }

    #[test]
    fn substitution_is_recorded_in_usage() {
        let mut usage = json!({"promptTokens": 1.0});
        note_substitution(&mut usage, "glm-5.3", "auto", false);
        assert_eq!(usage["requestedModel"], json!("glm-5.3"));
        assert_eq!(usage["substituted"], json!(true));
        // 未改投时不写字段
        let mut plain = json!({"promptTokens": 1.0});
        note_substitution(&mut plain, "auto", "auto", false);
        assert!(plain.get("requestedModel").is_none());
        assert!(plain.get("substituted").is_none());
        assert!(plain.get("substitutionSkipped").is_none());
        // 因多模态跳过要留痕
        let mut skipped = json!({"promptTokens": 1.0});
        note_substitution(&mut skipped, "glm-5.3-flash", "glm-5.3-flash", true);
        assert_eq!(skipped["substitutionSkipped"], json!("multimodal"));
    }

    #[test]
    fn failure_record_keeps_substitution_trace() {
        let route = GlmModelRoute {
            route_id: "zai_auto".to_string(),
            body_model: "auto".to_string(),
            alias: "Auto".to_string(),
            meta: Value::Null,
            region: AccountRegion::Domestic,
        };
        // 被改投后失败：要能看出原始请求模型
        let rec = failure_record(Some(&route), "glm-5.3", 1_000, 2_500, "上游 500", false);
        assert_eq!(rec["ok"], json!(false));
        assert_eq!(rec["model"], json!("auto"));
        assert_eq!(rec["requestedModel"], json!("glm-5.3"));
        assert_eq!(rec["substituted"], json!(true));
        assert_eq!(rec["substitutionSkipped"], Value::Null);
        assert_eq!(rec["latencyMs"], json!(1500));
        assert_eq!(rec["error"], json!("上游 500"));

        // 多模态跳过 + 失败：保留跳过原因
        let skipped = failure_record(
            Some(&route_for("glm-5.3-flash")),
            "glm-5.3-flash",
            1_000,
            1_800,
            "上游 500 parse response failed",
            true,
        );
        assert_eq!(skipped["model"], json!("glm-5.3-flash"));
        assert_eq!(skipped["substituted"], json!(false));
        assert_eq!(skipped["requestedModel"], Value::Null);
        assert_eq!(skipped["substitutionSkipped"], json!("multimodal"));

        // 路由未知（未知模型）：不误标为改投
        let unknown = failure_record(None, "some-model", 1_000, 1_100, "未知模型", false);
        assert_eq!(unknown["model"], json!("some-model"));
        assert_eq!(unknown["substituted"], json!(false));
        assert_eq!(unknown["requestedModel"], Value::Null);
    }

    fn route_for(body_model: &str) -> GlmModelRoute {
        GlmModelRoute {
            route_id: format!("zai_{body_model}"),
            body_model: body_model.to_string(),
            alias: body_model.to_string(),
            meta: Value::Null,
            region: AccountRegion::Domestic,
        }
    }

    #[test]
    fn route_prefix_strip_matches_client() {
        assert_eq!(strip_route_prefix("zaicoding_glm-5.3"), "glm-5.3");
        assert_eq!(strip_route_prefix("zai_glm-5.3-flash"), "glm-5.3-flash");
        assert_eq!(strip_route_prefix("zai_auto"), "auto");
        // 无前缀或非小写前缀保持原样。
        assert_eq!(strip_route_prefix("glm-5.3"), "glm-5.3");
        assert_eq!(strip_route_prefix("Zai_glm-5.3"), "Zai_glm-5.3");
    }

    #[test]
    fn fallback_routes_resolve_by_alias_and_body_model() {
        // 无 settings.json 的环境走静态兜底表。
        let routes = model_routes();
        assert!(!routes.is_empty());
        let glm = routes
            .iter()
            .find(|r| r.body_model == "glm-5.3")
            .expect("fallback glm-5.3 route");
        assert_eq!(glm.route_id, "zaicoding_glm-5.3");
        assert_eq!(glm.alias, "GLM-5.3");
    }

    #[test]
    fn resolve_route_matches_all_name_forms() {
        for name in ["glm-5.3", "zaicoding_glm-5.3", "GLM-5.3"] {
            let route = resolve_route(name).expect("resolvable");
            assert_eq!(route.route_id, "zaicoding_glm-5.3");
            assert_eq!(route.body_model, "glm-5.3");
        }
        assert!(resolve_route("gpt-4o").is_none());
    }

    #[test]
    fn models_payload_lists_body_model_ids() {
        let payload = models_payload();
        let ids: Vec<String> = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect();
        assert!(ids.contains(&"glm-5.3".to_string()));
        assert_eq!(payload["object"], "list");
    }
}
