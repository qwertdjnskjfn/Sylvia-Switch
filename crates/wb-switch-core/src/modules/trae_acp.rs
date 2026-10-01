//! TRAE ACP 真流式转发（T6 / P0-F16，fullAccess 前置门禁**默认关闭**）。
//!
//! # 协议（证据：仓库根 `acp_ctx.txt`，TRAE SOLO CN 客户端 bundle 提取）
//!
//! TRAE 桌面端内嵌的 Agent Client Protocol（ACP）HTTP 服务，连接生命周期：
//!
//! 1. `POST /api/v1/acp/connect`（loopback 免鉴权：`isLoopbackAddress` 直接放行，
//!    acp_ctx.txt @3750703/@3751837）→ JSON `{ connectionId, sessionToken }`；
//! 2. `POST /api/v1/acp`（Header `acp-connection-id`，Accept
//!    `application/json, text/event-stream`）发 JSON-RPC 2.0：
//!    - `initialize` `{protocolVersion:1, capabilities:{}, clientInfo}`；
//!    - `session/new` `{cwd, mcpServers:[]}` → `result.sessionId`（参数名是 cwd，
//!      实测 2026-09-22：workingDirectory 会被服务端 -32602 拒绝）；
//!    - `session/prompt` `{sessionId, prompt:[{type:"text",text}]}`；
//! 3. 每个 POST 的响应都是 SSE 流（NDJSON 口径：每行一个 JSON-RPC message；
//!    实际帧为 `event: message` + `data: {json}`，acp_ctx.txt @3649280/@4728562）。
//!    `session/prompt` 的流里实时推送 `session/update` 通知，其中
//!    `agent_message_chunk`（`content:{type:"text",text}`）即 Agent 文本增量；
//!    `session_end` 带 `stopReason`（end_turn/cancelled/refusal）。
//!
//! # 本模块职责
//!
//! - **端口发现**：读 `~/.workbuddy/sessions/<pid>.json` 注册表（TRAE 客户端按
//!   5 分钟心跳维护，实证 schema：`{pid,lastHeartbeat,sessionId,cwd,url/endpoint,...}`，
//!   本机 1045 条条目实测），取心跳最新的活跃条目 → `(端口, sessionId)`；
//! - **fullAccess 前置门禁（硬约束）**：`trae_proxy_config.acp_enabled` 默认
//!   false + `acp_allowed_dirs` 空 = 拒绝全部；开启且目录白名单非空、且会话
//!   cwd 落在白名单内才转发，否则 503 明示。**本周期不开启**；
//! - **翻译**：`agent_message_chunk` 增量块 → OpenAI `chat.completion.chunk`
//!   SSE 帧；断线（连接失败 / 流中断 / 空闲超时）**重连一次**——已向下游产出过
//!   内容的流式场景不重连（重连会重放整轮回答，造成重复）；
//! - 非流式请求聚合为单条 `chat.completion` JSON。
//!
//! # 签名说明（契约 §三 的偏差，刻意为之）
//!
//! 契约写作 `pub async fn chat_completions(body: Value) -> Result<Response,
//! (StatusCode, String)>`，但 **wb-switch-core 不依赖 axum**（`crates/wb-switch-core/
//! Cargo.toml` 无 axum，且 src-tauri 桌面端复用本 crate，引入 axum 会无谓拖入
//! hyper/tower）。因此按 `loomy_proxy::chat_completions` /
//! `workbuddy_proxy::chat_completions_stream` 的既有范式返回可转换结果
//! （`TraeAcpChat`），由 api.rs 的 T6 路由段转成 axum `Response`；错误码用
//! `(u16, String)`（503 门禁/发现失败、502 上游失败、400 请求非法）。

use bytes::Bytes;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::modules::config::{home_dir, now_ms};
use crate::modules::trae_proxy_config::{self, TraeProxyConfig};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 注册表目录：`~/.workbuddy/sessions/`（TRAE 客户端写 `<pid>.json`）。
const SESSIONS_REL_DIR: [&str; 2] = [".workbuddy", "sessions"];

/// 心跳有效期：5 分钟内的条目视为「运行中的 TRAE 会话」。
const HEARTBEAT_TTL_MS: i64 = 5 * 60 * 1000;

/// 时钟回拨容忍：心跳时间戳比当前时间快这么多仍算活跃（客户端/服务端时钟漂移）。
const CLOCK_SKEW_TOLERANCE_MS: i64 = 60 * 1000;

/// ACP HTTP 端点路径（loopback 免鉴权）。
const ACP_PATH: &str = "/api/v1/acp";

/// 连接超时（loopback，不通应立即失败）。
const CONNECT_TIMEOUT_SECS: u64 = 5;

/// 空闲读超时：连续这么久收不到任何字节判定断流（服务端 30s 心跳 /
/// prompt 60s keepalive，见 acp_ctx.txt @3722109，180s = 3~6 次保活缺失）。
const READ_IDLE_TIMEOUT_SECS: u64 = 180;

/// `initialize` / `session/new` 等短步调用的整体截止时间。
const STEP_DEADLINE_SECS: u64 = 60;

/// JSON-RPC 请求 id（服务端原样回显，用于在 SSE 流里认领响应）。
const ID_INITIALIZE: &str = "wb-acp-init";
const ID_SESSION_NEW: &str = "wb-acp-new";
const ID_SET_MODE: &str = "wb-acp-mode";
const ID_SET_MODEL: &str = "wb-acp-model";
const ID_PROMPT: &str = "wb-acp-prompt";

/// initialize 的 clientInfo。
const CLIENT_NAME: &str = "wb-switch";

/// session/set_mode 默认模式：API 转发场景无人应答工具权限请求，默认 bypass
/// （cwd 白名单才是真实控制面）；请求体可用 `acpMode` 覆盖。
const DEFAULT_MODE: &str = "bypassPermissions";

/// 内置模型目录 TTL 缓存（契约 §3.1：TTL 300s，`source:"acp-cache"` 供前端标注）。
const NATIVE_CATALOG_TTL_MS: i64 = 300 * 1000;

/// 目录探测专用 cwd 尾段（`%TEMP%\wb-native-catalog-probe`）：与 IDE 会话隔离，
/// 不污染用户正在用的项目会话（session/new 对同 cwd 会复用既有会话）。
const NATIVE_CATALOG_PROBE_DIR: &str = "wb-native-catalog-probe";

/// 「刚探测完」判定窗口：`now - probed_at` 小于该值视为 acp-live，否则 acp-cache。
/// probed_at 在探测完成后写入，新探测的 delta 通常 <1s。
const NATIVE_CATALOG_FRESH_MS: i64 = 3_000;

/// 档位别名（WorkBuddy UI 的三档快捷选择，非真实模型）。
const TIER_MODEL_IDS: [&str; 3] = ["fast-model", "balanced-model", "deep-model"];

/// 自定义接入条目前缀（用户自定义接入，随用户配置漂移）。
const CUSTOM_MODEL_PREFIX: &str = "custom-local:";

// ---------------------------------------------------------------------------
// 内置模型目录（契约 §3.1 / §3.3：ACP session/new 实时探测）
// ---------------------------------------------------------------------------

/// 一条内置模型目录条目。`kind`：`tier`（档位别名）| `model`（实模）| `custom`（自定义接入）。
///
/// `exposed`（契约 §2.1）：该 id 是否被 ACP `session/new` 的 `models.availableModels`
/// 实时暴露 —— ACP 命中=true；仅来自客户端产品配置补充源=false（2026-09-25 实测
/// 上游不再暴露内置实模，补充源是内置清单的唯一可用来源，可调用性以实测为准）。
#[derive(Clone, Debug, PartialEq)]
pub struct NativeModel {
    pub id: String,
    pub name: String,
    pub multiplier: Option<f64>,
    pub kind: &'static str,
    pub exposed: bool,
}

/// 一次目录探测的产物（契约 §3.3）。
#[derive(Clone, Debug)]
pub struct NativeCatalog {
    pub models: Vec<NativeModel>,
    /// 会话默认模型（session/new 的 `models.currentModelId`；缺失为空串）。
    pub current_model_id: String,
    /// 探测完成时刻（ms）。
    pub probed_at: i64,
    /// 探测命中的 WorkBuddy 实例端口。
    pub port: u16,
}

impl NativeCatalog {
    /// 请求模型是否在目录内（tier/model/custom 全部算「可走 native」；
    /// 契约 §3.2①：set_model 对未知 ID 假成功，兜底判定必须在网关侧对照目录）。
    pub fn contains_id(&self, id: &str) -> bool {
        self.models.iter().any(|m| m.id == id)
    }

    /// 探测是否「刚完成」（未走 TTL 缓存）——payload 组装据此标
    /// `source:"acp-live"`，否则 `source:"acp-cache"`（契约 §3.1）。
    pub fn is_fresh(&self) -> bool {
        now_ms() - self.probed_at < NATIVE_CATALOG_FRESH_MS
    }
}

/// 目录 TTL 缓存：`(写入时刻, 目录)`。进程内共享；测试不触碰（探测要走网络）。
static NATIVE_CATALOG_CACHE: Mutex<Option<(i64, NativeCatalog)>> = Mutex::new(None);

/// 目录探测 cwd：`%TEMP%\wb-native-catalog-probe`。
fn native_catalog_probe_cwd() -> String {
    std::env::temp_dir()
        .join(NATIVE_CATALOG_PROBE_DIR)
        .to_string_lossy()
        .to_string()
}

