//! WorkBuddy 模型反代：借道本机 OpenCode server 转发对话 + 模型目录。
//!
//! WorkBuddy 桌面端的 AI 内核是内嵌 OpenCode（启动时随机端口
//! `opencode serve --hostname=127.0.0.1 --port=<N>`），其 provider 表由
//! WorkBuddy 注入登录态（实测 `GET /provider` 可见 `connected` 清单，
//! 含 opencode/imodel/deepseek 等 11 个已鉴权 provider）。模型对话即
//! OpenCode 标准 API：
//! - `POST /session` 建会话 → `POST /session/{id}/message`（fire）→
//!   `GET /session/{id}/message` 轮询取 assistant 回复（含 tokens/cost）。
//!
//! 端口发现：扫描本机监听端口中 `GET /global/health` 返回
//! `{"healthy":true,"version":"..."}` 的 OpenCode server（响应体带
//! opencode 特征）。也可在配置里写死端口。
//!
//! OpenAI 兼容层：`/workbuddy/v1/chat/completions` 把 messages 拍平为
//! 单条 text（agent 场景），非流式聚合后合成 chunk 帧（与 glm_proxy / loomy_proxy
//! 相同的策略——OpenCode 的 message 提交是 fire-and-poll，无逐 token 流）。

use bytes::Bytes;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::notify;

const HEALTH_TIMEOUT_SECS: u64 = 3;
/// 看门狗未拉起时兜底提示的启动命令（可在配置 server_launch_cmd 覆盖）。
const DEFAULT_LAUNCH_CMD: &str = r"E:\opencode\start-serve-4097.bat";

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/workbuddy_proxy_config.json）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct WorkbuddyProxyConfig {
    pub enabled: bool,
    pub api_key: String,
    /// opencode server 端口（0 = 自动发现）。
    pub server_port: u16,
    /// server 不在时看门狗执行的启动命令（空 = 不自动重启）。
    pub server_launch_cmd: String,
    /// 模块级看门狗开关（与 Startup 文件夹的 vbs 看门狗协同，双重启由限流兜底）。
    pub watchdog_enabled: bool,
}

fn config_path() -> PathBuf {
    store_dir().join("workbuddy_proxy_config.json")
}

