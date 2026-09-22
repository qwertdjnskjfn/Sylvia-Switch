//! Qwen 网页版通道（chat.qwen.ai）—— CDP 浏览器桥 + 双通道调度。
//!
//! ## 为什么需要浏览器桥
//! 纯 HTTP 直连 `POST /api/v2/chat/completions` 会稳定触发阿里霸下风控
//! （RGV587_ERROR 滑动验证，2026-09-19 实测 3/3 被拦，acw_tc 会话 cookie
//! 也无效；`chats/new` 与 `/api/models` 不拦）。唯一稳定的过风控方式是
//! **在真实 chat.qwen.ai 页面上下文里发 fetch**：专用 Chrome（独立 profile
//! + 独立调试端口，与 grok_sso 的 19233 实例互不干扰）里由阿里 WAF 的
//! 页面 JS（bx-ua/bx-v 等动态令牌）自动生成，网关侧零逆向。
//!
//! ## 鉴权
//! 桌面端深链 JWT（`qwen::extract_desktop_token`，30 天有效）直接作为
//! `Authorization: Bearer` 头注入页面内 fetch——与页面登录态无关
//! （实测未登录页面 + Bearer 即可调通全部 API，2026-09-19）。
//!
//! ## 双通道
//! `channel = auto | web | qoder`：
//! - auto（默认）：网页版优先，任何失败（风控/浏览器缺失/超时）自动回落
//!   Qoder 通道（qoder::chat_completions_unified_for，独立 qwen 池）；
//! - web：只用网页版，失败即报错（不回落）；
//! - qoder：现状行为（纯 Qoder 算力池）。
//! 响应头 `X-Qwen-Channel` 标注实际使用的通道（web / qoder），
//! `X-Qwen-Channel-Note` 附回落原因（如有）。

use bytes::Bytes;
use base64::engine::general_purpose;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::modules::config::{atomic_write, store_dir};
use crate::modules::qwen::extract_desktop_token;

pub const VERSION: &str = "1.0.0";
const CDP_PORT: u16 = 19235;
const QWEN_WEB_URL: &str = "https://chat.qwen.ai/";
const DEFAULT_MODEL: &str = "qwen3.7-plus";
/// 单请求总预算基值（不含预填充/生成，那部分按输入长度加，见 budgets_for）。
const REQUEST_BUDGET_MS: u64 = 300_000;
/// 聊天单发（非流式）评估预算基值：45s 封顶，避免上游限流挂起时客户端干等。
const CHAT_EVAL_BUDGET_MS: u64 = 45_000;
/// 熔断器参数（借鉴 smanx/qwen2api 的分级风控）：连续失败阈值与熔断时长。
const WEB_CIRCUIT_THRESHOLD: usize = 3;
const WEB_CIRCUIT_SECS: u64 = 300;
static WEB_FAIL_STREAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static WEB_CIRCUIT_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
const POLL_INTERVAL_MS: u64 = 350;

/// 按输入长度与模型档位计算网页版通道预算（毫秒）。
///
/// 预填充耗时随上下文长度近似线性增长：1M token 量级的输入服务端通常要
/// 数十秒到数分钟才吐首 token。固定的 15s/60s 首内容预算会把长上下文请求
/// 一律误判为"零回复"并错误回落 Qoder（2026-09-20 验证 1M 时定位此为代码层
/// 主障碍）。按字符数线性放宽：
/// - 首内容预算 = base（max 60s / 其余 15s）+ 每 10 万字符 +60s；
/// - 总预算     = 300s + max 档再 +120s + 每 10 万字符 +120s；
/// - 封顶首内容 600s / 总预算 1800s：惩罚期也不把客户端无限挂住。
fn budgets_for(content_chars: usize, model: &str) -> (u64, u64) {
    let is_max = model.contains("max");
    let per_100k = (content_chars / 100_000) as u64;
    let first_ms = if is_max { 60_000 } else { 15_000 } + per_100k * 60_000;
    let total_ms =
        REQUEST_BUDGET_MS + if is_max { 120_000 } else { 0 } + per_100k * 120_000;
    (first_ms.min(600_000), total_ms.min(1_800_000))
}

// ---------------------------------------------------------------------------
// 模型显示名动态同步（降级可见性）
// ---------------------------------------------------------------------------

/// opencode 配置文件路径（opencode CLI 用；**WorkBuddy 不读它**）。
fn opencode_config_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir())?;
    Some(home.join(".config").join("opencode").join("opencode.json"))
}

/// WorkBuddy 模型注册表——**桌面端真正读取的文件**。
/// 2026-09-20 实测：app.asar 内无 `opencode.json` 引用，模型清单来自
/// `~/.workbuddy/models.json`（条目列表，非 provider 结构）。之前只改
/// opencode.json 对 WorkBuddy 完全无效。
fn workbuddy_models_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir())?;
    Some(home.join(".workbuddy").join("models.json"))
}

/// 走 Sylvia 通道调度的 qwen base URL（区别于固定 lite 档的 `/qwen-flash/v1`，
/// 后者不存在降级概念，同步时须排除）。
const QWEN_CHANNEL_BASE_URL: &str = "http://127.0.0.1:57890/qwen/v1";

/// 通道健康档位（决定模型显示名后缀）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QwenTierLabel {
    /// 网页版通道可用：原档质量。
    Web,
    /// 网页版不可用、由 Qoder lite 档兜底：降级。
    Degraded,
}

/// 把通道当前档位同步到 opencode 配置里 `provider.sylvia-qwen` 的模型显示名。
///
/// 动机：网页版被风控拦截 + Qoder Free 账号 max/pro 零额度时，网关会自动降级
/// 到 lite 档作答。此前模型列表里只显示"Qwen3.8-Max"，用户以为在用旗舰、
/// 实际拿到 lite 档回答（2026-09-20 实测踩到）。这里让降级在模型列表里可见。
///
/// 安全约束：**只改** `provider.sylvia-qwen.models.*.name` 一个字段，其余内容
/// （mcp、其他 provider、模型能力声明）原样保留；首次改写前备份；写失败只记
/// 日志不影响主流程。
/// 把通道当前档位同步到模型显示名（降级可见）。
///
/// 两个目标文件：
/// 1. `~/.workbuddy/models.json`——**WorkBuddy 桌面端真正读的注册表**（主目标）；
/// 2. `~/.config/opencode/opencode.json`——opencode CLI 的配置（次目标，保持兼容）。
///
/// 安全约束：两者都**只改显示名（name）**，其余内容原样保留；首次改写前各备份
/// 一次；幂等（同档位重复同步不产生变化）；写失败只记日志不影响主流程。
pub fn sync_opencode_tier_label(label: QwenTierLabel) {
    let suffix = match label {
        QwenTierLabel::Web => "网页版",
        QwenTierLabel::Degraded => "⚠lite降级",
    };
    sync_workbuddy_models_tier_label(suffix);
    sync_opencode_json_tier_label(suffix);
}

/// 同步 WorkBuddy 的 models.json：命中 `url == /qwen/v1` 的条目改 name。
/// 只碰我们自己的条目（固定 lite 档的 /qwen-flash/v1 排除，它没有降级语义）。
fn sync_workbuddy_models_tier_label(suffix: &str) {
    let Some(path) = workbuddy_models_path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return; // 没装 WorkBuddy 或还没生成注册表
    };
    let Ok(mut list) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(entries) = list.as_array_mut() else {
        return;
    };
    let mut changed = 0usize;
    for entry in entries.iter_mut() {
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        let url = obj.get("url").and_then(Value::as_str).unwrap_or("");
        if url.trim_end_matches('/') != QWEN_CHANNEL_BASE_URL {
            continue;
        }
        let Some(id) = obj.get("id").and_then(Value::as_str).map(String::from) else {
            continue;
        };
        let new_name = format!("{} (Sylvia·{})", pretty_model_name(&id), suffix);
        if obj.get("name").and_then(Value::as_str) != Some(new_name.as_str()) {
            obj.insert("name".to_string(), json!(new_name));
            changed += 1;
        }
    }
    if changed == 0 {
        return;
    }
    write_with_backup(&path, &list, "WorkBuddy models.json", changed, suffix);
}

/// 同步 opencode CLI 的 provider 模型显示名（兼容路径，非 WorkBuddy 主路径）。
fn sync_opencode_json_tier_label(suffix: &str) {
    let Some(path) = opencode_config_path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut cfg) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(models) = cfg
        .pointer_mut("/provider/sylvia-qwen/models")
        .and_then(Value::as_object_mut)
    else {
        return; // 没有我们的 provider：用户可能已删掉配置
    };
    let mut changed = 0usize;
    for (model_id, model) in models.iter_mut() {
        let Some(obj) = model.as_object_mut() else {
            continue;
        };
        // 以模型 ID 重建显示名，保证幂等（反复同步不会叠加后缀）。
        let new_name = format!("{} (Sylvia·{suffix})", pretty_model_name(model_id));
        if obj.get("name").and_then(Value::as_str) != Some(new_name.as_str()) {
            obj.insert("name".to_string(), json!(new_name));
            changed += 1;
        }
    }
    if changed == 0 {
        return;
    }
    write_with_backup(&path, &cfg, "opencode.json", changed, suffix);
}

/// 模型 ID → 展示名（未知 ID 原样透出，不臆造）。
fn pretty_model_name(model_id: &str) -> String {
    let base = model_id.to_ascii_uppercase().replace(['.', '-'], "");
    match base.as_str() {
        "QWEN38MAX" => "Qwen3.8-Max".to_string(),
        "QWEN37PLUS" => "Qwen3.7-Plus".to_string(),
        "QWEN38OMNIFLASH" => "Qwen3.8-Omni-Flash".to_string(),
        "QWEN37MAX" => "Qwen3.7-Max".to_string(),
        "QWEN36PLUS" => "Qwen3.6-Plus".to_string(),
        "QWEN35PLUS" => "Qwen3.5-Plus".to_string(),
        "QWEN35OMNIPLUS" => "Qwen3.5-Omni-Plus".to_string(),
        _ => model_id.to_string(),
    }
}