/// WorkBuddy 内置模型目录（契约 §3.3）：300s TTL 缓存，缓存内不重打上游。
pub async fn native_models_catalog() -> Result<NativeCatalog, String> {
    {
        let guard = NATIVE_CATALOG_CACHE.lock().unwrap();
        if let Some((at, catalog)) = guard.as_ref() {
            if now_ms() - *at < NATIVE_CATALOG_TTL_MS {
                return Ok(catalog.clone());
            }
        }
    }
    native_models_catalog_refresh().await
}

/// 强制刷新目录（绕过 TTL 缓存重探测一次）。
pub async fn native_models_catalog_refresh() -> Result<NativeCatalog, String> {
    let catalog = probe_native_catalog().await?;
    {
        let mut guard = NATIVE_CATALOG_CACHE.lock().unwrap();
        *guard = Some((now_ms(), catalog.clone()));
    }
    Ok(catalog)
}

/// 目录探测（契约 §3.3）：注册表取心跳最新实例 → connect → initialize →
/// session/new（探针 cwd，**不受 acp_allowed_dirs 门禁**——目录发现不涉内容
/// 转发）→ 解析 `models.availableModels` + `configOptions` 的 model select。
/// 连接随作用域关闭；探针会话由服务端生命周期管理，无需显式删除。
async fn probe_native_catalog() -> Result<NativeCatalog, String> {
    let entry = scan_registry().ok_or_else(|| {
        "未发现运行中的 WorkBuddy 会话（~/.workbuddy/sessions 注册表 5 分钟心跳内无活跃条目）".to_string()
    })?;
    let conn = acp_connect(entry.port).await?;
    acp_call(
        &conn,
        ID_INITIALIZE,
        "initialize",
        json!({
            "protocolVersion": 1,
            "capabilities": {},
            "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
        }),
    )
    .await?;
    let new_result = acp_call(
        &conn,
        ID_SESSION_NEW,
        "session/new",
        json!({
            "cwd": native_catalog_probe_cwd(),
            "mcpServers": [],
        }),
    )
    .await?;
    let mut catalog = parse_native_catalog(&new_result, entry.port).ok_or_else(|| {
        "ACP session/new 响应缺少 models.availableModels（无法构建内置模型目录）".to_string()
    })?;
    // 契约 §2.1：ACP 探测成功后合并「内置补充源」（客户端产品配置）。合并失败
    // 静默跳过——绝不让目录探测因此失败（补充源是 Electron 缓存，随时可能变形）。
    if let Ok(text) = std::fs::read_to_string(product_config_path()) {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            catalog = merge_product_config_models(catalog, &value);
        }
    }
    Ok(catalog)
}

// ---------------------------------------------------------------------------
// 内置补充源（契约 §2.1：~/.workbuddy/cache/acc-product-config-v3.json）
// ---------------------------------------------------------------------------

/// 客户端产品配置缓存（Electron 写入的只读缓存，宽松解析、绝不写入）。
const PRODUCT_CONFIG_REL_PATH: [&str; 3] = [".workbuddy", "cache", "acc-product-config-v3.json"];

/// 产品配置缓存路径（`~/.workbuddy/cache/acc-product-config-v3.json`，home 拼接）。
fn product_config_path() -> PathBuf {
    let mut dir = home_dir();
    for part in PRODUCT_CONFIG_REL_PATH {
        dir = dir.join(part);
    }
    dir
}

/// 宽松解析产品配置里的内置模型 id 清单（纯函数，注入 JSON 便于单测）：
/// 递归找 `name=="cli"` 且 `models` 为字符串数组的对象；找不到则回落「任何含
/// `fast-model` 的 models 字符串数组」；两者皆无 → 空表（调用方静默跳过）。
fn product_config_cli_model_ids(root: &Value) -> Vec<String> {
    let mut cli_hit: Option<Vec<String>> = None;
    let mut any_fast: Option<Vec<String>> = None;
    walk_product_config(root, &mut cli_hit, &mut any_fast);
    cli_hit.or(any_fast).unwrap_or_default()
}

/// 递归遍历：记录首个 `name=="cli"` 命中与首个含 `fast-model` 的 models 数组。
fn walk_product_config(
    value: &Value,
    cli_hit: &mut Option<Vec<String>>,
    any_fast: &mut Option<Vec<String>>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                walk_product_config(item, cli_hit, any_fast);
            }
        }
        Value::Object(map) => {
            if let Some(models) = map
                .get("models")
                .and_then(Value::as_array)
                .map(|items| models_string_list(items))
            {
                if !models.is_empty() {
                    if map.get("name").and_then(Value::as_str) == Some("cli") {
                        if cli_hit.is_none() {
                            *cli_hit = Some(models);
                        }
                    } else if any_fast.is_none()
                        && models.iter().any(|m| m == "fast-model")
                    {
                        *any_fast = Some(models);
                    }
                }
            }
            for child in map.values() {
                walk_product_config(child, cli_hit, any_fast);
            }
        }
        _ => {}
    }
}

/// models 数组 → 字符串清单（只收字符串元素；变形/混合类型静默剔除）。
fn models_string_list(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .collect()
}

/// 合并纯函数（契约 §2.1，注入产品配置 JSON 便于单测）：对不在 ACP
/// availableModels 里的 id 追加 `NativeModel { name: id 原样, multiplier: None,
/// kind: "model", exposed: false }`；ACP 命中的既有条目保持 `exposed: true`。
fn merge_product_config_models(mut catalog: NativeCatalog, product_config: &Value) -> NativeCatalog {
    for id in product_config_cli_model_ids(product_config) {
        if catalog.models.iter().any(|m| m.id == id) {
            continue;
        }
        catalog.models.push(NativeModel {
            name: id.clone(),
            multiplier: None,
            kind: "model",
            exposed: false,
            id,
        });
    }
    catalog
}

/// 解析 session/new result → 目录（纯函数，便于单测）。
///
/// - id 权威来源：`models.availableModels[].modelId`（顺序 = 服务端返回序）；
/// - name / multiplier 优先取 `configOptions` 中 `id=="model"` 的 select
///   （`options[].value/name/description`），缺失时回落 availableModels 条目；
/// - multiplier 从 description 的 `x<float>` 解析（如 "x0.21" → 0.21），失败为 null；
/// - kind：档位别名 → tier，`custom-local:*` → custom，其余 → model。
fn parse_native_catalog(result: &Value, port: u16) -> Option<NativeCatalog> {
    let available = result.pointer("/models/availableModels")?.as_array()?;
    let current_model_id = result
        .pointer("/models/currentModelId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // model select 的 (value → (name, description)) 映射（线性查表，条目量 ~50）。
    let mut select: Vec<(&str, &str, Option<&str>)> = Vec::new();
    if let Some(options) = result
        .pointer("/configOptions")
        .and_then(Value::as_array)
        .and_then(|opts| {
            opts.iter()
                .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))
                .and_then(|o| o.get("options"))
                .and_then(Value::as_array)
        })
    {
        for option in options {
            if let (Some(value), Some(name)) = (
                option.get("value").and_then(Value::as_str),
                option.get("name").and_then(Value::as_str),
            ) {
                select.push((value, name, option.get("description").and_then(Value::as_str)));
            }
        }
    }
    let models = available
        .iter()
        .filter_map(|entry| {
            let id = entry.get("modelId").and_then(Value::as_str)?;
            let hit = select.iter().find(|(value, _, _)| *value == id);
            let name = hit
                .map(|(_, name, _)| *name)
                .or_else(|| entry.get("name").and_then(Value::as_str))
                .unwrap_or(id);
            let description = hit
                .and_then(|(_, _, desc)| *desc)
                .or_else(|| entry.get("description").and_then(Value::as_str));
            Some(NativeModel {
                id: id.to_string(),
                name: name.to_string(),
                multiplier: description.and_then(parse_multiplier),
                kind: model_kind(id),
                // ACP availableModels 条目 = 实时暴露（契约 §2.1）。
                exposed: true,
            })
        })
        .collect();
    Some(NativeCatalog {
        models,
        current_model_id,
        probed_at: now_ms(),
        port,
    })
}

/// multiplier 解析：取 description 中首个 `x<float>`（如 "x0.21"、"credits x0.29"、
/// "x0.34 credits"）。解析失败 → None（契约：解析失败为 null）。纯函数，便于单测。
/// pub(crate)：池网关模型目录（workbuddy_upstream 的 credits 字段）复用同一解析口径。
pub(crate) fn parse_multiplier(description: &str) -> Option<f64> {
    let lower = description.trim().to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b'x' {
            continue;
        }
        let rest = &lower[idx + 1..];
        let number: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        // 仅有 `.`（如 "x."）不算数字。
        if number.is_empty() || number == "." {
            continue;
        }
        if let Ok(value) = number.parse::<f64>() {
            return Some(value);
        }
    }
    None
}

/// kind 判定（契约 §3.3）：档位别名 → tier，`custom-local:*` → custom，否则 model。
fn model_kind(id: &str) -> &'static str {
    if TIER_MODEL_IDS.contains(&id) {
        "tier"
    } else if id.starts_with(CUSTOM_MODEL_PREFIX) {
        "custom"
    } else {
        "model"
    }
}

/// 请求模型解析（契约 §3.3）：`model` 缺省/空 → None；"trae"/"workbuddy" 历史
/// 占位值视同 None（保证 /trae/v1 行为不变——占位不触发 set_model）。
/// /workbuddy-proxy 的 native-first 编排复用同一口径。
pub fn native_request_model(body: &Value) -> Option<String> {
    body.get("model")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| s != "trae" && s != "workbuddy")
}

