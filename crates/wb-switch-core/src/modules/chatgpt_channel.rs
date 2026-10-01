//! ChatGPT 通道：gpt-reg-review 工具链（批量注册 WebUI）托管与控制。
//!
//! 契约：docs/chatgpt-channel-contract.md §2/§3/§4/§5。v1 范围 = 服务托管
//! （WebUI 进程生命周期）+ 注册机控制（批量启动/暂停/停止/状态，均透传工具链
//! `/api/auto/*`）+ 账号表（`/api/registered` 透传 + 本地池镜像）。
//!
//! 零自动触发纪律：后台 watchdog 只负责把 WebUI **服务进程**拉起来（托管），
//! 绝不调用 `/api/auto/start`——批量注册只能由用户在通道页手动启动。
//!
//! 关键实现（契约 §3）：
//! - 探活与请求用 no_proxy reqwest 客户端（127.0.0.1 防 Clash 劫持，
//!   `grok_sso.rs` `cdp_http_client()` 先例）；
//! - spawn 用 `std::process::Command` + `DETACHED_PROCESS | CREATE_NO_WINDOW`
//!   + WorkingDir=project_dir，**必带 --no-browser**（start_webui.py 默认会
//!   webbrowser.open 弹浏览器）；
//! - stop 按命令行含 `start_webui.py` 匹配后逐个 Stop-Process（🔴 严禁按映像名
//!   杀 python；且先 auto_stop——WebUI 重启会 interrupt_running_runs + 释放
//!   in_use 占号，批量注册进行中不得停 WebUI）；
//! - 本地池镜像 `~/.wb-switch/chatgpt_accounts.json` 以 /api/registered（已注册
//!   ChatGPT 账号）为源，token 只回长度（at_len/st_len/rt_len），按 email 去重。

use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::modules::config::{atomic_write, now_ms, store_dir};

/// project_dir 兜底（契约 §0 尽调：工具仓库已克隆到工作区 gateways/ 下，实测存在）。
const DEFAULT_PROJECT_DIR: &str = "E:/WorkBuddy自动签到助手/gateways/gpt-reg-review";

/// WebUI 健康检查 GET /api/health 的超时（契约 §3：2s）。
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// ensure_webui 拉起后等待健康的上限（契约 §3：≤20s）。
const ENSURE_WAIT: Duration = Duration::from_secs(20);
/// 托管循环的轮询间隔（契约 §3：60s）。
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// 配置（契约 §2）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct ChatGptChannelConfig {
    /// 通道开关（false：watchdog 空转一次即退出，页面功能不再探活）。
    pub enabled: bool,
    /// 工具链 WebUI 端口（默认 8765）。
    pub webui_port: u16,
    /// gpt-reg-review 仓库目录（start_webui.py 所在）。
    pub project_dir: String,
    /// Python 解释器（默认 "python"，本机 3.13）。
    pub python_path: String,
    /// 托管开关：true 时 watchdog 自动拉起 WebUI **服务进程**（不是注册循环）。
    pub auto_start: bool,
}

impl Default for ChatGptChannelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            webui_port: 8765,
            project_dir: DEFAULT_PROJECT_DIR.to_string(),
            python_path: "python".to_string(),
            auto_start: true,
        }
    }
}

fn config_path() -> PathBuf {
    store_dir().join("chatgpt_channel.json")
}

/// 配置解析纯函数：`None` = 缺文件 → 全默认；`Some(partial)` = 缺字段逐项补默认。
/// 与 `load_config` 分离以便单测（契约 §3：读配置，缺字段/缺文件用默认）。
fn parse_config(v: Option<&Value>) -> ChatGptChannelConfig {
    let d = ChatGptChannelConfig::default();
    let Some(v) = v else { return d };
    ChatGptChannelConfig {
        enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(d.enabled),
        webui_port: v
            .get("webui_port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(d.webui_port),
        project_dir: v
            .get("project_dir")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&d.project_dir)
            .to_string(),
        python_path: v
            .get("python_path")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&d.python_path)
            .to_string(),
        auto_start: v
            .get("auto_start")
            .and_then(Value::as_bool)
            .unwrap_or(d.auto_start),
    }
}

pub fn load_config() -> ChatGptChannelConfig {
    let v = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    parse_config(v.as_ref())
}

