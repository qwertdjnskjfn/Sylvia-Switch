//! 账号保护与 AutoClaw 反代保活防封模块。
//!
//! 针对通过反代方式调用模型的账号以及平台全部账号，提供纵深防护体系：
//! 1. 前瞻式 JWT 提前静默续期：避免 access token 过期与业务调用 401 延迟；
//! 2. 常态化低频只读心跳：保持服务端会话活跃与活跃时间戳更新，防止被判定为休眠僵尸账号；
//! 3. 官方桌面客户端指纹拟真：消除 reqwest 默认 UA，对齐 Electron/Chromium 真实请求特征；
//! 4. 单账号并发槽位限制：防止外部 Agent 瞬时并发冲垮单账号触发服务端 403 频控；
//! 5. 动态自适应冷却熔断器：遇到 403 限流自动将账号置入冷静期，避免二次暴力冲击造成封禁。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::modules::autoclaw_accounts::{jwt_exp_from_access_token, pool_accounts};
use crate::modules::config::{atomic_write, now_ms, now_secs, store_dir};
use crate::modules::notify;
use crate::modules::platform_checkin::{
    autoclaw_app_version, get_autoclaw_status_with,
};

const PROTECTION_CONFIG_FILE: &str = "account_protection_config.json";
const MAX_LOG_HISTORY: usize = 50;

// ---------------------------------------------------------------------------
// 配置定义与持久化
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProtectionConfig {
    /// 是否开启全局账号保护
    pub enabled: bool,
    /// 是否开启后台定时主动保活心跳
    pub keepalive_enabled: bool,
    /// 主动保活心跳巡检间隔（分钟，默认 120 分钟）
    pub keepalive_interval_minutes: u64,
    /// 单账号最大并发请求槽位数（默认 2）
    pub max_concurrency_per_account: usize,
    /// 触发 403/429 限流后的自适应冷却时长（分钟，默认 5 分钟）
    pub cooldown_minutes: u64,
    /// JWT 有效期剩余低于多少分钟时提前自动刷新（默认 60 分钟）
    pub jwt_refresh_ahead_minutes: u64,
}

impl Default for AccountProtectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keepalive_enabled: true,
            keepalive_interval_minutes: 120,
            max_concurrency_per_account: 2,
            cooldown_minutes: 5,
            jwt_refresh_ahead_minutes: 60,
        }
    }
}

fn config_path() -> PathBuf {
    store_dir().join(PROTECTION_CONFIG_FILE)
}

pub fn load_protection_config() -> AccountProtectionConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<AccountProtectionConfig>(&t).ok())
        .unwrap_or_default()
}

pub fn save_protection_config(cfg: &AccountProtectionConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 官方桌面客户端 User-Agent 模拟
// ---------------------------------------------------------------------------

/// 构造官方 AutoClaw 桌面端（Electron）拟真 User-Agent，规避 reqwest 默认库指纹。
pub fn simulated_autoclaw_user_agent() -> String {
    let version = autoclaw_app_version();
    #[cfg(target_os = "windows")]
    {
        format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) AutoClaw/{version} Chrome/120.0.0.0 Electron/28.3.3 Safari/537.36"
        )
    }
    #[cfg(target_os = "macos")]
    {
        format!(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) AutoClaw/{version} Chrome/120.0.0.0 Electron/28.3.3 Safari/537.36"
        )
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        format!(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) AutoClaw/{version} Chrome/120.0.0.0 Electron/28.3.3 Safari/537.36"
        )
    }
}

const LOOMY_APP_VERSION_FALLBACK: &str = "0.9.37";

/// Loomy 客户端版本：优先读注册表卸载信息（随客户端升级自动变化），
/// 读不到时回退到已知版本。
pub fn loomy_app_version() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            #[cfg(target_os = "windows")]
            {
                use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
                use winreg::RegKey;
                for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
                    let Ok(uninstall) = RegKey::predef(hive)
                        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Uninstall")
                    else {
                        continue;
                    };
                    for key in uninstall.enum_keys().flatten() {
                        let Ok(sub) = uninstall.open_subkey(&key) else {
                            continue;
                        };
                        let name: String = sub.get_value("DisplayName").unwrap_or_default();
                        if !name.contains("Loomy") {
                            continue;
                        }
                        let version: String = sub.get_value("DisplayVersion").unwrap_or_default();
                        if !version.trim().is_empty() {
                            return version.trim().to_string();
                        }
                    }
                }
            }
            LOOMY_APP_VERSION_FALLBACK.to_string()
        })
        .clone()
}