/// `session/set_model` 参数（提取为纯函数供单测；2026-09-25 实测：
/// `{sessionId, modelId}`，成功回 `result:{modelId}`，对非法 ID 也假成功）。
fn build_set_model_params(session_id: &str, model_id: &str) -> Value {
    json!({"sessionId": session_id, "modelId": model_id})
}

// ---------------------------------------------------------------------------
// 公开 API
// ---------------------------------------------------------------------------

/// 端口发现（契约 §三）：读 `~/.workbuddy/sessions` 注册表，返回 5 分钟心跳内
/// 活跃条目的 `(端口, sessionId)`；无活跃条目（或目录不存在/全部损坏）返回 None。
pub async fn discover_active_session() -> Option<(u16, String)> {
    scan_registry().map(|hit| (hit.port, hit.session_id))
}

/// 同步判定是否存在 5 分钟心跳内的活跃 WorkBuddy 会话（契约 §2.2：gateway_overview
/// 的 `fetch_accounts` 是同步函数、不能 await 目录探测，用该信号近似「WorkBuddy
/// 在线」）。只读注册表文件，无网络请求。
pub fn has_active_session() -> bool {
    scan_registry().is_some()
}

/// WorkBuddy 桌面端自带模型：从内嵌 `codebuddy --serve` 进程的命令行提取
/// `--model <name>`（WorkBuddy 订阅内置模型，如 deepseek-v4.1-flash）。
/// ACP 会话本身不切换模型（session/set_model 为 IDE 侧交互能力），因此这里
/// 的进程命令行就是「WorkBuddy 自带模型」的权威来源。
pub fn builtin_model() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like '*codebuddy*serve*--model*' } | ForEach-Object { $_.CommandLine }",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(idx) = line.find("--model ") {
                let rest = &line[idx + 8..];
                let model = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_string();
                if !model.is_empty() {
                    return Some(model);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// 流式/非流式响应的统一封装（转 axum Response 由 api.rs 路由段完成）。
pub enum TraeAcpChat {
    /// 非流式：聚合后的 OpenAI chat.completion。
    Json(Value),
    /// 流式：OpenAI chunk SSE 字节流（`text/event-stream`）。
    Stream(TraeAcpUpstreamStream),
}

/// 流式响应（OpenAI chunk SSE 字节流）。结构与 `WorkbuddyUpstreamStream` 一致。
pub struct TraeAcpUpstreamStream {
    pub rx: tokio::sync::mpsc::Receiver<Result<Bytes, String>>,
}

/// TRAE 对话补全（OpenAI 兼容）：fullAccess 门禁 → 注册表发现 → ACP 真转发。
///
/// - 门禁未开 / 白名单空 / 目录越界 / 无活跃会话 → `Err((503, 明示文案))`；
/// - messages 为空 → `Err((400, ...))`；
/// - ACP 上游失败（重连一次后仍失败）→ `Err((502, ...))`；
/// - `stream:true` → `TraeAcpChat::Stream`（增量 OpenAI chunk）；
/// - 否则聚合为 `TraeAcpChat::Json`。
pub async fn chat_completions(body: Value) -> Result<TraeAcpChat, (u16, String)> {
    // ① fullAccess 前置门禁（硬约束，默认关闭；本周期不开启）。
    let cfg: TraeProxyConfig = trae_proxy_config::load_config();
    if !cfg.acp_enabled {
        return Err((
            status_code::SERVICE_UNAVAILABLE,
            "TRAE ACP 对话转发未开放（acp_enabled 未开启：fullAccess 前置门禁默认关闭；开启后还需配置 acp_allowed_dirs 目录白名单）"
                .to_string(),
        ));
    }
    if cfg.acp_allowed_dirs.is_empty() {
        return Err((
            status_code::SERVICE_UNAVAILABLE,
            "TRAE ACP 转发已被拒绝（acp_allowed_dirs 白名单为空：未授权任何目录，拒绝全部转发）"
                .to_string(),
        ));
    }

    // ② 请求解析。model 三态（契约 §3.3）：
    //    - `native_request_model()` → Some(id)：走 session/set_model 真切换；
    //    - None（缺省/空/"trae"/"workbuddy" 占位）：不 set_model，用会话默认模型
    //      （/trae/v1 历史占位语义由此保持不变——占位值不触发 set_model）；
    //    - 响应 `model` 字段回显（契约 §3.2）：请求显式值（含占位原样）优先，
    //      缺省时回显会话 currentModelId（session/new 后经 PromptEvent::Model 覆盖），
    //      再兜底 "trae"（连接早错、尚未拿到会话信息的极端路径）。
    let stream = body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let model_switch = native_request_model(&body);
    let raw_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let model_echo_fallback = raw_model.clone().unwrap_or_else(|| "trae".to_string());
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|m| m.as_slice())
        .unwrap_or(&[]);
    let prompt = flatten_messages(messages);
    if prompt.trim().is_empty() {
        return Err((status_code::BAD_REQUEST, "messages 不能为空".to_string()));
    }
    let mode = body
        .get("acpMode")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_MODE)
        .to_string();

    // ③ 端口发现：G5 优先按请求显式 cwd 匹配同一项目树的活跃会话，
    //    未指定或无匹配时回落心跳最新条目。
    let requested_cwd = body_cwd(&body);
    let entry = scan_registry_for(requested_cwd.as_deref()).ok_or_else(|| {
        (
            status_code::SERVICE_UNAVAILABLE,
            "未发现运行中的 TRAE 会话（~/.workbuddy/sessions 注册表 5 分钟心跳内无活跃条目；请先在 TRAE 客户端打开一个项目会话）"
                .to_string(),
        )
    })?;

    // ④ 目录白名单：请求显式 cwd 优先，否则取注册表条目的 cwd。
    let cwd = requested_cwd
        .or(entry.cwd.clone())
        .ok_or_else(|| {
            (
                status_code::SERVICE_UNAVAILABLE,
                "TRAE 会话未提供工作目录（cwd），且请求未指定；无法按 acp_allowed_dirs 白名单校验，已拒绝转发"
                    .to_string(),
            )
        })?;
    if !dir_allowed(&cwd, &cfg.acp_allowed_dirs) {
        return Err((
            status_code::SERVICE_UNAVAILABLE,
            format!(
                "TRAE 会话目录「{cwd}」不在 acp_allowed_dirs 白名单内，已拒绝转发（fullAccess 门禁）"
            ),
        ));
    }

    // ⑤ ACP 转发（pump 任务独立跑，事件经 channel 汇出；断线重连一次在 pump 内）。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PromptEvent>(64);
    let handle = tokio::spawn(pump_prompt(
        entry.port,
        cwd,
        mode,
        model_switch,
        raw_model,
        prompt,
        tx,
    ));

    if !stream {
        // 非流式：聚合 agent_message_chunk 文本为单条 completion。
        let mut text = String::new();
        let mut stop = "end_turn".to_string();
        let mut failure: Option<String> = None;
        // 响应 model 回显：请求显式值优先，缺省时取会话默认（PromptEvent::Model）。
        let mut echo_model = model_echo_fallback.clone();
        while let Some(event) = rx.recv().await {
            match event {
                PromptEvent::Model(model) => echo_model = model,
                PromptEvent::Text(chunk) => text.push_str(&chunk),
                PromptEvent::Finished(reason) => stop = reason,
                PromptEvent::Failed(message) => failure = Some(message),
            }
        }
        let _ = handle.await;
        if let Some(message) = failure {
            return Err((status_code::BAD_GATEWAY, message));
        }
        let id = format!("chatcmpl-trae-{}", uuid::Uuid::new_v4());
        return Ok(TraeAcpChat::Json(json!({
            "id": id,
            "object": "chat.completion",
            "created": now_ms() / 1000,
            "model": echo_model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": openai_finish_reason(&stop),
            }],
            // ACP 不回传 token 计量，按 0 上报（保持 OpenAI 形态完整）。
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
        })));
    }

    // 流式：事件 → OpenAI chunk 帧翻译。
    // 首帧（role）从「立即发」改为「拿到回显 model 后发」——run_attempt 在
    // session/new 后先汇 PromptEvent::Model（契约 §3.2 的 currentModelId 回显），
    // 该事件必先于任何文本增量，帧序不变；仅「连接即失败」的早错路径不再先发
    // role 帧（错误帧内容不变）。
    let (client_tx, client_rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    tokio::spawn(async move {
        let id = format!("chatcmpl-trae-{}", uuid::Uuid::new_v4());
        let mut model = model_echo_fallback;
        let mut role_sent = false;
        // 首帧：role（OpenAI 流式惯例）；model 回显就绪后立即发。
        macro_rules! send_role {
            () => {
                if !role_sent
                    && client_tx
                        .send(Ok(Bytes::from(openai_chunk(
                            &id,
                            &model,
                            json!({"role": "assistant", "content": ""}),
                            None,
                        ))))
                        .await
                        .is_err()
                {
                    return; // 下游已断开
                }
                role_sent = true;
            };
        }
        let mut stop = "end_turn".to_string();
        let mut failure: Option<String> = None;
        while let Some(event) = rx.recv().await {
            match event {
                PromptEvent::Model(echo) => {
                    model = echo;
                    send_role!();
                }
                PromptEvent::Text(chunk) => {
                    send_role!();
                    if client_tx
                        .send(Ok(Bytes::from(openai_chunk(
                            &id,
                            &model,
                            json!({"content": chunk}),
                            None,
                        ))))
                        .await
                        .is_err()
                    {
                        return; // 下游已断开
                    }
                }
                PromptEvent::Finished(reason) => {
                    stop = reason;
                    send_role!();
                }
                PromptEvent::Failed(message) => failure = Some(message),
            }
        }
        // pump 结束（tx drop）后 join，随后收尾。
        let _ = handle.await;
        if let Some(message) = failure {
            let _ = client_tx.send(Err(message)).await;
            return;
        }
        let _ = client_tx
            .send(Ok(Bytes::from(openai_chunk(
                &id,
                &model,
                json!({}),
                Some(openai_finish_reason(&stop)),
            ))))
            .await;
        let _ = client_tx
            .send(Ok(Bytes::from_static(b"data: [DONE]\n\n")))
            .await;
    });
    Ok(TraeAcpChat::Stream(TraeAcpUpstreamStream { rx: client_rx }))
}