pub fn save_config(cfg: &ChatGptChannelConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "enabled": cfg.enabled,
        "webui_port": cfg.webui_port,
        "project_dir": cfg.project_dir,
        "python_path": cfg.python_path,
        "auto_start": cfg.auto_start,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// HTTP：no_proxy 客户端 + 透传助手
// ---------------------------------------------------------------------------

fn webui_base(cfg: &ChatGptChannelConfig) -> String {
    format!("http://127.0.0.1:{}", cfg.webui_port)
}

/// 直连本机 WebUI 的 client：强制绕过系统代理（reqwest 默认读 Windows IE 代理
/// 设置，127.0.0.1 请求被送进 Clash 后拿不到响应；grok_sso.rs:112-121 先例）。
fn webui_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build chatgpt webui http client")
    })
}

/// WebUI 探活：GET /api/health（2s 超时）。
pub async fn webui_health() -> bool {
    let cfg = load_config();
    let url = format!("{}/api/health", webui_base(&cfg));
    match webui_client()
        .get(&url)
        .timeout(HEALTH_TIMEOUT)
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

async fn http_get_json(url: &str) -> Result<Value, String> {
    let resp = webui_client()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("请求 WebUI 失败: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取 WebUI 响应失败: {e}"))?;
    if !status.is_success() {
        return Err(format!("WebUI HTTP {status}: {}", truncate(&text, 200)));
    }
    serde_json::from_str(&text).map_err(|e| format!("WebUI 响应非 JSON: {e}"))
}

