//! # grok2api 进程管理（Build 通道托管）
//!
//! grok2api（127.0.0.1:8002，独立 Go 进程）是 Grok Build 通道的上游服务：
//! grok-4.7 等模型的实际推理层（账号池 + OAuth 自动续期）。它掉线时这些模型
//! 直接 502。本模块提供：
//!
//! - **探活**：GET {base}/readyz（no_proxy 客户端——127.0.0.1 请求防系统代理
//!   劫持，grok_sso.rs:106-117 同款坑）；
//! - **拉起**：DETACHED + CREATE_NO_WINDOW 启动 grok2api.exe（WorkingDir=
//!   项目根，读同目录 config.yaml），等就绪 ≤20s；
//! - **看门狗**：60s 巡检，掉线自动拉起（enabled+auto_start 才生效）；
//! - **请求链路自愈**：grok.rs / api.rs 的转发遇到连接类失败时调用
//!   [`ensure_alive`] 拉起后重试一次。
//!
//! 配置：复用 `~/.wb-switch/grok_g2a_proxy.json`（base_url/api_key/models 之外
//! 增补 exe_path / working_dir / auto_start 进程托管字段，缺省 auto_start=true、
//! exe_path 空 = 托管关闭仅探活）。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::modules::config::store_dir;

static ENSURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static WATCHDOG_SPAWNED: AtomicBool = AtomicBool::new(false);

/// grok_g2a_proxy.json（桥接与托管共用同一配置文件）。
fn config_path() -> PathBuf {
    store_dir().join("grok_g2a_proxy.json")
}

/// 托管配置（桥接字段 + 进程字段）。
#[derive(Debug, Clone, Default)]
pub struct ManagerConfig {
    pub base_url: String,
    pub exe_path: String,
    pub working_dir: String,
    pub auto_start: bool,
}

/// 读托管配置：读文件 → 解析；文件缺失/损坏 → None（托管关闭）。
fn load_manager_config() -> Option<ManagerConfig> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    manager_config_from_value(&v)
}

/// 从 JSON 解析托管配置（纯函数，便于单测）：base_url 必有；exe/working_dir/
/// auto_start 缺省给空/true。base_url 缺失 → None（托管关闭）。
fn manager_config_from_value(v: &Value) -> Option<ManagerConfig> {
    let base_url = v
        .get("base_url")
        .and_then(Value::as_str)
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .unwrap_or_default();
    if base_url.is_empty() {
        return None;
    }
    Some(ManagerConfig {
        base_url,
        exe_path: v
            .get("exe_path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        working_dir: v
            .get("working_dir")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        auto_start: v
            .get("auto_start")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

/// no_proxy 探活客户端（127.0.0.1 防 Clash 劫持，grok_sso.rs 同款坑）。
fn probe_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build grok2api probe client")
    })
}

/// 探活指定基址：GET {base}/readyz 2xx 即活。
async fn probe_ready(base_url: &str) -> bool {
    let url = format!("{base_url}/readyz");
    match probe_client().get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// 当前 grok2api 是否存活（供状态接口复用）。
pub async fn alive() -> bool {
    let Some(cfg) = load_manager_config() else {
        return false;
    };
    probe_ready(&cfg.base_url).await
}

/// 拉起 grok2api.exe（DETACHED + CREATE_NO_WINDOW，WorkingDir=项目根）。
fn spawn_process(cfg: &ManagerConfig) -> Result<(), String> {
    if cfg.exe_path.is_empty() || !std::path::Path::new(&cfg.exe_path).exists() {
        return Err(format!("grok2api 可执行文件未配置或不存在: {}", cfg.exe_path));
    }
    let mut cmd = std::process::Command::new(&cfg.exe_path);
    if !cfg.working_dir.is_empty() {
        cmd.current_dir(&cfg.working_dir);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008 | 0x08000000); // DETACHED_PROCESS | CREATE_NO_WINDOW
    }
    cmd.spawn()
        .map_err(|e| format!("启动 grok2api 失败: {e}"))?;
    Ok(())
}

/// 确保进程存活（托管自愈入口）：探活 → 不活且 exe_path 已配置 → 拉起 →
/// 等就绪 ≤20s。请求链路的转发失败自愈与看门狗共用本函数
/// （tokio Mutex 防并发拉起——guard 不跨 await 之外的 Send 问题）。
pub async fn ensure_alive() -> Result<(), String> {
    let _guard = ENSURE_LOCK.lock().await;
    let Some(cfg) = load_manager_config() else {
        return Err("grok2api 托管未配置（grok_g2a_proxy.json 缺失）".to_string());
    };
    if probe_ready(&cfg.base_url).await {
        return Ok(());
    }
    if let Err(error) = spawn_process(&cfg) {
        return Err(error);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(800)).await;
        if probe_ready(&cfg.base_url).await {
            return Ok(());
        }
    }
    Err("grok2api 拉起后 20s 内未就绪".to_string())
}

/// 看门狗：60s 巡检，掉线自动拉起。仅在 main 启动时 spawn 一次。
pub fn spawn_watchdog() {
    if WATCHDOG_SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let Some(cfg) = load_manager_config() else {
                continue;
            };
            if !cfg.auto_start || cfg.exe_path.is_empty() {
                continue;
            }
            if probe_ready(&cfg.base_url).await {
                continue;
            }
            let _ = ensure_alive().await; // 失败下一轮再试
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_config_defaults_and_overrides() {
        // 缺 exe/working_dir/auto_start → 默认值（托管关闭仅探活）
        let minimal = json!({"base_url": "http://127.0.0.1:8002/"});
        let cfg = manager_config_from_value(&minimal).expect("minimal 应解析成功");
        assert_eq!(cfg.base_url, "http://127.0.0.1:8002"); // 尾斜杠被剪
        assert!(cfg.exe_path.is_empty());
        assert!(cfg.auto_start);
        // 全字段
        let full = json!({
            "base_url": "http://127.0.0.1:8002",
            "exe_path": "E:/x/grok2api.exe",
            "working_dir": "E:/x",
            "auto_start": false
        });
        let cfg = manager_config_from_value(&full).expect("full 应解析成功");
        assert_eq!(cfg.exe_path, "E:/x/grok2api.exe");
        assert_eq!(cfg.working_dir, "E:/x");
        assert!(!cfg.auto_start);
        // base_url 缺失 → None
        assert!(manager_config_from_value(&json!({})).is_none());
        assert!(manager_config_from_value(&json!({"enabled": true})).is_none());
    }
}