/// HTTP 状态码（与 axum 的数值一致；避免 core 依赖 axum）。
mod status_code {
    pub const BAD_REQUEST: u16 = 400;
    pub const BAD_GATEWAY: u16 = 502;
    pub const SERVICE_UNAVAILABLE: u16 = 503;
}

// ---------------------------------------------------------------------------
// 注册表（~/.workbuddy/sessions/<pid>.json）
// ---------------------------------------------------------------------------

/// 一条活跃会话条目（注册表 + 解析结果）。
#[derive(Clone, Debug)]
struct RegistryHit {
    port: u16,
    session_id: String,
    cwd: Option<String>,
    pid: i64,
    last_heartbeat: i64,
}

fn sessions_dir() -> PathBuf {
    let mut dir = home_dir();
    for part in SESSIONS_REL_DIR {
        dir = dir.join(part);
    }
    dir
}

/// 解析注册表条目文本；缺关键字段（心跳 / 端口）或非 JSON → None。
fn parse_registry_entry(text: &str) -> Option<RegistryHit> {
    let v: Value = serde_json::from_str(text).ok()?;
    let pid = v.get("pid").and_then(Value::as_i64).unwrap_or(0);
    let last_heartbeat = v
        .get("lastHeartbeat")
        .and_then(Value::as_i64)
        .or_else(|| v.get("updatedAt").and_then(Value::as_i64))?;
    let port = v
        .get("url")
        .and_then(Value::as_str)
        .and_then(port_from_url)
        .or_else(|| {
            v.get("endpoint")
                .and_then(Value::as_str)
                .and_then(port_from_url)
        })?;
    let session_id = v
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("interactive-{pid}"));
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from);
    Some(RegistryHit {
        port,
        session_id,
        cwd,
        pid,
        last_heartbeat,
    })
}

/// 从 URL / endpoint 提取端口。容忍 `http://127.0.0.1:56875`、
/// `http://127.0.0.1:56875/api/v1/acp`、裸 `127.0.0.1:52928` 等形态。
fn port_from_url(url: &str) -> Option<u16> {
    let rest = url.trim();
    let rest = rest
        .strip_prefix("http://")
        .or_else(|| rest.strip_prefix("https://"))
        .unwrap_or(rest);
    let authority = rest.split('/').next().unwrap_or_default();
    let port_str = authority.rsplit(':').next()?;
    port_str.parse::<u16>().ok()
}

/// 心跳是否在 5 分钟有效期内（容忍少量时钟漂移）。
fn is_fresh(last_heartbeat: i64, now: i64) -> bool {
    let age = now - last_heartbeat;
    age < HEARTBEAT_TTL_MS && age > -CLOCK_SKEW_TOLERANCE_MS
}

/// 挑「心跳最新」的条目（同心跳取 pid 大者）。纯函数，便于测试。
fn freshest(mut hits: Vec<RegistryHit>) -> Option<RegistryHit> {
    hits.sort_by(|a, b| (a.last_heartbeat, a.pid).cmp(&(b.last_heartbeat, b.pid)));
    hits.pop()
}

/// 只读扫描注册表：解析所有 `<pid>.json`，过滤心跳过期/损坏条目，取最新。
fn scan_registry() -> Option<RegistryHit> {
    scan_registry_for(None)
}

/// G5：两个目录是否属于同一项目树（归一化后相等或互为父子）。
fn cwd_matches(want: &str, have: &str) -> bool {
    let want = normalize_dir(want);
    let have = normalize_dir(have);
    if want.is_empty() || have.is_empty() {
        return false;
    }
    want == have
        || want.starts_with(&format!("{have}/"))
        || have.starts_with(&format!("{want}/"))
}

/// G5：按请求 cwd 选择活跃会话——同一项目树的条目优先，命中多个取心跳
/// 最新；无 cwd 请求或没有匹配条目时回落「心跳最新」。修复「默认选中用户
/// 正在用的 IDE 项目会话、响应带其上下文」。
fn scan_registry_for(requested_cwd: Option<&str>) -> Option<RegistryHit> {
    let now = now_ms();
    let entries = std::fs::read_dir(sessions_dir()).ok()?;
    let mut hits = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // 注册表存在写坏的条目（本机 1045 条中 3 条非 JSON）：坏文件跳过。
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(hit) = parse_registry_entry(&text) else {
            continue;
        };
        if is_fresh(hit.last_heartbeat, now) {
            hits.push(hit);
        }
    }
    if let Some(want) = requested_cwd.map(normalize_dir).filter(|s| !s.is_empty()) {
        let mut matched: Vec<RegistryHit> = hits
            .iter()
            .filter(|h| h.cwd.as_deref().map(|c| cwd_matches(&want, c)).unwrap_or(false))
            .cloned()
            .collect();
        if !matched.is_empty() {
            return freshest(matched);
        }
    }
    freshest(hits)
}

// ---------------------------------------------------------------------------
// 目录白名单（fullAccess 门禁）
// ---------------------------------------------------------------------------

/// 统一目录形态：分隔符归一为 `/`、去尾部斜杠；Windows 下大小写不敏感。
fn normalize_dir(path: &str) -> String {
    let mut s = path.trim().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s = s.to_lowercase();
    }
    s
}

/// cwd 是否落在白名单某个目录之内（自身或子目录；前缀树边界，防
/// `/a/bc` 冒充 `/a/b` 的子目录）。
fn dir_allowed(cwd: &str, allowed: &[String]) -> bool {
    let cwd = normalize_dir(cwd);
    if cwd.is_empty() {
        return false;
    }
    allowed.iter().any(|dir| {
        let dir = normalize_dir(dir);
        !dir.is_empty() && (cwd == dir || cwd.starts_with(&format!("{dir}/")))
    })
}

// ---------------------------------------------------------------------------
// HTTP / ACP 客户端（loopback 免鉴权）
// ---------------------------------------------------------------------------

/// loopback ACP 客户端（全局单例连接池）。
///
/// 超时策略与 loomy 通路同一教训：**连接 + 空闲读** 在 client 层表达，严禁对
/// 流式请求叠请求级 `.timeout()`（会把长回答在上游仍正常输出时掐断）。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy() // 只走 127.0.0.1，不继承系统代理
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .read_timeout(Duration::from_secs(READ_IDLE_TIMEOUT_SECS))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// 一条 ACP 连接（connect 换来的 connectionId 贯穿后续请求）。
struct AcpConnection {
    port: u16,
    connection_id: String,
}

/// `POST /api/v1/acp/connect` → connectionId（loopback 免鉴权，无 body）。
async fn acp_connect(port: u16) -> Result<AcpConnection, String> {
    let url = format!("http://127.0.0.1:{port}{ACP_PATH}/connect");
    let resp = http_client()
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("ACP connect 请求失败: {}", super::glm_anthropic::error_chain(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        let hint = if status == 404 {
            "（TRAE 客户端可能未以 server/ACP 模式启动）"
        } else {
            ""
        };
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "ACP connect 返回 {status}{hint}: {}",
            truncate(&text, 200)
        ));
    }
    let value: Value = resp
        .json()
        .await
        .map_err(|e| format!("ACP connect 响应解析失败: {}", super::glm_anthropic::error_chain(&e)))?;
    let connection_id = value
        .get("connectionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "ACP connect 响应缺少 connectionId".to_string())?
        .to_string();
    Ok(AcpConnection {
        port,
        connection_id,
    })
}

/// 发一条 JSON-RPC 请求（响应为 SSE 流；调用方负责读取）。
async fn acp_post(
    conn: &AcpConnection,
    id: &str,
    method: &str,
    params: Value,
) -> Result<reqwest::Response, String> {
    let url = format!("http://127.0.0.1:{}{ACP_PATH}", conn.port);
    http_client()
        .post(&url)
        .header("acp-connection-id", &conn.connection_id)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .send()
        .await
        .map_err(|e| format!("ACP {method} 请求失败: {}", super::glm_anthropic::error_chain(&e)))
}