/// 带备份的原子写入（首次改写前备份一次，固定名避免堆副本）。
fn write_with_backup(path: &Path, value: &Value, label: &str, changed: usize, suffix: &str) {
    let backup = path.with_extension("json.sylvia-bak");
    if !backup.exists() {
        if let Ok(original) = std::fs::read_to_string(path) {
            let _ = std::fs::write(&backup, original);
        }
    }
    match serde_json::to_string_pretty(value) {
        Ok(content) => {
            if atomic_write(path, &content).is_ok() {
                log_debug(&format!(
                    "模型显示名已同步为「{suffix}」（{changed} 个模型）→ {label}"
                ));
            }
        }
        Err(e) => log_debug(&format!("模型显示名同步失败（序列化）: {e}")),
    }
}

/// 探活结果 → 档位标签（并触发显示名同步）。
fn tier_label_from_probe(ok: bool) -> QwenTierLabel {
    if ok {
        QwenTierLabel::Web
    } else {
        QwenTierLabel::Degraded
    }
}


// ---------------------------------------------------------------------------
// 通道配置（~/.wb-switch/qwen_config.json）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct QwenChannelConfig {
    /// auto（网页版优先 + 自动回落）/ web（仅网页版）/ qoder（仅 Qoder）。
    pub channel: String,
    /// 兼容旧字段：channel=qoder 时等价关闭网页版；保留用于显式开关回落。
    pub auto_fallback: bool,
}

fn config_path() -> PathBuf {
    store_dir().join("qwen_config.json")
}

pub fn load_channel_config() -> QwenChannelConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| QwenChannelConfig {
            channel: v
                .get("channel")
                .and_then(Value::as_str)
                .map(|s| match s {
                    "web" => "web".to_string(),
                    "qoder" => "qoder".to_string(),
                    _ => "auto".to_string(),
                })
                .unwrap_or_else(|| "auto".to_string()),
            auto_fallback: v
                .get("auto_fallback")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
        .unwrap_or(QwenChannelConfig {
            channel: "auto".to_string(),
            auto_fallback: true,
        })
}

pub fn save_channel_config(channel: &str, auto_fallback: bool) -> Result<(), String> {
    let channel = match channel {
        "web" => "web",
        "qoder" => "qoder",
        _ => "auto",
    };
    let content = serde_json::to_string_pretty(&json!({
        "channel": channel,
        "auto_fallback": auto_fallback,
        "updatedAt": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&config_path(), &content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 调试日志（内存环形缓冲，最近 80 条）
// ---------------------------------------------------------------------------

static DEBUG_LOG: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

pub fn log_debug(msg: &str) {
    let ts = chrono_like_now();
    let mut log = DEBUG_LOG.lock().unwrap();
    log.push_back(format!("[{ts}] {msg}"));
    while log.len() > 80 {
        log.pop_front();
    }
}

/// 最近 n 条调试日志（旧→新）。
pub fn debug_log_tail(n: usize) -> Vec<String> {
    let log = DEBUG_LOG.lock().unwrap();
    let skip = log.len().saturating_sub(n);
    log.iter().skip(skip).cloned().collect()
}

fn chrono_like_now() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = ms / 1000;
    let today = secs % 86400;
    format!("{:02}:{:02}:{:02}.{:03}", today / 3600, (today % 3600) / 60, today % 60, ms % 1000)
}

// ---------------------------------------------------------------------------
// CDP 基建（同 grok_sso 模式；独立端口与 profile，互不影响）
// ---------------------------------------------------------------------------

fn cdp_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy() // 127.0.0.1 请求不能进系统代理（Clash 会掐断）
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build qwen cdp http client")
    })
}

/// 探测 Chrome/Edge 可执行文件（与 grok_sso 同一张已知路径表）。
fn resolve_chrome_path() -> Option<PathBuf> {
    let known = [
        r"E:\驱动精灵搬家目录(勿删改)\软件\Google Chrome\安装目录\541\chrome.exe",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    ];
    for k in known {
        let p = PathBuf::from(k);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn profile_dir() -> PathBuf {
    store_dir().join("qwen-chrome-profile")
}

async fn cdp_alive(port: u16) -> bool {
    match cdp_http_client()
        .get(&format!("http://127.0.0.1:{port}/json/version"))
        .send()
        .await
    {
        Ok(resp) => matches!(
            resp.json::<Value>().await,
            Ok(v) if v.get("Browser").is_some()
        ),
        Err(_) => false,
    }
}

/// 确保专用 Chrome 在跑（独立实例：独立 profile + 独立调试端口）。
async fn ensure_browser() -> Result<(), String> {
    if cdp_alive(CDP_PORT).await {
        return Ok(());
    }
    let chrome =
        resolve_chrome_path().ok_or_else(|| "未找到 Chrome/Edge 可执行文件，无法启动网页版通道浏览器".to_string())?;
    let profile = profile_dir();
    std::fs::create_dir_all(&profile).map_err(|e| format!("创建 Qwen 浏览器 profile 失败: {e}"))?;
    let mut cmd = std::process::Command::new(&chrome);
    cmd.args([
        &format!("--remote-debugging-port={CDP_PORT}"),
        &format!("--user-data-dir={}", profile.display()),
        "--no-first-run",
        "--no-default-browser-check",
        "--remote-allow-origins=*",
        QWEN_WEB_URL,
    ]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008); // DETACHED_PROCESS
    }
    cmd.spawn().map_err(|e| format!("启动 Chrome 失败: {e}"))?;
    log_debug(&format!("已拉起专用 Chrome（pid 未知，端口 {CDP_PORT}）"));
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        if cdp_alive(CDP_PORT).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("Chrome 已启动但调试端口未就绪".to_string())
}

/// 定位（或新开）chat.qwen.ai 页面并返回其 WebSocket 调试地址。
async fn ensure_page_ws_url() -> Result<String, String> {
    let list: Vec<Value> = cdp_http_client()
        .get(format!("http://127.0.0.1:{CDP_PORT}/json"))
        .send()
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?
        .json()
        .await
        .map_err(|e| format!("CDP /json 解析失败: {e}"))?;
    let qwen_page = list.iter().find(|t| {
        t.get("type").and_then(Value::as_str) == Some("page")
            && t.get("url")
                .and_then(Value::as_str)
                .map(|u| u.contains("chat.qwen.ai"))
                .unwrap_or(false)
    });
    if let Some(page) = qwen_page {
        if let Some(ws) = page.get("webSocketDebuggerUrl").and_then(Value::as_str) {
            return Ok(ws.to_string());
        }
    }
    // 无 qwen 页面：新开一个 tab（Chrome 111+ 要求 PUT；老版本回退 GET）。
    let new_url = format!("http://127.0.0.1:{CDP_PORT}/json/new?{QWEN_WEB_URL}");
    let put_result = cdp_http_client().put(&new_url).send().await;
    let put_ok = matches!(
        put_result.as_ref().map(|r| r.status().is_success()),
        Ok(true)
    );
    let target: Value = if put_ok {
        let resp = put_result.ok().ok_or_else(|| "新开标签页失败".to_string())?;
        resp.json::<Value>()
            .await
            .map_err(|e| format!("新开标签页响应解析失败: {e}"))?
    } else {
        let resp = cdp_http_client()
            .get(&new_url)
            .send()
            .await
            .map_err(|e| format!("新开 chat.qwen.ai 标签页失败: {e}"))?;
        resp.json::<Value>()
            .await
            .map_err(|e| format!("新开标签页响应解析失败: {e}"))?
    };
    tokio::time::sleep(Duration::from_millis(1500)).await;
    target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| "新标签页缺少 webSocketDebuggerUrl".to_string())
}

