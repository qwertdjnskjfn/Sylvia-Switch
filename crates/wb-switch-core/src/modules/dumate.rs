//! DuMate（百度搭子）渠道核心模块（契约 docs/dumate-channel-contract.md §0/§2）。
//!
//! DuMate 是百度的桌面 AI 办公智能体（Electron），内嵌 OpenCode 引擎
//! （`dumate-opencode.exe`，动态端口）。本模块提供：
//! - **引擎发现**：读进程环境变量（`DUMATE_HOST_URL` / `OPENCODE_SERVER_PORT` /
//!   `DUMATE_INAPP_KEY`），按「进程环境 → 本进程环境变量 → 配置文件 → 默认端口
//!   52795」覆盖；请求时惰性发现 + 60s 缓存 + 30s 负缓存，零自动触发
//!   （不 spawn 常驻循环，DuMate 桌面端由用户自行启动）；
//! - **会话反代**：`/dumate/v1` OpenAI 兼容网关的数据层。`POST /session` →
//!   `POST /session/{id}/message`（**同步阻塞直接返回最终 assistant 消息**，
//!   实测勿做轮询）→ `DELETE /session/{id}` 清理；档位头在**消息体 headers
//!   字段**（非 HTTP 头）：lite=L0 / turbo=L1 / ultra=L2；
//! - **积分查询**：console.bce.baidu.com quota_overview（凭据解密见
//!   [`crate::modules::dumate_credentials`]，csrftoken 取 bce-user-info cookie 值）；
//! - **静态模型目录**（4 个）+ 状态聚合 + 配置读写。

use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::modules::config::{atomic_write, http_request_raw, now_ms, store_dir};
use crate::modules::dumate_credentials;
use crate::modules::glm_proxy;
use crate::modules::workbuddy_proxy;

// ---------------------------------------------------------------------------
// 常量（契约 §0 实测）
// ---------------------------------------------------------------------------

pub const MODEL_LITE: &str = "dumate-lite";
pub const MODEL_TURBO: &str = "dumate-turbo";
pub const MODEL_ULTRA: &str = "dumate-ultra";
pub const MODEL_POINTS: &str = "dumate-points";

/// 静态模型目录：(模型 ID, 档位, 说明)。
const MODELS: [(&str, &str, &str); 4] = [
    (MODEL_LITE, "L0", "极速档，日常对话"),
    (MODEL_TURBO, "L1", "增强档，内容与代码"),
    (MODEL_ULTRA, "L2", "专业档，深度推理"),
    (MODEL_POINTS, "", "积分查询（直接返回积分概览，不消耗模型积分）"),
];

/// 引擎默认端口（进程环境 / 配置都未命中时的最后探活位）。
const DEFAULT_ENGINE_PORT: u16 = 52795;

/// 消息提交超时（秒）：POST message 同步阻塞到生成完成，超时必须覆盖
/// 模型生成时长（契约：lite 120s / turbo 240s / ultra 300s）。
fn message_timeout_secs(model: &str) -> u64 {
    match model {
        MODEL_LITE => 120,
        MODEL_TURBO => 240,
        _ => 300,
    }
}

/// 档位映射（消息体 headers 字段 `X-Dumate-AutoModel-Level`，不是 HTTP 头）。
fn level_for_model(model: &str) -> &'static str {
    match model {
        MODEL_LITE => "L0",
        MODEL_TURBO => "L1",
        _ => "L2",
    }
}

// ---------------------------------------------------------------------------
// 对话错误（契约 §2.2：kind 枚举 + 中文人话）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumateChatErrorKind {
    EngineNotFound,
    EngineError,
    Timeout,
    UpstreamRejected,
}

impl DumateChatErrorKind {
    /// 机器可读标签（API 层错误分类用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EngineNotFound => "engine_not_found",
            Self::EngineError => "engine_error",
            Self::Timeout => "timeout",
            Self::UpstreamRejected => "upstream_rejected",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::EngineNotFound => "引擎未发现",
            Self::EngineError => "引擎错误",
            Self::Timeout => "超时",
            Self::UpstreamRejected => "上游拒绝",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DumateChatError {
    pub kind: DumateChatErrorKind,
    pub message: String,
}

impl DumateChatError {
    pub fn new(kind: DumateChatErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DumateChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "DuMate 对话失败（{}）：{}",
            self.kind.label(),
            self.message
        )
    }
}

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/dumate_proxy_config.json）—— 与其它网关密钥彻底分开
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DumateProxyConfig {
    pub enabled: bool,
    pub api_key: String,
    /// 引擎静态地址（空 = 自动发现；优先级低于进程环境）。
    pub opencode_url: String,
    /// 引擎 inapp key 静态值（空 = 从进程环境读取）。
    pub inapp_key: String,
}

impl Default for DumateProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key: String::new(),
            opencode_url: String::new(),
            inapp_key: String::new(),
        }
    }
}

fn config_path() -> PathBuf {
    store_dir().join("dumate_proxy_config.json")
}

/// 解析配置 JSON（纯函数，便于单测；缺省字段取默认值）。
fn parse_config_json(v: &Value) -> DumateProxyConfig {
    DumateProxyConfig {
        enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        api_key: v
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        opencode_url: v
            .get("opencode_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        inapp_key: v
            .get("inapp_key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

pub fn load_config() -> DumateProxyConfig {
    let mut cfg = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| parse_config_json(&v))
        .unwrap_or_default();
    // 首次使用自动生成密钥并落盘（对齐 opencode 网关范式）。
    if cfg.api_key.is_empty() {
        cfg.api_key = glm_proxy::generate_api_key();
        let _ = save_config(&cfg);
    }
    cfg
}

pub fn save_config(cfg: &DumateProxyConfig) -> Result<(), String> {
    let body = json!({
        "enabled": cfg.enabled,
        "api_key": cfg.api_key,
        "opencode_url": cfg.opencode_url,
        "inapp_key": cfg.inapp_key,
        "updatedAt": now_ms(),
    });
    atomic_write(&config_path(), &body.to_string())
        .map_err(|e| format!("写入 DuMate 配置失败: {e}"))
}

/// 配置脱敏展示（api_key 掩码；对齐前端 DumateConfigInfo 形态）。
pub fn config_info() -> Value {
    let cfg = load_config();
    json!({
        "enabled": cfg.enabled,
        "apiKey": mask_secret(&cfg.api_key),
        "endpoint": "/dumate/v1",
        "note": "独立密钥，与 WorkBuddy / OpenCode / TRAE 网关不共用；鉴权支持本密钥或主网关密钥",
        "opencodeUrl": cfg.opencode_url,
        "inappKeyPreview": preview_key(&cfg.inapp_key),
    })
}

// ---------------------------------------------------------------------------
// 模型归一化（契约 §0：静态 4 模型 + 客户端友好映射）
// ---------------------------------------------------------------------------

/// 归一化请求模型名：`dumate-lite/turbo/ultra/points` 及大小写/连字符变体归一；
/// `*haiku*`→lite、`*sonnet*`→turbo、`*opus*`→ultra；未识别默认 lite。
pub fn normalize_model(model: &str) -> &'static str {
    let compact: String = model
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    match compact.as_str() {
        "dumatelite" | "lite" => return MODEL_LITE,
        "dumateturbo" | "turbo" => return MODEL_TURBO,
        "dumateultra" | "ultra" => return MODEL_ULTRA,
        "dumatepoints" | "points" => return MODEL_POINTS,
        _ => {}
    }
    let l = model.to_ascii_lowercase();
    if l.contains("haiku") {
        MODEL_LITE
    } else if l.contains("sonnet") {
        MODEL_TURBO
    } else if l.contains("opus") {
        MODEL_ULTRA
    } else {
        MODEL_LITE
    }
}

// ---------------------------------------------------------------------------
// 引擎发现（惰性 + 缓存；零自动触发）
// ---------------------------------------------------------------------------

/// 发现到的引擎：基址 + inapp key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumateEngine {
    pub base_url: String,
    pub inapp_key: String,
}

/// 发现成功缓存（60s TTL）。
static ENGINE_CACHE: Mutex<Option<(i64, DumateEngine)>> = Mutex::new(None);
const ENGINE_TTL_MS: i64 = 60 * 1000;
/// 发现失败负缓存（30s）：失败后短窗内直接快速返回，避免每请求重跑整轮发现。
static ENGINE_NEG_CACHE: Mutex<Option<i64>> = Mutex::new(None);
const ENGINE_NEG_TTL_MS: i64 = 30 * 1000;

fn neg_cache_active() -> bool {
    let guard = ENGINE_NEG_CACHE.lock().unwrap();
    guard
        .as_ref()
        .map(|at| now_ms() - *at < ENGINE_NEG_TTL_MS)
        .unwrap_or(false)
}

fn note_discovery_success(engine: DumateEngine) {
    {
        let mut guard = ENGINE_CACHE.lock().unwrap();
        *guard = Some((now_ms(), engine));
    }
    let mut guard = ENGINE_NEG_CACHE.lock().unwrap();
    *guard = None;
}

fn note_discovery_failure() {
    let mut guard = ENGINE_NEG_CACHE.lock().unwrap();
    *guard = Some(now_ms());
}

/// 作废成功缓存（连接失败重发现路径用）。
pub fn invalidate_engine_cache() {
    let mut guard = ENGINE_CACHE.lock().unwrap();
    *guard = None;
}

/// 归一化引擎基址：去空白/尾斜杠，缺协议头时补 http://（引擎只在本机回环）。
fn normalize_base_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if u.is_empty() {
        return String::new();
    }
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("http://{u}")
    }
}