async fn http_post_json(url: &str, body: Option<&Value>) -> Result<Value, String> {
    let mut req = webui_client().post(url);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求 WebUI 失败: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取 WebUI 响应失败: {e}"))?;
    if !status.is_success() {
        return Err(format!("WebUI HTTP {status}: {}", truncate(&text, 200)));
    }
    serde_json::from_str(&text).map_err(|e| format!("WebUI 响应非 JSON: {e}"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

// ---------------------------------------------------------------------------
// WebUI 进程托管（契约 §3：ensure / stop）
// ---------------------------------------------------------------------------

/// ensure 互斥锁：watchdog 与手动「启动 WebUI」并发时只拉起一个进程。
/// （等健康最长 20s，必须跨 await 持锁 → 用 tokio 的异步互斥锁。）
static ENSURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 确保 WebUI 在跑：已活直接返回端口；否则 spawn `python start_webui.py
/// --port {port} --no-browser`（DETACHED + CREATE_NO_WINDOW，WorkingDir=
/// project_dir）后等健康 ≤20s。
pub async fn ensure_webui(cfg: &ChatGptChannelConfig) -> Result<u16, String> {
    let _guard = ENSURE_LOCK.lock().await;
    if webui_health().await {
        return Ok(cfg.webui_port);
    }
    let dir = Path::new(&cfg.project_dir);
    if !dir.is_dir() {
        return Err(format!(
            "project_dir 不存在：{}（请修正 ~/.wb-switch/chatgpt_channel.json 的 project_dir）",
            cfg.project_dir
        ));
    }
    let mut cmd = std::process::Command::new(&cfg.python_path);
    cmd.args([
        "start_webui.py",
        "--port",
        &cfg.webui_port.to_string(),
        // 🔴 必带：start_webui.py 默认 webbrowser.open，托管拉起弹浏览器骚扰用户。
        "--no-browser",
    ])
    .current_dir(dir)
    // 分离进程不接管道：避免子进程写满 stdout/stderr 管道缓冲而阻塞。
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS(0x8) + CREATE_NO_WINDOW(0x08000000)：脱离父进程生命周期
        // 且不闪控制台窗口（grok_sso.rs:192 / grok_register.rs:680 两个先例的合并）。
        cmd.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    cmd.spawn()
        .map_err(|e| format!("拉起 WebUI 失败（python={}）: {e}", cfg.python_path))?;
    let deadline = Instant::now() + ENSURE_WAIT;
    while Instant::now() < deadline {
        if webui_health().await {
            return Ok(cfg.webui_port);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("WebUI 进程已拉起但 20s 内 /api/health 未就绪（检查 python 依赖是否装全）".to_string())
}

/// 只结束「命令行含 start_webui.py」的进程，返回是否达成停止。
///
/// 🔴 严禁按映像名杀 python —— 那会连用户其它 Python 进程一起杀掉。必须按
/// CommandLine 匹配（Get-CimInstance Win32_Process + Where-Object CommandLine，
/// grok_register.rs:672-686 先例）。先取出 ProcessId 再逐个 `Stop-Process -Id`
/// （不要管道直连 Stop-Process，Win32_Process 属性绑定会报错）。
///
/// ⚠️ WebUI 重启会 release_stale_in_use(0) + interrupt_running_runs()——因此
/// 这里先 auto_stop 停掉注册循环（尽力而为），批量进行中由前端避免调用本函数。
pub async fn stop_webui(cfg: &ChatGptChannelConfig) -> bool {
    let _ = auto_stop(cfg).await; // 尽力而为：循环没在跑时工具链返回 400，忽略
    kill_webui_processes()
}

#[cfg(target_os = "windows")]
fn kill_webui_processes() -> bool {
    let script = "$procs = Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like '*start_webui.py*' }; \
                  $killed = 0; $failed = 0; \
                  foreach ($p in $procs) { try { Stop-Process -Id $p.ProcessId -Force -ErrorAction Stop; $killed++ } catch { $failed++ } }; \
                  Write-Output \"$($procs.Count) $($killed) $($failed)\"";
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-Command", script]);
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：不闪控制台窗口
    }
    let matched_killed_failed = cmd
        .output()
        .ok()
        .map(|out| {
            // 解析 "matched killed failed" 三元组；解析失败按 (0,0,0) 处理（幂等无害）
            let text = String::from_utf8_lossy(&out.stdout);
            let nums: Vec<u64> = text
                .split_whitespace()
                .filter_map(|t| t.parse::<u64>().ok())
                .collect();
            (
                nums.first().copied().unwrap_or(0),
                nums.get(1).copied().unwrap_or(0),
                nums.get(2).copied().unwrap_or(0),
            )
        })
        .unwrap_or((0, 0, 0));
    let (matched, killed, failed) = matched_killed_failed;
    // 0 个匹配 = 本来就没在跑，视为停止成功（幂等）；有匹配但没杀干净 = 失败。
    matched == 0 || (failed == 0 && killed >= matched)
}

#[cfg(not(target_os = "windows"))]
fn kill_webui_processes() -> bool {
    // 非 Windows 兜底：pkill -f 按完整命令行匹配，同样不按映像名。
    std::process::Command::new("pkill")
        .args(["-f", "start_webui.py"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 注册机控制 + 账号表透传（契约 §3 / §4）
// ---------------------------------------------------------------------------

/// 批量注册启动：body（AutoLoopStartReq）透传 /api/auto/start。
/// 注意 target_count=0 = 不限量，调用方（通道页）必须显式传 ≥1。
pub async fn auto_start(cfg: &ChatGptChannelConfig, body: Value) -> Result<Value, String> {
    http_post_json(&format!("{}/api/auto/start", webui_base(cfg)), Some(&body)).await
}

/// 批量注册状态：GET /api/auto/status。WebUI 不可达时返回
/// {ok:false, error}（不 Err——状态轮询需要稳定形状）。
pub async fn auto_status(cfg: &ChatGptChannelConfig) -> Value {
    match http_get_json(&format!("{}/api/auto/status", webui_base(cfg))).await {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "state": "unknown", "error": e }),
    }
}

/// 批量注册暂停：POST /api/auto/pause。
pub async fn auto_pause(cfg: &ChatGptChannelConfig) -> Result<Value, String> {
    http_post_json(&format!("{}/api/auto/pause", webui_base(cfg)), None).await
}

/// 批量注册恢复：POST /api/auto/resume。
pub async fn auto_resume(cfg: &ChatGptChannelConfig) -> Result<Value, String> {
    http_post_json(&format!("{}/api/auto/resume", webui_base(cfg)), None).await
}

/// 批量注册停止：POST /api/auto/stop。
pub async fn auto_stop(cfg: &ChatGptChannelConfig) -> Result<Value, String> {
    http_post_json(&format!("{}/api/auto/stop", webui_base(cfg)), None).await
}

/// 已注册 ChatGPT 账号表：GET /api/registered 透传（{ok, items, total}；
/// token 只回 at_len/st_len/rt_len 长度）。
pub async fn registered(cfg: &ChatGptChannelConfig) -> Value {
    match http_get_json(&format!("{}/api/registered", webui_base(cfg))).await {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "items": [], "total": 0, "error": e }),
    }
}

/// 邮箱号池（outlook_accounts，**不是** ChatGPT 账号，勿混用）：GET /api/accounts
/// 透传（{ok, items, total, by_kind}）。
pub async fn accounts(cfg: &ChatGptChannelConfig) -> Value {
    match http_get_json(&format!("{}/api/accounts", webui_base(cfg))).await {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "items": [], "total": 0, "error": e }),
    }
}

/// 池镜像落盘路径：~/.wb-switch/chatgpt_accounts.json。
pub fn pool_mirror_path() -> PathBuf {
    store_dir().join("chatgpt_accounts.json")
}

/// 镜像映射纯函数（契约 §3④）：email / hasAccessToken=at_len>0（registered 只回
/// token 长度不回原文）/ addedAt=created_at；按 email 去重（首现保留——源按
/// created_at DESC 排序，首现即最新）。
pub(crate) fn mirror_registered_items(items: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(items.len());
    let mut seen: HashSet<String> = HashSet::new();
    for it in items {
        let email = it
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if email.is_empty() || !seen.insert(email.clone()) {
            continue;
        }
        out.push(json!({
            "email": email,
            "hasAccessToken": it.get("at_len").and_then(Value::as_i64).unwrap_or(0) > 0,
            "addedAt": it.get("created_at").cloned().unwrap_or(Value::Null),
        }));
    }
    out
}

/// 从 /api/registered 透传 JSON 拉取 items 并镜像落盘。
pub async fn sync_pool(cfg: &ChatGptChannelConfig) -> Value {
    if !webui_health().await {
        return json!({ "ok": false, "error": "WebUI 未运行，无法同步账号池" });
    }
    let url = format!("{}/api/registered?limit=10000&offset=0", webui_base(cfg));
    let reg = match http_get_json(&url).await {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let items = reg
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = reg.get("total").and_then(Value::as_i64).unwrap_or(0);
    let mirrored = mirror_registered_items(&items);
    let path = pool_mirror_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return json!({ "ok": false, "error": format!("创建目录失败: {e}") });
        }
    }
    let content = match serde_json::to_string_pretty(&json!({
        "syncedAt": now_ms(),
        "count": mirrored.len(),
        "items": mirrored,
    })) {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": format!("序列化失败: {e}") }),
    };
    match atomic_write(&path, &content) {
        Ok(_) => json!({
            "ok": true,
            "mirrored": mirrored.len(),
            "total": total,
            "path": path.display().to_string(),
        }),
        Err(e) => json!({ "ok": false, "error": format!("写入镜像失败: {e}") }),
    }
}