/// CDP 单命令：连 WS → 发命令 → 收对应响应。
async fn cdp_call_on(ws_url: &str, method: &str, params: Value) -> Result<Value, String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| format!("CDP WebSocket 连接失败: {e}"))?;
    ws.send(Message::Text(json!({"id": 1, "method": method, "params": params}).to_string().into()))
        .await
        .map_err(|e| format!("CDP 发送失败: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let msg = tokio::time::timeout(Duration::from_secs(25), ws.next())
            .await
            .map_err(|_| "CDP 响应超时".to_string())?
            .ok_or_else(|| "CDP 连接关闭".to_string())?
            .map_err(|e| format!("CDP 读错误: {e}"))?;
        if let Message::Text(text) = msg {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if value.get("id").and_then(Value::as_u64) == Some(1) {
                    if let Some(err) = value.get("error") {
                        return Err(format!("CDP 错误: {err}"));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }
    Err("CDP 响应超时".to_string())
}

/// 在页面上下文执行表达式并取回 returnByValue 的值。
async fn eval_on(ws_url: &str, expression: &str, timeout_ms: u64) -> Result<Value, String> {
    let result = cdp_call_on(
        ws_url,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "awaitPromise": true,
            "returnByValue": true,
            "timeout": timeout_ms,
        }),
    )
    .await?;
    if let Some(detail) = result.get("exceptionDetails") {
        let text = detail
            .pointer("/exception/description")
            .or_else(|| detail.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(format!("页面 JS 异常: {text}"));
    }
    Ok(result.get("result").cloned().unwrap_or(Value::Null))
}

/// 确保页面就绪（加载完成且在 chat.qwen.ai），返回 ws_url。
async fn ensure_page_ready() -> Result<String, String> {
    ensure_browser().await?;
    let ws_url = ensure_page_ws_url().await?;
    // 最多等 40s 页面加载完成；期间页面可能还在跑 WAF 挑战。
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let state = eval_on(
            &ws_url,
            "JSON.stringify({href: location.href, ready: document.readyState})",
            8000,
        )
        .await;
        match state {
            Ok(v) => {
                let s = v.get("value").and_then(Value::as_str).unwrap_or("");
                let parsed: Value = serde_json::from_str(s).unwrap_or(Value::Null);
                let href = parsed.get("href").and_then(Value::as_str).unwrap_or("");
                let ready = parsed.get("ready").and_then(Value::as_str).unwrap_or("");
                if href.contains("_____tmd_____") || href.contains("punish") {
                    // 页面被阿里 WAF 导航到人机验证页：回到主页重试（可见窗口
                    // 内若仍弹滑块，用户可手动完成一次，之后 cookie 自动通行）。
                    log_debug("页面处于 WAF 验证页，导航回主页");
                    let _ = cdp_call_on(&ws_url, "Page.navigate", json!({"url": QWEN_WEB_URL})).await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                } else if !href.contains("chat.qwen.ai") {
                    // 页面被导航走了（例如手工改地址）：拉回来。
                    let _ = cdp_call_on(&ws_url, "Page.navigate", json!({"url": QWEN_WEB_URL})).await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                } else if ready == "complete" {
                    return Ok(ws_url);
                }
            }
            Err(e) => {
                if Instant::now() > deadline {
                    return Err(e);
                }
            }
        }
        if Instant::now() > deadline {
            return Err("chat.qwen.ai 页面加载超时".to_string());
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

// ---------------------------------------------------------------------------
// 消息拍平与 JS 构造
// ---------------------------------------------------------------------------

fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// OpenAI messages → chat.qwen.ai 单条 user prompt（多轮拍平转写）。
fn flatten_messages(body: &Value) -> Result<(String, String), String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "缺少 messages".to_string())?;
    if messages.is_empty() {
        return Err("messages 为空".to_string());
    }
    let mut out = String::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if content.trim().is_empty() {
            continue;
        }
        let tag = match role {
            "system" | "developer" => "System",
            "assistant" => "Assistant",
            "tool" => "Tool",
            _ => "User",
        };
        out.push_str(&format!("[{tag}]\n{content}\n\n"));
    }
    if out.is_empty() {
        return Err("messages 内容为空".to_string());
    }
    out.push_str("[Assistant]\n");
    Ok((out, model))
}

fn build_chat_payload(chat_id: &str, model: &str, content: &str) -> Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    json!({
        "stream": true,
        "version": "2.1",
        "incremental_output": true,
        "chatId": chat_id,
        "chat_id": chat_id,
        "chat_mode": "normal",
        "model": model,
        "parentId": "",
        "parent_id": Value::Null,
        "messages": [{
            "id": Value::Null,
            "fid": uuid::Uuid::new_v4().simple().to_string(),
            "parentId": Value::Null,
            "childrenIds": [uuid::Uuid::new_v4().simple().to_string()],
            "role": "user",
            "content": content,
            "user_action": "chat",
            "files": [],
            "timestamp": ts,
            "models": [model],
            "chat_type": "t2t",
            "feature_config": {
                "thinking_enabled": false,
                "output_schema": "phase",
                "research_mode": "normal",
                "auto_thinking": false,
                "thinking_mode": "Disabled",
                "thinking_format": "summary",
                "auto_search": false,
                "code_interpreter": false,
                "plugins_enabled": false,
                "function_calling": false,
                "enable_tools": false,
                "enable_function_call": false,
                "tool_choice": "none",
            },
            "extra": {"meta": {"subChatType": "t2t"}},
            "sub_chat_type": "t2t",
            "parent_id": Value::Null,
        }],
        "timestamp": ts,
    })
}

/// 页面内 fetch 的公共头（Authorization 用桌面端 JWT，鉴权与页面登录态无关）。
/// 页面内实时取阿里 baxia 令牌并拼进 fetch 头（2026-09-19 逆向实测：带
/// bx-ua/bx-umidtoken/bx-v 头可过 baxia 门控，裸请求被 punish 页拦截）。
/// - `window.__baxia__.getFYModule` 是 object（SDK 版本差异），内部有
///   `getUidToken()`（T2gA 前缀 uid）与 `getFYToken()`（每次不同的 fy 令牌）；
/// - smanx 公式：bx-ua = fy || '231!' + uid，bx-umidtoken = uid，bx-v = '2.5.37'；
/// - baxia SDK 未加载/被改时优雅降级为空对象（fetch 退回无 bx 头形态）。
const BAXIA_HEADERS_JS: &str = r#"(function(){
  try {
    const m = window.__baxia__ && window.__baxia__.getFYModule;
    if (!m || typeof m.getUidToken !== 'function') return {};
    const uid = m.getUidToken();
    const fy = (typeof m.getFYToken === 'function') ? m.getFYToken() : '';
    const ua = fy || ('231!' + uid);
    return { 'bx-ua': ua, 'bx-umidtoken': uid, 'bx-v': '2.5.37' };
  } catch (e) { return {}; }
})()"#;

/// 带 baxia 令牌的页面内 fetch 头（在页面上下文内联执行 BAXIA_HEADERS_JS）。
/// 🔴 2026-09-21 起不再发送 `Authorization: Bearer <桌面 JWT>`——实测该 JWT
/// 会被服务端吊销（"session has expired"，即使 JWT 本身未过期），而**页面
/// Cookie 登录态始终有效**（同源 fetch 自动携带）。token 参数保留仅为兼容
/// 旧调用点，不再注入头。
fn baxia_headers_js(_token: &str) -> String {
    // 🔴 2026-09-21 fetch 劫持抓包对齐：真实 App 的 completions 请求带
    // Version/source/Timezone/X-Request-Id/X-Accel-Buffering/Accept-Language
    // 全套头，缺这些会被静默挑战（fetch 永不返回）。Accept 用 application/json
    // （真实 App 流式请求也发这个，勿改成 text/event-stream）。
    format!(
        "Object.assign({{ 'Content-Type': 'application/json', Accept: 'application/json', 'X-Accel-Buffering': 'no', 'X-Request-Id': (crypto.randomUUID ? crypto.randomUUID() : 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, c => {{ const r = Math.random()*16|0, v = c==='x'?r:(r&0x3|0x8); return v.toString(16); }})), 'Version': '0.2.91', 'source': 'web', 'Timezone': new Date().toString().replace(/\\s*\\([^)]*\\)$/, ''), 'Accept-Language': 'zh-CN,zh;q=0.9' }}, {})",
        BAXIA_HEADERS_JS,
    )
}

/// 从上游 SSE 文本提取 (answer, reasoning, usage, punished, finished)。
#[derive(Default, Debug)]
struct SseAggregate {
    answer: String,
    reasoning: String,
    usage: Option<Value>,
    punished: bool,
    finished: bool,
}

fn feed_sse_line(agg: &mut SseAggregate, line: &str) {
    // 🔴 WAF/风控挑战特征（smanx/qwen2api punish 画像扩充）——必须在 data: 前缀
    // 分流**之前**扫全行：WAF 的 HTML 挑战页/SLB 503 会混进非 data 行。
    // HTTP 200 携带这些特征 = 静默挑战，绝不是正常空回复。
    if line.contains("RGV587")
        || line.contains("_____tmd_____")
        || line.contains("rgv587_flag")
        || line.contains("rgv-flag")
        || line.contains("FAIL_SYS_USER_VALIDATE")
        || line.contains("aliyun_waf")
        || line.contains("/punish")
        || line.contains("captcha-content-container")
        || line.contains("bixi.alicdn.com")
    {
        agg.punished = true;
    }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return,
    };
    if data.is_empty() || data == "[DONE]" {
        return;
    }
    let frame: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return,
    };
    if frame.get("response.created").is_some() {
        return;
    }
    if let Some(delta) = frame.pointer("/choices/0/delta") {
        let phase = delta.get("phase").and_then(Value::as_str).unwrap_or("");
        let content = delta.get("content").and_then(Value::as_str).unwrap_or("");
        let status = delta.get("status").and_then(Value::as_str).unwrap_or("");
        match phase {
            "thinking" | "reasoning" => agg.reasoning.push_str(content),
            _ => agg.answer.push_str(content),
        }
        if status == "finished" {
            agg.finished = true;
        }
    }
    if let Some(u) = frame.get("usage").filter(|u| u.is_object()) {
        agg.usage = Some(u.clone());
    }
    if line.contains("RGV587") || line.contains("_____tmd_____") {
        agg.punished = true;
    }
}

fn usage_to_openai(usage: Option<&Value>) -> Value {
    match usage {
        Some(u) => json!({
            "prompt_tokens": u.get("input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": u.get("output_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": u.get("total_tokens").cloned().unwrap_or(json!(0)),
        }),
        None => json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}),
    }
}