/// 从进程环境变量表提取 `(base_url, inapp_key)`：
/// 取 `DUMATE_HOST_URL`（如 `http://127.0.0.1:52002`）或 `OPENCODE_SERVER_PORT`，
/// `DUMATE_INAPP_KEY`（64 字符）。实测 main-server 无 HOST_URL、opencode 有。
#[cfg_attr(not(windows), allow(dead_code))]
fn env_engine_from_map(env: &[(String, String)]) -> Option<(String, String)> {
    let get = |name: &str| -> Option<String> {
        env.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let key = get("DUMATE_INAPP_KEY").unwrap_or_default();
    if let Some(url) = get("DUMATE_HOST_URL") {
        let base = normalize_base_url(&url);
        if !base.is_empty() {
            return Some((base, key));
        }
    }
    if let Some(port) = get("OPENCODE_SERVER_PORT") {
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            return Some((format!("http://127.0.0.1:{port}"), key));
        }
    }
    None
}

/// 引擎探活：`GET /global/health` → `{"healthy":true,...}`（3s 超时）。
pub async fn engine_alive(base: &str, key: &str) -> bool {
    let url = format!("{}/global/health", base.trim_end_matches('/'));
    let mut req = http_client()
        .get(&url)
        .timeout(Duration::from_secs(3));
    if !key.is_empty() {
        req = req.header("X-Dumate-Inapp-Key", key);
    }
    match req.send().await {
        Ok(resp) => resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("healthy").and_then(Value::as_bool).map(|b| b == true))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// 引擎发现（契约 §0 覆盖顺序）：进程环境 → 本进程环境变量
/// `DUMATE_OPENCODE_URL`/`DUMATE_INAPP_KEY` → 配置文件静态值 → 默认端口
/// 52795 探活。60s 成功缓存 + 30s 负缓存；请求时惰性调用，无任何常驻任务。
pub async fn discover_engine() -> Result<DumateEngine, String> {
    {
        let guard = ENGINE_CACHE.lock().unwrap();
        if let Some((at, engine)) = guard.as_ref() {
            if now_ms() - *at < ENGINE_TTL_MS {
                return Ok(engine.clone());
            }
        }
    }
    if neg_cache_active() {
        return Err(
            "未发现运行中的 DuMate OpenCode 引擎（30s 内刚探测失败，处于负缓存期）。\
             请确认 DuMate 桌面客户端已启动并完成登录"
                .to_string(),
        );
    }

    // 1) 进程环境（实测：main-server 无 HOST_URL，opencode 有 → 两个进程都要试）。
    if let Some(engine) = discover_from_processes().await {
        note_discovery_success(engine.clone());
        return Ok(engine);
    }

    // 2) 本进程环境变量覆盖（部署机直配场景）。
    let own_url = std::env::var("DUMATE_OPENCODE_URL")
        .ok()
        .map(|s| normalize_base_url(&s))
        .filter(|s| !s.is_empty());
    let own_key = std::env::var("DUMATE_INAPP_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if let Some(base) = own_url {
        if engine_alive(&base, &own_key).await {
            let engine = DumateEngine {
                base_url: base,
                inapp_key: own_key,
            };
            note_discovery_success(engine.clone());
            return Ok(engine);
        }
    }

    // 3) 配置文件静态值。
    let cfg = load_config();
    let cfg_url = normalize_base_url(&cfg.opencode_url);
    if !cfg_url.is_empty() && engine_alive(&cfg_url, &cfg.inapp_key).await {
        let engine = DumateEngine {
            base_url: cfg_url,
            inapp_key: cfg.inapp_key,
        };
        note_discovery_success(engine.clone());
        return Ok(engine);
    }

    // 4) 默认端口 52795 探活。
    let default_base = format!("http://127.0.0.1:{DEFAULT_ENGINE_PORT}");
    if engine_alive(&default_base, &cfg.inapp_key).await {
        let engine = DumateEngine {
            base_url: default_base,
            inapp_key: cfg.inapp_key,
        };
        note_discovery_success(engine.clone());
        return Ok(engine);
    }

    note_discovery_failure();
    Err(
        "未发现运行中的 DuMate OpenCode 引擎（进程环境 / 环境变量 / 配置 / 默认端口 52795 均未命中）。\
         请先打开 DuMate 桌面客户端并完成登录"
            .to_string(),
    )
}

#[cfg(windows)]
async fn discover_from_processes() -> Option<DumateEngine> {
    let mut diag: Vec<String> = Vec::new();
    for image in ["dumate-opencode.exe", "dumate-main-server.exe"] {
        match process_env_by_image(image) {
            Ok(env) => match env_engine_from_map(&env) {
                Some((base, key)) => {
                    if engine_alive(&base, &key).await {
                        return Some(DumateEngine {
                            base_url: base,
                            inapp_key: key,
                        });
                    }
                    diag.push(format!("{image}: 探活失败 {base}（进程环境齐全）"));
                }
                None => diag.push(format!(
                    "{image}: 读到 {} 项环境变量但缺 HOST_URL/PORT/KEY 组合",
                    env.len()
                )),
            },
            Err(e) => diag.push(e),
        }
    }
    *DISCOVERY_DIAG.lock().unwrap() = diag.join("；");
    None
}

#[cfg(not(windows))]
async fn discover_from_processes() -> Option<DumateEngine> {
    None
}

/// 最近一轮进程发现的失败明细（诊断用，status 透出 engineDiag）。
static DISCOVERY_DIAG: Mutex<String> = Mutex::new(String::new());

/// 按映像名收集目标进程的环境变量表（首个读取成功的进程生效）。
#[cfg(windows)]
pub(crate) fn process_env_by_image(image: &str) -> Result<Vec<(String, String)>, String> {
    let rows = crate::modules::process::windows_tasklist_image_rows(image);
    if rows.is_empty() {
        return Err(format!("未找到运行中的 {image} 进程"));
    }
    let mut last_err = String::new();
    for row in rows {
        match proc_env::read_process_env(row.pid) {
            Ok(env) if !env.is_empty() => return Ok(env),
            Ok(_) => last_err = format!("{}(pid {}) 环境块为空", image, row.pid),
            Err(e) => last_err = format!("{}(pid {}): {}", image, row.pid, e),
        }
    }
    Err(format!("读取 {image} 进程环境失败: {last_err}"))
}

#[cfg(not(windows))]
pub(crate) fn process_env_by_image(_image: &str) -> Result<Vec<(String, String)>, String> {
    Err("DuMate 进程环境发现仅支持 Windows（读 PEB 环境块）".to_string())
}

/// 读目标进程环境变量的底层实现（仅 Windows，照 `wb_memtoken.rs` 的
/// ReadProcessMemory 模式）。
///
/// 链路（契约 §0 实测）：OpenProcess(QUERY_INFORMATION|VM_READ) →
/// NtQueryInformationProcess(ProcessBasicInformation) → PEB+0x20=ProcessParameters
/// → PP+0x80=Environment 指针 → ReadProcessMemory 读 UTF-16 环境块
/// （≤2MB，NUL 分隔，双 NUL 结束）。
/// `NtQueryInformationProcess` 不在默认导入库内，运行期从 ntdll 解析。
#[cfg(windows)]
mod proc_env {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    /// 环境块读取上限（契约 §0：≤2MB）。
    /// 分块必须小（4KB）：ReadProcessMemory 请求范围一旦含未提交页会**整体失败**，
    /// 32KB 大块会在环境块尾部的提交边界上直接撞墙（2026-10-01 实测 opencode
    /// pid 环境块后即有未提交页，32KB 块读 0 字节；python 4KB 块同场景全量成功）。
    const ENV_MAX_BYTES: usize = 2 * 1024 * 1024;
    const CHUNK_BYTES: usize = 0x1000;

    type NtQueryInformationProcessFn =
        unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;

    // kernel32 默认在链接集合内，无需导入库。
    extern "system" {
        fn LoadLibraryW(lpLibFileName: *const u16) -> isize;
        fn GetProcAddress(hModule: isize, lpProcName: *const u8) -> isize;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(crate) fn read_process_env(pid: u32) -> Result<Vec<(String, String)>, String> {
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) }
                .map_err(|e| format!("OpenProcess({pid}) 失败: {e}"))?;
        let result = read_env_after_open(handle);
        unsafe {
            let _ = CloseHandle(handle);
        }
        result
    }

    fn read_env_after_open(handle: HANDLE) -> Result<Vec<(String, String)>, String> {
        // 1) PEB 地址：NtQueryInformationProcess(ProcessBasicInformation)。
        let peb = query_peb_address(handle)?;
        if peb == 0 {
            return Err("目标进程 PEB 为空（非 x64 用户进程？）".to_string());
        }
        // 2) PEB+0x20 = ProcessParameters（x64）。
        let pp = read_usize(handle, peb + 0x20)?;
        if pp == 0 {
            return Err("目标进程 ProcessParameters 为空".to_string());
        }
        // 3) PP+0x80 = Environment 指针（x64，契约 §0 实测）。
        let env_ptr = read_usize(handle, pp + 0x80)?;
        if env_ptr == 0 {
            return Err("目标进程环境块指针为空".to_string());
        }
        // 4) 读 UTF-16 环境块并解析。
        let data = read_available(handle, env_ptr, ENV_MAX_BYTES)?;
        parse_env_block(&data)
    }

    fn query_peb_address(handle: HANDLE) -> Result<usize, String> {
        unsafe {
            let ntdll = LoadLibraryW(wide("ntdll.dll").as_ptr());
            if ntdll == 0 {
                return Err("加载 ntdll.dll 失败".to_string());
            }
            let proc_addr = GetProcAddress(ntdll, b"NtQueryInformationProcess\0".as_ptr());
            if proc_addr == 0 {
                return Err("解析 NtQueryInformationProcess 失败".to_string());
            }
            let nt_query: NtQueryInformationProcessFn = std::mem::transmute(proc_addr);
            // PROCESS_BASIC_INFORMATION = 0；x64 布局 48 字节，PEB 指针在 +0x08。
            let mut pbi = [0u8; 48];
            let mut ret_len = 0u32;
            let status = nt_query(handle, 0, pbi.as_mut_ptr() as *mut c_void, 48, &mut ret_len);
            if status < 0 {
                return Err(format!(
                    "NtQueryInformationProcess 失败: NTSTATUS 0x{:08X}",
                    status as u32
                ));
            }
            let mut peb_buf = [0u8; 8];
            peb_buf.copy_from_slice(&pbi[8..16]);
            Ok(usize::from_le_bytes(peb_buf))
        }
    }

    fn read_usize(handle: HANDLE, addr: usize) -> Result<usize, String> {
        let mut buf = [0u8; 8];
        read_exact(handle, addr, &mut buf)?;
        Ok(usize::from_le_bytes(buf))
    }

    fn read_exact(handle: HANDLE, addr: usize, buf: &mut [u8]) -> Result<(), String> {
        let got = try_read_bytes(handle, addr, buf);
        if got != buf.len() {
            return Err(format!(
                "ReadProcessMemory(0x{addr:x}) 读取 {got}/{} 字节",
                buf.len()
            ));
        }
        Ok(())
    }

    /// 单次 ReadProcessMemory，返回实际读取字节数（失败返回 0，调用方决定是否容错）。
    fn try_read_bytes(handle: HANDLE, addr: usize, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let mut got = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                handle,
                addr as *const c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                Some(&mut got),
            )
        }
        .is_ok();
        if ok {
            got
        } else {
            0
        }
    }

    /// 从 base 起尽量读满 limit 字节；遇不可读页 / 短读即停（环境块末尾即不可读页）。
    fn read_available(handle: HANDLE, base: usize, limit: usize) -> Result<Vec<u8>, String> {
        let mut data: Vec<u8> = Vec::with_capacity(CHUNK_BYTES);
        let mut chunk = vec![0u8; CHUNK_BYTES];
        loop {
            if data.len() >= limit {
                break;
            }
            let want = chunk.len().min(limit - data.len());
            let got = try_read_bytes(handle, base + data.len(), &mut chunk[..want]);
            if got == 0 {
                break;
            }
            data.extend_from_slice(&chunk[..got]);
            if has_env_terminator(&data) || got < want {
                break;
            }
        }
        if data.is_empty() {
            return Err("进程环境块不可读（区域未提交或权限不足）".to_string());
        }
        Ok(data)
    }

    /// 环境块双 NUL 终止符是否已读入（UTF-16LE：连续两个 0x0000）。
    fn has_env_terminator(data: &[u8]) -> bool {
        let mut prev_zero = false;
        for c in data.chunks_exact(2) {
            let u = u16::from_le_bytes([c[0], c[1]]);
            if u == 0 {
                if prev_zero {
                    return true;
                }
                prev_zero = true;
            } else {
                prev_zero = false;
            }
        }
        false
    }

    /// 解析 UTF-16LE 环境块：条目 `NAME=VALUE` 以单 NUL 分隔、双 NUL 结束。
    fn parse_env_block(data: &[u8]) -> Result<Vec<(String, String)>, String> {
        let mut units: Vec<u16> = data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        // 某些进程的环境块以一个 NUL 起始（重分配痕迹），跳过。
        if units.first() == Some(&0) {
            units.remove(0);
        }
        let mut out = Vec::new();
        let mut cur: Vec<u16> = Vec::new();
        for &u in &units {
            if u == 0 {
                if cur.is_empty() {
                    break; // 双 NUL：环境块结束
                }
                let entry = String::from_utf16_lossy(&cur);
                cur.clear();
                if let Some((name, value)) = entry.split_once('=') {
                    if !name.is_empty() {
                        out.push((name.to_string(), value.to_string()));
                    }
                }
            } else {
                cur.push(u);
            }
        }
        if out.is_empty() {
            return Err("进程环境块为空或未读到完整条目".to_string());
        }
        Ok(out)
    }
}