pub fn load_config() -> WorkbuddyProxyConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| WorkbuddyProxyConfig {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            api_key: v
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            server_port: v
                .get("server_port")
                .and_then(Value::as_u64)
                .map(|p| p as u16)
                .unwrap_or(0),
            server_launch_cmd: v
                .get("server_launch_cmd")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(DEFAULT_LAUNCH_CMD)
                .to_string(),
            watchdog_enabled: v
                .get("watchdog_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
        .unwrap_or_else(|| {
            let cfg = WorkbuddyProxyConfig {
                enabled: true,
                api_key: crate::modules::glm_proxy::generate_api_key(),
                server_port: 0,
                server_launch_cmd: DEFAULT_LAUNCH_CMD.to_string(),
                watchdog_enabled: true,
            };
            let _ = save_config(&cfg);
            cfg
        })
}

pub fn save_config(cfg: &WorkbuddyProxyConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "enabled": cfg.enabled,
        "api_key": cfg.api_key,
        "server_port": cfg.server_port,
        "server_launch_cmd": cfg.server_launch_cmd,
        "watchdog_enabled": cfg.watchdog_enabled,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Server 发现
// ---------------------------------------------------------------------------

static PORT_CACHE: Mutex<Option<(i64, u16)>> = Mutex::new(None);
const PORT_TTL_MS: i64 = 60 * 1000;
/// 发现失败的负缓存：失败后 30s 内直接返回错误，不再每请求重跑整轮发现。
static NEG_CACHE: Mutex<Option<i64>> = Mutex::new(None);
const NEG_TTL_MS: i64 = 30 * 1000;
/// 最近一次发现成功的端口（无 TTL，供看门狗与 channel_health 轻探测）。
static LAST_KNOWN_PORT: Mutex<Option<u16>> = Mutex::new(None);
static LAST_DISCOVERY_OK_MS: AtomicI64 = AtomicI64::new(0);

fn note_discovery_success(port: u16) {
    {
        let mut guard = PORT_CACHE.lock().unwrap();
        *guard = Some((now_ms(), port));
    }
    {
        let mut guard = LAST_KNOWN_PORT.lock().unwrap();
        *guard = Some(port);
    }
    {
        let mut guard = NEG_CACHE.lock().unwrap();
        *guard = None;
    }
    LAST_DISCOVERY_OK_MS.store(now_ms(), Ordering::Relaxed);
}

fn note_discovery_failure() {
    let mut guard = NEG_CACHE.lock().unwrap();
    *guard = Some(now_ms());
}

/// 负缓存是否仍在生效（失败后 30s 内）。
fn neg_cache_active() -> bool {
    let guard = NEG_CACHE.lock().unwrap();
    guard
        .as_ref()
        .map(|at| now_ms() - at < NEG_TTL_MS)
        .unwrap_or(false)
}

/// G2：判断错误文案是否为「负缓存期」瞬态（看门狗恢复窗口，通常 ≤60s）。
/// api 层据此给前端机器可读的 `recovering` 标记，避免把瞬态显示成稳态故障。
pub fn is_neg_cache_error(msg: &str) -> bool {
    msg.contains("负缓存期")
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// 探测端口是否为 opencode server（/global/health 特征）。
async fn is_opencode_server(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/global/health");
    let resp = http_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(HEALTH_TIMEOUT_SECS))
        .send()
        .await;
    match resp {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(v) => v.get("healthy").and_then(Value::as_bool) == Some(true),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// 进程发现用的 powershell 脚本文本（见 `discover_server` 调用点的完整来龙去脉）。
///
/// 🔴 抽成常量只为让单测能固化下面两条**易碎约束**——改脚本前先跑
/// `discover_ps_script_satisfies_powershell_quoting_rules`：
///   ① **不得含内层双引号**：`-Command <string>` 解析时会剥掉其中的 `"`，
///      `-Filter "Name='x'"` 会退化成 `-Filter Name='x'` ⇒ `无效查询`（实测踩中）；
///   ② 必须用**进程名**筛 `opencode.exe`（而非在命令行里匹配 `opencode\s+serve`）：
///      实际命令行是 `"E:\opencode\opencode.exe"  serve …`，`.exe` 会挡住 `\s+`。
const DISCOVER_PS_SCRIPT: &str = "Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'opencode.exe' -and $_.CommandLine -match 'serve' } | ForEach-Object { $_.CommandLine }";

/// 从一条进程命令行里取 `--port` 的值（纯函数，便于单测）。
///
/// 兼容两种写法——**两种都真实出现过**：
/// - `--port 4097`（空格；`start-serve-4097.bat` 用的就是这种）
/// - `--port=4097`（等号）
///
/// 🔴 只认等号是 2026-10-13 修掉的一个长期缺陷：即便进程筛对了，端口也取不出来。
fn port_from_cmdline(line: &str) -> Option<u16> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    for (i, t) in tokens.iter().enumerate() {
        if let Some(v) = t.strip_prefix("--port=") {
            if let Ok(p) = v.parse::<u16>() {
                return Some(p);
            }
        }
        if *t == "--port" {
            if let Some(v) = tokens.get(i + 1) {
                if let Ok(p) = v.trim_matches('"').parse::<u16>() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// 发现 opencode server 端口：配置端口优先，否则扫描候选常见端口 + 监听端口。
///
/// 失败路径写 30s 负缓存：server 不在时每个请求都要重跑整轮发现（powershell +
/// netstat + 最多 80 个端口探测，最坏十几秒），负缓存让连续失败快速返回、
/// 把算力留给看门狗恢复。
pub async fn discover_server() -> Result<u16, String> {
    {
        let guard = PORT_CACHE.lock().unwrap();
        if let Some((at, port)) = guard.as_ref() {
            if now_ms() - at < PORT_TTL_MS {
                return Ok(*port);
            }
        }
    }
    if neg_cache_active() {
        return Err(NEG_CACHE_ERR.to_string());
    }
    let cfg = load_config();
    if cfg.server_port > 0 {
        if is_opencode_server(cfg.server_port).await {
            note_discovery_success(cfg.server_port);
            return Ok(cfg.server_port);
        }
        note_discovery_failure();
        return Err(format!(
            "配置的 opencode 端口 {} 未响应（opencode serve 实例未运行？）",
            cfg.server_port
        ));
    }
    // 优先从 opencode serve 进程的命令行抓端口（最可靠，无端口范围假设）。
    //
    // 🔴 2026-10-13 修（本条曾**长期是死代码**，两处都有毛病，一直在靠 netstat 兜底）：
    //   ① 筛选：原正则 `opencode\s+serve` 与实际命令行不匹配——实际形态是
    //      `"E:\opencode\opencode.exe"  serve --hostname ... --port 4097`，
    //      "opencode" 后面紧跟 ".exe"，`\s+` 永远落不到 "serve" 上。改为按
    //      **进程名**（opencode.exe）筛、再在命令行里找 serve，与路径写法解耦。
    //   ② 取值：原解析只认 `--port=4097`（等号），而 bat 实际传的是 `--port 4097`
    //      （空格，两个独立 token）⇒ 即使筛到了也取不出端口。
    // 代价：修复前每次发现都白起一个 powershell（实测 ~2.5s）且阻塞一个 async worker。
    //
    // 🔴🔴 -Command 的脚本文本里**绝对不能出现内层双引号**：PowerShell 解析
    //   `-Command <string>` 时会把其中的 `"` 当引号剥掉，`-Filter "Name='x'"` 会退化成
    //   `-Filter Name='x'` ⇒ 报 `Get-CimInstance : 无效查询`（2026-10-13 实测踩中）。
    //   必须用 `Where-Object { $_.Name -eq '…' }` 这种**只用单引号**的等价写法。
    if let Ok(out) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", DISCOVER_PS_SCRIPT])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(port) = port_from_cmdline(line) {
                if is_opencode_server(port).await {
                    note_discovery_success(port);
                    return Ok(port);
                }
            }
        }
    }
    // 兜底：扫描当前监听的本地端口（含常见临时端口与 4000-5000 段）。
    let mut candidates = Vec::new();
    if let Ok(out) = std::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 && parts[3] == "LISTENING" {
                if let Some(local) = parts[1].rsplit(':').next() {
                    if let Ok(port) = local.parse::<u16>() {
                        // 排除知名端口；opencode serve 默认 4096 起，动态端口 49152+
                        if (port >= 4096 && port < 49152) || (49152..=65535).contains(&port) {
                            if !candidates.contains(&port) {
                                candidates.push(port);
                            }
                        }
                    }
                }
            }
        }
    }
    // 按端口从小到大尝试，最多 80 个。
    candidates.sort_unstable();
    candidates.truncate(80);
    for chunk in candidates.chunks(16) {
        let results = futures_util::future::join_all(
            chunk.iter().map(|p| is_opencode_server(*p)),
        )
        .await;
        for (idx, ok) in results.iter().enumerate() {
            if *ok {
                let port = chunk[idx];
                note_discovery_success(port);
                return Ok(port);
            }
        }
    }
    note_discovery_failure();
    Err(NEG_CACHE_ERR.to_string())
}

const NEG_CACHE_ERR: &str = "未发现运行中的 OpenCode server（30s 内刚探测过，处于负缓存期；看门狗会尝试自动拉起，也可手动执行 E:\\opencode\\start-serve-4097.bat）";

// ---------------------------------------------------------------------------
// 模块级看门狗（与 Startup 文件夹的 opencode-server-watchdog.vbs 协同）
// ---------------------------------------------------------------------------

const WATCHDOG_INTERVAL_SECS: u64 = 30;
const WATCHDOG_MAX_RESTARTS_PER_HOUR: u32 = 4;
/// 看门狗重启尝试的时间窗计数（窗口起点 ms, 次数）。
static WATCHDOG_WINDOW: Mutex<Option<(i64, u32)>> = Mutex::new(None);
static WATCHDOG_NEXT_TRY_MS: AtomicI64 = AtomicI64::new(0);

/// 供 channel_health 等轻探测使用：缓存里认为可用的端口（不保证新鲜）。
pub fn cached_server_port() -> Option<u16> {
    LAST_KNOWN_PORT.lock().unwrap().as_ref().copied()
}

/// 轻探测指定端口是否为健康 opencode server（channel_health 快路径用）。
/// timeout 由调用方给定：channel_health 用「内层长超时 + 外层 tokio::timeout 短包裹」，
/// 从而把「连接被拒」（快速 Err）与「挂起无响应」（外层超时）区分开。
pub async fn probe_port_health(port: u16, timeout: std::time::Duration) -> bool {
    let url = format!("http://127.0.0.1:{port}/global/health");
    let resp = http_client().get(&url).timeout(timeout).send().await;
    match resp {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(v) => v.get("healthy").and_then(Value::as_bool) == Some(true),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// 最近一次发现成功的时刻（ms；0 = 本进程从未成功）。
pub fn last_discovery_ok_ms() -> i64 {
    LAST_DISCOVERY_OK_MS.load(Ordering::Relaxed)
}

/// opencode.exe 是否作为独立进程在跑（端口漂移场景：进程在、旧端口不通，
/// 此时不重启，交给发现逻辑找新端口）。
fn opencode_process_running() -> bool {
    if let Ok(out) = std::process::Command::new("tasklist")
        .args(["/fi", "imagename eq opencode.exe", "/nh"])
        .output()
    {
        return String::from_utf8_lossy(&out.stdout)
            .to_lowercase()
            .contains("opencode.exe");
    }
    false
}

/// 记录一次重启尝试；每小时超过上限返回 false（不再启动，仅 toast）。
fn record_restart_attempt(now: i64) -> bool {
    let mut guard = WATCHDOG_WINDOW.lock().unwrap();
    let state = guard.get_or_insert((now, 0));
    if now - state.0 > 3600 * 1000 {
        *state = (now, 0);
    }
    if state.1 >= WATCHDOG_MAX_RESTARTS_PER_HOUR {
        return false;
    }
    state.1 += 1;
    true
}

/// G3：启动后下一轮探活确认进程没真正起来（端口被占 → opencode 启动即退）时
/// 回滚计数——「启动即失败」不应消耗每小时 4 次的重启额度（2026-09-22 黑洞
/// 占位事故：4 次额度全耗在无法 bind 的启动上，自愈断了约 6 分钟）。
fn rollback_restart_attempt() {
    let mut guard = WATCHDOG_WINDOW.lock().unwrap();
    if let Some((_, count)) = guard.as_mut() {
        if *count > 0 {
            *count -= 1;
        }
    }
}

/// 当前小时窗内的重启尝试计数（供测试与诊断）。
#[cfg(test)]
fn restart_attempt_count(now: i64) -> u32 {
    let mut guard = WATCHDOG_WINDOW.lock().unwrap();
    let state = guard.get_or_insert((now, 0));
    if now - state.0 > 3600 * 1000 {
        *state = (now, 0);
    }
    state.1
}

/// 模块级看门狗协程：每 30s 探活已知端口；进程不在时限流执行启动命令。
/// 与 Startup 文件夹的 vbs 看门狗共用同一启动命令，双重启由每小时 4 次
/// 限流 + 指数退避兜底，后到者发现已恢复即跳过。
pub fn spawn_server_watchdog() {
    tokio::spawn(async move {
        // 启动延迟 10s，避开服务刚起来的峰值。
        tokio::time::sleep(Duration::from_secs(10)).await;
        let mut attempt: u32 = 0;
        // G3：上一轮发起过启动命令；本轮先验证进程是否真的起来了，
        // 没起来（端口被占 → opencode 启动即退）则回滚重启计数。
        let mut launched_last_round = false;
        loop {
            let cfg = load_config();
            if !cfg.watchdog_enabled {
                tokio::time::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS)).await;
                continue;
            }

            if launched_last_round {
                launched_last_round = false;
                if !opencode_process_running() {
                    rollback_restart_attempt();
                }
            }

            // 1) 快路径：探测最近已知端口（死端口 connect 会立刻拒绝，不耗 3s 超时）。
            let known = cached_server_port();
            let healthy = match known {
                Some(port) => is_opencode_server(port).await,
                None => false,
            };
            if healthy {
                attempt = 0;
                tokio::time::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS)).await;
                continue;
            }

            // 2) 没有已知端口（或已知端口已死）时跑一次完整发现，刷新缓存与负缓存。
            if discover_server().await.is_ok() {
                attempt = 0;
                tokio::time::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS)).await;
                continue;
            }

            // 3) 进程在但端口不通：可能在启动中或换了端口，不干预。
            if opencode_process_running() {
                tokio::time::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS)).await;
                continue;
            }

            // 4) 进程不在：限流 + 退避后执行启动命令。
            let now = now_ms();
            if now < WATCHDOG_NEXT_TRY_MS.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS)).await;
                continue;
            }
            if cfg.server_launch_cmd.trim().is_empty() {
                tokio::time::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS)).await;
                continue;
            }
            if !record_restart_attempt(now) {
                notify::notify(
                    "workbuddy-server",
                    "WorkBuddy 反代上游看门狗",
                    &format!(
                        "OpenCode server 持续不在，本轮小时重启次数已达上限（{} 次/小时），暂停自动重启；可手动执行 {}",
                        WATCHDOG_MAX_RESTARTS_PER_HOUR, cfg.server_launch_cmd
                    ),
                );
                tokio::time::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS)).await;
                continue;
            }
            attempt += 1;
            let backoff_ms: i64 = match attempt {
                1 => 60_000,
                2 => 120_000,
                _ => 240_000,
            };
            WATCHDOG_NEXT_TRY_MS.store(now + backoff_ms, Ordering::Relaxed);
            match std::process::Command::new("cmd")
                .args(["/c", &cfg.server_launch_cmd])
                .spawn()
            {
                Ok(_) => {
                    // G3：标记待验证——下一轮若进程没起来则回滚计数。
                    launched_last_round = true;
                    notify::notify(
                        "workbuddy-server",
                        "WorkBuddy 反代上游看门狗",
                        &format!(
                            "OpenCode server 不在且进程未运行，已执行启动命令（第 {} 次，下次退避 {}s）：{}",
                            attempt,
                            backoff_ms / 1000,
                            cfg.server_launch_cmd
                        ),
                    )
                }
                Err(e) => notify::notify(
                    "workbuddy-server",
                    "WorkBuddy 反代上游看门狗",
                    &format!("启动命令执行失败：{e}（{}）", cfg.server_launch_cmd),
                ),
            }
            tokio::time::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// 模型目录（provider 接口，仅已鉴权 provider）