fn completion_id() -> String {
    format!("chatcmpl-qwenweb-{}", uuid::Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// 对话主流程
// ---------------------------------------------------------------------------

pub enum QwenWebOutput {
    Json(Value),
    /// SSE 字节流（已合成 OpenAI chunk 格式）。
    Stream(tokio::sync::mpsc::Receiver<Result<Bytes, String>>),
}

/// 非流式返回的 OpenAI chat.completion。
fn completion_json(id: &str, model: &str, agg: &SseAggregate) -> Value {
    let mut message = json!({"role": "assistant", "content": agg.answer});
    if !agg.reasoning.is_empty() {
        message["reasoning_content"] = json!(agg.reasoning);
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": usage_to_openai(agg.usage.as_ref()),
        "qwen_channel": "web",
    })
}

/// 网页版通道对话。失败返回 Err（由调用方决定是否回落 Qoder）。
// ---------------------------------------------------------------------------
// chat_id 预热池：消费即回补（借鉴 qwen2api-rs），吃掉 chats/new 握手 + 落库间隔
// ---------------------------------------------------------------------------

/// 池内条目：(chat_id, 创建时刻)。
static CHAT_ID_POOL: Mutex<Option<Vec<(String, Instant)>>> = Mutex::new(None);
/// 池目标容量（单账号形态取小值；容量上限同时充当"溢出即丢弃"）。
const CHAT_ID_POOL_TARGET: usize = 2;
/// 池内 id 的最小年龄：chats/new 与 completions 间隔 >=1.5s（2026-09-19 A/B 实测）。
const CHAT_ID_MIN_AGE_MS: u128 = 1500;
/// 池内 id 最长停留：上游空会话保留时长未知，超时丢弃重建。
const CHAT_ID_TTL: Duration = Duration::from_secs(120);

/// 从池中取最旧且满足（年龄 >=1.5s 且 <TTL）的 chat_id。
fn take_pooled_chat_id() -> Option<String> {
    let mut guard = CHAT_ID_POOL.lock().unwrap();
    let pool = guard.get_or_insert_with(Vec::new);
    pool.retain(|(_, t)| t.elapsed() < CHAT_ID_TTL);
    let now = Instant::now();
    for i in 0..pool.len() {
        if now.duration_since(pool[i].1).as_millis() >= CHAT_ID_MIN_AGE_MS {
            return Some(pool.remove(i).0);
        }
    }
    None
}

/// 回补入池（池满丢弃）。
fn push_pooled_chat_id(id: String) {
    let mut guard = CHAT_ID_POOL.lock().unwrap();
    let pool = guard.get_or_insert_with(Vec::new);
    pool.retain(|(_, t)| t.elapsed() < CHAT_ID_TTL);
    if pool.len() < CHAT_ID_POOL_TARGET {
        pool.push((id, Instant::now()));
    }
}

/// 硬重启专用浏览器。
///
/// 🔴 页面僵死场景（2026-09-19 实测）：CDP WebSocket 连得上一一但页面
/// Runtime.evaluate 永不返回——`cdp_alive` 仍为 true，`ensure_browser` 认为
/// 浏览器健康、`ensure_page_ready` 的 readyState 轮询 40s 超时也救不回来，
/// 每次请求都要白等两轮 CDP 超时（25s×2=50s）。唯一恢复手段 = 按 19235
/// 端口找 PID 杀进程（不能按进程名杀——会误伤用户自己的 Chrome）后重拉。
fn kill_browser() {
    #[cfg(target_os = "windows")]
    {
        let script = "Get-NetTCPConnection -LocalPort 19235 -State Listen -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess -Unique | ForEach-Object { Stop-Process -Id $_ -Force -ErrorAction SilentlyContinue }";
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-Command", script]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        let _ = cmd.status();
    }
    std::thread::sleep(Duration::from_millis(800));
}

/// 页面内 fetch 创建新会话，返回 chat_id（chats/new 失败重试一次：页面可能短暂被 WAF 打断）。
async fn create_chat_id(ws_url: &str, token: &str, model: &str) -> Result<String, String> {
    let new_chat_js = format!(
        "(async () => {{ const r = await fetch('/api/v2/chats/new', {{ method: 'POST', headers: {headers}, body: JSON.stringify({{ title: 'gateway', models: [{model_js}], chat_type: 't2t', timestamp: Date.now() }}) }}); const t = await r.text(); return JSON.stringify({{ s: r.status, b: t.slice(0, 300) }}); }})()",
        headers = baxia_headers_js(token),
        model_js = js_string(model),
    );
    let new_val = match eval_on(ws_url, &new_chat_js, 30_000).await {
        Ok(v) => v,
        Err(first_err) => {
            log_debug(&format!("chats/new 首次失败（{first_err}），恢复页面后重试"));
            // CDP 超时/连接类错误 = 页面可能僵死：硬重启专用浏览器（而非只
            // ensure_page_ready——它对"进程活着页面僵死"无能为力）
            if first_err.contains("CDP 响应超时")
                || first_err.contains("CDP 连接失败")
                || first_err.contains("CDP 连接关闭")
                || first_err.contains("CDP 读错误")
            {
                kill_browser();
            }
            let ws_retry = ensure_page_ready().await?;
            eval_on(&ws_retry, &new_chat_js, 30_000).await?
        }
    };
    let wrapped: Value = serde_json::from_str(
        new_val.get("value").and_then(Value::as_str).unwrap_or(""),
    )
    .map_err(|_| "chats/new 响应解析失败".to_string())?;
    let new_body: Value =
        serde_json::from_str(wrapped.get("b").and_then(Value::as_str).unwrap_or(""))
            .unwrap_or(Value::Null);
    let chat_id = new_body
        .pointer("/data/id")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| {
            format!(
                "chats/new 失败: {}",
                wrapped.get("b").and_then(Value::as_str).unwrap_or("")
            )
        })?;
    log_debug(&format!("chats/new ok chat_id={chat_id}"));
    Ok(chat_id)
}