/// 短步调用（initialize / session/new / session/set_mode）：发请求并等在 SSE
/// 流里认领对应 id 的 JSON-RPC 响应，返回 `result`。
async fn acp_call(
    conn: &AcpConnection,
    id: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let resp = acp_post(conn, id, method, params).await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "ACP {method} 返回 {status}: {}",
            truncate(&text, 200)
        ));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(STEP_DEADLINE_SECS);
    let mut reader = SseLines::new(resp);
    loop {
        let message = match tokio::time::timeout_at(deadline, reader.next_msg()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => return Err(format!("ACP {method} 的 SSE 流在返回响应前结束")),
            Err(_) => {
                return Err(format!(
                    "ACP {method} 等待响应超时（{STEP_DEADLINE_SECS}s）"
                ))
            }
        };
        if message.get("id").and_then(Value::as_str) == Some(id) {
            if let Some(error) = message.get("error") {
                let desc = error
                    .pointer("/message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                return Err(format!("ACP {method} 返回错误: {desc}"));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
        // 其它 id / 通知：继续等本步的响应。
    }
}

// ---------------------------------------------------------------------------
// SSE / NDJSON 行读取
// ---------------------------------------------------------------------------

/// SSE 行的三种归类。
enum LineKind {
    /// 注释 / 元信息 / 空行 / 非 JSON data：跳过。
    Skip,
    /// 一个完整的 JSON-RPC message 文本。
    Json(String),
}

/// 归类一行 SSE/NDJSON 文本。
fn parse_sse_line(line: &str) -> LineKind {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        // `:ok` / `: heartbeat` 注释帧（acp_ctx.txt @3722109）。
        return LineKind::Skip;
    }
    if let Some(payload) = line.strip_prefix("data:") {
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" || !payload.starts_with('{') {
            return LineKind::Skip;
        }
        return LineKind::Json(payload.to_string());
    }
    if line.starts_with("event:")
        || line.starts_with("id:")
        || line.starts_with("retry:")
        || !line.starts_with('{')
    {
        return LineKind::Skip;
    }
    // 裸 NDJSON：每行一个 JSON-RPC message。
    LineKind::Json(line.to_string())
}

/// 把 reqwest 响应体切成「一条条 JSON-RPC 消息」。
///
/// 兼容两种帧格式：标准 SSE（`event: message` + `data: {json}`，服务端
/// writeSseEvent 的实际输出）与裸 NDJSON（协议文档口径）。UTF-8 码点跨 chunk
/// 截留复用 `glm_anthropic::decode_stream_chunk`。
struct SseLines {
    resp: reqwest::Response,
    carry: Vec<u8>,
    buf: String,
}

impl SseLines {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            resp,
            carry: Vec::new(),
            buf: String::new(),
        }
    }

    /// 取下一条 JSON-RPC 消息；流正常结束 → None；读失败 → Some(Err)。
    async fn next_msg(&mut self) -> Option<Result<Value, String>> {
        loop {
            // 1) 缓冲区内已攒下的整行。
            while let Some(pos) = self.buf.find('\n') {
                let line: String = self.buf.drain(..=pos).collect();
                match parse_sse_line(line.trim_end_matches(['\n', '\r'])) {
                    LineKind::Skip => continue,
                    LineKind::Json(text) => {
                        return Some(serde_json::from_str(&text).map_err(|e| {
                            format!("ACP SSE 消息解析失败: {e}（{}）", truncate(&text, 120))
                        }))
                    }
                }
            }
            // 2) 读更多字节。
            match self.resp.chunk().await {
                Ok(Some(chunk)) => {
                    let text = super::glm_anthropic::decode_stream_chunk(&mut self.carry, &chunk);
                    self.buf.push_str(&text);
                }
                Ok(None) => {
                    // 流结束：尾行可能没有换行符（NDJSON），按最后一行处理。
                    let tail = std::mem::take(&mut self.buf);
                    return match parse_sse_line(tail.trim_end_matches(['\n', '\r'])) {
                        LineKind::Skip => None,
                        LineKind::Json(text) => Some(
                            serde_json::from_str(&text)
                                .map_err(|e| format!("ACP SSE 消息解析失败: {e}")),
                        ),
                    };
                }
                Err(e) => {
                    return Some(Err(format!(
                        "ACP SSE 读取失败: {}",
                        super::glm_anthropic::error_chain(&e)
                    )))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// prompt 主链路（含断线重连一次）
// ---------------------------------------------------------------------------

/// pump 向上层（流式翻译器 / 聚合器）汇出的事件。
enum PromptEvent {
    /// 响应 `model` 字段的回显值（session/new 后汇出：请求显式 model 优先，
    /// 否则该会话的 currentModelId；契约 §3.2「缺省时回显 currentModelId」）。
    Model(String),
    /// agent_message_chunk 增量文本。
    Text(String),
    /// 本轮正常结束（stopReason）。
    Finished(String),
    /// 失败（重连一次后仍失败，或已产出内容后断流）。
    Failed(String),
}

/// 单轮 attempt 的产出。
struct AttemptOutcome {
    stop_reason: String,
}

/// 单轮 attempt 的失败（携带已产出块数，供重连策略判断）。
struct AttemptError {
    message: String,
    emitted: usize,
}

/// pump 主任务：跑 attempt → 断线重连一次 → 事件汇出后自然结束（tx drop）。
async fn pump_prompt(
    port: u16,
    cwd: String,
    mode: String,
    model: Option<String>,
    echo_hint: Option<String>,
    prompt: String,
    tx: tokio::sync::mpsc::Sender<PromptEvent>,
) {
    let mut first_error: Option<String> = None;
    let mut emitted = false;
    for attempt in 0..2u8 {
        match run_attempt(
            port,
            &cwd,
            &mode,
            model.as_deref(),
            echo_hint.as_deref(),
            &prompt,
            &tx,
        )
        .await
        {
            Ok(outcome) => {
                let _ = tx
                    .send(PromptEvent::Finished(outcome.stop_reason))
                    .await;
                return;
            }
            Err(error) => {
                emitted |= error.emitted > 0;
                if emitted {
                    // 已向下游产出过内容：重连会重放整轮回答，不再重试。
                    let _ = tx
                        .send(PromptEvent::Failed(format!(
                            "TRAE ACP 连接中断（已输出部分内容，不再重连以免重复）: {}",
                            error.message
                        )))
                        .await;
                    return;
                }
                if attempt == 0 {
                    // 断线重连一次（整条链路重新走：connect→initialize→new→prompt）。
                    first_error = Some(error.message);
                    continue;
                }
                let _ = tx
                    .send(PromptEvent::Failed(format!(
                        "TRAE ACP 连接中断（已重连一次仍失败）: {} / {}",
                        first_error.unwrap_or_default(),
                        error.message
                    )))
                    .await;
                return;
            }
        }
    }
    // 循环内必定 return；兜底防未来改动漏分支。
    let _ = tx
        .send(PromptEvent::Failed(
            "TRAE ACP 连接中断（未知原因）".to_string(),
        ))
        .await;
}

/// 单轮 attempt：connect → initialize → session/new →(best-effort set_mode)→
/// (Some(model) 时 session/set_model)→ session/prompt，增量块经 tx 汇出。
/// `echo_hint`：请求显式 model 原值（含 "trae"/"workbuddy" 占位），回显优先级
/// 高于会话 currentModelId——显式占位请求的响应 model 字段保持历史行为。
async fn run_attempt(
    port: u16,
    cwd: &str,
    mode: &str,
    model: Option<&str>,
    echo_hint: Option<&str>,
    prompt: &str,
    tx: &tokio::sync::mpsc::Sender<PromptEvent>,
) -> Result<AttemptOutcome, AttemptError> {
    let mut emitted = 0usize;
    // 1) connect（loopback 免鉴权）。
    let conn = match acp_connect(port).await {
        Ok(conn) => conn,
        Err(message) => return Err(AttemptError { message, emitted }),
    };
    // 2) initialize。
    if let Err(message) = acp_call(
        &conn,
        ID_INITIALIZE,
        "initialize",
        json!({
            "protocolVersion": 1,
            "capabilities": {},
            "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
        }),
    )
    .await
    {
        return Err(AttemptError { message, emitted });
    }
    // 3) session/new（每次 attempt 新建会话：API 无状态语义；不用注册表
    //    sessionId 做 session/load——那会把 IDE 侧历史当增量块重放到响应里）。
    //    注意：参数名是 `cwd`（bundle 文档注释写的 workingDirectory 是错的，
    //    实测传 workingDirectory 服务端返回 -32602 Invalid params：
    //    "cwd: expected string, received undefined"，2026-09-22 实测）。
    //    同一 cwd 会复用该 cwd 已绑定的会话（如 IDE 正开的项目会话），响应里
    //    可能带历史增量块——需要干净会话时传一个没有历史绑定的 cwd。
    let new_result = match acp_call(
        &conn,
        ID_SESSION_NEW,
        "session/new",
        json!({
            "cwd": cwd,
            "mcpServers": [],
        }),
    )
    .await
    {
        Ok(result) => result,
        Err(message) => return Err(AttemptError { message, emitted }),
    };
    let session_id = match new_result
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(session_id) => session_id.to_string(),
        None => {
            return Err(AttemptError {
                message: "ACP session/new 响应缺少 sessionId".to_string(),
                emitted,
            })
        }
    };
    // 4) best-effort set_mode：API 转发场景无人应答工具权限请求，默认 bypass
    //    （真实控制面是 cwd 白名单）；模式不存在等失败不影响主流程。
    let _ = acp_call(
        &conn,
        ID_SET_MODE,
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": mode}),
    )
    .await;
    // 4b) session/set_model（契约 §3.3）：请求显式指定内置模型时真切换。
    //     失败走 AttemptError 重试链（重连一次后仍失败 → 上层 502/兜底）。
    //     注意：set_model 对非法 ID 假成功，合法性由调用方（网关侧）对照目录校验。
    //     响应 model 回显（契约 §3.2）：请求显式值（含占位原样）优先，缺省时
    //     回显会话 currentModelId（session/new 的 models.currentModelId）。
    let current_model_id = new_result
        .pointer("/models/currentModelId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let echo_model = echo_hint
        .map(String::from)
        .or_else(|| (!current_model_id.is_empty()).then_some(current_model_id));
    if let Some(echo) = echo_model {
        let _ = tx.send(PromptEvent::Model(echo)).await;
    }
    if let Some(model_id) = model {
        if let Err(message) = acp_call(
            &conn,
            ID_SET_MODEL,
            "session/set_model",
            build_set_model_params(&session_id, model_id),
        )
        .await
        {
            return Err(AttemptError { message, emitted });
        }
    }
    // 5) session/prompt（SSE 实时流）。
    let resp = match acp_post(
        &conn,
        ID_PROMPT,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": prompt}],
        }),
    )
    .await
    {
        Ok(resp) => resp,
        Err(message) => return Err(AttemptError { message, emitted }),
    };
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(AttemptError {
            message: format!("ACP session/prompt 返回 {status}: {}", truncate(&text, 200)),
            emitted,
        });
    }
    let mut reader = SseLines::new(resp);
    loop {
        let message = match reader.next_msg().await {
            Some(Ok(message)) => message,
            Some(Err(message)) => return Err(AttemptError { message, emitted }),
            None => {
                return Err(AttemptError {
                    message: "ACP session/prompt 的 SSE 流在收到响应前结束".to_string(),
                    emitted,
                })
            }
        };
        // 5a) 本步 JSON-RPC 响应：{stopReason, errorMessage?}，本轮结束。
        if message.get("id").and_then(Value::as_str) == Some(ID_PROMPT) {
            if let Some(error) = message.get("error") {
                let desc = error
                    .pointer("/message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                return Err(AttemptError {
                    message: format!("ACP session/prompt 返回错误: {desc}"),
                    emitted,
                });
            }
            let stop_reason = message
                .pointer("/result/stopReason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn")
                .to_string();
            if let Some(error_message) = message
                .pointer("/result/errorMessage")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return Err(AttemptError {
                    message: format!("ACP session/prompt 失败: {error_message}"),
                    emitted,
                });
            }
            return Ok(AttemptOutcome { stop_reason });
        }
        // 5b) session/update 通知。
        if message.get("method").and_then(Value::as_str) == Some("session/update") {
            let update = message
                .pointer("/params/update")
                .cloned()
                .unwrap_or(Value::Null);
            match update.pointer("/sessionUpdate").and_then(Value::as_str) {
                Some("agent_message_chunk") => {
                    if let Some(text) = chunk_text(&update) {
                        if tx.send(PromptEvent::Text(text)).await.is_err() {
                            return Err(AttemptError {
                                message: "下游连接已断开，放弃本轮 ACP 流".to_string(),
                                emitted,
                            });
                        }
                        emitted += 1;
                    }
                }
                Some("session_end") => {
                    // 本轮对话结束（stopReason = end_turn/cancelled/refusal）。
                    let stop_reason = update
                        .pointer("/stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn")
                        .to_string();
                    return Ok(AttemptOutcome { stop_reason });
                }
                Some("interruption_request") => {
                    // 工具权限请求：API 代理无人应答权限，绝不静默放行。
                    return Err(AttemptError {
                        message: "TRAE 请求工具权限（interruption_request），当前代理不自动放行；可在请求体用 acpMode=bypassPermissions 重试"
                            .to_string(),
                        emitted,
                    });
                }
                _ => {} // agent_thought_chunk / tool_call / user_message_chunk 等不透出。
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI 形态翻译
// ---------------------------------------------------------------------------

/// 取 agent_message_chunk 的文本。兼容 content 为 string / `{type:"text",text}` /
/// ContentBlock 数组三种形态（acp_ctx.txt @3616043 getAcpUpdateText 同款兜底）。
fn chunk_text(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    if let Some(text) = content.pointer("/text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    if let Some(parts) = content.as_array() {
        let joined: Vec<&str> = parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect();
        if !joined.is_empty() {
            return Some(joined.join(""));
        }
    }
    None
}

/// 组一帧 OpenAI `chat.completion.chunk` SSE（`data: {...}\n\n`）。
fn openai_chunk(id: &str, model: &str, delta: Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": now_ms() / 1000,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
    )
}

/// ACP stopReason → OpenAI finish_reason。
fn openai_finish_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "refusal" => "content_filter",
        _ => "stop", // end_turn / cancelled / 其它
    }
}