/// 桌面端是否在运行（tasklist 有 `DuMate.exe` / `dumate-*.exe`）。
pub fn desktop_running() -> bool {
    #[cfg(windows)]
    {
        let Some(out) = crate::modules::process::run_cmd_timeout("tasklist", &["/FO", "CSV", "/NH"], 5)
        else {
            return false;
        };
        crate::modules::process::parse_tasklist_csv(&String::from_utf8_lossy(&out.stdout))
            .iter()
            .any(|row| is_dumate_image(&row.name))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// DuMate 桌面端映像名判定（纯函数，便于单测）。
fn is_dumate_image(name: &str) -> bool {
    let l = name.trim().trim_matches('"').to_ascii_lowercase();
    l == "dumate.exe" || l.starts_with("dumate-")
}

// ---------------------------------------------------------------------------
// console.bce.baidu.com 积分 API（签到接线由 platform_checkin（W2）复用 bce_api）
// ---------------------------------------------------------------------------

/// console.bce.baidu.com API 通用请求（契约 §0 请求头约定）：
/// Cookie（域匹配 + 未过期 + 特殊字符值加引号）+ `csrftoken`（取 bce-user-info
/// cookie 的值）+ `X-Requested-With: XMLHttpRequest` + Origin/Referer。
/// 不跟随重定向（登录失效时上游 302 → 登录页，必须原样暴露给调用方判定）。
/// `proxy`：None/空 = 直连（现状）；Some(url) = 该请求走 `reqwest::Proxy::all`
/// 出站代理（DuMate 账号池「一号一 IP」，契约 unified-accounts §1.3；
/// 代理地址无效时由 `http_request_raw` 返回 status=0 并带中文报错）。
/// 返回 `(HTTP 状态码, 解析后的 JSON)`；解密失败 / cookie 为空 / 网络失败 → Err（人话）。
pub(crate) async fn bce_api(
    method: &str,
    path: &str,
    body: Option<Value>,
    proxy: Option<&str>,
) -> Result<(u16, Value), String> {
    let cookies = dumate_credentials::decrypt_cookies()?;
    bce_api_with_cookies(method, path, body, &cookies, proxy).await
}

/// 指定 cookie 集的 bce API 请求（DuMate 多账号池用：per-account 凭据 +
/// per-account 出站代理；本机路径 `bce_api` 解密客户端凭据后也汇入此处）。
pub(crate) async fn bce_api_with_cookies(
    method: &str,
    path: &str,
    body: Option<Value>,
    cookies: &[dumate_credentials::DumateCookie],
    proxy: Option<&str>,
) -> Result<(u16, Value), String> {
    let (cookie, csrf) = dumate_credentials::bce_cookie_header(cookies, "console.bce.baidu.com");
    if cookie.is_empty() {
        return Err(
            "未解析到 console.bce.baidu.com 域的有效 cookie，请打开 DuMate 客户端重新登录"
                .to_string(),
        );
    }
    let mut headers = HashMap::new();
    headers.insert("Cookie".to_string(), cookie);
    headers.insert("csrftoken".to_string(), csrf);
    headers.insert("X-Requested-With".to_string(), "XMLHttpRequest".to_string());
    headers.insert("Origin".to_string(), "https://console.bce.baidu.com".to_string());
    headers.insert("Referer".to_string(), "https://console.bce.baidu.com/".to_string());
    let url = format!("https://console.bce.baidu.com{path}");
    // 空 proxy 串视同直连（http_request_raw 会过滤空白）。
    let (status, _resp_headers, text) =
        http_request_raw(&url, method, body, Some(&headers), proxy, false).await;
    if status == 0 {
        return Err(format!("请求 console.bce.baidu.com 失败: {text}"));
    }
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, json))
}