// ---------------------------------------------------------------------------
// 状态聚合（契约 §3：status_value）
// ---------------------------------------------------------------------------

/// 从 /api/auto/status 透传 JSON 提取 state（"running"/"paused"/"stopped"；
/// 字段缺失时 "unknown"——透传保真，不为未知枚举造词）。
pub(crate) fn registrar_state(status: &Value) -> String {
    status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

/// 从 /api/accounts 透传 JSON 汇总 available 号数（by_kind[*].available 之和；
/// by_kind 形状见工具链 db.py stats_by_kind）。
pub(crate) fn pool_available_count(accounts_json: &Value) -> u64 {
    accounts_json
        .get("by_kind")
        .and_then(Value::as_object)
        .map(|kinds| {
            kinds
                .values()
                .filter_map(|k| k.get("available").and_then(Value::as_u64))
                .sum()
        })
        .unwrap_or(0)
}

/// 通道状态聚合：契约 §3 形状 + projectDirExists/hint（§2「找不到时 status 给出
/// 明确指引」）。accountCount=/api/registered total；poolCount=/api/accounts 的
/// available 数。
pub async fn status_value() -> Value {
    let cfg = load_config();
    let dir_exists = Path::new(&cfg.project_dir).is_dir();
    let running = cfg.enabled && webui_health().await;
    let (registrar, account_count, pool_count) = if running {
        let cfg_ref = &cfg;
        let reg = registered(cfg_ref).await;
        let acc = accounts(cfg_ref).await;
        (
            auto_status(cfg_ref).await,
            reg.get("total").and_then(Value::as_i64).unwrap_or(0),
            pool_available_count(&acc),
        )
    } else {
        (
            json!({ "ok": false, "state": "unknown", "error": "WebUI 未运行" }),
            0,
            0,
        )
    };
    let mut hint = String::new();
    if !cfg.enabled {
        hint = "通道已禁用（~/.wb-switch/chatgpt_channel.json enabled=false）".to_string();
    } else if !dir_exists {
        hint = format!(
            "project_dir 不存在：{}（请修正配置或克隆 gpt-reg-review 到该路径）",
            cfg.project_dir
        );
    } else if !running {
        hint = format!(
            "WebUI 未运行：点「启动 WebUI」或访问 {} 手动启动",
            webui_base(&cfg)
        );
    }
    json!({
        "enabled": cfg.enabled,
        "autoStart": cfg.auto_start,
        "webuiRunning": running,
        "webuiPort": cfg.webui_port,
        "webuiBase": webui_base(&cfg),
        "projectDir": cfg.project_dir,
        "projectDirExists": dir_exists,
        "registrar": registrar,
        "registrarState": registrar_state(&registrar),
        "accountCount": account_count,
        "poolCount": pool_count,
        "hint": hint,
    })
}

// ---------------------------------------------------------------------------
// 托管循环（契约 §3 / §5）
// ---------------------------------------------------------------------------

/// 后台 60s 循环：enabled + auto_start 且不活 → ensure（复用 ensure_webui 的
/// 互斥锁防并发拉起）。禁用时空转一次即退出（重新启用需重启进程生效）。
///
/// 🔴 零自动触发纪律：这里只拉 WebUI **服务进程**，绝不调 /api/auto/start。
pub fn spawn_watchdog() {
    tokio::spawn(async move {
        loop {
            let cfg = load_config();
            if !cfg.enabled {
                return; // 禁用：空转一次即退出
            }
            if cfg.auto_start && !webui_health().await {
                let _ = ensure_webui(&cfg).await; // 失败静默：下一轮再试，不打日志刷屏
            }
            tokio::time::sleep(WATCHDOG_INTERVAL).await;
        }
    });
}

// ---------------------------------------------------------------------------
// 测试（任务点 5：配置解析 / 透传解析纯函数 / 池镜像去重）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // 配置解析（缺文件 / 缺字段 / 全字段）
    // ---------------------------------------------------------------------

    #[test]
    fn parse_config_missing_file_uses_defaults() {
        // None = 缺文件：全默认（契约 §2 示例值）。
        let cfg = parse_config(None);
        assert!(cfg.enabled);
        assert_eq!(cfg.webui_port, 8765);
        assert_eq!(cfg.project_dir, DEFAULT_PROJECT_DIR);
        assert_eq!(cfg.python_path, "python");
        assert!(cfg.auto_start);
    }

    #[test]
    fn parse_config_partial_fields_backfill_defaults() {
        // 缺字段逐项补默认；显式字段生效。
        let v = serde_json::from_str::<Value>(r#"{"enabled": false, "webui_port": 9000}"#).unwrap();
        let cfg = parse_config(Some(&v));
        assert!(!cfg.enabled);
        assert_eq!(cfg.webui_port, 9000);
        assert_eq!(cfg.project_dir, DEFAULT_PROJECT_DIR);
        assert_eq!(cfg.python_path, "python");
        assert!(cfg.auto_start);
    }

    #[test]
    fn parse_config_full_fields_roundtrip() {
        let v = serde_json::from_str::<Value>(
            r#"{"enabled": false, "webui_port": 1234, "project_dir": "D:/reg",
                "python_path": "python3.13", "auto_start": false}"#,
        )
        .unwrap();
        let cfg = parse_config(Some(&v));
        assert_eq!(
            cfg,
            ChatGptChannelConfig {
                enabled: false,
                webui_port: 1234,
                project_dir: "D:/reg".to_string(),
                python_path: "python3.13".to_string(),
                auto_start: false,
            }
        );
    }

    #[test]
    fn parse_config_ignores_invalid_types_and_empty_strings() {
        // 类型不对（webui_port 字符串/越界、enabled 非布尔）与空串回退默认。
        let v = serde_json::from_str::<Value>(
            r#"{"enabled": "yes", "webui_port": 99999, "project_dir": "  ",
                "python_path": "", "auto_start": 1}"#,
        )
        .unwrap();
        let cfg = parse_config(Some(&v));
        assert_eq!(cfg, ChatGptChannelConfig::default());
    }

    // ---------------------------------------------------------------------
    // 透传解析纯函数：registrar state / pool available 计数
    // ---------------------------------------------------------------------

    #[test]
    fn registrar_state_extracts_running_paused_stopped() {
        // 三态正常提取（字段见工具链 auto_loop.py _snapshot）。
        for state in ["running", "paused", "stopped"] {
            let v = json!({ "ok": true, "state": state, "registered_ok": 1 });
            assert_eq!(registrar_state(&v), state);
        }
    }

    #[test]
    fn registrar_state_missing_field_is_unknown() {
        assert_eq!(registrar_state(&json!({ "ok": false })), "unknown");
        assert_eq!(registrar_state(&json!({ "state": 42 })), "unknown");
        assert_eq!(registrar_state(&Value::Null), "unknown");
    }

    #[test]
    fn pool_available_counts_sum_by_kind() {
        // by_kind 形状：{kind: {available, in_use, done, failed, total}}。
        let v = json!({
            "ok": true, "items": [], "total": 30,
            "by_kind": {
                "outlook":  { "available": 12, "in_use": 1, "done": 3, "failed": 2, "total": 18 },
                "cf_temp":  { "available": 8,  "in_use": 0, "done": 0, "failed": 0, "total": 8  }
            }
        });
        assert_eq!(pool_available_count(&v), 20);
    }

    #[test]
    fn pool_available_handles_missing_or_empty() {
        assert_eq!(pool_available_count(&json!({ "ok": true })), 0);
        assert_eq!(pool_available_count(&json!({ "by_kind": {} })), 0);
        assert_eq!(pool_available_count(&Value::Null), 0);
    }

    // ---------------------------------------------------------------------
    // 池镜像：字段映射 + email 去重
    // ---------------------------------------------------------------------

    #[test]
    fn mirror_maps_fields_and_dedups_by_email() {
        // registered item 只回 token 长度（db.py list_registered：at_len/st_len/rt_len）。
        let items = vec![
            json!({
                "email": "a@x.com", "password": "p1",
                "at_len": 120, "st_len": 0, "rt_len": 0,
                "created_at": 111
            }),
            json!({
                "email": "b@x.com",
                "at_len": 0, "st_len": 55, "rt_len": 0,
                "created_at": 222
            }),
            // 同 email 重复：去重，首现保留（源按 created_at DESC → 首现最新）。
            json!({ "email": "a@x.com", "at_len": 9, "created_at": 333 }),
        ];
        let mirrored = mirror_registered_items(&items);
        assert_eq!(mirrored.len(), 2);
        assert_eq!(mirrored[0], json!({ "email": "a@x.com", "hasAccessToken": true, "addedAt": 111 }));
        assert_eq!(mirrored[1], json!({ "email": "b@x.com", "hasAccessToken": false, "addedAt": 222 }));
    }

    #[test]
    fn mirror_skips_missing_email_and_trims() {
        let items = vec![
            json!({ "at_len": 5, "created_at": 1 }), // 无 email：跳过
            json!({ "email": "  a@x.com  ", "at_len": 1, "created_at": 2 }),
            Value::Null, // 非 object：无 email，跳过
        ];
        let mirrored = mirror_registered_items(&items);
        assert_eq!(mirrored.len(), 1);
        assert_eq!(mirrored[0]["email"], "a@x.com"); // 已 trim
        assert_eq!(mirrored[0]["hasAccessToken"], true);
    }

    #[test]
    fn mirror_missing_created_at_maps_to_null() {
        let items = vec![json!({ "email": "c@x.com" })];
        let mirrored = mirror_registered_items(&items);
        assert_eq!(mirrored[0]["addedAt"], Value::Null);
        assert_eq!(mirrored[0]["hasAccessToken"], false);
    }
}