/// 请求体里的工作目录覆盖字段（非标准 OpenAI 扩展）。
fn body_cwd(body: &Value) -> Option<String> {
    for key in ["cwd", "workingDirectory", "working_directory"] {
        if let Some(dir) = body.get(key).and_then(Value::as_str) {
            let dir = dir.trim();
            if !dir.is_empty() {
                return Some(dir.to_string());
            }
        }
    }
    None
}

/// messages → 单条 prompt 文本（与 workbuddy_proxy 同策略：角色前缀拍平）。
fn flatten_messages(messages: &[Value]) -> String {
    let mut chunks = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = match message.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(Value::as_str) == Some("text") {
                        p.get("text").and_then(Value::as_str).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !content.trim().is_empty() {
            chunks.push(format!("[{role}] {content}"));
        }
    }
    chunks.join("\n\n")
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// 测试（纯函数；不触网、不读真实注册表）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_from_url_variants() {
        assert_eq!(port_from_url("http://127.0.0.1:56875"), Some(56875));
        assert_eq!(port_from_url("http://127.0.0.1:56875/api/v1/acp"), Some(56875));
        assert_eq!(port_from_url("https://127.0.0.1:52928"), Some(52928));
        assert_eq!(port_from_url("127.0.0.1:62278"), Some(62278));
        // 无端口 / 非法端口 → None。
        assert_eq!(port_from_url("http://127.0.0.1"), None);
        assert_eq!(port_from_url("http://127.0.0.1:abc"), None);
        assert_eq!(port_from_url(""), None);
    }

    #[test]
    fn parse_registry_entry_reads_fields() {
        let text = r#"{
            "pid": 11396,
            "lastHeartbeat": 1787156185640,
            "sessionId": "f19dfaeb-993d-4111-91ff-aa1732cc1a9b",
            "cwd": "E:\\WorkBuddy\\06-会话记录\\2026-06-06-19-02-35",
            "kind": "interactive",
            "url": "http://127.0.0.1:52928",
            "mode": "local",
            "version": "2.147.0"
        }"#;
        let hit = parse_registry_entry(text).expect("合法条目应解析成功");
        assert_eq!(hit.port, 52928);
        assert_eq!(hit.session_id, "f19dfaeb-993d-4111-91ff-aa1732cc1a9b");
        assert_eq!(hit.pid, 11396);
        assert_eq!(hit.last_heartbeat, 1787156185640);
        assert_eq!(
            hit.cwd.as_deref(),
            Some("E:\\WorkBuddy\\06-会话记录\\2026-06-06-19-02-35")
        );
    }

    #[test]
    fn parse_registry_entry_fallbacks_and_rejects() {
        // endpoint 兜底 + sessionId 缺失时按 pid 派生。
        let text = r#"{"pid": 42, "lastHeartbeat": 1, "endpoint": "http://127.0.0.1:6000/api/v1/acp"}"#;
        let hit = parse_registry_entry(text).expect("endpoint 可兜底");
        assert_eq!(hit.port, 6000);
        assert_eq!(hit.session_id, "interactive-42");
        assert!(hit.cwd.is_none());
        // 坏 JSON / 缺心跳 / 缺端口 → None（扫描时跳过）。
        assert!(parse_registry_entry("   ").is_none());
        assert!(parse_registry_entry("not json").is_none());
        assert!(parse_registry_entry(r#"{"pid":1,"url":"http://127.0.0.1:6000"}"#).is_none());
        assert!(parse_registry_entry(r#"{"pid":1,"lastHeartbeat":1,"url":"http://127.0.0.1"}"#).is_none());
    }

    #[test]
    fn heartbeat_freshness_window() {
        let now = 1_000_000_000i64;
        assert!(is_fresh(now, now));
        assert!(is_fresh(now - HEARTBEAT_TTL_MS + 1, now));
        assert!(!is_fresh(now - HEARTBEAT_TTL_MS, now));
        assert!(!is_fresh(now - 10 * 60 * 1000, now));
        // 时钟漂移容忍（心跳戳略新于当前时间）。
        assert!(is_fresh(now + 30_000, now));
        assert!(!is_fresh(now + 10 * 60 * 1000, now));
    }

    #[test]
    fn freshest_picks_latest_heartbeat() {
        let hits = vec![
            RegistryHit {
                port: 1,
                session_id: "a".into(),
                cwd: None,
                pid: 1,
                last_heartbeat: 100,
            },
            RegistryHit {
                port: 2,
                session_id: "b".into(),
                cwd: None,
                pid: 2,
                last_heartbeat: 300,
            },
            RegistryHit {
                port: 3,
                session_id: "c".into(),
                cwd: None,
                pid: 3,
                last_heartbeat: 200,
            },
        ];
        let best = freshest(hits).expect("应有最新条目");
        assert_eq!(best.port, 2);
        assert_eq!(best.session_id, "b");
        assert!(freshest(Vec::new()).is_none());
    }

    #[test]
    fn cwd_matches_same_tree_only() {
        // 相等（含分隔符/大小写差异）。
        assert!(cwd_matches("E:\\proj", "E:/PROJ"));
        assert!(cwd_matches("E:\\proj\\sub", "E:\\proj"));
        // 父子双向命中。
        assert!(cwd_matches("E:\\proj", "E:\\proj\\a\\b"));
        // 兄弟目录/无关目录不命中（前缀树边界）。
        assert!(!cwd_matches("E:\\proj", "E:\\project-other"));
        assert!(!cwd_matches("E:\\proj", "F:\\proj"));
        // 空值不命中。
        assert!(!cwd_matches("", "E:\\proj"));
        assert!(!cwd_matches("E:\\proj", ""));
    }

    #[test]
    fn dir_allowed_prefix_and_boundaries() {
        let allowed = vec!["E:\\proj".to_string()];
        assert!(dir_allowed("E:\\proj", &allowed));
        assert!(dir_allowed("E:\\proj\\sub\\deep", &allowed));
        // 前缀树边界：同前缀的兄弟目录不得绕过。
        assert!(!dir_allowed("E:\\project-other", &allowed));
        assert!(!dir_allowed("E:\\", &allowed));
        assert!(!dir_allowed("", &allowed));
        // 白名单自身形态：尾部斜杠 / 反斜杠混用都应归一。
        let allowed2 = vec!["E:/proj/".to_string()];
        assert!(dir_allowed("E:\\proj\\sub", &allowed2));
        // 多条目任一命中即可。
        let allowed3 = vec!["/a".to_string(), "/b".to_string()];
        assert!(dir_allowed("/b/x", &allowed3));
        assert!(!dir_allowed("/c", &allowed3));
    }

    #[test]
    fn chunk_text_handles_all_content_shapes() {
        // string 形态。
        let u = json!({"sessionUpdate": "agent_message_chunk", "content": "hi"});
        assert_eq!(chunk_text(&u).as_deref(), Some("hi"));
        // {type:"text",text} 形态（服务端 schema 主形态）。
        let u = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello"}});
        assert_eq!(chunk_text(&u).as_deref(), Some("hello"));
        // ContentBlock 数组形态。
        let u = json!({"sessionUpdate": "agent_message_chunk", "content": [
            {"type": "text", "text": "a"}, {"type": "image", "data": "xx"}, {"type": "text", "text": "b"}
        ]});
        assert_eq!(chunk_text(&u).as_deref(), Some("ab"));
        // 无 content / 非文本块 → None。
        assert!(chunk_text(&json!({"sessionUpdate": "agent_message_chunk"})).is_none());
        assert!(chunk_text(&json!({"content": {"type": "image"}})).is_none());
    }

    #[test]
    fn parse_sse_line_classification() {
        assert!(matches!(parse_sse_line(":ok"), LineKind::Skip));
        assert!(matches!(parse_sse_line(": heartbeat"), LineKind::Skip));
        assert!(matches!(parse_sse_line(""), LineKind::Skip));
        assert!(matches!(parse_sse_line("event: message"), LineKind::Skip));
        assert!(matches!(parse_sse_line("data: [DONE]"), LineKind::Skip));
        assert!(matches!(parse_sse_line("data: not-json"), LineKind::Skip));
        assert!(matches!(parse_sse_line("garbage"), LineKind::Skip));
        match parse_sse_line("data: {\"a\":1}") {
            LineKind::Json(text) => assert_eq!(text, "{\"a\":1}"),
            _ => panic!("data: 行应为 Json"),
        }
        match parse_sse_line("{\"jsonrpc\":\"2.0\"}") {
            LineKind::Json(text) => assert_eq!(text, "{\"jsonrpc\":\"2.0\"}"),
            _ => panic!("裸 NDJSON 行应为 Json"),
        }
    }

    #[test]
    fn flatten_messages_roles_and_parts() {
        let messages = vec![
            json!({"role": "system", "content": "be nice"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "user", "content": [{"type": "text", "text": "part1"}, {"type": "image_url", "x": 1}]}),
        ];
        assert_eq!(
            flatten_messages(&messages),
            "[system] be nice\n\n[user] hello\n\n[user] part1"
        );
        assert!(flatten_messages(&[]).is_empty());
    }

    #[test]
    fn openai_frames_and_finish_reason() {
        assert_eq!(openai_finish_reason("end_turn"), "stop");
        assert_eq!(openai_finish_reason("cancelled"), "stop");
        assert_eq!(openai_finish_reason("refusal"), "content_filter");
        assert_eq!(openai_finish_reason("whatever"), "stop");
        let frame = openai_chunk("id1", "m", json!({"content": "hi"}), None);
        assert!(frame.starts_with("data: {"));
        assert!(frame.ends_with("\n\n"));
        let value: Value = serde_json::from_str(frame.trim_start_matches("data: ").trim())
            .expect("帧体应为合法 JSON");
        assert_eq!(value["object"], "chat.completion.chunk");
        assert_eq!(value["choices"][0]["delta"]["content"], "hi");
    }

    // -------------------------------------------------------------------------
    // 内置模型目录（契约 §3.3 新增）
    // -------------------------------------------------------------------------

    #[test]
    fn parse_multiplier_from_description() {
        // 2026-09-25 实测 description 形态（"x0.21"）。
        assert_eq!(parse_multiplier("x0.21"), Some(0.21));
        assert_eq!(parse_multiplier("x1.20"), Some(1.2));
        assert_eq!(parse_multiplier("x0.06"), Some(0.06));
        assert_eq!(parse_multiplier("x0"), Some(0.0));
        // 嵌在文案里也能取到。
        assert_eq!(parse_multiplier("credits x0.29 per call"), Some(0.29));
        // 无倍率 / 空串 / 非 x 前缀 → None（契约：解析失败为 null）。
        assert_eq!(parse_multiplier(""), None);
        assert_eq!(parse_multiplier("no multiplier here"), None);
        assert_eq!(parse_multiplier("0.5x faster"), None);
        assert_eq!(parse_multiplier("x."), None);
    }

    #[test]
    fn model_kind_classification() {
        // 档位别名 3 个（2026-09-25 实测）。
        assert_eq!(model_kind("fast-model"), "tier");
        assert_eq!(model_kind("balanced-model"), "tier");
        assert_eq!(model_kind("deep-model"), "tier");
        // 自定义接入条目。
        assert_eq!(model_kind("custom-local:qwen3.8-flash"), "custom");
        assert_eq!(model_kind("custom-local:opencode/mimo-v2.6-flash-free"), "custom");
        // 实模。
        assert_eq!(model_kind("glm-5.3-flash"), "model");
        assert_eq!(model_kind("kimi-k3-1"), "model");
        assert_eq!(model_kind("deepseek-v4.1-flash"), "model");
    }

    #[test]
    fn build_set_model_params_shape() {
        let params = build_set_model_params("sess-1", "glm-5.3-flash");
        assert_eq!(
            params,
            json!({"sessionId": "sess-1", "modelId": "glm-5.3-flash"})
        );
    }

    #[test]
    fn native_request_model_placeholder_semantics() {
        // 占位值视同 None（/trae/v1 行为不变）。
        assert_eq!(native_request_model(&json!({"model": "trae"})), None);
        assert_eq!(native_request_model(&json!({"model": "workbuddy"})), None);
        assert_eq!(native_request_model(&json!({"model": " trae "})), None);
        // 缺省 / 空 → None。
        assert_eq!(native_request_model(&json!({})), None);
        assert_eq!(native_request_model(&json!({"model": ""})), None);
        assert_eq!(native_request_model(&json!({"model": "   "})), None);
        // 显式模型 → Some。
        assert_eq!(
            native_request_model(&json!({"model": "glm-5.3-flash"})),
            Some("glm-5.3-flash".to_string())
        );
    }

    #[test]
    fn parse_native_catalog_from_session_new_result() {
        // 仿 2026-09-25 实测 session/new result 形态（字段名逐一对齐）。
        let result = json!({
            "sessionId": "probe-session",
            "models": {
                "availableModels": [
                    {"modelId": "fast-model", "name": "快速", "description": "x0.21"},
                    {"modelId": "glm-5.3-flash", "name": "GLM-5.3-Flash", "description": "x0.06"},
                    {"modelId": "kimi-k3-1", "name": "Kimi-K3-1", "description": "x1.62"},
                    {"modelId": "custom-local:qwen3.8-flash", "name": "自定义", "description": null}
                ],
                "currentModelId": "fast-model"
            },
            "configOptions": [
                {"id": "mode", "type": "select", "options": []},
                {"id": "model", "type": "select", "options": [
                    {"value": "fast-model", "name": "快速", "description": "x0.21"},
                    {"value": "glm-5.3-flash", "name": "GLM-5.3-Flash", "description": "x0.06"},
                    {"value": "kimi-k3-1", "name": "Kimi-K3-1", "description": "x1.62"},
                    {"value": "custom-local:qwen3.8-flash", "name": "自定义", "description": null}
                ]}
            ]
        });
        let catalog = parse_native_catalog(&result, 56599).expect("应解析成功");
        assert_eq!(catalog.port, 56599);
        assert_eq!(catalog.current_model_id, "fast-model");
        // 顺序保持服务端返回序（与 WorkBuddy UI 一致）。
        let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["fast-model", "glm-5.3-flash", "kimi-k3-1", "custom-local:qwen3.8-flash"]);
        // kind / multiplier。
        assert_eq!(catalog.models[0].kind, "tier");
        assert_eq!(catalog.models[0].multiplier, Some(0.21));
        assert_eq!(catalog.models[1].kind, "model");
        assert_eq!(catalog.models[1].multiplier, Some(0.06));
        assert_eq!(catalog.models[2].kind, "model");
        assert_eq!(catalog.models[2].multiplier, Some(1.62));
        assert_eq!(catalog.models[3].kind, "custom");
        assert_eq!(catalog.models[3].multiplier, None);
        assert_eq!(catalog.models[0].name, "快速");
        // contains_id：tier/model/custom 全部命中；目录外 id 不命中。
        assert!(catalog.contains_id("fast-model"));
        assert!(catalog.contains_id("custom-local:qwen3.8-flash"));
        assert!(!catalog.contains_id("totally-fake-model"));
    }

    #[test]
    fn parse_native_catalog_falls_back_to_available_models() {
        // configOptions 缺 model select 时，name/description 回落 availableModels 条目。
        let result = json!({
            "models": {
                "availableModels": [
                    {"modelId": "glm-5.3", "name": "GLM-5.3", "description": "x0.79"}
                ],
                "currentModelId": "glm-5.3"
            }
        });
        let catalog = parse_native_catalog(&result, 1).expect("应解析成功");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].name, "GLM-5.3");
        assert_eq!(catalog.models[0].multiplier, Some(0.79));
        assert_eq!(catalog.models[0].kind, "model");
        // ACP availableModels 命中 = exposed:true（契约 §2.1）。
        assert!(catalog.models[0].exposed);
        // 缺 availableModels → None。
        assert!(parse_native_catalog(&json!({"sessionId": "x"}), 1).is_none());
        assert!(parse_native_catalog(&Value::Null, 1).is_none());
    }

    // -------------------------------------------------------------------------
    // 内置补充源合并（契约 §2.1 新增）
    // -------------------------------------------------------------------------

    #[test]
    fn product_config_cli_model_ids_prefers_cli_agent() {
        // 仿实测形态：agents 数组里恰 1 个 name=="cli" 的对象，models 为字符串数组
        // （2026-09-25 实测 18 个内置 id，此处取代表）；另有含 fast-model 的诱饵对象。
        let config = json!({
            "version": 3,
            "agents": [
                {"name": "ide", "models": ["not-a-real-model", "decoy"]},
                {
                    "name": "cli",
                    "models": [
                        "fast-model", "balanced-model", "deep-model",
                        "glm-5.3-flash", "kimi-k3-1", "deepseek-v4.1-flash"
                    ]
                }
            ],
            "other": {"models": ["fast-model", "fake-from-decoy"]}
        });
        let ids = product_config_cli_model_ids(&config);
        assert_eq!(
            ids,
            vec![
                "fast-model",
                "balanced-model",
                "deep-model",
                "glm-5.3-flash",
                "kimi-k3-1",
                "deepseek-v4.1-flash"
            ]
        );
    }

    #[test]
    fn product_config_cli_model_ids_fallback_to_any_fast_model() {
        // 无 name=="cli" 对象 → 回落「任何含 fast-model 的 models 字符串数组」。
        let config = json!({
            "nested": {"agents": [{"name": "ide", "models": ["a", "fast-model", "b"]}]}
        });
        assert_eq!(
            product_config_cli_model_ids(&config),
            vec!["a", "fast-model", "b"]
        );
        // 完全没有 models 字符串数组 → 空表（调用方静默跳过）。
        assert!(product_config_cli_model_ids(&json!({"agents": [{"name": "cli"}]})).is_empty());
        assert!(product_config_cli_model_ids(&Value::Null).is_empty());
    }

    #[test]
    fn product_config_cli_model_ids_deformed_models_array_is_skipped() {
        // models 混入非字符串/空串：只收字符串元素；全非字符串视为无该数组。
        let config = json!({
            "agents": [
                {"name": "cli", "models": ["glm-5.3", 42, null, "", {"id": "x"}]}
            ]
        });
        assert_eq!(product_config_cli_model_ids(&config), vec!["glm-5.3"]);
        let config = json!({"agents": [{"name": "cli", "models": [1, 2]}]});
        assert!(product_config_cli_model_ids(&config).is_empty());
        // models 不是数组（如对象）→ 不命中。
        let config = json!({"agents": [{"name": "cli", "models": {"0": "fast-model"}}]});
        assert!(product_config_cli_model_ids(&config).is_empty());
    }

    #[test]
    fn merge_product_config_models_appends_missing_and_marks_exposed() {
        // ACP 只回了 3 档位 + 1 实模；产品配置含档位（已存在，跳过）+ 2 个补充实模。
        let catalog = parse_native_catalog(
            &json!({
                "models": {
                    "availableModels": [
                        {"modelId": "fast-model", "name": "快速", "description": "x0.21"},
                        {"modelId": "deep-model", "name": "深度", "description": "x1.20"},
                        {"modelId": "glm-5.3-flash", "name": "GLM-5.3-Flash", "description": "x0.06"}
                    ],
                    "currentModelId": "fast-model"
                }
            }),
            56599,
        )
        .expect("应解析成功");
        let product_config = json!({
            "agents": [{"name": "cli", "models": [
                "fast-model", "deep-model", "glm-5.3-flash",
                "kimi-k3-1", "deepseek-v4-pro", "custom-local:qwen3.8-flash"
            ]}]
        });
        let merged = merge_product_config_models(catalog, &product_config);
        let ids: Vec<&str> = merged.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["fast-model", "deep-model", "glm-5.3-flash", "kimi-k3-1", "deepseek-v4-pro", "custom-local:qwen3.8-flash"]
        );
        // 既有 ACP 条目：exposed=true，其余字段不变。
        let fast = merged.models.iter().find(|m| m.id == "fast-model").unwrap();
        assert!(fast.exposed);
        assert_eq!(fast.kind, "tier");
        assert_eq!(fast.multiplier, Some(0.21));
        assert_eq!(fast.name, "快速");
        // 补充条目：kind="model"、multiplier=None、name=id 原样、exposed=false。
        for id in ["kimi-k3-1", "deepseek-v4-pro"] {
            let m = merged.models.iter().find(|x| x.id == id).unwrap();
            assert!(!m.exposed, "{id} 仅来自产品配置，应 exposed=false");
            assert_eq!(m.kind, "model");
            assert_eq!(m.multiplier, None);
            assert_eq!(m.name, id);
        }
        // custom-local:* 若 ACP 未暴露也按补充源原样追加（kind 仍为 model——
        // 补充源无 custom 语义，前端按 exposed=false 展示；custom 判定仅对
        // ACP availableModels 条目生效，见 model_kind）。
        let custom = merged
            .models
            .iter()
            .find(|m| m.id == "custom-local:qwen3.8-flash")
            .unwrap();
        assert!(!custom.exposed);
        assert_eq!(custom.kind, "model");
        // contains_id 对补充条目同样生效（native-first 兜底判定依赖目录全集）。
        assert!(merged.contains_id("kimi-k3-1"));
    }

    #[test]
    fn merge_product_config_models_tolerates_garbage_config() {
        // 变形配置（空对象/Null/无 cli）→ 目录原样返回，绝不 panic。
        let catalog = parse_native_catalog(
            &json!({
                "models": {"availableModels": [{"modelId": "fast-model", "name": "快速"}]}
            }),
            1,
        )
        .unwrap();
        for garbage in [
            Value::Null,
            json!({}),
            json!({"agents": [{"name": "cli", "models": "not-an-array"}]}),
        ] {
            let merged = merge_product_config_models(catalog.clone(), &garbage);
            assert_eq!(merged.models.len(), 1);
            assert!(merged.models[0].exposed);
        }
    }

    /// 开发期自检（默认跳过，需本机 WorkBuddy 运行中）：
    /// `cargo test -p wb-switch-core native_catalog_live_probe -- --ignored --nocapture`
    /// 打真实 ACP session/new（探针 cwd）验证目录探测链路。
    #[test]
    #[ignore = "live probe：需本机 WorkBuddy 运行，默认套件跳过"]
    fn native_catalog_live_probe() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let catalog = rt
            .block_on(async { crate::modules::trae_acp::native_models_catalog().await })
            .expect("live 探测应成功（WorkBuddy 未运行则本测试失败属预期）");
        let native = catalog
            .models
            .iter()
            .filter(|m| m.kind != "custom")
            .count();
        let tiers = catalog.models.iter().filter(|m| m.kind == "tier").count();
        assert!(native >= 15, "native 条目应 ≥15，实得 {native}");
        assert_eq!(tiers, 3, "档位别名应为 3 个");
        assert!(catalog.contains_id("fast-model"), "档位 fast-model 应在目录");
        assert!(catalog.contains_id("glm-5.3-flash"), "实模 glm-5.3-flash 应在目录");
        assert!(catalog.contains_id("kimi-k3-1"), "实模 kimi-k3-1 应在目录");
        assert!(catalog
            .models
            .iter()
            .all(|m| m.id == "fast-model" || m.multiplier.is_some() || m.kind == "custom"),
            "native 条目应有 multiplier（custom 可空）");
        println!(
            "live catalog: port={} current={} models={} (native={native}, tiers={tiers})",
            catalog.port,
            catalog.current_model_id,
            catalog.models.len()
        );
        for m in catalog.models.iter().take(5) {
            println!("  {:12} {:18} {:>6} {:?}", m.kind, m.id, format!("{:?}", m.multiplier), m.name);
        }
    }
}