/// `code` 字段取值（数字或数字字符串兼容）。
pub(crate) fn code_num(body: &Value) -> Option<i64> {
    match body.get("code") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn credits_error(reason: &str, message: &str) -> Value {
    json!({
        "ok": false,
        "reason": reason,
        "message": message,
        "error": message,
        "totalPoints": Value::Null,
        "usedPoints": Value::Null,
        "availablePoints": Value::Null,
        "isSubscribed": false,
        "subscription": [],
        "incremental": [],
        "throttled": false,
        "throttleReason": Value::Null,
        "throttleType": Value::Null,
    })
}

/// 积分概览：`GET /api/dumate/points/quota_overview` 组装
/// `{ok,totalPoints,usedPoints,availablePoints,isSubscribed,subscription[],incremental[],throttled}`。
/// 上游数字是字符串（如 "16020.00"），原样透传；HTTP 302/401 或 code=401 → 登录态失效。
pub async fn get_credits() -> Value {
    match bce_api("GET", "/api/dumate/points/quota_overview", None, None).await {
        Err(e) => credits_error("credentials_missing", &e),
        Ok((status, body)) => {
            if status == 302 || status == 401 {
                return credits_error(
                    "login_expired",
                    "百度登录态已失效，请打开 DuMate 客户端重新登录",
                );
            }
            match code_num(&body) {
                Some(0) => assemble_credits(body.get("result").unwrap_or(&Value::Null)),
                Some(401) => credits_error(
                    "login_expired",
                    "百度登录态已失效，请打开 DuMate 客户端重新登录",
                ),
                _ => credits_error(
                    "upstream_error",
                    &format!(
                        "积分查询失败（HTTP {status}）: {}",
                        truncate(&body.to_string(), 200)
                    ),
                ),
            }
        }
    }
}

/// quota_overview 的 result → 积分概览 payload（纯函数，便于单测 mock）。
pub(crate) fn assemble_credits(result: &Value) -> Value {
    let total = string_field(result, "totalPoints");
    let used = string_field(result, "usedPoints");
    let available = format!("{:.2}", parse_points(&total) - parse_points(&used));
    let throttle = result.get("modelThrottleInfo").cloned().unwrap_or(Value::Null);
    json!({
        "ok": true,
        "totalPoints": total,
        "usedPoints": used,
        "availablePoints": available,
        "isSubscribed": result.get("isSubscribed").and_then(Value::as_bool).unwrap_or(false),
        "subscription": result.get("subscription").cloned().unwrap_or(json!([])),
        "incremental": result.get("incremental").cloned().unwrap_or(json!([])),
        "throttled": throttle.get("throttled").and_then(Value::as_bool).unwrap_or(false),
        "throttleReason": throttle.get("reason").and_then(Value::as_str).map(|s| json!(s)).unwrap_or(Value::Null),
        "throttleType": throttle.get("throttleType").and_then(Value::as_str).map(|s| json!(s)).unwrap_or(Value::Null),
    })
}

fn string_field(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "0".to_string(),
    }
}