// ---------------------------------------------------------------------------

/// G4：作废 60s 成功缓存，让下一个 discover_server() 重新发现。
/// 用于「缓存端口连接失败」（server 重启换端口、旧端口已拒听）的场景。
pub fn invalidate_cached_port() {
    let mut guard = PORT_CACHE.lock().unwrap();
    *guard = None;
}

/// 判断错误串是否为连接类失败（端口拒听/连接被拒），用于触发 G4 重发现。
/// 0.1.85 起 `pub(crate)`：OpenCode 网关的 fallback 触发判定复用同一签名，不另写一套。
pub(crate) fn is_connect_error(msg: &str) -> bool {
    msg.contains("error sending request")
        || msg.contains("Connection refused")
        || msg.contains("tcp connect")
        || msg.contains("连接")
}

/// G4：GET /provider 一次（端口由调用方给定）。
async fn fetch_provider(port: u16) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{port}/provider");
    let resp = http_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("读取 provider 失败: {e}"))?;
    resp.json().await.map_err(|e| e.to_string())
}

/// 模型目录：从 /provider 抓取（只列 connected provider 的模型）。
pub async fn models_catalog() -> Result<Value, String> {
    let port = discover_server().await?;
    let data = match fetch_provider(port).await {
        Ok(data) => data,
        Err(e) => {
            // G4：连接失败 = 端口漂移窗口；作废缓存并重发现一次再试，
            // 把「必然 502」变成「一次慢请求」。
            if !is_connect_error(&e) {
                return Err(e);
            }
            invalidate_cached_port();
            let port2 = discover_server().await?;
            fetch_provider(port2).await?
        }
    };
    let connected: Vec<String> = data
        .get("connected")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let mut providers = Vec::new();
    if let Some(all) = data.get("all").and_then(Value::as_array) {
        for provider in all {
            let Some(pid) = provider.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !connected.iter().any(|c| c == pid) {
                continue;
            }
            // 🔴 自引用过滤（2026-09-22）：baseURL 指向本机回环的 provider，等于「网关把自己的
            // 路由当成上游」。例如实测存在 `sylvia-qwen → http://127.0.0.1:57890/qwen/v1`
            // （即 Sylvia Switch 自身的 /qwen/v1），它同时造成两个问题：
            //   1. 模型目录虚增（同一批模型被多算一遍，拉高 modelCount）；
            //   2. 潜在请求环：WorkBuddy 网关借道 OpenCode → 命中自己的路由 → 若再回落
            //      到 OpenCode 便形成环。
            // 故此类 provider 一律不计入目录。
            if provider
                .pointer("/options/baseURL")
                .or_else(|| provider.pointer("/options/baseUrl"))
                .and_then(Value::as_str)
                .map(is_loopback_url)
                .unwrap_or(false)
            {
                continue;
            }
            let mut models = Vec::new();
            if let Some(map) = provider.get("models").and_then(Value::as_object) {
                for (mid, model) in map {
                    models.push(json!({
                        "id": mid,
                        "name": model.get("name").cloned().unwrap_or(Value::Null),
                        "reasoning": model.pointer("/capabilities/reasoning").cloned().unwrap_or(Value::Bool(false)),
                        "context": model.pointer("/limit/context").cloned().unwrap_or(Value::Null),
                    }));
                }
            }
            models.sort_by(|a, b| {
                let ai = a.get("id").and_then(Value::as_str).unwrap_or("");
                let bi = b.get("id").and_then(Value::as_str).unwrap_or("");
                ai.cmp(bi)
            });
            providers.push(json!({
                "id": pid,
                "name": provider.get("name").cloned().unwrap_or(Value::Null),
                "models": models,
            }));
        }
    }
    providers.sort_by(|a, b| {
        let ai = a.get("id").and_then(Value::as_str).unwrap_or("");
        let bi = b.get("id").and_then(Value::as_str).unwrap_or("");
        ai.cmp(bi)
    });
    Ok(json!({"ok": true, "providers": providers}))
}

/// baseURL 是否指向本机回环 —— 用于识别「自引用 provider」（网关把自己的路由当上游）。
fn is_loopback_url(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    l.contains("127.0.0.1") || l.contains("localhost") || l.contains("[::1]")
}

/// OpenAI /v1/models 形态。
pub async fn models_payload() -> Result<Value, String> {
    let catalog = models_catalog().await?;
    let mut data = Vec::new();
    if let Some(providers) = catalog.get("providers").and_then(Value::as_array) {
        for provider in providers {
            let pid = provider.get("id").and_then(Value::as_str).unwrap_or("unknown");
            if let Some(models) = provider.get("models").and_then(Value::as_array) {
                for model in models {
                    if let Some(id) = model.get("id").and_then(Value::as_str) {
                        // 模型 ID 用 "provider/model" 形式（与 opencode 一致）。
                        data.push(json!({
                            "id": format!("{pid}/{id}"),
                            "object": "model",
                            "created": 0,
                            "owned_by": pid,
                        }));
                    }
                }
            }
        }
    }
    Ok(json!({"object": "list", "data": data}))
}

/// 桌面端未探到内嵌会话时的兜底内置模型（与 `api_workbuddy_native_models` 同源同值）。
const DEFAULT_BUILTIN_MODEL: &str = "deepseek-v4.1-flash";

/// WorkBuddy **内置模型**目录（桌面端内嵌会话自带，走 WorkBuddy 订阅·经 ACP 转发）。
///
/// 🔴 为什么单独抽一个函数，而不是复用 `models_payload()`：
/// `models_payload()` / `models_catalog()` 是**借道本机 OpenCode server 展开上游 provider 全量清单**
/// （实测 591 条 / 14 渠道）。那份清单按设计**归 OpenCode 网关**呈现
/// （见 `docs/gateway-split-20260922.md`：WorkBuddy「不拥有 provider 清单」）。
/// 而且 **`opencode.rs` 正在复用这两个函数**（`opencode::models_payload()` / `counts()`）——
/// 所以**绝不能改它们的语义**，否则会一起打断 OpenCode 网关与 OpenCode 页。
/// WorkBuddy 自己的目录一律走本函数。
///
/// 与 `/workbuddy-native/v1/models` 共用同一数据源（`trae_acp::builtin_model()`），
/// 保证「目录里列的」与「ACP 通道实际能跑的」永远一致。
pub fn builtin_models_payload() -> Value {
    let model = crate::modules::trae_acp::builtin_model()
        .unwrap_or_else(|| DEFAULT_BUILTIN_MODEL.to_string());
    json!({
        "object": "list",
        "data": [{
            "id": model,
            "object": "model",
            "created": 0,
            "owned_by": "workbuddy",
        }],
    })
}