/// 构造官方 Loomy 桌面端（Electron）拟真 User-Agent，消除 reqwest 默认库指纹。
pub fn simulated_loomy_user_agent() -> String {
    let version = loomy_app_version();
    #[cfg(target_os = "windows")]
    {
        format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Loomy/{version} Chrome/144.0.7559.236 Electron/40.8.2 Safari/537.36"
        )
    }
    #[cfg(target_os = "macos")]
    {
        format!(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Loomy/{version} Chrome/144.0.7559.236 Electron/40.8.2 Safari/537.36"
        )
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        format!(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Loomy/{version} Chrome/144.0.7559.236 Electron/40.8.2 Safari/537.36"
        )
    }
}

// ---------------------------------------------------------------------------
// 自适应冷却熔断器（Cool-down Circuit Breaker）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoolingAccountEntry {
    pub account_id: String,
    pub label: String,
    pub cooled_at: i64,
    pub cooldown_until: i64,
    pub reason: String,
}

static COOLING_ACCOUNTS: Mutex<Option<HashMap<String, CoolingAccountEntry>>> = Mutex::new(None);

/// 冷却表落盘路径（1.5.0）：此前纯内存，部署重启即丢——封禁 12h 冷却
/// 会在每次发版后凭空消失（2026-10-02 实测），故持久化到 store 目录。
fn cooling_file() -> std::path::PathBuf {
    crate::modules::config::store_dir().join("protection_cooling.json")
}

fn with_cooling_map<R>(f: impl FnOnce(&mut HashMap<String, CoolingAccountEntry>) -> R) -> R {
    let mut lock = COOLING_ACCOUNTS.lock().unwrap();
    if lock.is_none() {
        // 首次访问：从磁盘恢复（重启不丢冷却）
        let restored = std::fs::read_to_string(cooling_file())
            .ok()
            .and_then(|t| serde_json::from_str::<HashMap<String, CoolingAccountEntry>>(&t).ok())
            .unwrap_or_default();
        lock.insert(restored);
    }
    let map = lock.as_mut().unwrap();
    let result = f(map);
    // 变更后落盘（含过期条目清理后的状态）
    let _ = std::fs::write(cooling_file(), serde_json::to_string_pretty(map).unwrap_or_default());
    result
}

/// 检查账号是否正处于冷却保护中（已过期的条目自动清理）。
pub fn is_account_cooling(account_id: &str) -> bool {
    let cfg = load_protection_config();
    if !cfg.enabled {
        return false;
    }
    let now = now_ms();
    with_cooling_map(|map| {
        if let Some(entry) = map.get(account_id) {
            if entry.cooldown_until > now {
                return true;
            }
            map.remove(account_id);
        }
        false
    })
}

/// 标记账号进入冷却保护，并在桌面弹出警示通知。
pub fn mark_account_cooling(account_id: &str, label: &str, reason: &str, cooldown_minutes: u64) {
    let cfg = load_protection_config();
    if !cfg.enabled {
        return;
    }
    let now = now_ms();
    let cooldown_duration_ms = (cooldown_minutes.max(1) as i64) * 60 * 1000;
    let cooldown_until = now + cooldown_duration_ms;

    with_cooling_map(|map| {
        map.insert(
            account_id.to_string(),
            CoolingAccountEntry {
                account_id: account_id.to_string(),
                label: label.to_string(),
                cooled_at: now,
                cooldown_until,
                reason: reason.to_string(),
            },
        );
    });

    // 记录保活与安全防护日志
    record_protection_log(
        account_id,
        label,
        "cooling_triggered",
        &format!("触发冷却熔断 {cooldown_minutes} 分钟：{reason}"),
    );

    // 桌面 Toast 告警
    let (platform_title, notify_tag) = if account_id.starts_with("loomy:") {
        ("Loomy 账号防护熔断", "loomy_proxy")
    } else if account_id.starts_with("trae_work_cn:") {
        ("TRAE 账号防护熔断", "trae_proxy")
    } else {
        ("AutoClaw 账号防护熔断", "glm_proxy")
    };
    notify::notify(
        notify_tag,
        platform_title,
        &format!(
            "账号「{label}」因「{reason}」已进入 {cooldown_minutes} 分钟防护冷却期，代理选号将自动避开。"
        ),
    );
}