fn parse_points(s: &str) -> f64 {
    s.trim().replace(',', "").parse::<f64>().unwrap_or(0.0)
}

/// dumate-points 模型的 assistant 回复文案（纯函数，便于单测）。
pub(crate) fn format_points_reply(credits: &Value) -> String {
    let get_str = |k: &str| credits.get(k).and_then(Value::as_str).unwrap_or("0");
    let is_sub = credits
        .get("isSubscribed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let n_sub = credits
        .get("subscription")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let n_inc = credits
        .get("incremental")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let throttled = credits
        .get("throttled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut lines = vec![
        "DuMate 积分概览：".to_string(),
        format!("· 总积分：{}", get_str("totalPoints")),
        format!("· 已使用：{}", get_str("usedPoints")),
        format!("· 可用：{}", get_str("availablePoints")),
        format!(
            "· 订阅包：{}（{n_sub} 个）",
            if is_sub { "已订阅" } else { "未订阅" }
        ),
        format!("· 加油包/赠送：{n_inc} 个"),
    ];
    if let Some(arr) = credits.get("subscription").and_then(Value::as_array) {
        for s in arr {
            let name = s
                .get("packageType")
                .and_then(Value::as_str)
                .unwrap_or("订阅包");
            let total_p = s.get("totalPoints").and_then(Value::as_str).unwrap_or("?");
            let used_p = s.get("usedPoints").and_then(Value::as_str).unwrap_or("?");
            let expire = s.get("expireDate").and_then(Value::as_str).unwrap_or("未知");
            lines.push(format!(
                "  · {name}：已用 {used_p} / {total_p}（到期 {expire}）"
            ));
        }
    }
    if throttled {
        lines.push("· 注意：当前处于模型限流状态，请降低调用频率".to_string());
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// 会话反代（POST /session → POST message（同步）→ DELETE）
// ---------------------------------------------------------------------------

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::new())
}

/// prompt 组装：与 `workbuddy_proxy::flatten_messages` 同逻辑（该函数为模块
/// 私有，按文件归属纪律不跨文件改动，此处按契约语义对齐实现）。
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

/// 拆出 system 消息 → 引擎**原生 `system` 字段**，其余消息照旧拍平。
///
/// 为什么需要：引擎自带框架会压制 user 文本里的指令（2026-10-01 实测：三层档位
/// dumate-lite/turbo/ultra 均无视 user 里的"工具调用协议"，反而直接编造答案）；
/// 而 message API 的原生 `system` 字段优先级更高，是唯一能覆盖默认行为的通道。
/// 用途：工具调用桥（分享层）把协议放进 system，让模型按协议输出 tool_calls。
fn split_system(messages: &[Value]) -> (String, Vec<Value>) {
    let mut system = Vec::new();
    let mut rest = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            let text = match message.get("content") {
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
            if !text.trim().is_empty() {
                system.push(text);
            }
        } else {
            rest.push(message.clone());
        }
    }
    (system.join("\n\n"), rest)
}

/// 创建引擎会话：`POST /session`（title=Sylvia Switch，directory 空串 = 客户端默认目录）。
async fn post_session(client: &reqwest::Client, engine: &DumateEngine) -> Result<Value, String> {
    let mut req = client
        .post(format!("{}/session", engine.base_url))
        .json(&json!({"title": "Sylvia Switch", "directory": ""}))
        .timeout(Duration::from_secs(15));
    if !engine.inapp_key.is_empty() {
        req = req.header("X-Dumate-Inapp-Key", &engine.inapp_key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("创建会话失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "创建会话失败 {status}: {}",
            truncate(&text, 200)
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("解析会话响应失败: {e}"))
}

/// 提交消息并**同步等待**最终 assistant 消息（契约实测：该接口阻塞到生成完成，
/// 实测 3.9s 返回，勿做轮询）。
async fn post_message(
    client: &reqwest::Client,
    engine: &DumateEngine,
    session_id: &str,
    payload: &Value,
    timeout_secs: u64,
) -> Result<Value, DumateChatError> {
    let mut req = client
        .post(format!(
            "{}/session/{session_id}/message",
            engine.base_url
        ))
        .json(payload)
        .timeout(Duration::from_secs(timeout_secs));
    if !engine.inapp_key.is_empty() {
        req = req.header("X-Dumate-Inapp-Key", &engine.inapp_key);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(DumateChatError::new(
                    DumateChatErrorKind::EngineError,
                    format!("提交消息失败 {status}: {}", truncate(&text, 200)),
                ));
            }
            serde_json::from_str(&text).map_err(|e| {
                DumateChatError::new(
                    DumateChatErrorKind::EngineError,
                    format!("解析消息响应失败: {e}"),
                )
            })
        }
        Err(e) if e.is_timeout() => Err(DumateChatError::new(
            DumateChatErrorKind::Timeout,
            format!("上游生成超时（{timeout_secs}s 内未返回最终回复）"),
        )),
        Err(e) => Err(DumateChatError::new(
            DumateChatErrorKind::EngineError,
            format!("提交消息失败: {e}"),
        )),
    }
}

/// 清理会话：`DELETE /session/{id}` → 200 true（失败不阻塞主流程）。
async fn cleanup_session(client: &reqwest::Client, engine: &DumateEngine, session_id: &str) {
    let mut req = client
        .delete(format!("{}/session/{session_id}", engine.base_url))
        .timeout(Duration::from_secs(10));
    if !engine.inapp_key.is_empty() {
        req = req.header("X-Dumate-Inapp-Key", &engine.inapp_key);
    }
    let _ = req.send().await;
}

/// OpenAI chat.completion 响应组装：usage 从 `info.tokens`
/// （prompt=input / completion=output / total=total），routing 透传。
fn assemble_completion(model: &str, content: &str, tokens: &Value, routing: &Value) -> Value {
    json!({
        "id": format!("chatcmpl-dumate-{}", now_ms()),
        "object": "chat.completion",
        "created": now_ms() / 1000,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": tokens.get("input").cloned().unwrap_or(json!(0)),
            "completion_tokens": tokens.get("output").cloned().unwrap_or(json!(0)),
            "total_tokens": tokens.get("total").cloned().unwrap_or(json!(0)),
        },
        "dumateServedBy": "dumate-opencode",
        "dumateRouting": routing,
    })
}