/// **每渠道各取 1 个代表模型**（2026-09-23：用户要求「WorkBuddy 里的 14 个模型」）。
///
/// 背景与取舍（改动前务必读完，否则会走回头路）：
/// - WorkBuddy 网关借道本机 OpenCode server 能拿到 **14 个渠道 / 591 个模型**；
///   用户先嫌 591 太多（要求只留内置），随后又要「14 个」。
/// - 实测 WorkBuddy 侧**不存在「14 个内置模型」这个集合**：客户端产品配置共 76 个模型
///   （51 个无 url 走订阅、25 个是自定义接入），native ACP 通道只有 1 个；
///   唯一稳定等于 14 的就是**渠道数**。
///   ⇒ 折中：每个渠道保留 1 个代表，既覆盖全部 14 个渠道、又保持目录精简（约 14 条）。
///
/// 选取口径与页面「速度优先」一致：① 免费优先 ② 快速档 ③ 名称升序。
/// 上游不可达 / 目录为空时回落 `builtin_models_payload()`（内置模型），保证目录永不为空。
///
/// ⚠️ 与 `builtin_models_payload()` 的分工：
///    - `/workbuddy-native/v1/models`（ACP 通道）→ **只列内置模型**（那才是真正能走订阅的）；
///    - `/workbuddy/v1/models`（代理目录）    → 本函数（每渠道代表）。
pub async fn channel_pick_models_payload() -> Value {
    let mut data = Vec::new();
    if let Ok(catalog) = models_catalog().await {
        if let Some(providers) = catalog.get("providers").and_then(Value::as_array) {
            for provider in providers {
                let pid = provider.get("id").and_then(Value::as_str).unwrap_or("unknown");
                let mut ids: Vec<String> = provider
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|ms| {
                        ms.iter()
                            .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    continue;
                }
                ids.sort_by(|a, b| pick_score(a).cmp(&pick_score(b)).then_with(|| a.cmp(b)));
                data.push(json!({
                    "id": format!("{pid}/{}", ids[0]),
                    "object": "model",
                    "created": 0,
                    "owned_by": pid,
                }));
            }
        }
    }
    if data.is_empty() {
        return builtin_models_payload();
    }
    json!({ "object": "list", "data": data })
}

/// 代表模型排序分：**免费(0) < 快速档(1) < 其它(2)**。
/// 免费判定复用 `opencode::is_free_model`，与 OpenCode 网关和前端口径一致。
fn pick_score(id: &str) -> u8 {
    if crate::modules::opencode::is_free_model(id) {
        return 0;
    }
    let l = id.to_ascii_lowercase();
    for kw in ["flash", "lite", "lightning", "mini", "nano", "turbo", "fast", "small"] {
        if l.contains(kw) {
            return 1;
        }
    }
    2
}

// ---------------------------------------------------------------------------
// 内置模型目录 payload（契约 §3.1：native ACP 实时探测 + static 回落）
// ---------------------------------------------------------------------------

/// session/new 实时目录 → §3.1 payload（纯函数，便于单测）。
///
/// - `source`：探测刚完成 → `acp-live`，命中 TTL 缓存 → `acp-cache`；
/// - 默认只列 native（kind=tier|model，**含 exposed=false 的补充实模**——契约
///   §2.1：默认列出 kind∈{tier,model}）；`include_custom` 追加 `custom-local:*`
///   条目（kind="custom"，owned_by="workbuddy-custom"）；
/// - 每条带 `exposed`（契约 §2.1：ACP 实时暴露=true，产品配置补充=false）；
/// - 顺序保持服务端返回序（ACP 条目在前，补充条目追加在后）。
pub fn catalog_to_payload(
    catalog: &crate::modules::trae_acp::NativeCatalog,
    include_custom: bool,
) -> Value {
    let source = if catalog.is_fresh() { "acp-live" } else { "acp-cache" };
    let region = desktop_region();
    let data: Vec<Value> = catalog
        .models
        .iter()
        .filter(|m| include_custom || m.kind != "custom")
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": 0,
                "owned_by": if m.kind == "custom" { "workbuddy-custom" } else { "workbuddy" },
                "name": m.name,
                "multiplier": m.multiplier,
                "kind": m.kind,
                "exposed": m.exposed,
                "region": region,
            })
        })
        .collect();
    json!({
        "object": "list",
        "source": source,
        "probedAt": catalog.probed_at,
        "currentModelId": if catalog.current_model_id.is_empty() {
            Value::Null
        } else {
            json!(catalog.current_model_id)
        },
        "region": region,
        "data": data,
    })
}

/// ACP 不可达时的静态回落形态（契约 §3.1）：`builtin_models_payload()` 原样数据
/// （恒 1 条，语义不改）+ 来源标记三件套。目录永不为空。
fn static_fallback_models_payload() -> Value {
    let mut payload = builtin_models_payload();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("source".to_string(), json!("static-fallback"));
        obj.insert("probedAt".to_string(), Value::Null);
        obj.insert("currentModelId".to_string(), Value::Null);
    }
    payload
}

/// GET /workbuddy-native/v1/models 的数据源（契约 §3.1）：
/// ACP 实时探测目录优先（300s TTL 缓存同源），ACP 不可达 → static 回落。
pub async fn native_models_payload(include_custom: bool) -> Value {
    match crate::modules::trae_acp::native_models_catalog().await {
        Ok(catalog) => catalog_to_payload(&catalog, include_custom),
        Err(_) => static_fallback_models_payload(),
    }
}

/// 桌面端登录区域（"cn" | "intl" | "unknown"）——账号页区域分类的数据源。
///
/// 判定链：`~/.workbuddy/settings.json` 的 `legacyOwnerUid`（桌面端登录账号）
/// → 在 `~/.wb-switch/accounts.json` 按 uid 匹配（前缀/包含均可，状态表展示的
/// 是截断 uid）→ 该账号 domain 后缀派生（含 ".ai" → intl，否则 cn）。
/// 任一环节读不到 → "unknown"（前端显示「未知」徽标，不做猜测）。
pub fn desktop_region() -> &'static str {
    let settings_path = crate::modules::config::home_dir()
        .join(".workbuddy")
        .join("settings.json");
    let settings = std::fs::read_to_string(settings_path).unwrap_or_default();
    let accounts_path = crate::modules::config::home_dir()
        .join(".wb-switch")
        .join("accounts.json");
    let accounts = std::fs::read_to_string(accounts_path).unwrap_or_default();
    resolve_desktop_region(&settings, &accounts)
}