/// 手动解除账号的冷却状态。
pub fn clear_account_cooling(account_id: &str) -> bool {
    with_cooling_map(|map| map.remove(account_id).is_some())
}

/// 获取当前所有正在冷却保护中的账号列表（含剩余倒计时秒数）。
pub fn get_cooling_accounts() -> Vec<Value> {
    let now = now_ms();
    with_cooling_map(|map| {
        // 清理过期条目
        map.retain(|_, entry| entry.cooldown_until > now);
        let mut list: Vec<Value> = map
            .values()
            .map(|entry| {
                let remaining_secs = ((entry.cooldown_until - now) / 1000).max(0);
                json!({
                    "accountId": entry.account_id,
                    "label": entry.label,
                    "cooledAt": entry.cooled_at,
                    "cooldownUntil": entry.cooldown_until,
                    "remainingSeconds": remaining_secs,
                    "reason": entry.reason,
                })
            })
            .collect();
        list.sort_by_key(|v| v["remainingSeconds"].as_i64().unwrap_or(0));
        list
    })
}

// ---------------------------------------------------------------------------
// 单账号并发槽位限制（Concurrency Limiter）
// ---------------------------------------------------------------------------

static ACTIVE_REQUESTS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

pub struct ConcurrencyGuard {
    account_id: String,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        let mut lock = ACTIVE_REQUESTS.lock().unwrap();
        if let Some(map) = lock.as_mut() {
            if let Some(count) = map.get_mut(&self.account_id) {
                if *count > 1 {
                    *count -= 1;
                } else {
                    map.remove(&self.account_id);
                }
            }
        }
    }
}

/// 尝试获取单账号的并发槽位。若当前并发数已达上限，返回 None。
pub fn try_acquire_concurrency_slot(
    account_id: &str,
    max_concurrency: usize,
) -> Option<ConcurrencyGuard> {
    let mut lock = ACTIVE_REQUESTS.lock().unwrap();
    let map = lock.get_or_insert_with(HashMap::new);
    let current = map.get(account_id).copied().unwrap_or(0);
    if current < max_concurrency {
        map.insert(account_id.to_string(), current + 1);
        Some(ConcurrencyGuard {
            account_id: account_id.to_string(),
        })
    } else {
        None
    }
}

