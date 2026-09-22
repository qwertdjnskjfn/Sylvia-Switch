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
    CLIENT.get_or_init(|| reqwest::Client::new())
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
    if let Ok(out) = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'opencode\\s+serve' } | ForEach-Object { $_.CommandLine }",
        ])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            for token in line.split_whitespace() {
                if let Some(port_str) = token.strip_prefix("--port=") {
                    if let Ok(port) = port_str.parse::<u16>() {
                        if is_opencode_server(port).await {
                            note_discovery_success(port);
                            return Ok(port);
                        }
                    }
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
fn is_connect_error(msg: &str) -> bool {
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
pub async fn chat_completions_json(body: Value) -> Result<Value, String> {
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
    // 2) 提交消息（fire-and-poll：POST 立即返回，回复异步产出）。
    let payload = json!({
        "parts": [{"type": "text", "text": prompt}],
        "model": {"providerID": provider_id, "modelID": model_id},
        "agent": "build",
    });
    let submit = client
        .post(format!("{base}/session/{session_id}/message"))
        .json(&payload)
        .timeout(std::time::Duration::from_secs(15))
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