/// dumate-points 特殊模型：直接用 get_credits() 组装 assistant 回复
/// （不消耗模型积分；不触引擎）。
async fn points_completion() -> Result<Value, DumateChatError> {
    let credits = get_credits().await;
    if credits.get("ok").and_then(Value::as_bool) != Some(true) {
        let msg = credits
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("积分查询失败，请确认百度登录态有效");
        return Err(DumateChatError::new(
            DumateChatErrorKind::UpstreamRejected,
            msg.to_string(),
        ));
    }
    let content = format_points_reply(&credits);
    let mut v = assemble_completion(MODEL_POINTS, &content, &Value::Null, &Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.remove("dumateServedBy");
        obj.insert("dumatePointsQuery".to_string(), json!(true));
    }
    Ok(v)
}

/// 非流式对话补全：归一化模型 → points 直查积分 → 发现引擎 → 建会话 →
/// 同步提交消息 → 组装 OpenAI 响应。失败与成功都清理会话。
pub async fn chat_completions_json(model: &str, body: &Value) -> Result<Value, DumateChatError> {
    let normalized = normalize_model(model);
    if normalized == MODEL_POINTS {
        return points_completion().await;
    }
    let messages: Vec<Value> = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (system_text, headless_messages) = split_system(&messages);
    let prompt = flatten_messages(&headless_messages);
    if prompt.trim().is_empty() {
        return Err(DumateChatError::new(
            DumateChatErrorKind::EngineError,
            "请求 messages 不能为空",
        ));
    }
    let mut engine = discover_engine()
        .await
        .map_err(|e| DumateChatError::new(DumateChatErrorKind::EngineNotFound, e))?;
    let client = http_client();
    // 建会话。连接失败 = 端口漂移窗口：作废缓存重发现一次再试。
    let session = match post_session(&client, &engine).await {
        Ok(v) => v,
        Err(e) if workbuddy_proxy::is_connect_error(&e) => {
            invalidate_engine_cache();
            engine = discover_engine()
                .await
                .map_err(|e2| DumateChatError::new(DumateChatErrorKind::EngineNotFound, e2))?;
            post_session(&client, &engine)
                .await
                .map_err(|e2| DumateChatError::new(DumateChatErrorKind::EngineError, e2))?
        }
        Err(e) => return Err(DumateChatError::new(DumateChatErrorKind::EngineError, e)),
    };
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Err(DumateChatError::new(
            DumateChatErrorKind::EngineError,
            "创建会话失败：响应缺少 id",
        ));
    }
    // 档位头在消息体 headers 字段（不是 HTTP 头）。
    // 1.0.12：system 消息走引擎**原生 `system` 字段**（优先级高于拍平文本），
    // 供工具调用桥下发协议说明 —— 引擎默认框架会压制 user 文本里的同类指令。
    let mut payload = json!({
        "parts": [{"type": "text", "text": prompt}],
        "headers": {"X-Dumate-AutoModel-Level": level_for_model(normalized)},
    });
    if !system_text.trim().is_empty() {
        payload["system"] = json!(system_text);
    }
    let reply = match post_message(
        &client,
        &engine,
        &session_id,
        &payload,
        message_timeout_secs(normalized),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            cleanup_session(&client, &engine, &session_id).await;
            return Err(e);
        }
    };
    // parts 可能多条，取所有 type=="text" 拼接。
    let content = reply
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .map(String::from)
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let tokens = reply.pointer("/info/tokens").cloned().unwrap_or(Value::Null);
    let routing = reply.pointer("/info/routing").cloned().unwrap_or(Value::Null);
    // 成功也清理会话（不留垃圾）。
    cleanup_session(&client, &engine, &session_id).await;
    if content.trim().is_empty() {
        return Err(DumateChatError::new(
            DumateChatErrorKind::UpstreamRejected,
            "上游未返回文本回复（DuMate 引擎响应 parts 无 text）",
        ));
    }
    Ok(assemble_completion(normalized, &content, &tokens, &routing))
}

/// 合成单个 OpenAI chunk 帧（聚合后仿流式，对齐 workbuddy 反代形态）。
fn sse_frame(id: &str, model: &str, delta: Value, finish: Option<&str>) -> String {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_ms() / 1000,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
    .to_string()
}

/// 流式对话：聚合完整结果后合成 chunk 帧序列（role 帧 → 内容帧 → usage+stop 帧
/// → [DONE]）。DuMate 引擎无逐 token 流；帧数恒定且小于通道容量，直接同步入队，
/// 无需 spawn 泵（契约零自动触发约束）。
/// 返回 `(字节流, 实际使用的模型名)`，对齐 opencode 网关的 api 层用法。
pub async fn chat_completions_stream(
    model: &str,
    body: &Value,
) -> Result<(workbuddy_proxy::WorkbuddyUpstreamStream, String), DumateChatError> {
    let completion = chat_completions_json(model, body).await?;
    let out_model = completion
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(normalize_model(model))
        .to_string();
    let content = completion
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = completion.get("usage").cloned().unwrap_or(Value::Null);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    let id = format!("chatcmpl-dumate-{}", now_ms());
    let _ = tx
        .send(Ok(Bytes::from(format!(
            "data: {}\n\n",
            sse_frame(&id, &out_model, json!({"role": "assistant", "content": ""}), None)
        ))))
        .await;
    if !content.is_empty() {
        let _ = tx
            .send(Ok(Bytes::from(format!(
                "data: {}\n\n",
                sse_frame(&id, &out_model, json!({"content": content}), None)
            ))))
            .await;
    }
    let mut usage_json = usage;
    if let Some(obj) = usage_json.as_object_mut() {
        obj.insert("stream".to_string(), json!(true));
    }
    let _ = tx
        .send(Ok(Bytes::from(format!(
            "data: {}\n\n",
            json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": now_ms() / 1000,
                "model": out_model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": usage_json,
            })
            .to_string()
        ))))
        .await;
    let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
    Ok((workbuddy_proxy::WorkbuddyUpstreamStream { rx }, out_model))
}

// ---------------------------------------------------------------------------
// OpenAI /v1/models 模型目录（静态 4 个）
// ---------------------------------------------------------------------------

pub fn models_payload() -> Value {
    let data: Vec<Value> = MODELS
        .iter()
        .map(|(id, level, desc)| {
            json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "dumate",
                "description": desc,
                "level": level,
            })
        })
        .collect();
    json!({"object": "list", "data": data})
}

// ---------------------------------------------------------------------------
// 状态聚合（契约 §2.3：camelCase，键不省略）
// ---------------------------------------------------------------------------

/// 渠道状态聚合。credits 在凭据缺失时为 null，查询失败时为 ok=false 的对象。
pub async fn status() -> Value {
    let cfg = load_config();
    let desktop = desktop_running();
    let creds = dumate_credentials::credentials_present();
    let engine = discover_engine().await.ok();
    let engine_alive_now = match &engine {
        Some(e) => engine_alive(&e.base_url, &e.inapp_key).await,
        None => false,
    };
    let engine_url = engine
        .as_ref()
        .map(|e| json!(e.base_url))
        .unwrap_or(Value::Null);
    let inapp_preview = match &engine {
        Some(e) => opt_str(preview_key(&e.inapp_key)),
        None => opt_str(preview_key(&cfg.inapp_key)),
    };
    let credits = if creds { get_credits().await } else { Value::Null };
    let message = status_message(desktop, engine.is_some(), engine_alive_now, creds, cfg.enabled);
    let diag = DISCOVERY_DIAG.lock().unwrap().clone();
    json!({
        "ok": true,
        "desktopRunning": desktop,
        "engineDiscovered": engine.is_some(),
        "engineUrl": engine_url,
        "engineAlive": engine_alive_now,
        "inappKeyPreview": inapp_preview,
        "credentialsPresent": creds,
        "gatewayEnabled": cfg.enabled,
        "apiKeyPreview": opt_str(mask_secret(&cfg.api_key)),
        "engineDiag": if engine.is_some() { Value::Null } else { Value::String(diag) },
        "credits": credits,
        "message": message,
    })
}