/// 桌面区域解析（纯函数，便于单测）。
///
/// ⚠️ `legacyOwnerUid` 实测嵌在 `settings.claw` 下（2026-09-25：根层读取恒
/// unknown——第一版就栽在这里）；兼容根层与 `/claw` 两种位置。
fn resolve_desktop_region(settings_text: &str, accounts_text: &str) -> &'static str {
    let Ok(settings) = serde_json::from_str::<Value>(settings_text) else {
        return "unknown";
    };
    let owner = settings
        .get("legacyOwnerUid")
        .or_else(|| settings.pointer("/claw/legacyOwnerUid"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if owner.is_empty() {
        return "unknown";
    }
    let Ok(accounts) = serde_json::from_str::<Value>(accounts_text) else {
        return "unknown";
    };
    let list: &[Value] = match accounts {
        Value::Array(ref arr) => arr.as_slice(),
        ref v => v
            .get("accounts")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or(&[]),
    };
    for acc in list {
        let uid = acc.get("uid").and_then(Value::as_str).unwrap_or_default();
        if uid.is_empty() || !(uid.starts_with(owner) || owner.starts_with(uid)) {
            continue;
        }
        let domain = acc.get("domain").and_then(Value::as_str).unwrap_or_default();
        return if domain.contains(".ai") { "intl" } else { "cn" };
    }
    "unknown"
}

/// 强制刷新目录（契约 §2.1：清缓存重探一次，绕过 300s TTL）→ §3.1 目录结构。
/// 探测失败原样上抛 `Err`（api 层的 refresh 路由按 502 报错，不落 static 回落
/// ——显式刷新要如实告诉用户「为什么刷不出」）。
pub async fn refresh_native_catalog() -> Result<crate::modules::trae_acp::NativeCatalog, String> {
    crate::modules::trae_acp::native_models_catalog_refresh().await
}

/// GET /workbuddy/v1/models 的数据源（契约 T4）：内置目录优先；
/// 目录不可用时回落现有 `channel_pick_models_payload()`——其内部 fresh 发现
/// OpenCode server 并展开上游目录，此即「不可用时重新再去获取模型内容」。
pub async fn v1_models_payload() -> Value {
    match crate::modules::trae_acp::native_models_catalog().await {
        Ok(catalog) => catalog_to_payload(&catalog, false),
        Err(_) => channel_pick_models_payload().await,
    }
}

/// native-first 兜底判定（契约 §3.2①②，纯函数便于单测）：
/// - model 缺省 → native（用会话默认模型）；
/// - 目录不可用（ACP 探测失败）→ 仍先试 native，失败走 ③ 兜底；
/// - 目录可用时 model ∈ 目录 ids（含 tier/custom）→ native；∉ → 直接兜底
///   （②：set_model 对未知 ID 假成功，不能靠它报错）。
fn should_try_native(catalog: Option<&crate::modules::trae_acp::NativeCatalog>, requested: Option<&str>) -> bool {
    match (catalog, requested) {
        (_, None) => true,
        (None, Some(_)) => true,
        (Some(catalog), Some(id)) => catalog.contains_id(id),
    }
}

/// 状态探测。
pub async fn proxy_status() -> Value {
    let cfg = load_config();
    let server = match discover_server().await {
        Ok(port) => json!({"ok": true, "port": port}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    let catalog = models_catalog().await.ok();
    let model_count = catalog
        .as_ref()
        .and_then(|c| c.get("providers"))
        .and_then(Value::as_array)
        .map(|providers| {
            providers
                .iter()
                .filter_map(|p| p.get("models"))
                .filter_map(Value::as_array)
                .map(|m| m.len())
                .sum::<usize>()
        })
        .unwrap_or(0);
    let providers_value = catalog
        .as_ref()
        .and_then(|c| c.get("providers"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "enabled": cfg.enabled,
        "server": server,
        "providers": providers_value,
        "modelCount": model_count,
    })
}

// ---------------------------------------------------------------------------
// Chat 转发（OpenAI 兼容）
// ---------------------------------------------------------------------------

fn flatten_messages(messages: &[Value]) -> String {
    let mut chunks = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
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

/// 非流式对话补全：建会话 → 发消息 → 轮询取 assistant 回复。
/// OpenCode server 1.18.21 的已知竞态：并发「建会话 + 立即提交消息」时，server 端
/// `insert into "message"` 会因 **session 行尚未落库** 而报
/// `SQLiteError: FOREIGN KEY constraint failed`，而该失败在客户端被 reqwest 报成
/// `error sending request`（**极具误导性，看着像网络故障，实际是 DB 外键失败**）。
/// 2026-09-23 实测复现：WorkBuddy 自动重试造成 8 次连续失败（同一秒内建多个会话）。
///
/// 对策（三管齐下，都在本网关侧，无需改 opencode）：
/// 1. **串行化**：整个转发用进程级异步锁串起来，消掉同一时刻的建会话/提交突发；
/// 2. **确认落库**：建完会话先轮询 `GET /session/{id}`（实测可立即 200）再提交；
/// 3. **定向重试**：仅命中「提交失败 + FK/连接异常」签名时重试一次 ——
///    此时 server 侧根本没生成内容，重试不会造成重复投递。
static CHAT_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 「提交消息失败且疑似 session 竞态」的判定（用于定向重试，不殃及其它错误）。
fn is_session_race_error(msg: &str) -> bool {
    msg.contains("提交消息失败")
        && (msg.contains("error sending request") || msg.contains("FOREIGN KEY"))
}

pub async fn chat_completions_json(body: Value) -> Result<Value, String> {
    let _guard = CHAT_SERIAL.lock().await;
    match chat_completions_once(body.clone()).await {
        Ok(value) => Ok(value),
        Err(e) if is_session_race_error(&e) => {
            notify::notify(
                "workbuddy",
                "OpenCode 会话竞态，已自动重试",
                &truncate(&e, 160),
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            chat_completions_once(body).await
        }
        Err(e) => Err(e),
    }
}

/// 单次转发（建会话 → 确认落库 → 提交消息 → 轮询取回复）。
async fn chat_completions_once(body: Value) -> Result<Value, String> {
    let port = discover_server().await?;
    let base = format!("http://127.0.0.1:{port}");
    let model = body.get("model").and_then(Value::as_str).unwrap_or("opencode/mimo-v2.6-flash-free");
    let (provider_id, model_id) = match model.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => ("opencode".to_string(), model.to_string()),
    };
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let prompt = flatten_messages(&messages);
    if prompt.trim().is_empty() {
        return Err("messages 不能为空".to_string());
    }
    let client = http_client();
    // 1) 建会话。G4：连接失败（端口漂移）时作废缓存、重发现后重试一次
    //    （建会话无副作用：连接根本没建立，server 侧不会留下半截会话）。
    let session: Value = match client
        .post(format!("{base}/session"))
        .json(&json!({}))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        Ok(resp) => resp
            .json()
            .await
            .map_err(|e| format!("解析会话响应失败: {e}"))?,
        Err(e) => {
            let msg = format!("创建会话失败: {e}");
            if !is_connect_error(&msg) {
                return Err(msg);
            }
            invalidate_cached_port();
            let port2 = discover_server().await?;
            let base2 = format!("http://127.0.0.1:{port2}");
            client
                .post(format!("{base2}/session"))
                .json(&json!({}))
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e2| format!("创建会话失败: {e2}"))?
                .json()
                .await
                .map_err(|e| format!("解析会话响应失败: {e}"))?
        }
    };
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "创建会话失败：响应缺少 id".to_string())?
        .to_string();
    // 2a) 确认 session 已落库再提交，规避「message 外键指向的 session 行不存在」
    //     （表现为提交失败 + error sending request，见上方 CHAT_SERIAL 注释）。
    wait_session_ready(client, &base, &session_id).await;
    // 2) 提交消息。⚠️ 2026-09-25 实测 opencode 1.18.21 的 POST message 已改为
    //    **阻塞到生成完成才返回**（直打实测 12.7s 后才回 200 + 完整 parts），
    //    不再是注释里假设的 fire-and-poll——超时必须覆盖模型生成时长，
    //    15s 会在上游稍慢时超时（reqwest 报 "error sending request"），
    //    叠加外层重试 3 轮 ≈ 44s 后 502。放宽到 120s；成功后轮询立即取到回复。
    let payload = json!({
        "parts": [{"type": "text", "text": prompt}],
        "model": {"providerID": provider_id, "modelID": model_id},
        "agent": "build",
    });
    let submit = client
        .post(format!("{base}/session/{session_id}/message"))
        .json(&payload)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await;
    match submit {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // 会话已建立但提交失败：清理会话后报错。
            let _ = client
                .delete(format!("{base}/session/{session_id}"))
                .send()
                .await;
            return Err(format!("提交消息失败 {status}: {}", truncate(&text, 200)));
        }
        Err(e) => {
            // G4：连接类失败大概率是端口漂移，作废缓存让下一个请求重新发现
            // （不在此处自动重试：避免重复投递同一 prompt）。
            invalidate_cached_port();
            let _ = client
                .delete(format!("{base}/session/{session_id}"))
                .send()
                .await;
            return Err(format!("提交消息失败: {e}"));
        }
    }
    // 3) 轮询取 assistant 回复（上限 5 分钟）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut tokens = Value::Null;
    let mut cost = Value::Null;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let messages: Vec<Value> = match client
            .get(format!("{base}/session/{session_id}/message"))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) => resp.json().await.unwrap_or_default(),
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                continue;
            }
        };
        // 最后一条 assistant 消息即当前轮回复。
        if let Some(last) = messages.iter().rev().find(|m| {
            m.pointer("/info/role").and_then(Value::as_str) == Some("assistant")
        }) {
            let completed = last
                .pointer("/info/time/completed")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let texts: Vec<String> = last
                .get("parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            if completed > 0 {
                tokens = last.pointer("/info/tokens").cloned().unwrap_or(Value::Null);
                cost = last.pointer("/info/cost").cloned().unwrap_or(Value::Null);
                let reply = texts.join("");
                // 清理会话（不留垃圾）。
                let _ = client
                    .delete(format!("{base}/session/{session_id}"))
                    .send()
                    .await;
                return Ok(json!({
                    "id": format!("chatcmpl-{}", session_id),
                    "object": "chat.completion",
                    "created": now_ms() / 1000,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": reply},
                        "finish_reason": "stop",
                    }],
                    "usage": {
                        "prompt_tokens": tokens.get("input").cloned().unwrap_or(json!(0)),
                        "completion_tokens": tokens.get("output").cloned().unwrap_or(json!(0)),
                        "total_tokens": tokens.get("total").cloned().unwrap_or(json!(0)),
                        "cost": cost,
                    },
                }));
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    let _ = client
        .delete(format!("{base}/session/{session_id}"))
        .send()
        .await;
    Err("等待回复超时（模型可能排队中或不可用）".to_string())
}