pub async fn chat_completions(body: Value) -> Result<QwenWebOutput, String> {
    static WEB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = WEB_LOCK.lock().await; // 页面级单任务：__qwenState 是全局槽位

    // 🔴 熔断器（借鉴 smanx/qwen2api）：连续 3 次风控挑战 → 网页版通道熔断 300s，
    // 期间请求直接报 Err（由上层 auto 回落 Qoder 池），避免惩罚期每条请求都在
    // 门控里空转 2-4 分钟（2026-09-19 实测：惩罚深期失败转移链 120-273s）。
    {
        let until = WEB_CIRCUIT_UNTIL.lock().unwrap().clone();
        if let Some(t) = until {
            if Instant::now() < t {
                let left = (t - Instant::now()).as_secs() + 1;
                return Err(format!(
                    "网页版通道熔断中（连续风控挑战），约 {left} 秒后自动恢复尝试；本次直接走 Qoder 池"
                ));
            }
        }
    }

    let started = Instant::now();
    let Some(token) = extract_desktop_token() else {
        log_debug("缺 Qwen 桌面端登录态（debug.log 未提取到 JWT）");
        return Err("未提取到 Qwen 桌面端登录态：请打开 Qwen 桌面客户端登录一次".to_string());
    };
    // 🔴 工具降级（与 Qoder 路由同协议）：lite/网页版均不解析 tools 数组——
    // 剥离注入为提示词、转写历史，聚合/抓取后嗅探合成。带 tools 时强制整段语义。
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map_or(false, |a| !a.is_empty());
    let mut work = body;
    if has_tools {
        crate::modules::qoder::rewrite_tool_history(&mut work);
        crate::modules::qoder::inject_tools_prompt(&mut work);
    }
    let (content, model) = flatten_messages(&work)?;
    let stream = if has_tools {
        false
    } else {
        work.get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };

    let ws_url = ensure_page_ready().await.map_err(|e| {
        log_debug(&format!("浏览器页面不可用: {e}"));
        e
    })?;
    log_debug(&format!("网页版通道开始：model={model} stream={stream} content={}字符", content.len()));

    // 1) chats/new——优先消费预热池（池内 id 创建已超 1.5s，天然满足落库间隔），
    //    未命中才现建；成功后后台回补一个（消费即回补，借鉴 qwen2api-rs）。
    let (chat_id, from_pool) = match take_pooled_chat_id() {
        Some(id) => {
            log_debug("chats/new 命中预热池");
            (id, true)
        }
        None => match create_chat_id(&ws_url, &token, &model).await {
            Ok(id) => (id, false),
            Err(new_err) => {
                // 🔴 chats/new 被风控静默挑战（fetch 永不返回）时，绝不能就此失败——
                // UI 真人模式不需要 chats/new（页面自己建会话），直接降级。
                log_debug(&format!("chats/new 失败（{new_err}）——跳过 fetch，直接降级 UI 真人模式"));
                return match ui_chat_send(&content, &model, stream, has_tools).await {
                    Ok(out) => Ok(out),
                    Err(ui_err) => Err(format!("{new_err}；UI 真人模式亦失败：{ui_err}")),
                };
            }
        },
    };
    if !from_pool {
        // chats/new 与 chat/completions 之间保留间隔：实测紧跟着发（<0.5s）会稳定
        // 触发服务端 RequestValidationError（Field '1' JSON decode error），推测
        // 会话索引异步落库需要时间；>=1.5s 稳定通过（2026-09-19 A/B 实测）。
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    // 后台回补预热池（cdp_call_on 每次独立 WebSocket，与主流程并发安全）
    {
        let ws = ws_url.clone();
        let tk = token.clone();
        let m = model.clone();
        tokio::spawn(async move {
            if let Ok(new_id) = create_chat_id(&ws, &tk, &m).await {
                push_pooled_chat_id(new_id);
            }
        });
    }

    // 2) chat/completions
    let payload = build_chat_payload(&chat_id, &model, &content);
    let payload_js = serde_json::to_string(&serde_json::to_string(&payload).unwrap_or_default())
        .unwrap_or_else(|_| "\"{}\"".to_string());
    let cleanup_js = format!(
        "(async () => (await fetch({url}, {{ method: 'DELETE' }})).status)()",
        url = js_string(&format!("/api/v2/chats/{chat_id}")),
    );
    let result = if stream {
        // 串行锁与清理随流式任务持有到流结束（页面 __qwenState 为全局槽位；
        // 会话删除必须在流读完后执行，否则会掐断正在进行的生成）。
        let model_owned = model.clone();
        run_stream(
            ws_url,
            token,
            payload_js,
            model_owned,
            started,
            guard,
            cleanup_js,
            content.chars().count(),
        )
        .await
    } else {
        let out = run_oneshot(&ws_url, &token, &payload_js, &model, started, content.chars().count()).await;
        let _ = tokio::time::timeout(Duration::from_secs(10), eval_on(&ws_url, &cleanup_js, 8_000)).await;
        out
    };
    // fetch 聚合成功且带 tools：对聚合结果做工具嗅探合成
    let result = match result {
        Ok(QwenWebOutput::Json(v)) if has_tools => {
            Ok(QwenWebOutput::Json(crate::modules::qoder::apply_tool_synthesis(v)))
        }
        other => other,
    };

    // 3) UI 真人模式降级：fetch 通道被风控"静默挑战"（零回复）时，改走页面原生
    //    交互——往输入框注入文本、点发送、从回复区抓全文。这是风控眼里的真人
    //    操作流（页面自己的登录态与请求管道），fetch 被挑战时通常仍可出话。
    match result {
        Ok(out) => {
            // 任一路径成功：清零失败计数（熔断器只在连续失败时触发）
            WEB_FAIL_STREAK.store(0, std::sync::atomic::Ordering::SeqCst);
            Ok(out)
        }
        Err(fetch_err) => {
            log_debug("网页版 fetch 通道失败，降级 UI 真人模式重试");
            match ui_chat_send(&content, &model, stream, has_tools).await {
                Ok(out) => Ok(out),
                Err(ui_err) => {
                    // 整条网页版链路（fetch + UI）都失败：累计失败计数并判定熔断
                    let streak = WEB_FAIL_STREAK.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    if streak >= WEB_CIRCUIT_THRESHOLD {
                        *WEB_CIRCUIT_UNTIL.lock().unwrap() =
                            Some(Instant::now() + Duration::from_secs(WEB_CIRCUIT_SECS));
                        WEB_FAIL_STREAK.store(0, std::sync::atomic::Ordering::SeqCst);
                        log_debug(&format!(
                            "网页版通道连续 {WEB_CIRCUIT_THRESHOLD} 次失败，熔断 {WEB_CIRCUIT_SECS}s"
                        ));
                    }
                    Err(format!("{fetch_err}；UI 真人模式亦失败：{ui_err}"))
                }
            }
        }
    }
}

/// UI 真人模式：页面原生交互发消息并抓取回复全文。
/// 步骤：新建对话 → 注入文本（React native setter + input 事件）→ 点击 send-button
/// → 轮询最后一条 assistant 消息的 phase-answer 正文直至稳定。
/// `has_tools` 时回复文本会经工具嗅探合成（可能产出带 tool_calls 的响应）。
async fn ui_chat_send(prompt: &str, model: &str, want_stream: bool, has_tools: bool) -> Result<QwenWebOutput, String> {
    let ws_url = ensure_page_ready().await?;

    // 1) 新建对话并**选中请求的模型**：导航到带 ?models= 参数的首页（网页版官方
    //    的模型指定方式，比点击模型下拉菜单稳定）。同时隔离每请求上下文。
    let nav_js = format!(
        "location.href = {}",
        js_string(&format!("https://chat.qwen.ai/?models={model}")),
    );
    let _ = eval_on(&ws_url, &nav_js, 10_000).await;
    // 等待页面回到根路径且加载完成（导航后 evaluate 上下文会重建，逐次重试）
    let mut page_ready = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let st = eval_on(
            &ws_url,
            "JSON.stringify({ p: location.pathname, r: document.readyState, m: (document.querySelector('.wms-trigger')||{innerText:''}).innerText })",
            8_000,
        )
        .await
        .ok()
        .and_then(|v| v.get("value").and_then(Value::as_str).map(String::from));
        if let Some(s) = st {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                let p = v.get("p").and_then(Value::as_str).unwrap_or("");
                let r = v.get("r").and_then(Value::as_str).unwrap_or("");
                let m = v.get("m").and_then(Value::as_str).unwrap_or("");
                if p == "/" && r == "complete" && !m.is_empty() {
                    log_debug(&format!("UI 页面就绪，当前模型: {m}"));
                    page_ready = true;
                    break;
                }
            }
        }
    }
    if !page_ready {
        log_debug("UI 页面导航后未确认就绪（继续尝试注入）");
    }

    // 2) 注入文本（React 受控组件：native setter + input 事件）
    let inject_js = format!(
        "(function(){{
        const ta = document.querySelector('textarea.message-input-textarea');
        if (!ta) return 'NO_TA';
        const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
        setter.call(ta, {prompt});
        ta.dispatchEvent(new Event('input', {{ bubbles: true }}));
        return 'OK len=' + ta.value.length;
    }})()",
        prompt = js_string(prompt),
    );
    let injected = eval_on(&ws_url, &inject_js, 10_000)
        .await
        .ok()
        .and_then(|v| v.get("value").and_then(Value::as_str).map(String::from))
        .unwrap_or_default();
    if !injected.starts_with("OK") {
        return Err(format!("UI 输入框注入失败: {injected}"));
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    // 2.5) 记录发送前的 assistant 消息数基线——回复必须是本次发送产生的新消息。
    // 🔴 防串台（2026-09-20 实测）：导航到 ?models= 页失败时页面停留在旧会话，
    // 轮询会误抓旧回复（上轮 T3a 的 "crimson, teal" 串到了 max 请求里）。
    let count_before: usize = eval_on(
        &ws_url,
        "String(document.querySelectorAll('.qwen-chat-message-assistant').length)",
        8_000,
    )
    .await
    .ok()
    .and_then(|v| {
        v.get("value")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<usize>().ok())
    })
    .unwrap_or(0);
    log_debug(&format!("UI 发送前 assistant 消息数基线: {count_before}"));

    // 3) 点击发送
    let click = "(function(){ const b = document.querySelector('button.send-button'); if (!b) return 'NO_BTN'; if (b.disabled) return 'DISABLED'; b.click(); return 'clicked'; })()";
    let clicked = eval_on(&ws_url, click, 8_000)
        .await
        .ok()
        .and_then(|v| v.get("value").and_then(Value::as_str).map(String::from))
        .unwrap_or_default();
    if clicked != "clicked" {
        return Err(format!("UI 发送按钮不可用: {clicked}"));
    }
    log_debug("UI 已点击发送，等待回复渲染…");

    // 4) 轮询抓取最后一条 assistant 消息的正文（phase-answer），文本连续 2 次稳定即完成
    let poll_js = "(function(){\
        const msgs = document.querySelectorAll('.qwen-chat-message-assistant');\
        const last = msgs[msgs.length - 1];\
        const body = last ? (last.querySelector('[class*=\"phase-answer\"]') || last.querySelector('.response-message-content')) : null;\
        const text = ((body && body.innerText) || '').trim();\
        const btn = document.querySelector('button.send-button');\
        const generating = !!btn && /stop|暂停/i.test(btn.getAttribute('aria-label') || '');\
        return JSON.stringify({ ready: !generating && text.length > 0, text, count: msgs.length });\
    })()";
    // UI 等待按模型与输入长度：max 生成/思考周期长；长上下文预填充也会
    // 显著推迟首 token（2026-09-20 验证 1M 时的定位），按 10 万字符 +60s 放宽。
    let ui_wait = {
        let (first_ms, _) = budgets_for(prompt.chars().count(), model);
        (if model.contains("max") { 120u64 } else { 90u64 }).max(first_ms / 1000).min(600)
    };
    let deadline = Instant::now() + Duration::from_secs(ui_wait);
    let mut last_text = String::new();
    let mut stable = 0;
    loop {
        if Instant::now() > deadline {
            return Err(format!(
                "UI 真人模式等待回复超时（{ui_wait}s，可能被风控拦截或网络异常）"
            ));
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        let snap = match eval_on(&ws_url, poll_js, 10_000).await {
            Ok(v) => v,
            Err(_) => continue, // 页面瞬断：继续轮询
        };
        let parsed: Value = serde_json::from_str(snap.get("value").and_then(Value::as_str).unwrap_or("{}"))
            .unwrap_or(Value::Null);
        let text = parsed.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let count = parsed.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
        let ready = parsed.get("ready").and_then(Value::as_bool).unwrap_or(false);
        // 回复必须是本次发送产生的新消息（assistant 消息数 > 发送前基线），
        // 否则视为串台（旧会话残留回复），继续等待导航/发送生效。
        let is_new_reply = count > count_before;
        if ready && !text.is_empty() && is_new_reply {
            if text == last_text {
                stable += 1;
                if stable >= 2 {
                    log_debug(&format!("UI 真人模式完成：{}字符", text.chars().count()));
                    return ui_output(text, model, want_stream, has_tools);
                }
            } else {
                stable = 0;
            }
            last_text = text;
        }
    }
}