/// 状态人话汇总（纯函数，便于单测）：按故障链给可操作建议。
fn status_message(desktop: bool, discovered: bool, alive: bool, creds: bool, enabled: bool) -> String {
    let mut hints: Vec<&str> = Vec::new();
    if !desktop {
        hints.push("未检测到 DuMate 桌面端进程，请先打开 DuMate 客户端");
    }
    if !creds {
        hints.push("未找到 qianfan 登录凭据（auth.json / .cookie-key），请打开 DuMate 客户端重新登录");
    }
    if desktop && !discovered {
        hints.push("未能发现 OpenCode 引擎（进程环境 / 环境变量 / 配置 / 默认端口 52795 均未命中），请确认 DuMate 已登录并完成初始化");
    }
    if discovered && !alive {
        hints.push("引擎健康检查未通过，请稍后重试或重启 DuMate 客户端");
    }
    if !enabled {
        hints.push("DuMate 网关已停用（enabled=false），/dumate/v1 不对外服务");
    }
    if hints.is_empty() {
        "DuMate 通道正常：引擎在线，凭据与网关配置齐备".to_string()
    } else {
        format!("DuMate 通道异常：{}", hints.join("；"))
    }
}

// ---------------------------------------------------------------------------
// 展示辅助
// ---------------------------------------------------------------------------

/// 空串 → null（前端 DumateStatus 预览字段类型为 string | null，键不省略）。
fn opt_str(s: String) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        json!(s)
    }
}

/// inapp key 预览：前 6 位 + "…"（契约 §2.3 示例 "0807f3…"）。
pub(crate) fn preview_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() >= 6 {
        let head: String = chars[..6].iter().collect();
        format!("{head}…")
    } else if chars.is_empty() {
        String::new()
    } else {
        format!("{key}…")
    }
}