/// 轮询确认 session 已可查（`GET /session/{id}` → 200），最多约 1.2 秒。
/// 始终查不到也放行 —— 由后续既有错误路径兜底，这里只是把落库窗口让出来，
/// 顺带把"会话根本没建成"与"server 抽了"两种情况错开。
async fn wait_session_ready(client: &reqwest::Client, base: &str, session_id: &str) {
    for _ in 0..8 {
        let ok = matches!(
            client
                .get(format!("{base}/session/{session_id}"))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await,
            Ok(resp) if resp.status().is_success()
        );
        if ok {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// 流式响应封装（OpenAI chunk SSE 字节流）。
///
/// 原为跨模块共用的流式封装类型，现已内聚到本模块并使用中性命名。**结构与行为完全不变**。
pub struct WorkbuddyUpstreamStream {
    pub rx: tokio::sync::mpsc::Receiver<Result<Bytes, String>>,
}

/// 流式：非流式结果合成 OpenAI chunk 帧（OpenCode message 提交无逐 token 流）。
pub async fn chat_completions_stream(
    body: Value,
) -> Result<WorkbuddyUpstreamStream, String> {
    let completion = chat_completions_json(body).await?;
    let model = completion
        .get("model")
        .cloned()
        .unwrap_or(json!("workbuddy"));
    let content = completion
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = completion.get("usage").cloned().unwrap_or(Value::Null);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    tokio::spawn(async move {
        let frame = |delta: Value, finish: Option<&str>| {
            json!({
                "id": format!("chatcmpl-workbuddy-{}", now_ms()),
                "object": "chat.completion.chunk",
                "created": now_ms() / 1000,
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
            .to_string()
        };
        let _ = tx.send(Ok(Bytes::from(format!(
            "data: {}\n\n",
            frame(json!({"role": "assistant", "content": ""}), None)
        )))).await;
        if !content.is_empty() {
            let _ = tx.send(Ok(Bytes::from(format!(
                "data: {}\n\n",
                frame(json!({"content": content}), None)
            )))).await;
        }
        let mut usage_json = usage;
        if let Some(obj) = usage_json.as_object_mut() {
            obj.insert("stream".to_string(), json!(true));
        }
        let _ = tx.send(Ok(Bytes::from(format!(
            "data: {}\n\n",
            json!({
                "id": format!("chatcmpl-workbuddy-{}", now_ms()),
                "object": "chat.completion.chunk",
                "created": now_ms() / 1000,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": usage_json,
            })
            .to_string()
        )))).await;
        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
    });
    Ok(WorkbuddyUpstreamStream { rx })
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// native 优先 + OpenCode 兜底编排（契约 §3.2）
// ---------------------------------------------------------------------------

/// wbServedBy 来源标记：native ACP 内置模型链路。
pub const SERVED_BY_NATIVE: &str = "workbuddy-native";
/// wbServedBy 来源标记：OpenCode 借道兜底链路。
pub const SERVED_BY_OPENCODE_FALLBACK: &str = "opencode-fallback";

/// native-first 对话结果：来源标记 + 上游产物（流式/非流式统一 mpsc 字节流形态）。
pub enum NativeFirstChat {
    /// 非流式：聚合后的 OpenAI chat.completion（调用方可再补 wbServedBy 字段）。
    Json {
        served_by: &'static str,
        value: Value,
    },
    /// 流式：OpenAI chunk SSE 字节流（调用方负责 `x-wb-served-by` 响应头）。
    Stream {
        served_by: &'static str,
        rx: tokio::sync::mpsc::Receiver<Result<Bytes, String>>,
    },
}

/// native 腿瞬态上游错误的有界重试参数。
///
/// 2026-09-25 17:17 实测案例：宿主 prompt 路径出现 ~32s 坏窗口，窗内 12 个
/// 内置实模连吃 `ACP session/prompt 返回 500`（窗外 18/18 恢复）。提交期失败
/// 时宿主未产出任何内容，fresh session 重发无重复投递；重试 4 次（退避
/// 3/6/9/12s）+ 单次尝试耗时可覆盖 ~50s 窗口。流式请求 pump 失败在流中后置
/// 显现，不经过本层（见函数文档「已知边界」）。
const NATIVE_RETRY_MAX: usize = 4;
const NATIVE_RETRY_BACKOFF_MS: [u64; NATIVE_RETRY_MAX] = [3_000, 6_000, 9_000, 12_000];

/// 「宿主 prompt 提交期 500（UnknownError 族）」判定：只重试这一族瞬态错误。
/// 门禁/无活跃会话（503）应尽快走兜底；400 请求自身错；其余错误原路返回。
fn is_native_transient_error(msg: &str) -> bool {
    (msg.contains("session/prompt")
        && (msg.contains("Internal Server Error") || msg.contains("UnknownError")))
        || msg.contains("提交消息失败 500 Internal Server Error")
}

/// native 失败后是否还落 OpenCode 兜底。
///
/// 请求 model ∈ 目录（tier/model/custom）→ **不兜底**：OpenCode 腿对裸
/// WorkBuddy id 按 provider="opencode" 解析必失败（500 UnknownError），既
/// 浪费一次调用又用兜底错误掩盖 native 真实错误（17:17 案例的观测障碍）。
/// 缺省 model（None）→ 兜底 OpenCode 默认模型（WorkBuddy 未开时网关仍可用）；
/// 目录不可用（探测失败）→ 兜底（保持「无活跃实例→兜底」的原有语义）。
fn native_failure_should_fallback(
    requested: Option<&str>,
    catalog: Option<&crate::modules::trae_acp::NativeCatalog>,
    message: &str,
) -> bool {
    // 🔴 会话未隔离（宿主把 API 调用并到用户当前对话）→ **必须**回落。
    // 此时 native 不是「模型不匹配」而是「通道不可安全使用」，与目录内外的
    // 判定无关；继续返回错误只会让调用方拿不到答案，回落才是可用的正解。
    if message.contains(crate::modules::trae_acp::SESSION_REUSE_MARK) {
        return true;
    }
    match (requested, catalog) {
        (None, _) => true,
        (_, None) => true,
        (Some(id), Some(catalog)) => !catalog.contains_id(id),
    }
}

/// 对话补全（OpenAI 兼容）——native 优先 + OpenCode 兜底（契约 §3.2）：
///
/// ① model 为 None（缺省/空/"trae"/"workbuddy" 占位）或 ∈ 目录 ids（含
///    tier/custom）→ native ACP（connect → initialize → session/new →
///    set_mode → set_model → prompt）；提交期瞬态上游 500 在本层有界重试
///    （NATIVE_RETRY_MAX，fresh session 重发）；
/// ② model ∉ 目录 ids（目录可用时）→ **直接落 OpenCode 兜底**（不试 native：
///    set_model 对未知 ID 假成功，不能靠它报错）；
/// ③ native attempt 失败（503 门禁/无活跃会话 / 502 ACP 链路失败）→ 缺省
///    model 或目录不可用时落 OpenCode 兜底；**model ∈ 目录时不兜底**（裸 id
///    兜底必失败且掩盖 native 真实错误，见 `native_failure_should_fallback`）；
///    400 请求自身错不兜底；
/// ④ 兜底也失败 → `Err((502, 现有错误文案))`。
///
/// 已知边界：`stream:true` 时 native 的 pump 失败在流中后置显现（SSE 错误帧），
/// 无法事后切兜底或重试；同步期失败（门禁/发现/400）仍按 ②③ 正常兜底。
pub async fn chat_completions_native_first(body: Value) -> Result<NativeFirstChat, (u16, String)> {
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let requested = crate::modules::trae_acp::native_request_model(&body);
    // 目录判定：走 §3.1 同源 TTL 缓存，不额外打上游。
    let catalog = crate::modules::trae_acp::native_models_catalog()
        .await
        .ok();
    if !should_try_native(catalog.as_ref(), requested.as_deref()) {
        // ② 请求模型不在内置目录 → 直接 OpenCode 兜底。
        return opencode_fallback(body, stream).await;
    }
    // ① native ACP 尝试（瞬态上游 500 有界重试：提交期失败无内容产出，重发安全）。
    let mut retries = 0usize;
    let native = loop {
        match crate::modules::trae_acp::chat_completions(body.clone()).await {
            Ok(trae_acp_chat) => break Ok(trae_acp_chat),
            // 400 = 请求自身错（如 messages 为空），兜底也一样失败，直接透传。
            Err((400, message)) => break Err((400, message)),
            Err((code, message)) => {
                if retries < NATIVE_RETRY_MAX
                    && code >= 500
                    && is_native_transient_error(&message)
                {
                    retries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        NATIVE_RETRY_BACKOFF_MS[retries - 1],
                    ))
                    .await;
                    continue;
                }
                let message = if retries > 0 {
                    format!("{message}（瞬态上游错误已重试 {retries} 次仍未恢复）")
                } else {
                    message
                };
                break Err((code, message));
            }
        }
    };
    match native {
        Ok(crate::modules::trae_acp::TraeAcpChat::Json(mut value)) => {
            // 重试恢复的成功请求带上次数，便于在响应侧观测坏窗口。
            if retries > 0 {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("wbNativeRetries".to_string(), json!(retries));
                }
            }
            Ok(NativeFirstChat::Json {
                served_by: SERVED_BY_NATIVE,
                value,
            })
        }
        Ok(crate::modules::trae_acp::TraeAcpChat::Stream(upstream)) => Ok(NativeFirstChat::Stream {
            served_by: SERVED_BY_NATIVE,
            rx: upstream.rx,
        }),
        // ③ native 失败 → 缺省 model / 目录不可用时兜底；目录内显式 model 不兜底。
        //    例外：会话未隔离（SESSION_REUSE_MARK）**一律兜底**（见该函数文档）。
        Err(err) => {
            if native_failure_should_fallback(requested.as_deref(), catalog.as_ref(), &err.1) {
                opencode_fallback(body, stream).await
            } else {
                Err(err)
            }
        }
    }
}

/// OpenCode 兜底（契约 §3.2③）：请求 model 原样透传（含 `provider/model` 形式）；
/// model 缺省（含占位值视同 None）时沿用 OpenCode 链路现有默认模型。
/// 失败 → `Err((502, ...))`（沿用现有错误文案）。
async fn opencode_fallback(body: Value, stream: bool) -> Result<NativeFirstChat, (u16, String)> {
    let mut body = body;
    if crate::modules::trae_acp::native_request_model(&body).is_none() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".to_string(), Value::Null);
        }
    }
    if stream {
        let rx = chat_completions_stream(body)
            .await
            .map(|upstream| upstream.rx)
            .map_err(|e| (status_502(), e))?;
        Ok(NativeFirstChat::Stream {
            served_by: SERVED_BY_OPENCODE_FALLBACK,
            rx,
        })
    } else {
        let value = chat_completions_json(body)
            .await
            .map_err(|e| (status_502(), e))?;
        Ok(NativeFirstChat::Json {
            served_by: SERVED_BY_OPENCODE_FALLBACK,
            value,
        })
    }
}

/// 502（core 不依赖 axum，用数值；与 trae_acp::status_code::BAD_GATEWAY 同值）。
fn status_502() -> u16 {
    502
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_ps_script_satisfies_powershell_quoting_rules() {
        // 🔴 两条易碎约束（都在 2026-10-13 实测踩过），改动脚本前必须仍满足：
        // ① 不得含内层双引号 —— PowerShell 的 `-Command <string>` 会把内层 `"` 剥掉，
        //    `-Filter "Name='x'"` 退化成 `-Filter Name='x'` ⇒ `Get-CimInstance : 无效查询`。
        assert!(
            !DISCOVER_PS_SCRIPT.contains('"'),
            "脚本内不得出现双引号（会被 -Command 剥掉）：{DISCOVER_PS_SCRIPT}"
        );
        // ② 必须按**进程名**筛，而不是在命令行里匹配 `opencode\\s+serve` ——
        //    实际命令行 `"E:\\opencode\\opencode.exe"  serve …` 里的 `.exe` 会挡住 `\\s+`，
        //    后者实测输出 0 行（这正是本常量存在的理由）。
        assert!(
            DISCOVER_PS_SCRIPT.contains("$_.Name -eq 'opencode.exe'"),
            "必须按进程名筛选：{DISCOVER_PS_SCRIPT}"
        );
        assert!(DISCOVER_PS_SCRIPT.contains("'serve'"));
        assert!(
            !DISCOVER_PS_SCRIPT.contains(r"opencode\s+serve"),
            "不得退回那个匹配不上真实命令行的正则"
        );
    }

    #[test]
    fn port_from_cmdline_handles_space_and_equals_forms() {
        // 🔴 本用例固化 2026-10-13 修掉的两个长期缺陷。**真实命令行原样**贴在下面，
        // 改动 `discover_server` 的筛选/解析前先跑它——这两个写法都是实测存在的。
        // ① 空格式（start-serve-4097.bat 实际启动形态；注意 exe 路径被引号包住、
        //    且引号与 serve 之间是两个空格）：
        let real = r#""E:\opencode\opencode.exe"  serve --hostname 127.0.0.1 --port 4097 --print-logs"#;
        assert_eq!(port_from_cmdline(real), Some(4097), "空格式 --port 4097 必须取到");

        // ② 等号式（旧代码只认这种）：
        assert_eq!(
            port_from_cmdline("opencode.exe serve --port=4098"),
            Some(4098),
            "等号式 --port=… 仍需兼容"
        );

        // ③ 无端口 / 非法端口 / 空行 ⇒ None（不得 panic，也不得瞎猜成 0）。
        assert_eq!(port_from_cmdline("opencode.exe serve"), None);
        assert_eq!(port_from_cmdline("opencode.exe serve --port abc"), None);
        assert_eq!(port_from_cmdline("opencode.exe serve --port 99999"), None);
        assert_eq!(port_from_cmdline(""), None);
        // ④ `--port` 出现在行尾（没有值）⇒ None，不得越界。
        assert_eq!(port_from_cmdline("opencode.exe serve --port"), None);
        // ⑤ 引号包裹的端口值（某些启动方式会把值也加上引号）。
        assert_eq!(port_from_cmdline("opencode.exe serve --port \"4100\""), Some(4100));
    }

    #[test]
    fn pick_score_prefers_free_then_speed() {
        // 免费优先（与 opencode::is_free_model 同口径）
        assert_eq!(pick_score("mimo-v2.6-flash-free"), 0);
        assert_eq!(pick_score("some/model:free"), 0);
        assert_eq!(pick_score("qwen/qwen3-8b:0"), 0);
        // 其次快速档
        assert_eq!(pick_score("deepseek-v4-flash"), 1);
        assert_eq!(pick_score("glm-5.3-turbo"), 1);
        assert_eq!(pick_score("MiniMax-M2-Lightning"), 1);
        // 其余
        assert_eq!(pick_score("claude-opus-4-8"), 2);
        assert_eq!(pick_score("kimi-k3-1"), 2);
    }

    #[test]
    fn pick_score_is_total_and_deterministic() {
        // 排序必须能给出全序：免费 < 快速 < 其它，同档按名称
        let mut ids = vec![
            "z-plain",
            "a-free-model",
            "b-flash",
            "a-free-2",
        ];
        ids.sort_by(|a, b| pick_score(a).cmp(&pick_score(b)).then_with(|| a.cmp(b)));
        assert_eq!(ids, vec!["a-free-2", "a-free-model", "b-flash", "z-plain"]);
    }

    #[test]
    fn builtin_models_payload_always_has_exactly_one_entry() {
        let v = builtin_models_payload();
        let data = v.get("data").and_then(Value::as_array).unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].get("owned_by").and_then(Value::as_str), Some("workbuddy"));
        assert!(!data[0].get("id").and_then(Value::as_str).unwrap_or_default().is_empty());
    }

    #[test]
    fn resolve_desktop_region_matches_settings_and_accounts() {
        // settings.json 的 legacyOwnerUid 实测嵌在 claw 下（根层读取恒 unknown）。
        let settings = r#"{"claw":{"legacyOwnerUid":"05efd60f-3079-41a8-ab31-745eb9c09a40"}}"#;
        let accounts = r#"[
            {"uid":"05efd60f-3079-41a8-ab31-745eb9c09a40","domain":"www.workbuddy.cn"},
            {"uid":"cbea9317-a9df-4e81-809b-1162a4e6be40","domain":"www.workbuddy.ai"}
        ]"#;
        assert_eq!(resolve_desktop_region(settings, accounts), "cn");

        // 桌面端登录国际账号 → intl。
        let settings_intl =
            r#"{"claw":{"legacyOwnerUid":"cbea9317-a9df-4e81-809b-1162a4e6be40"}}"#;
        assert_eq!(resolve_desktop_region(settings_intl, accounts), "intl");

        // owner 缺失 / 账号不匹配 / 坏 JSON → unknown。
        assert_eq!(resolve_desktop_region("{}", accounts), "unknown");
        assert_eq!(
            resolve_desktop_region(
                r#"{"claw":{"legacyOwnerUid":"nobody"}}"#,
                accounts
            ),
            "unknown"
        );
        assert_eq!(resolve_desktop_region("not-json", accounts), "unknown");
    }

    #[test]
    fn static_fallback_models_payload_shape() {
        // 契约 §3.1：ACP 不可达 → builtin 目录 + static-fallback 三件套，目录永不为空。
        let v = static_fallback_models_payload();
        assert_eq!(v.get("object").and_then(Value::as_str), Some("list"));
        assert_eq!(v.get("source").and_then(Value::as_str), Some("static-fallback"));
        assert!(v.get("probedAt").map(Value::is_null).unwrap_or(false));
        assert!(v.get("currentModelId").map(Value::is_null).unwrap_or(false));
        let data = v.get("data").and_then(Value::as_array).unwrap();
        assert_eq!(data.len(), 1, "static 回落沿用 builtin_models_payload（恒 1 条）");
        assert_eq!(data[0].get("owned_by").and_then(Value::as_str), Some("workbuddy"));
    }

    #[test]
    fn catalog_to_payload_live_and_cache_shapes() {
        let catalog = crate::modules::trae_acp::NativeCatalog {
            models: vec![
                crate::modules::trae_acp::NativeModel {
                    id: "fast-model".into(),
                    name: "快速".into(),
                    multiplier: Some(0.21),
                    kind: "tier",
                    exposed: true,
                },
                crate::modules::trae_acp::NativeModel {
                    id: "glm-5.3-flash".into(),
                    name: "GLM-5.3-Flash".into(),
                    multiplier: Some(0.06),
                    kind: "model",
                    exposed: true,
                },
                // 契约 §2.1：产品配置补充实模（ACP 未暴露）默认也要列出。
                crate::modules::trae_acp::NativeModel {
                    id: "kimi-k3-1".into(),
                    name: "kimi-k3-1".into(),
                    multiplier: None,
                    kind: "model",
                    exposed: false,
                },
                crate::modules::trae_acp::NativeModel {
                    id: "custom-local:qwen3.8-flash".into(),
                    name: "自定义".into(),
                    multiplier: None,
                    kind: "custom",
                    exposed: true,
                },
            ],
            current_model_id: "fast-model".into(),
            probed_at: now_ms(),
            port: 56599,
        };
        // 刚探测完 → acp-live；默认列出 tier|model（含 exposed=false 补充实模），不含 custom。
        let v = catalog_to_payload(&catalog, false);
        assert_eq!(v.get("object").and_then(Value::as_str), Some("list"));
        assert_eq!(v.get("source").and_then(Value::as_str), Some("acp-live"));
        assert_eq!(v.get("currentModelId").and_then(Value::as_str), Some("fast-model"));
        let data = v.get("data").and_then(Value::as_array).unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(data[0].get("id").and_then(Value::as_str), Some("fast-model"));
        assert_eq!(data[0].get("kind").and_then(Value::as_str), Some("tier"));
        assert_eq!(data[0].get("multiplier").and_then(Value::as_f64), Some(0.21));
        assert_eq!(data[0].get("owned_by").and_then(Value::as_str), Some("workbuddy"));
        assert_eq!(data[0].get("exposed").and_then(Value::as_bool), Some(true));
        assert_eq!(data[1].get("multiplier").and_then(Value::as_f64), Some(0.06));
        // 补充实模（exposed=false）在默认列表中，带 exposed 标记。
        assert_eq!(data[2].get("id").and_then(Value::as_str), Some("kimi-k3-1"));
        assert_eq!(data[2].get("exposed").and_then(Value::as_bool), Some(false));
        assert_eq!(data[2].get("kind").and_then(Value::as_str), Some("model"));
        assert!(data[2].get("multiplier").map(Value::is_null).unwrap_or(false));
        assert!(
            !data.iter().any(|m| m.get("kind").and_then(Value::as_str) == Some("custom")),
            "默认列表不得含 custom 条目"
        );
        // include=custom：追加 custom 条目（owned_by=workbuddy-custom，multiplier null）。
        let v = catalog_to_payload(&catalog, true);
        let data = v.get("data").and_then(Value::as_array).unwrap();
        assert_eq!(data.len(), 4);
        assert_eq!(data[3].get("id").and_then(Value::as_str), Some("custom-local:qwen3.8-flash"));
        assert_eq!(data[3].get("owned_by").and_then(Value::as_str), Some("workbuddy-custom"));
        assert_eq!(data[3].get("kind").and_then(Value::as_str), Some("custom"));
        assert!(data[3].get("multiplier").map(Value::is_null).unwrap_or(false));
        assert_eq!(data[3].get("exposed").and_then(Value::as_bool), Some(true));
        // 缓存命中（probed_at 超过「刚探测完」窗口）→ acp-cache。
        let mut stale = catalog.clone();
        stale.probed_at = now_ms() - 10_000;
        let v = catalog_to_payload(&stale, false);
        assert_eq!(v.get("source").and_then(Value::as_str), Some("acp-cache"));
        // currentModelId 缺失（空串）→ null。
        let mut no_current = catalog.clone();
        no_current.current_model_id = String::new();
        let v = catalog_to_payload(&no_current, false);
        assert!(v.get("currentModelId").map(Value::is_null).unwrap_or(false));
    }

    #[test]
    fn should_try_native_matrix() {
        let catalog = crate::modules::trae_acp::NativeCatalog {
            models: vec![crate::modules::trae_acp::NativeModel {
                id: "glm-5.3-flash".into(),
                name: "GLM-5.3-Flash".into(),
                multiplier: Some(0.06),
                kind: "model",
                exposed: true,
            }],
            current_model_id: "fast-model".into(),
            probed_at: now_ms(),
            port: 1,
        };
        // ① 缺省 → native。
        assert!(should_try_native(Some(&catalog), None));
        assert!(should_try_native(None, None));
        // ① 目录可用且模型在目录 → native（tier/model/custom 均按目录判断）。
        assert!(should_try_native(Some(&catalog), Some("glm-5.3-flash")));
        // ② 目录可用且模型不在目录 → 直接兜底。
        assert!(!should_try_native(Some(&catalog), Some("opencode/mimo-v2.6-flash-free")));
        assert!(!should_try_native(Some(&catalog), Some("totally-fake-model")));
        // 目录不可用 → 仍先试 native（失败走 ③）。
        assert!(should_try_native(None, Some("opencode/mimo-v2.6-flash-free")));
    }

    #[test]
    fn native_transient_error_predicate() {
        // 2026-09-25 17:17 案例的 native 腿真实文案（prompt 提交期 500）。
        assert!(is_native_transient_error(
            "ACP session/prompt 返回 500 Internal Server Error: {\"name\":\"UnknownError\",\"data\":{\"message\":\"Unexpected server error.\"}}"
        ));
        assert!(is_native_transient_error(
            "ACP session/prompt 返回 502 Bad Gateway: {\"name\":\"UnknownError\"}"
        ));
        // 非瞬态：门禁/无会话/请求错/流中失败，均不重试。
        assert!(!is_native_transient_error(
            "未发现运行中的 TRAE 会话（~/.workbuddy/sessions 注册表 5 分钟心跳内无活跃条目）"
        ));
        assert!(!is_native_transient_error(
            "TRAE ACP 对话转发未开放（acp_enabled 未开启）"
        ));
        assert!(!is_native_transient_error("messages 不能为空"));
        assert!(!is_native_transient_error(
            "提交消息失败: error sending request"
        ));
    }

    #[test]
    fn native_failure_fallback_policy() {
        let catalog = crate::modules::trae_acp::NativeCatalog {
            models: vec![crate::modules::trae_acp::NativeModel {
                id: "glm-5.3-flash".into(),
                name: "GLM-5.3-Flash".into(),
                multiplier: Some(0.06),
                kind: "model",
                exposed: true,
            }],
            current_model_id: "fast-model".into(),
            probed_at: now_ms(),
            port: 1,
        };
        // 目录内显式 model → 不兜底（裸 id 兜底必失败且掩盖 native 真实错误）。
        assert!(!native_failure_should_fallback(
            Some("glm-5.3-flash"),
            Some(&catalog),
            "无活跃实例"
        ));
        // 缺省 model → 兜底 OpenCode 默认（WorkBuddy 未开时网关仍可用）。
        assert!(native_failure_should_fallback(None, Some(&catalog), "无活跃实例"));
        // 目录不可用 → 兜底（保持「无活跃实例→兜底」语义）。
        assert!(native_failure_should_fallback(
            Some("glm-5.3-flash"),
            None,
            "无活跃实例"
        ));
        assert!(native_failure_should_fallback(None, None, "无活跃实例"));
        // 目录可用但 id ∉ 目录 → 兜底（该路径本就 ② 直落兜底，防御性覆盖）。
        assert!(native_failure_should_fallback(
            Some("opencode/mimo-v2.6-flash-free"),
            Some(&catalog),
            "无活跃实例"
        ));
        // 🔴 会话未隔离 → **目录内显式 model 也必须兜底**（否则调用方拿不到答案，
        // 且 native 会继续把请求注入用户对话）。
        let reuse_msg = format!(
            "{}：session/new 返回的会话与宿主正在使用的会话相同",
            crate::modules::trae_acp::SESSION_REUSE_MARK
        );
        assert!(native_failure_should_fallback(
            Some("glm-5.3-flash"),
            Some(&catalog),
            &reuse_msg
        ));
    }
}