/// 获取当前各账号正在运行中的并发数快照。
pub fn get_active_concurrency_map() -> HashMap<String, usize> {
    ACTIVE_REQUESTS
        .lock()
        .unwrap()
        .as_ref()
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 保活日志与历史记录
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtectionLogEntry {
    pub ts: i64,
    pub account_id: String,
    pub label: String,
    pub action: String,
    pub detail: String,
}

static PROTECTION_LOGS: Mutex<Option<VecDeque<ProtectionLogEntry>>> = Mutex::new(None);
static LAST_KEEPALIVE_TS: AtomicI64 = AtomicI64::new(0);
static NEXT_KEEPALIVE_TS: AtomicI64 = AtomicI64::new(0);

pub fn record_protection_log(account_id: &str, label: &str, action: &str, detail: &str) {
    let mut lock = PROTECTION_LOGS.lock().unwrap();
    let logs = lock.get_or_insert_with(VecDeque::new);
    if logs.len() >= MAX_LOG_HISTORY {
        logs.pop_front();
    }
    logs.push_back(ProtectionLogEntry {
        ts: now_ms(),
        account_id: account_id.to_string(),
        label: label.to_string(),
        action: action.to_string(),
        detail: detail.to_string(),
    });
}

pub fn get_recent_protection_logs() -> Vec<ProtectionLogEntry> {
    PROTECTION_LOGS
        .lock()
        .unwrap()
        .as_ref()
        .map(|d| d.iter().cloned().rev().collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 主动保活巡检任务
// ---------------------------------------------------------------------------

/// 单次保活巡检报告
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeepaliveCycleReport {
    pub executed_at: i64,
    pub total_accounts: usize,
    pub healthy_accounts: usize,
    pub refreshed_accounts: usize,
    pub cooling_skipped: usize,
    pub failed_accounts: usize,
    /// TRAE Work CN 巡检计数（与上面 AutoClaw 字段并列，互斥占用）。
    #[serde(default)]
    pub trae_total_accounts: usize,
    #[serde(default)]
    pub trae_healthy_accounts: usize,
    #[serde(default)]
    pub trae_refreshed_accounts: usize,
    #[serde(default)]
    pub trae_cooling_skipped: usize,
    #[serde(default)]
    pub trae_failed_accounts: usize,
}

/// 执行一轮 AutoClaw 账号的主动保活巡检：
/// 1. 检查 JWT exp：若剩余有效期低于阈值，提前发起静默续期；
/// 2. 心跳探测：调用官方只读接口（零积分消耗），保持网关会话活跃与活跃时间戳；
/// 3. 若遇 401 自动执行刷新自愈。
pub async fn run_keepalive_cycle() -> KeepaliveCycleReport {
    let cfg = load_protection_config();
    let now = now_ms();
    LAST_KEEPALIVE_TS.store(now, Ordering::Relaxed);
    let next = now + (cfg.keepalive_interval_minutes as i64) * 60 * 1000;
    NEXT_KEEPALIVE_TS.store(next, Ordering::Relaxed);

    let pool = pool_accounts();
    let total_accounts = pool.len();
    let mut healthy_accounts = 0usize;
    let mut refreshed_accounts = 0usize;
    let mut cooling_skipped = 0usize;
    let mut failed_accounts = 0usize;

    for account in pool {
        if is_account_cooling(&account.id) {
            cooling_skipped += 1;
            record_protection_log(
                &account.id,
                &account.label,
                "cooling_skipped",
                "账号处于冷却期，本次保活跳过",
            );
            continue;
        }

        // 1. JWT 有效期前瞻式检查
        let mut current_token = account.access_token.clone();
        if let Some(exp_sec) = jwt_exp_from_access_token(&current_token) {
            let current_sec = now_secs();
            let remaining_sec = exp_sec - current_sec;
            let refresh_ahead_sec = (cfg.jwt_refresh_ahead_minutes as i64) * 60;

            if remaining_sec < refresh_ahead_sec {
                // 剩余时间不足，提前静默刷新
                match account.refresh().await {
                    Ok(new_token) => {
                        current_token = new_token;
                        refreshed_accounts += 1;
                        record_protection_log(
                            &account.id,
                            &account.label,
                            "jwt_refreshed",
                            &format!(
                                "Token 剩余 {:.1} 小时，前瞻式静默续期成功",
                                remaining_sec as f64 / 3600.0
                            ),
                        );
                    }
                    Err(e) => {
                        record_protection_log(
                            &account.id,
                            &account.label,
                            "jwt_refresh_failed",
                            &format!("前瞻式续期失败: {e}"),
                        );
                    }
                }
            }
        }

        // 2. 发起只读心跳探测（复用 get_autoclaw_status_with，查 task-list 零积分消耗）
        let bearer = format!("Bearer {}", current_token.trim_start_matches("Bearer "));
        let status = get_autoclaw_status_with(&bearer).await;

        if status.get("ok").and_then(Value::as_bool) == Some(true) {
            healthy_accounts += 1;
            record_protection_log(
                &account.id,
                &account.label,
                "heartbeat_ok",
                "心跳探测成功，网关会话保持活跃",
            );
        } else {
            // 心跳异常，尝试刷新自愈一次
            let err_msg = status
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("未知异常");
            record_protection_log(
                &account.id,
                &account.label,
                "heartbeat_retry",
                &format!("心跳探测未通（{err_msg}），尝试刷新续期..."),
            );

            match account.refresh().await {
                Ok(new_token) => {
                    let new_bearer =
                        format!("Bearer {}", new_token.trim_start_matches("Bearer "));
                    let retry_status = get_autoclaw_status_with(&new_bearer).await;
                    if retry_status.get("ok").and_then(Value::as_bool) == Some(true) {
                        healthy_accounts += 1;
                        refreshed_accounts += 1;
                        record_protection_log(
                            &account.id,
                            &account.label,
                            "recovered",
                            "续期后心跳恢复正常",
                        );
                    } else {
                        failed_accounts += 1;
                        record_protection_log(
                            &account.id,
                            &account.label,
                            "failed",
                            "续期后心跳仍不可用",
                        );
                    }
                }
                Err(e) => {
                    failed_accounts += 1;
                    record_protection_log(
                        &account.id,
                        &account.label,
                        "failed",
                        &format!("登录态续期失败: {e}"),
                    );
                }
            }
        }

        // 账号间随机平滑间隔 500ms，避免并发打扰
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    KeepaliveCycleReport {
        executed_at: now,
        total_accounts,
        healthy_accounts,
        refreshed_accounts,
        cooling_skipped,
        failed_accounts,
        trae_total_accounts: 0,
        trae_healthy_accounts: 0,
        trae_refreshed_accounts: 0,
        trae_cooling_skipped: 0,
        trae_failed_accounts: 0,
    }
}

static LAST_LOOMY_KEEPALIVE_TS: AtomicI64 = AtomicI64::new(0);
static NEXT_LOOMY_KEEPALIVE_TS: AtomicI64 = AtomicI64::new(0);

/// 执行一轮 Loomy 账号的主动保活巡检：
/// 1. 检查 Session 有效期（14 天到期预警与守护）；
/// 2. 心跳探测：调用官方积分记录接口（零积分消耗），保持网关会话活跃；
/// 3. 若遇 401/100002 自动侦测本地 auth-session.json 变动进行静默自愈。
pub async fn run_loomy_keepalive_cycle() -> KeepaliveCycleReport {
    let cfg = load_protection_config();
    let now = now_ms();
    LAST_LOOMY_KEEPALIVE_TS.store(now, Ordering::Relaxed);
    let next = now + (cfg.keepalive_interval_minutes as i64) * 60 * 1000;
    NEXT_LOOMY_KEEPALIVE_TS.store(next, Ordering::Relaxed);

    let pool = crate::modules::loomy::pool_sessions();
    let total_accounts = pool.len();
    let mut healthy_accounts = 0usize;
    let mut refreshed_accounts = 0usize;
    let mut cooling_skipped = 0usize;
    let mut failed_accounts = 0usize;

    for (account_id, session, store_id) in pool {
        let label = store_id.as_deref().unwrap_or(&account_id);
        if is_account_cooling(&account_id) {
            cooling_skipped += 1;
            record_protection_log(
                &account_id,
                label,
                "cooling_skipped",
                "账号处于冷却期，本次保活跳过",
            );
            continue;
        }

        // 1. 发起只读心跳探测（查积分零积分消耗，保持会话活跃并更新 balance）
        let credits = crate::modules::loomy::get_loomy_credits_with(&session).await;
        if credits.get("ok").and_then(Value::as_bool) == Some(true) {
            healthy_accounts += 1;
            let bal = credits.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
            let daily = credits.get("dailyBalance").and_then(Value::as_f64).unwrap_or(0.0);
            record_protection_log(
                &account_id,
                label,
                "heartbeat_ok",
                &format!("心跳正常，网关会话活跃（总余额: {bal:.0}, 今日: {daily:.0}）"),
            );
        } else {
            // 心跳异常：若为登录失效，尝试从本地客户端自愈
            let err_msg = credits
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("网关校验未通过");

            record_protection_log(
                &account_id,
                label,
                "heartbeat_retry",
                &format!("心跳未通（{err_msg}），尝试探测本地新凭证自愈..."),
            );

            let (synced, new_session_opt) = crate::modules::loomy::sync_local_session_if_changed();
            if synced && new_session_opt.is_some() {
                let fresh_session = new_session_opt.unwrap();
                let retry_credits = crate::modules::loomy::get_loomy_credits_with(&fresh_session).await;
                if retry_credits.get("ok").and_then(Value::as_bool) == Some(true) {
                    healthy_accounts += 1;
                    refreshed_accounts += 1;
                    record_protection_log(
                        &account_id,
                        label,
                        "recovered",
                        "侦测到本地客户端新 Session，已自动无缝自愈！",
                    );
                } else {
                    failed_accounts += 1;
                    record_protection_log(
                        &account_id,
                        label,
                        "failed",
                        "更新本地凭证后心跳仍失败",
                    );
                }
            } else {
                failed_accounts += 1;
                record_protection_log(
                    &account_id,
                    label,
                    "failed",
                    &format!("心跳探测失败：{err_msg}"),
                );
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    KeepaliveCycleReport {
        executed_at: now,
        total_accounts,
        healthy_accounts,
        refreshed_accounts,
        cooling_skipped,
        failed_accounts,
        trae_total_accounts: 0,
        trae_healthy_accounts: 0,
        trae_refreshed_accounts: 0,
        trae_cooling_skipped: 0,
        trae_failed_accounts: 0,
    }
}

static LAST_TRAE_KEEPALIVE_TS: AtomicI64 = AtomicI64::new(0);
static NEXT_TRAE_KEEPALIVE_TS: AtomicI64 = AtomicI64::new(0);

/// 执行一轮 TRAE Work CN 账号的主动保活巡检：
/// 1. 已到期（entry_expired）与冷却期条目跳过；
/// 2. expiredAt 落入前瞻窗口（默认 60 分钟）的条目先静默续期（不判死，见 trae_accounts F1）；
/// 3. 只读心跳：credits_stored 额度查询（零积分消耗）；
/// 4. 心跳失败自动刷新自愈一次；
/// 5. 账号间 500ms 平滑间隔；连续 2 个以上失败弹一条 toast（category trae-keepalive）。
pub async fn run_trae_keepalive_cycle() -> KeepaliveCycleReport {
    let cfg = load_protection_config();
    let now = now_ms();
    LAST_TRAE_KEEPALIVE_TS.store(now, Ordering::Relaxed);
    let next = now + (cfg.keepalive_interval_minutes as i64) * 60 * 1000;
    NEXT_TRAE_KEEPALIVE_TS.store(next, Ordering::Relaxed);

    let pool = crate::modules::trae_accounts::load_accounts();
    let trae_total_accounts = pool.len();
    let mut trae_healthy_accounts = 0usize;
    let mut trae_refreshed_accounts = 0usize;
    let mut trae_cooling_skipped = 0usize;
    let mut trae_failed_accounts = 0usize;

    for entry in pool {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let label = entry
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_string();
        if crate::modules::trae_accounts::entry_expired(&entry) {
            record_protection_log(&id, &label, "expired_skipped", "条目已到期，需重新导入快照");
            continue;
        }
        if is_account_cooling(&id) {
            trae_cooling_skipped += 1;
            record_protection_log(&id, &label, "cooling_skipped", "账号处于冷却期，本次保活跳过");
            continue;
        }

        // 1) expiredAt 前瞻：临近过期先静默续期（失败不判死，仅记 lastError）。
        if let Some(expired_at) = entry.get("expiredAt").and_then(Value::as_str) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(expired_at) {
                let remaining_sec = dt.timestamp() - now_secs();
                let ahead_sec = (cfg.jwt_refresh_ahead_minutes as i64) * 60;
                if remaining_sec < ahead_sec {
                    match crate::modules::trae_accounts::refresh_stored_account(&id).await {
                        Ok(_) => {
                            trae_refreshed_accounts += 1;
                            record_protection_log(
                                &id,
                                &label,
                                "jwt_refreshed",
                                &format!(
                                    "Token 剩余 {:.1} 小时，前瞻式静默续期成功",
                                    remaining_sec as f64 / 3600.0
                                ),
                            );
                        }
                        Err(e) => {
                            record_protection_log(
                                &id,
                                &label,
                                "jwt_refresh_failed",
                                &format!("前瞻式续期失败（不判死）: {e}"),
                            );
                        }
                    }
                }
            }
        }

        // 2) 只读心跳：额度查询（零积分消耗）。
        let credits = crate::modules::trae_accounts::credits_stored(&id).await;
        if credits.get("ok").and_then(Value::as_bool) == Some(true) {
            trae_healthy_accounts += 1;
            record_protection_log(&id, &label, "heartbeat_ok", "TRAE 额度查询心跳正常，凭证保持活跃");
        } else {
            // 3) 心跳失败：刷新自愈一次后复检（credits_stored 内部已带一次续期，这里显式再刷一次）。
            let err_msg = credits
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("未知异常");
            record_protection_log(
                &id,
                &label,
                "heartbeat_retry",
                &format!("TRAE 心跳未通（{err_msg}），尝试刷新自愈..."),
            );
            match crate::modules::trae_accounts::refresh_stored_account(&id).await {
                Ok(_) => {
                    let retry = crate::modules::trae_accounts::credits_stored(&id).await;
                    if retry.get("ok").and_then(Value::as_bool) == Some(true) {
                        trae_healthy_accounts += 1;
                        trae_refreshed_accounts += 1;
                        record_protection_log(&id, &label, "recovered", "续期后 TRAE 心跳恢复正常");
                    } else {
                        trae_failed_accounts += 1;
                        record_protection_log(&id, &label, "failed", "续期后 TRAE 心跳仍不可用");
                    }
                }
                Err(e) => {
                    trae_failed_accounts += 1;
                    record_protection_log(
                        &id,
                        &label,
                        "failed",
                        &format!("TRAE 登录态续期失败（不判死）: {e}"),
                    );
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if trae_failed_accounts >= 2 {
        notify::notify(
            "trae-keepalive",
            "TRAE 账号保活巡检异常",
            &format!(
                "本轮 {} 个账号中 {} 个保活失败，请检查 TRAE 客户端登录态与网络。",
                trae_total_accounts, trae_failed_accounts
            ),
        );
    }

    KeepaliveCycleReport {
        executed_at: now,
        total_accounts: 0,
        healthy_accounts: 0,
        refreshed_accounts: 0,
        cooling_skipped: 0,
        failed_accounts: 0,
        trae_total_accounts,
        trae_healthy_accounts,
        trae_refreshed_accounts,
        trae_cooling_skipped,
        trae_failed_accounts,
    }
}

/// TRAE 保护状态汇总（供 API 与前端面板展示，前缀 trae_work_cn: 隔离）。
pub fn get_trae_protection_status() -> Value {
    let cfg = load_protection_config();
    let all_cooling = get_cooling_accounts();
    let cooling: Vec<Value> = all_cooling
        .into_iter()
        .filter(|a| {
            a.get("accountId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .starts_with("trae_work_cn:")
        })
        .collect();
    let active_concurrency: HashMap<String, usize> = get_active_concurrency_map()
        .into_iter()
        .filter(|(k, _)| k.starts_with("trae_work_cn:"))
        .collect();
    let all_logs = get_recent_protection_logs();
    let logs: Vec<ProtectionLogEntry> = all_logs
        .into_iter()
        .filter(|l| l.account_id.starts_with("trae_work_cn:"))
        .collect();
    let pool = crate::modules::trae_accounts::load_accounts();

    json!({
        "config": cfg,
        "status": if cfg.enabled { "active" } else { "disabled" },
        "lastKeepaliveAt": LAST_TRAE_KEEPALIVE_TS.load(Ordering::Relaxed),
        "nextKeepaliveAt": NEXT_TRAE_KEEPALIVE_TS.load(Ordering::Relaxed),
        "coolingAccounts": cooling,
        "activeConcurrency": active_concurrency,
        "poolAccountCount": pool.len(),
        "recentLogs": logs,
    })
}

/// 后台常驻保活协程
pub fn spawn_protection_loop() {
    tokio::spawn(async move {
        // 启动延迟 15 秒执行首次保活巡检，避开开机启动峰值
        tokio::time::sleep(Duration::from_secs(15)).await;
        let _ = run_keepalive_cycle().await;
        let _ = run_loomy_keepalive_cycle().await;
        let _ = run_trae_keepalive_cycle().await;

        loop {
            let cfg = load_protection_config();
            let interval_mins = if cfg.enabled && cfg.keepalive_enabled {
                cfg.keepalive_interval_minutes.max(15)
            } else {
                30
            };

            // 增加 ±3 分钟的离散抖动，防止固定周期特征被风控识别
            let jitter_secs = (uuid::Uuid::new_v4().as_u128() % 360) as i64 - 180;
            let total_secs = ((interval_mins * 60) as i64 + jitter_secs).max(60) as u64;

            tokio::time::sleep(Duration::from_secs(total_secs)).await;
            if cfg.enabled && cfg.keepalive_enabled {
                let _ = run_keepalive_cycle().await;
                let _ = run_loomy_keepalive_cycle().await;
                let _ = run_trae_keepalive_cycle().await;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 状态汇总（供 API 与前端面板展示）
// ---------------------------------------------------------------------------

pub fn get_protection_status() -> Value {
    let cfg = load_protection_config();
    let all_cooling = get_cooling_accounts();
    let cooling: Vec<Value> = all_cooling
        .into_iter()
        .filter(|a| !a.get("accountId").and_then(Value::as_str).unwrap_or("").starts_with("loomy:"))
        .collect();
    let active_concurrency: HashMap<String, usize> = get_active_concurrency_map()
        .into_iter()
        .filter(|(k, _)| !k.starts_with("loomy:"))
        .collect();
    let all_logs = get_recent_protection_logs();
    let logs: Vec<ProtectionLogEntry> = all_logs
        .into_iter()
        .filter(|l| !l.account_id.starts_with("loomy:"))
        .collect();
    let pool = pool_accounts();

    json!({
        "config": cfg,
        "status": if cfg.enabled { "active" } else { "disabled" },
        "lastKeepaliveAt": LAST_KEEPALIVE_TS.load(Ordering::Relaxed),
        "nextKeepaliveAt": NEXT_KEEPALIVE_TS.load(Ordering::Relaxed),
        "coolingAccounts": cooling,
        "activeConcurrency": active_concurrency,
        "poolAccountCount": pool.len(),
        "simulatedUserAgent": simulated_autoclaw_user_agent(),
        "recentLogs": logs,
    })
}

pub fn get_loomy_protection_status() -> Value {
    let cfg = load_protection_config();
    let all_cooling = get_cooling_accounts();
    let cooling: Vec<Value> = all_cooling
        .into_iter()
        .filter(|a| a.get("accountId").and_then(Value::as_str).unwrap_or("").starts_with("loomy:"))
        .collect();
    let active_concurrency: HashMap<String, usize> = get_active_concurrency_map()
        .into_iter()
        .filter(|(k, _)| k.starts_with("loomy:"))
        .collect();
    let all_logs = get_recent_protection_logs();
    let logs: Vec<ProtectionLogEntry> = all_logs
        .into_iter()
        .filter(|l| l.account_id.starts_with("loomy:"))
        .collect();
    let pool = crate::modules::loomy::pool_sessions();

    json!({
        "config": cfg,
        "status": if cfg.enabled { "active" } else { "disabled" },
        "lastKeepaliveAt": LAST_LOOMY_KEEPALIVE_TS.load(Ordering::Relaxed),
        "nextKeepaliveAt": NEXT_LOOMY_KEEPALIVE_TS.load(Ordering::Relaxed),
        "coolingAccounts": cooling,
        "activeConcurrency": active_concurrency,
        "poolAccountCount": pool.len(),
        "simulatedUserAgent": simulated_loomy_user_agent(),
        "recentLogs": logs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose;
    use base64::Engine;

    #[test]
    fn test_simulated_user_agent() {
        let ua = simulated_autoclaw_user_agent();
        assert!(ua.contains("AutoClaw/"));
        assert!(ua.contains("Chrome/"));
        assert!(ua.contains("Electron/"));
        assert!(ua.contains("Mozilla/5.0"));
    }

    #[test]
    fn test_simulated_loomy_user_agent() {
        let ua = simulated_loomy_user_agent();
        assert!(ua.contains("Loomy/"));
        assert!(ua.contains("Chrome/"));
        assert!(ua.contains("Electron/"));
        assert!(ua.contains("Mozilla/5.0"));
    }

    #[test]
    fn test_cooling_circuit_breaker() {
        let account_id = "autoclaw:test_cooling_acc";
        assert!(!is_account_cooling(account_id));

        // 标记冷却 5 分钟
        mark_account_cooling(account_id, "测试账号", "测试限流熔断", 5);
        assert!(is_account_cooling(account_id));

        let cooling = get_cooling_accounts();
        assert!(cooling.iter().any(|a| a["accountId"] == account_id));

        // 清理冷却
        assert!(clear_account_cooling(account_id));
        assert!(!is_account_cooling(account_id));
    }

    #[test]
    fn test_concurrency_limiter() {
        let account_id = "autoclaw:test_concurrency_acc";
        // 允许最多 2 个并发
        let guard1 = try_acquire_concurrency_slot(account_id, 2);
        assert!(guard1.is_some());

        let guard2 = try_acquire_concurrency_slot(account_id, 2);
        assert!(guard2.is_some());

        // 第 3 个应该被拒绝
        let guard3 = try_acquire_concurrency_slot(account_id, 2);
        assert!(guard3.is_none());

        // 释放一个槽位
        drop(guard1);

        // 现在可以获取了
        let guard4 = try_acquire_concurrency_slot(account_id, 2);
        assert!(guard4.is_some());
    }

    #[test]
    fn test_jwt_exp_parsing() {
        let header = general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = general_purpose::URL_SAFE_NO_PAD.encode(r#"{"user_id":"test_u1","exp":1999999999}"#);
        let token = format!("{header}.{payload}.fake_signature");

        let exp = jwt_exp_from_access_token(&token);
        assert_eq!(exp, Some(1999999999));
    }
}