/// api_key 掩码：前 6 位 + "…" + 后 4 位；过短 / 为空不给内容。
pub(crate) fn mask_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > 10 {
        let head: String = chars[..6].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    } else if chars.is_empty() {
        String::new()
    } else {
        "…".to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// 单测（纯函数；不依赖网络 / 真实 DuMate 进程）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_model_mapping_table() {
        // 静态目录直映（含大小写 / 连字符 / 下划线 / 空格 / 点变体）
        assert_eq!(normalize_model("dumate-lite"), MODEL_LITE);
        assert_eq!(normalize_model("DUMATE-TURBO"), MODEL_TURBO);
        assert_eq!(normalize_model("DuMate_Ultra"), MODEL_ULTRA);
        assert_eq!(normalize_model("dumate points"), MODEL_POINTS);
        assert_eq!(normalize_model("dumate.points"), MODEL_POINTS);
        // 客户端友好映射：*haiku* → lite、*sonnet* → turbo、*opus* → ultra
        assert_eq!(normalize_model("claude-3-5-haiku@2025"), MODEL_LITE);
        assert_eq!(normalize_model("X-Sonnet-Pro"), MODEL_TURBO);
        assert_eq!(normalize_model("some-opus-max"), MODEL_ULTRA);
        // 未识别 → 默认 lite
        assert_eq!(normalize_model("gpt-who-knows"), MODEL_LITE);
        assert_eq!(normalize_model(""), MODEL_LITE);
    }

    #[test]
    fn level_and_timeout_by_model() {
        assert_eq!(level_for_model(MODEL_LITE), "L0");
        assert_eq!(level_for_model(MODEL_TURBO), "L1");
        assert_eq!(level_for_model(MODEL_ULTRA), "L2");
        assert_eq!(message_timeout_secs(MODEL_LITE), 120);
        assert_eq!(message_timeout_secs(MODEL_TURBO), 240);
        assert_eq!(message_timeout_secs(MODEL_ULTRA), 300);
    }

    #[test]
    fn models_payload_shape() {
        let v = models_payload();
        assert_eq!(v.get("object").and_then(Value::as_str), Some("list"));
        let data = v.get("data").and_then(Value::as_array).unwrap();
        assert_eq!(data.len(), 4);
        let ids: Vec<&str> = data
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(
            ids,
            vec![MODEL_LITE, MODEL_TURBO, MODEL_ULTRA, MODEL_POINTS]
        );
        for m in data {
            assert_eq!(m.get("object").and_then(Value::as_str), Some("model"));
            assert_eq!(m.get("owned_by").and_then(Value::as_str), Some("dumate"));
            assert!(
                m.get("description").and_then(Value::as_str).unwrap_or("").len() > 0,
                "每个模型带描述"
            );
        }
    }

    #[test]
    fn parse_config_defaults_and_overrides() {
        let cfg = parse_config_json(&json!({}));
        assert!(cfg.enabled, "缺省 enabled=true");
        assert!(cfg.api_key.is_empty());
        assert!(cfg.opencode_url.is_empty());
        assert!(cfg.inapp_key.is_empty());
        let cfg = parse_config_json(&json!({
            "enabled": false,
            "api_key": "sk-x",
            "opencode_url": "http://127.0.0.1:52002",
            "inapp_key": "k".repeat(64),
        }));
        assert!(!cfg.enabled);
        assert_eq!(cfg.api_key, "sk-x");
        assert_eq!(cfg.opencode_url, "http://127.0.0.1:52002");
        assert_eq!(cfg.inapp_key.len(), 64);
    }

    #[test]
    fn assemble_credits_from_mock_result() {
        // 契约 §0 实测形态：数字是字符串。
        let result = json!({
            "isSubscribed": true,
            "usedPoints": "55.73",
            "totalPoints": "16020.00",
            "modelThrottleInfo": {"throttled": true, "reason": "高峰限流", "throttleType": "soft"},
            "subscription": [{"packageId": "sub1", "totalPoints": "15000.00", "usedPoints": "50.00"}],
            "incremental": [{"packageType": "grant_point", "source": "login_bonus", "totalPoints": "1020.00"}],
        });
        let v = assemble_credits(&result);
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(v.get("totalPoints").and_then(Value::as_str), Some("16020.00"));
        assert_eq!(v.get("usedPoints").and_then(Value::as_str), Some("55.73"));
        assert_eq!(
            v.get("availablePoints").and_then(Value::as_str),
            Some("15964.27"),
            "available = total - used（保留两位小数）"
        );
        assert_eq!(v.get("isSubscribed").and_then(Value::as_bool), Some(true));
        assert_eq!(v.get("throttled").and_then(Value::as_bool), Some(true));
        assert_eq!(v.get("throttleReason").and_then(Value::as_str), Some("高峰限流"));
        assert_eq!(v.get("throttleType").and_then(Value::as_str), Some("soft"));
        assert_eq!(
            v.pointer("/subscription/0/packageId").and_then(Value::as_str),
            Some("sub1")
        );
        assert_eq!(
            v.pointer("/incremental/0/source").and_then(Value::as_str),
            Some("login_bonus")
        );
    }

    #[test]
    fn assemble_credits_missing_fields_fall_back() {
        let v = assemble_credits(&json!({}));
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(v.get("totalPoints").and_then(Value::as_str), Some("0"));
        assert_eq!(v.get("availablePoints").and_then(Value::as_str), Some("0.00"));
        assert_eq!(v.get("throttled").and_then(Value::as_bool), Some(false));
        assert!(v.get("subscription").and_then(Value::as_array).unwrap().is_empty());
        assert!(v.get("incremental").and_then(Value::as_array).unwrap().is_empty());
        assert!(v.get("throttleReason").map(Value::is_null).unwrap_or(false));
    }

    #[test]
    fn format_points_reply_contains_key_numbers() {
        let credits = assemble_credits(&json!({
            "isSubscribed": true,
            "usedPoints": "55.73",
            "totalPoints": "16020.00",
            "subscription": [{"packageType": "sub", "totalPoints": "15000.00", "usedPoints": "50.00", "expireDate": "2026-12-31"}],
        }));
        let text = format_points_reply(&credits);
        assert!(text.contains("15964.27"), "{text}");
        assert!(text.contains("2026-12-31"), "{text}");
        assert!(text.contains("已订阅"), "{text}");
    }

    #[test]
    fn env_engine_map_prefers_host_url_then_port() {
        let env = vec![
            ("DUMATE_INAPP_KEY".to_string(), "k".repeat(64)),
            ("DUMATE_HOST_URL".to_string(), "http://127.0.0.1:52002/".to_string()),
            ("OPENCODE_SERVER_PORT".to_string(), "52795".to_string()),
        ];
        let (base, key) = env_engine_from_map(&env).expect("HOST_URL 优先");
        assert_eq!(base, "http://127.0.0.1:52002");
        assert_eq!(key.len(), 64);

        let env = vec![("OPENCODE_SERVER_PORT".to_string(), "52795".to_string())];
        let (base, key) = env_engine_from_map(&env).expect("无 HOST_URL 时取端口");
        assert_eq!(base, "http://127.0.0.1:52795");
        assert!(key.is_empty());

        assert!(env_engine_from_map(&[]).is_none());
        let junk = vec![
            ("OPENCODE_SERVER_PORT".to_string(), "not-a-port".to_string()),
            ("DUMATE_INAPP_KEY".to_string(), "k".to_string()),
        ];
        assert!(env_engine_from_map(&junk).is_none(), "非数字端口不采信");
    }

    #[test]
    fn normalize_base_url_rules() {
        assert_eq!(
            normalize_base_url("http://127.0.0.1:52002/"),
            "http://127.0.0.1:52002"
        );
        assert_eq!(
            normalize_base_url(" 127.0.0.1:52002 "),
            "http://127.0.0.1:52002"
        );
        assert_eq!(normalize_base_url(""), "");
    }

    #[test]
    fn process_env_discovery_errors_on_missing_process() {
        // 不存在的映像名 → Err（不 panic）。Windows 走 tasklist 查无此进程；
        // 其它平台直接 Err。
        let r = process_env_by_image("dumate-opencode-not-exist-xyz.exe");
        assert!(r.is_err());
    }

    #[test]
    fn is_dumate_image_matches() {
        assert!(is_dumate_image("DuMate.exe"));
        assert!(is_dumate_image("dumate-opencode.exe"));
        assert!(is_dumate_image("dumate-main-server.exe"));
        assert!(!is_dumate_image("workbuddy.exe"));
        assert!(!is_dumate_image("dumate.exe.bak"));
        assert!(!is_dumate_image(""));
    }

    #[test]
    fn flatten_messages_joins_roles() {
        let msgs = vec![
            json!({"role": "system", "content": "你是助手"}),
            json!({"role": "user", "content": [{"type": "text", "text": "你好"}]}),
            json!({"role": "assistant", "content": "在的"}),
        ];
        assert_eq!(
            flatten_messages(&msgs),
            "[system] 你是助手\n\n[user] 你好\n\n[assistant] 在的"
        );
        assert_eq!(flatten_messages(&[]), "");
    }

    #[test]
    fn sse_frame_shape() {
        let raw = sse_frame("chatcmpl-dumate-1", MODEL_LITE, json!({"content": "hi"}), None);
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v.get("id").and_then(Value::as_str), Some("chatcmpl-dumate-1"));
        assert_eq!(
            v.get("object").and_then(Value::as_str),
            Some("chat.completion.chunk")
        );
        assert_eq!(v.get("model").and_then(Value::as_str), Some(MODEL_LITE));
        assert_eq!(v.pointer("/choices/0/finish_reason"), Some(&Value::Null));
        let raw = sse_frame("chatcmpl-dumate-1", MODEL_LITE, json!({}), Some("stop"));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v.pointer("/choices/0/finish_reason").and_then(Value::as_str),
            Some("stop")
        );
    }

    #[test]
    fn assemble_completion_usage_from_tokens() {
        let tokens = json!({"total": 100, "input": 80, "output": 20});
        let v = assemble_completion(MODEL_TURBO, "回复内容", &tokens, &json!({"task_type": "chat"}));
        assert_eq!(v.get("model").and_then(Value::as_str), Some(MODEL_TURBO));
        assert_eq!(v.pointer("/usage/prompt_tokens").and_then(Value::as_i64), Some(80));
        assert_eq!(v.pointer("/usage/completion_tokens").and_then(Value::as_i64), Some(20));
        assert_eq!(v.pointer("/usage/total_tokens").and_then(Value::as_i64), Some(100));
        assert_eq!(
            v.get("dumateServedBy").and_then(Value::as_str),
            Some("dumate-opencode")
        );
        assert_eq!(
            v.pointer("/dumateRouting/task_type").and_then(Value::as_str),
            Some("chat")
        );
        assert_eq!(
            v.pointer("/choices/0/message/content").and_then(Value::as_str),
            Some("回复内容")
        );
    }

    #[test]
    fn status_message_composes_hints() {
        let ok = status_message(true, true, true, true, true);
        assert!(ok.contains("正常"), "{ok}");
        let bad = status_message(false, false, false, false, false);
        assert!(bad.contains("未检测到 DuMate 桌面端进程"), "{bad}");
        assert!(bad.contains("重新登录"), "{bad}");
        assert!(bad.contains("网关已停用"), "{bad}");
    }

    #[test]
    fn mask_and_preview_helpers() {
        let key = "0807f3abcdef1234567890";
        assert_eq!(preview_key(key), "0807f3…");
        assert_eq!(preview_key(""), "");
        assert_eq!(mask_secret(key), "0807f3…7890");
        assert_eq!(mask_secret("short"), "…");
        assert_eq!(mask_secret(""), "");
        assert_eq!(opt_str(String::new()), Value::Null);
        assert_eq!(opt_str("x".to_string()), json!("x"));
    }

    #[test]
    fn code_num_parses_number_and_string() {
        assert_eq!(code_num(&json!({"code": 0})), Some(0));
        assert_eq!(code_num(&json!({"code": "401"})), Some(401));
        assert_eq!(code_num(&json!({})), None);
    }
}