/// UI 真人模式结果组装：纯文本 → 标准 OpenAI 形状；带 tools 时先经工具嗅探合成。
/// want_stream 时整段文本合成 SSE 字节流（等价一次整段输出）。
fn ui_output(
    text: String,
    model: &str,
    want_stream: bool,
    has_tools: bool,
) -> Result<QwenWebOutput, String> {
    let mut content = text;
    let mut tool_calls: Option<Vec<Value>> = None;
    let mut finish_reason = "stop";
    if has_tools {
        let agg = json!({
            "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]
        });
        let out = crate::modules::qoder::apply_tool_synthesis(agg);
        tool_calls = out
            .pointer("/choices/0/message/tool_calls")
            .and_then(Value::as_array)
            .cloned();
        content = out
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default();
        if tool_calls.is_some() {
            finish_reason = "tool_calls";
        }
    }
    let id = completion_id();
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if !want_stream {
        let mut message = json!({"role": "assistant", "content": content});
        if let Some(tc) = &tool_calls {
            message["tool_calls"] = json!(tc);
        }
        return Ok(QwenWebOutput::Json(json!({
            "id": id, "object": "chat.completion", "created": created, "model": model,
            "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
            // UI 真人模式拿不到上游 usage（页面渲染不含它），输出零值对象保证协议完整
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
        })));
    }
    // 整段文本合成 SSE 帧（帧串在 spawn 前生成，任务只负责发送）
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    let chunk_base = json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
    });
    let mut frames = String::new();
    let mut push_frame = |delta: Value, finish: Value| {
        let mut c = chunk_base.clone();
        c["choices"] = json!([{"index": 0, "delta": delta, "finish_reason": finish}]);
        frames.push_str(&format!("data: {}\n\n", c));
    };
    push_frame(json!({"role": "assistant", "content": ""}), Value::Null);
    if let Some(tcs) = &tool_calls {
        for (i, tc) in tcs.iter().enumerate() {
            push_frame(
                json!({
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
                Value::Null,
            );
        }
        push_frame(json!({}), json!(finish_reason));
    } else if !content.is_empty() {
        push_frame(json!({"content": content}), Value::Null);
        push_frame(json!({}), json!(finish_reason));
    } else {
        push_frame(json!({}), json!(finish_reason));
    }
    frames.push_str("data: [DONE]\n\n");
    tokio::spawn(async move {
        let _ = tx.send(Ok(Bytes::from(frames))).await;
        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
    });
    Ok(QwenWebOutput::Stream(rx))
}

/// 非流式：页面内一次性聚合全文。
async fn run_oneshot(
    ws_url: &str,
    token: &str,
    payload_js: &str,
    model: &str,
    started: Instant,
    content_chars: usize,
) -> Result<QwenWebOutput, String> {
    let js = format!(
        "(async () => {{ const t0 = Date.now(); const r = await fetch('/api/v2/chat/completions?chat_id=' + JSON.parse({payload}).chat_id, {{ method: 'POST', headers: {headers}, body: JSON.parse({payload}) }}); const text = await r.text(); return JSON.stringify({{ s: r.status, b: text.slice(0, 2097152) }}); }})()",
        payload = payload_js,
        headers = baxia_headers_js(token),
    );
    // 聊天单发预算：等整个上游响应（含长上下文预填充）。基值 45s / max 180s，
    // 再按输入长度放宽（budgets_for 的总预算同源）；惩罚期快速失败仍成立
    // ——预填充超时由 CDP evaluate timeout 兜底，不会无限挂住客户端。
    let (first_content_ms, total_budget_ms) = budgets_for(content_chars, model);
    let eval_budget = if model.contains("max") {
        180_000u64.max(total_budget_ms)
    } else {
        CHAT_EVAL_BUDGET_MS.max(first_content_ms).max(total_budget_ms / 2)
    };
    let val = eval_on(ws_url, &js, eval_budget)
        .await
        .map_err(|e| {
            log_debug(&format!("网页版非流式失败: {e}"));
            e
        })?;
    let wrapped: Value = serde_json::from_str(val.get("value").and_then(Value::as_str).unwrap_or(""))
        .map_err(|_| "chat/completions 响应解析失败".to_string())?;
    let text = wrapped.get("b").and_then(Value::as_str).unwrap_or("");
    let mut agg = SseAggregate::default();
    for line in text.lines() {
        feed_sse_line(&mut agg, line);
    }
    if agg.punished {
        log_debug("网页版非流式触发 RGV587 风控");
        return Err("网页版通道触发阿里风控（RGV587）".to_string());
    }
    if agg.answer.trim().is_empty() && agg.reasoning.trim().is_empty() {
        // 零回复一律按失败处理（含"正常结束但无内容"）——否则上层把空 JSON 当成功，
        // 客户端只会看到 "No response received from model"，也不会回落 Qoder 池。
        let msg = format!(
            "网页版无回复（HTTP {}，{}）：{}",
            wrapped.get("s").and_then(Value::as_u64).unwrap_or(0),
            if agg.finished { "正常结束但零内容" } else { "未等到 finished 帧" },
            {
                let head: String = text.chars().take(160).collect();
                head
            }
        );
        log_debug(&msg);
        return Err(msg);
    }
    log_debug(&format!(
        "网页版非流式完成：answer={}字符 elapsed={}ms",
        agg.answer.chars().count(),
        started.elapsed().as_millis()
    ));
    Ok(QwenWebOutput::Json(completion_json(&completion_id(), model, &agg)))
}

/// 流式：页面内后台 fetch 写 window.__qwenState，网关轮询增量并合成 OpenAI chunk。
/// 串行锁随任务持有到流结束；cleanup_js 在流结束后执行（删除临时会话）。
async fn run_stream(
    ws_url: String,
    token: String,
    payload_js: String,
    model: String,
    started: Instant,
    guard: tokio::sync::MutexGuard<'static, ()>,
    cleanup_js: String,
    content_chars: usize,
) -> Result<QwenWebOutput, String> {
    let boot_js = format!(
        "(async () => {{
            window.__qwenState = {{ buf: '', done: false, status: 0, error: '' }};
            const payload = JSON.parse({payload});
            const cid = payload.chat_id;
            const run = async () => {{
                try {{
                    const r = await fetch('/api/v2/chat/completions?chat_id=' + cid, {{ method: 'POST', headers: {headers}, body: payload }});
                    window.__qwenState.status = r.status;
                    const reader = r.body.getReader();
                    const dec = new TextDecoder();
                    while (true) {{
                        const {{ done, value }} = await reader.read();
                        if (done) break;
                        window.__qwenState.buf += dec.decode(value, {{ stream: true }});
                    }}
                }} catch (e) {{
                    window.__qwenState.error = String(e);
                }} finally {{
                    window.__qwenState.done = true;
                }}
            }};
            run();
            return 'started';
        }})()",
        payload = payload_js,
        headers = baxia_headers_js(&token),
    );
    let boot = eval_on(&ws_url, &boot_js, 15_000).await;
    match boot.as_ref().map(|v| v.get("value").and_then(Value::as_str)) {
        Ok(Some("started")) => {}
        other => {
            log_debug(&format!("网页版流任务启动失败: {:?}", other));
            return Err("网页版流任务启动失败".to_string());
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(64);
    // 首内容信号：任务在首个内容增量出现时发 Ok(())，零回复/异常发 Err(reason)——
    // run_stream 据此决定是否把流交给上层（零回复一律 Err → 上层回落 Qoder 池）。
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    // oneshot::send 消费所有权，而收尾路径可能多次触达——用 Option+take 让首次信号生效
    let outcome_slot = Some(outcome_tx);
    let id = completion_id();
    // 首内容门控按输入长度与模型档位自适应（长上下文预填充慢，见 budgets_for）
    let (first_content_wait_ms, total_budget_ms) = budgets_for(content_chars, &model);
    let first_content_wait = first_content_wait_ms / 1000;
    let task = tokio::spawn(async move {
        let mut outcome_slot = outcome_slot;
        let mut agg = SseAggregate::default();
        let mut remote_cursor: usize = 0;
        let mut pending = String::new();
        // 差量推送游标（本请求局部）。
        let mut sent_answer: usize = 0;
        let mut sent_reasoning: usize = 0;
        let first_token_deadline = Instant::now() + Duration::from_millis(first_content_wait_ms);
        let budget = Instant::now() + Duration::from_millis(total_budget_ms);
        let mut emitted_any = false;

        // 首帧：role
        let role_chunk = json!({
            "id": id, "object": "chat.completion.chunk", "created": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": Value::Null}],
        });
        let _ = tx.send(Ok(Bytes::from(format!("data: {role_chunk}\n\n")))).await;

        loop {
            if Instant::now() > budget {
                if !emitted_any {
                    if let Some(t) = outcome_slot.take() { let _ = t.send(Err("预算耗尽仍无内容".to_string())); }
                }
                let _ = tx.send(Err("网页版通道请求超时（预算耗尽）".to_string())).await;
                break;
            }
            let poll_js = format!(
                "(async () => JSON.stringify({{ buf: window.__qwenState ? window.__qwenState.buf.slice({cursor}) : '', done: window.__qwenState ? window.__qwenState.done : false, status: window.__qwenState ? window.__qwenState.status : 0, error: window.__qwenState ? window.__qwenState.error : '' }}))()",
                cursor = remote_cursor,
            );
            let poll = tokio::time::timeout(
                Duration::from_secs(30),
                eval_on(&ws_url, &poll_js, 12_000),
            )
            .await;
            match poll {
                Ok(Ok(v)) => {
                    let wrapped: Value = serde_json::from_str(
                        v.get("value").and_then(Value::as_str).unwrap_or(""),
                    )
                    .unwrap_or(Value::Null);
                    let chunk = wrapped.get("buf").and_then(Value::as_str).unwrap_or("");
                    let done = wrapped.get("done").and_then(Value::as_bool).unwrap_or(false);
                    let status = wrapped.get("status").and_then(Value::as_u64).unwrap_or(0);
                    let page_err = wrapped.get("error").and_then(Value::as_str).unwrap_or("");
                    remote_cursor += chunk.chars().count();

                    pending.push_str(chunk);
                    let mut lines: Vec<&str> = pending.split('\n').collect();
                    let tail = lines.pop().unwrap_or("").to_string();
                    for line in &lines {
                        feed_sse_line(&mut agg, line);
                        if agg.punished {
                            break;
                        }
                    }
                    pending = tail;

                    if agg.punished {
                        if !emitted_any {
                            if let Some(t) = outcome_slot.take() { let _ = t.send(Err("触发阿里风控（RGV587）".to_string())); }
                        }
                        let _ = tx.send(Err("网页版通道触发阿里风控（RGV587）".to_string())).await;
                        break;
                    }
                    if !page_err.is_empty() {
                        if !emitted_any {
                            if let Some(t) = outcome_slot.take() { let _ = t.send(Err(format!("页面 fetch 失败: {page_err}"))); }
                        }
                        let _ = tx.send(Err(format!("页面 fetch 失败: {page_err}"))).await;
                        break;
                    }
                    // emitted_any 仅在真正出现内容增量时置位（finished 不算）——
                    // 上游"正常结束但零回复"必须留给零回复检测，不能当首 token。
                    if !emitted_any && (!agg.answer.is_empty() || !agg.reasoning.is_empty()) {
                        emitted_any = true;
                        if let Some(t) = outcome_slot.take() { let _ = t.send(Ok(())); }
                        log_debug(&format!("网页版首 token 延迟 {}ms", started.elapsed().as_millis()));
                    }

                    // 增量差量推送（answer / reasoning 可能交错增长）。
                    if agg.reasoning.len() > sent_reasoning {
                        let delta = agg.reasoning[sent_reasoning..].to_string();
                        sent_reasoning = agg.reasoning.len();
                        let chunk = json!({
                            "id": id, "object": "chat.completion.chunk",
                            "created": std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                            "model": model,
                            "choices": [{"index": 0, "delta": {"reasoning_content": delta}, "finish_reason": Value::Null}],
                        });
                        if tx.send(Ok(Bytes::from(format!("data: {chunk}\n\n")))).await.is_err() {
                            break;
                        }
                    }
                    if agg.answer.len() > sent_answer {
                        let delta = agg.answer[sent_answer..].to_string();
                        sent_answer = agg.answer.len();
                        let chunk = json!({
                            "id": id, "object": "chat.completion.chunk",
                            "created": std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                            "model": model,
                            "choices": [{"index": 0, "delta": {"content": delta}, "finish_reason": Value::Null}],
                        });
                        if tx.send(Ok(Bytes::from(format!("data: {chunk}\n\n")))).await.is_err() {
                            break;
                        }
                    }

                    if (done && agg.finished) || (done && status >= 400) {
                        if status >= 400 {
                            if !emitted_any {
                                if let Some(t) = outcome_slot.take() { let _ = t.send(Err(format!("上游返回 HTTP {status}"))); }
                            }
                            let _ = tx.send(Err(format!("网页版上游返回 HTTP {status}"))).await;
                            break;
                        }
                        if !emitted_any {
                            // 上游"正常结束但零回复"：不发 finish/[DONE]，按失败处理触发回落
                            if let Some(t) = outcome_slot.take() { let _ = t.send(Err("上游正常结束但零回复".to_string())); }
                            log_debug("网页版流式：上游正常结束但零回复，按失败处理");
                            break;
                        }
                        let finish = json!({
                            "id": id, "object": "chat.completion.chunk",
                            "created": std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                            "model": model,
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                            "usage": usage_to_openai(agg.usage.as_ref()),
                        });
                        let _ = tx.send(Ok(Bytes::from(format!("data: {finish}\n\ndata: [DONE]\n\n")))).await;
                        log_debug(&format!(
                            "网页版流式完成：answer={}字符 elapsed={}ms",
                            agg.answer.chars().count(),
                            started.elapsed().as_millis()
                        ));
                        break;
                    }
                    if done {
                        // fetch 结束但没等到 finished 帧：补推 [DONE] 收尾。
                        if !emitted_any {
                            if let Some(t) = outcome_slot.take() { let _ = t.send(Err("fetch 结束仍零回复".to_string())); }
                            log_debug("网页版流式：fetch 结束仍零回复，按失败处理");
                            break;
                        }
                        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                        log_debug("网页版流式收尾（未显式 finished 帧）");
                        break;
                    }
                    if Instant::now() > first_token_deadline && !emitted_any {
                        if let Some(t) = outcome_slot.take() { let _ = t.send(Err("首 token 超时（可能被风控拦截或网络异常）".to_string())); }
                        let _ = tx.send(Err("网页版首 token 超时（可能被风控拦截或网络异常）".to_string())).await;
                        log_debug("网页版首 token 超时");
                        break;
                    }
                }
                Ok(Err(e)) => {
                    if !emitted_any {
                        if let Some(t) = outcome_slot.take() { let _ = t.send(Err(format!("轮询失败: {e}"))); }
                    }
                    let _ = tx.send(Err(format!("网页版轮询失败: {e}"))).await;
                    break;
                }
                Err(_) => {
                    if !emitted_any {
                        if let Some(t) = outcome_slot.take() { let _ = t.send(Err("轮询超时（页面可能已关闭）".to_string())); }
                    }
                    let _ = tx.send(Err("网页版轮询超时（页面可能已关闭）".to_string())).await;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
        // 流结束（含异常收尾）：删除临时会话，并释放页面级串行锁。
        let _ = tokio::time::timeout(Duration::from_secs(10), eval_on(&ws_url, &cleanup_js, 8_000)).await;
        drop(guard);
    });

    // 等首个内容增量再下发流：零回复/异常一律 Err，让上层回落 Qoder 池。
    // 上限 15s——客户端自己的超时通常更短，网页版被限流挂起时快速失败才有意义
    // （真在出内容的话 reasoning/answer 增量几秒内就会到，不会等到 15s）。
    // 🔴 超时/失败必须 abort 任务：任务持有页面级串行锁，不中止的话后续请求
    // 会在锁上排队干等它自己 90s 的 first-token deadline（实测放大到 51-94s）。
    match tokio::time::timeout(Duration::from_secs(first_content_wait), outcome_rx).await
    {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => {
            task.abort();
            return Err(format!("网页版通道零回复：{e}"));
        }
        Ok(Err(_)) => {
            task.abort();
            return Err("网页版通道流任务提前退出".to_string());
        }
        Err(_) => {
            task.abort();
            return Err("网页版通道等待首内容超时".to_string());
        }
    }
    Ok(QwenWebOutput::Stream(rx))
}

// ---------------------------------------------------------------------------
// 状态探测
// ---------------------------------------------------------------------------

/// 网页版通道健康状态（供 UI 与自动回落决策）。
/// 网页版通道真实探活缓存。
///
/// 页面加载正常 ≠ 能完成对话：baxia 惩罚期页面打得开（readyState=complete），
/// 但 chat/completions 被门控挂起——表层检查会给出乐观的 `available=true`
/// （2026-09-20 实测踩到）。因此探活必须走一次真实短调用，结果缓存 TTL 5 分钟
/// 并单飞执行，避免 UI 15s 轮询把上游打爆、也不该在惩罚期反复自伤。
static WEB_PROBE: Mutex<Option<WebProbe>> = Mutex::new(None);
static WEB_PROBE_RUNNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
const WEB_PROBE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
struct WebProbe {
    at: Instant,
    ok: bool,
    latency_ms: u128,
    channel: String,
    error: String,
}

/// 触发一次真实探活（超时 TTL 或 force 时），返回探活结果视图。
/// 探活在后台任务执行、单飞：本次调用只返回缓存快照，不阻塞 HTTP 响应。
pub fn trigger_web_channel_probe(force: bool) -> Value {
    let now = Instant::now();
    let (cached, stale) = {
        let g = WEB_PROBE.lock().unwrap();
        match g.as_ref() {
            Some(p) => {
                let age = now.duration_since(p.at);
                (Some(p.clone()), age >= WEB_PROBE_TTL)
            }
            None => (None, true),
        }
    };
    if force || stale {
        // 单飞：已有探活在跑就不重复触发。
        if !WEB_PROBE_RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tokio::spawn(async {
                let started = Instant::now();
                let outcome = chat_completions(json!({
                    "model": DEFAULT_MODEL,
                    "messages": [{"role": "user", "content": "回复两个字：在线"}],
                    "stream": false,
                }))
                .await;
                let (ok, channel, error) = match outcome {
                    Ok(_) => (true, "web".to_string(), String::new()),
                    Err(e) => (false, "web".to_string(), e),
                };
                let mut g = WEB_PROBE.lock().unwrap();
                *g = Some(WebProbe {
                    at: Instant::now(),
                    ok,
                    latency_ms: started.elapsed().as_millis(),
                    channel,
                    error,
                });
                WEB_PROBE_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
                drop(g);
                // 探活出结果后把档位同步到 opencode 模型显示名（降级可见）。
                sync_opencode_tier_label(tier_label_from_probe(ok));
            });
        }
    }
    let running = WEB_PROBE_RUNNING.load(std::sync::atomic::Ordering::SeqCst);
    match cached {
        Some(p) => json!({
            "probed": true,
            "probeOk": p.ok,
            "probeAtAgeSecs": now.duration_since(p.at).as_secs(),
            "probeLatencyMs": p.latency_ms,
            "probeChannel": p.channel,
            "probeError": if p.error.is_empty() { Value::Null } else { json!(p.error) },
            "probeInFlight": running,
            "probeTtlSecs": WEB_PROBE_TTL.as_secs(),
        }),
        None => json!({
            "probed": false,
            "probeOk": Value::Null,
            "probeInFlight": running,
            "probeTtlSecs": WEB_PROBE_TTL.as_secs(),
            "note": "尚无探活结果：首次调用本端点后自动触发，约数十秒后刷新可见",
        }),
    }
}

pub async fn web_channel_status() -> Value {
    let token_ready = extract_desktop_token().is_some();
    let browser_running = cdp_alive(CDP_PORT).await;
    let (page_ready, page_latency_ms) = if browser_running {
        let t0 = Instant::now();
        match ensure_page_ws_url().await {
            Ok(ws) => match eval_on(
                &ws,
                "JSON.stringify({href: location.href, ready: document.readyState})",
                8000,
            )
            .await
            {
                Ok(v) => {
                    let s = v.get("value").and_then(Value::as_str).unwrap_or("");
                    let ok = s.contains("chat.qwen.ai")
                        && s.contains("complete")
                        && !s.contains("_____tmd_____")
                        && !s.contains("punish");
                    (ok, Some(t0.elapsed().as_millis() as u64))
                }
                Err(_) => (false, Some(t0.elapsed().as_millis() as u64)),
            },
            Err(_) => (false, Some(t0.elapsed().as_millis() as u64)),
        }
    } else {
        (false, None)
    };
    let models = crate::modules::qwen::fetch_desktop_models().await;
    json!({
        "available": token_ready && page_ready,
        "tokenReady": token_ready,
        "browserRunning": browser_running,
        "browserPort": CDP_PORT,
        "pageReady": page_ready,
        "pageLatencyMs": page_latency_ms,
        "modelsCount": models.len(),
        "upstream": QWEN_WEB_URL.trim_end_matches('/'),
        "version": VERSION,
    })
}

/// JWT payload 里的用户 id（状态展示用）。
pub fn token_user_hint() -> Option<String> {
    let token = extract_desktop_token()?;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let mut payload_b64 = parts[1].replace('-', "+").replace('_', "/");
    while payload_b64.len() % 4 != 0 {
        payload_b64.push('=');
    }
    let decoded = general_purpose::STANDARD.decode(payload_b64).ok()?;
    let v: Value = serde_json::from_slice(&decoded).ok()?;
    v.get("id").and_then(Value::as_str).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_scale_with_content_length() {
        // 短输入保持原基值（不变更现有行为）
        let (first, total) = budgets_for(2_000, "qwen3.7-plus");
        assert_eq!(first, 15_000);
        assert_eq!(total, 300_000);
        // max 档首内容基值放宽
        let (first_max, total_max) = budgets_for(2_000, "qwen3.8-max");
        assert_eq!(first_max, 60_000);
        assert_eq!(total_max, 420_000);
        // 每 10 万字符 +60s 首内容 / +120s 总预算
        let (first_100k, total_100k) = budgets_for(100_000, "qwen3.7-plus");
        assert_eq!(first_100k, 75_000);
        assert_eq!(total_100k, 420_000);
        // 1M 字符：首内容 615s→封顶 600s；总预算 300s+10x120s=1500s（未触上限）
        let (first_1m, total_1m) = budgets_for(1_000_000, "qwen3.7-plus");
        assert_eq!(first_1m, 600_000);
        assert_eq!(total_1m, 1_500_000);
        // 1.5M 字符 max：总预算 300+120+15x120=2220s→封顶 1800s
        let (_, total_max_1_5m) = budgets_for(1_500_000, "qwen3.8-max");
        assert_eq!(total_max_1_5m, 1_800_000);
        // 3M 字符仍封顶（不无限放宽）
        let (first_3m, total_3m) = budgets_for(3_000_000, "qwen3.8-max");
        assert_eq!(first_3m, 600_000);
        assert_eq!(total_3m, 1_800_000);
        // 单调不减：更长输入不会拿到更短预算
        let mut prev = 0;
        for n in [1_000, 50_000, 100_000, 500_000, 1_000_000] {
            let (f, t) = budgets_for(n, "qwen3.7-plus");
            assert!(f >= prev);
            assert!(t >= 300_000);
            prev = f;
        }
    }
}

#[cfg(test)]
mod tier_label_tests {
    use super::*;
    use serde_json::json;

    /// 与 sync_opencode_tier_label 同源的显示名推导（单测专用镜像，避免碰用户真实配置）。
    fn display_name(model_id: &str, label: QwenTierLabel) -> String {
        let base = model_id.to_ascii_uppercase().replace(['.', '-'], "");
        let pretty = match base.as_str() {
            "QWEN38MAX" => "Qwen3.8-Max",
            "QWEN37PLUS" => "Qwen3.7-Plus",
            "QWEN38OMNIFLASH" => "Qwen3.8-Omni-Flash",
            _ => model_id,
        };
        let suffix = match label {
            QwenTierLabel::Web => "网页版",
            QwenTierLabel::Degraded => "⚠lite降级",
        };
        format!("{pretty} (Sylvia·{suffix})")
    }

    #[test]
    fn tier_label_names_are_idempotent_and_informative() {
        // 同一档位重复推导必须得到同一名字（同步函数依赖这一点保证幂等）
        let a = display_name("qwen3.8-max", QwenTierLabel::Degraded);
        let b = display_name("qwen3.8-max", QwenTierLabel::Degraded);
        assert_eq!(a, b);
        assert_eq!(a, "Qwen3.8-Max (Sylvia·⚠lite降级)");
        // 切回原档时名字必须能还原（不会叠加后缀）
        assert_eq!(
            display_name("qwen3.8-max", QwenTierLabel::Web),
            "Qwen3.8-Max (Sylvia·网页版)"
        );
        // 未知模型 ID 原样透出，不臆造
        assert_eq!(
            display_name("qwen9.9-ultra", QwenTierLabel::Web),
            "qwen9.9-ultra (Sylvia·网页版)"
        );
    }

    #[test]
    fn tier_label_sync_preserves_other_config_fields() {
        // 模拟用户配置：我们的 provider + 其他内容必须原样保留
        let mut cfg = json!({
            "$schema": "https://opencode.ai/config.json",
            "mcp": {"codegraph": {"type": "local", "enabled": true}},
            "provider": {
                "other": {"npm": "@ai-sdk/openai-compatible", "models": {"x": {"name": "X"}}},
                "sylvia-qwen": {
                    "npm": "@ai-sdk/openai-compatible",
                    "options": {"baseURL": "http://127.0.0.1:57890/qwen/v1", "apiKey": "k"},
                    "models": {
                        "qwen3.8-max": {"name": "old", "limit": {"context": 1048576, "output": 32768}, "tool_call": true}
                    }
                }
            }
        });
        let before_mcp = cfg.get("mcp").cloned();
        let before_other = cfg.pointer("/provider/other").cloned();
        let before_limit = cfg.pointer("/provider/sylvia-qwen/models/qwen3.8-max/limit").cloned();
        let before_base = cfg.pointer("/provider/sylvia-qwen/options/baseURL").cloned();

        // 只改 name 字段
        let models = cfg
            .pointer_mut("/provider/sylvia-qwen/models")
            .and_then(Value::as_object_mut)
            .unwrap();
        for (id, m) in models.iter_mut() {
            if let Some(obj) = m.as_object_mut() {
                obj.insert("name".to_string(), json!(display_name(id, QwenTierLabel::Degraded)));
            }
        }

        assert_eq!(cfg.get("mcp"), before_mcp.as_ref(), "mcp 必须原样保留");
        assert_eq!(cfg.pointer("/provider/other"), before_other.as_ref(), "其他 provider 必须保留");
        assert_eq!(
            cfg.pointer("/provider/sylvia-qwen/models/qwen3.8-max/limit"),
            before_limit.as_ref(),
            "模型 limit 必须保留"
        );
        assert_eq!(
            cfg.pointer("/provider/sylvia-qwen/options/baseURL"),
            before_base.as_ref(),
            "baseURL 必须保留"
        );
        assert_eq!(
            cfg.pointer("/provider/sylvia-qwen/models/qwen3.8-max/name").and_then(Value::as_str),
            Some("Qwen3.8-Max (Sylvia·⚠lite降级)")
        );
    }
}

#[cfg(test)]
mod workbuddy_models_tests {
    use super::*;

    /// 与 sync_workbuddy_models_tier_label 同源的条目筛选+改名逻辑（单测镜像，
    /// 用临时 Value 而非用户真实文件）。
    fn apply(list: &mut Value, suffix: &str) -> usize {
        let Some(entries) = list.as_array_mut() else {
            return 0;
        };
        let mut changed = 0usize;
        for entry in entries.iter_mut() {
            let Some(obj) = entry.as_object_mut() else {
                continue;
            };
            let url = obj.get("url").and_then(Value::as_str).unwrap_or("");
            if url.trim_end_matches('/') != QWEN_CHANNEL_BASE_URL {
                continue;
            }
            let Some(id) = obj.get("id").and_then(Value::as_str).map(String::from) else {
                continue;
            };
            let new_name = format!("{} (Sylvia·{})", pretty_model_name(&id), suffix);
            if obj.get("name").and_then(Value::as_str) != Some(new_name.as_str()) {
                obj.insert("name".to_string(), json!(new_name));
                changed += 1;
            }
        }
        changed
    }

    #[test]
    fn workbuddy_models_sync_targets_only_channel_entries() {
        let mut list = json!([
            // 我们的通道条目（应改）
            {"id": "qwen3.8-max", "name": "old", "url": "http://127.0.0.1:57890/qwen/v1",
             "maxInputTokens": 1048576, "supportsToolCall": true},
            {"id": "qwen3.7-plus", "name": "old", "url": "http://127.0.0.1:57890/qwen/v1/"},
            // 固定 lite 档（不应改：无降级语义）
            {"id": "qwen3.8-flash", "name": "qoder中转(57890)·日常编码最快",
             "url": "http://127.0.0.1:57890/qwen-flash/v1"},
            // 并行会话的临时条目（不应改）
            {"id": "qwen3.8-flash-temp", "name": "暂时的Qwen3.8-Flash模型",
             "url": "http://127.0.0.1:57890/qwen-flash/v1"},
            // 其他厂商（不应改）
            {"id": "glm-5.3", "name": "本地 GLM-5.3", "url": "http://127.0.0.1:57890/v1",
             "vendor": "ainb"},
            {"id": "step-5-preview", "name": "Step 5 Preview", "url": "https://api.stepfun.com/step_plan/v1"},
        ]);
        let before_glm = list[4].clone();
        let before_flash = list[2].clone();
        let before_temp = list[3].clone();

        let changed = apply(&mut list, "⚠lite降级");
        assert_eq!(changed, 2, "只应改 2 个通道条目");
        assert_eq!(list[0]["name"], "Qwen3.8-Max (Sylvia·⚠lite降级)");
        assert_eq!(list[1]["name"], "Qwen3.7-Plus (Sylvia·⚠lite降级)");
        // 其余条目一个字节都不能动
        assert_eq!(list[2], before_flash, "qwen-flash 条目必须原样保留");
        assert_eq!(list[3], before_temp, "临时条目必须原样保留");
        assert_eq!(list[4], before_glm, "glm 条目必须原样保留");

        // 幂等：再跑一次 changed=0
        assert_eq!(apply(&mut list, "⚠lite降级"), 0);
        // 切回原档可还原
        assert_eq!(apply(&mut list, "网页版"), 2);
        assert_eq!(list[0]["name"], "Qwen3.8-Max (Sylvia·网页版)");
        // 能力字段不被触碰
        assert_eq!(list[0]["maxInputTokens"], json!(1048576));
        assert_eq!(list[0]["supportsToolCall"], json!(true));
    }
}
