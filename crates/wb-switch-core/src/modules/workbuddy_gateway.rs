//! WorkBuddy 网关账号池（P1：账号池化 + 错误治理）。
//!
//! 契约见 `docs/workbuddy-gateway-refactor-contract.md`（唯一事实来源），
//! 接口钉定见契约 §13（签名一字不改）。选号/冷却/熔断/模型冷却口径对照参考实现：
//! - `_workbuddy-gateway-ref/momo0410-switch-gateway/crates/wb-switch-gateway/src/pool.rs`
//! - `_workbuddy-gateway-ref/momo0410-switch-gateway/go-gateway/internal/pool/pool.go`
//!   （模型级冷却 Rust 迁移版尚未实现，按契约 §2-6 直接抄 Go 版口径）
//!
//! # 状态模型（每个账号三个正交维度）
//!
//! 1. **健康维度**：`healthy = !disabled && until 未生效 && breaker_until 未生效`
//!    - `until`：按错误类型的即时冷却（软 429 / 硬 余额耗尽 / 404）
//!    - `breaker_until`：连续失败累计触发熔断的指数退避截止
//! 2. **并发维度**：`in_flight`（在途租约，运行态，不持久化）
//! 3. **统计维度**：`success_count` / `err_total` / `last_used` / `last_used_seq`
//!
//! 与之正交的是**模型级冷却** `model_cools`（uid+model）：上游 6004 是模型级限流，
//! 只封该账号的该模型，不喂熔断器（配额信号 ≠ 账号故障）。
//!
//! # 时间口径
//!
//! 全部使用**毫秒 epoch**（`now_ms()`），状态文件里 0 = None（与 Go 侧零值语义一致）；
//! 读盘时额外兼容秒级旧值（< 10_000_000_000 视为秒，自动 ×1000）。
//!
//! # 文件
//!
//! - `~/.wb-switch/accounts.json`：账号库（唯一真源），网关只读；
//!   唯一例外是 token 刷新成功后的回写（`writeback_tokens`，按过期较新者、空 refresh 不覆盖）。
//! - `~/.wb-switch/workbuddy_gateway_config.json`：池配置（PATCH 即写即生效）。
//! - `~/.wb-switch/workbuddy_gateway_state.json`：池运行态（原子写 + 启动加载）。
//!
//! # 路由层接线约定（供 api.rs 薄接线参考）
//!
//! 粘性入口的「按 model 过滤」由路由层把 sticky uid 放进 `tried` 实现：
//! `sticky_get(key)` → 命中则 `pick_account(model, &[])` 后比对 uid，成功即
//! `sticky_set(key, 实际 uid)` 重绑（对齐参考实现 session.Bind 的「粘性跟随最终成功号」），
//! 失败则 `sticky_clear(key)` 解绑；未命中直接 `pick_account(model, &[])`。
//! 因为 `pick_account` 的统一候选过滤（健康 / 模型冷却 / 在途 / tried）对粘性号一视同仁。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use chrono::{Datelike, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::modules::account::get_str;
use crate::modules::config::{accounts_file, atomic_write, norm_ts, now_ms, store_dir};
use crate::modules::platform_checkin::{account_platform, PLATFORM_WORKBUDDY};
use crate::modules::workbuddy_errors::{self, ErrKind};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 防并发撞号窗口：同一账号在该窗口内不重复被选中（除非候选全部刚被用过）。
/// 对照参考实现 pool.rs:1098 `MIN_PICK_GAP` / pool.go:941 `minPickGap`。
const MIN_PICK_GAP_MS: i64 = 100;
/// 加权短名单大小（Top5）。对照 pool.go `shortlistSize`。
const SHORTLIST_SIZE: usize = 5;
/// 闲置补偿：每小时未用增加的权重。对照 defaultIdleWeightPerHour = 0.5。
const IDLE_WEIGHT_PER_HOUR: f64 = 0.5;
/// 闲置补偿封顶。对照 defaultIdleWeightMax = 5.0。
const IDLE_WEIGHT_MAX: f64 = 5.0;
/// 模型级限流（6004）解析不出重置时间时的回退冷却：600s（契约 §4 表）。
const MODEL_RATE_FALLBACK_MS: i64 = 600_000;
/// 404 短冷却：60s（契约 §4 表）。
const NOT_FOUND_COOLDOWN_MS: i64 = 60_000;
/// 粘性 TTL 缺省：60 分钟滚动（契约 P1 §3）。
const DEFAULT_STICKY_TTL_MS: i64 = 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// 契约 §13 钉定结构：池账号条目 / 池配置
// ---------------------------------------------------------------------------

/// 池账号条目（契约 §11 凭证桥映射产物 + 巡检回填的余额）。
#[derive(Clone, Debug, Default)]
pub struct PoolAccount {
    pub uid: String,
    pub nickname: String,
    /// 区域："cn"（国服，domain 后缀 .cn）| "intl"（国际版，后缀 .ai）。
    pub region: String,
    pub access_token: String,
    pub refresh_token: String,
    /// access token 过期时刻（ms epoch）。
    pub expires_at: i64,
    /// refresh token 过期时刻（ms epoch）。
    pub refresh_expires_at: i64,
    /// 剩余积分；0 = 未知（首轮巡检后回填）。
    pub credits: i64,
    /// 「最近到期积分」的到期时刻（ms epoch）；0 = 未知。驱动选号分层。
    pub credit_expires_at: i64,
}

/// 池配置（契约 §5 / §13）。落 `~/.wb-switch/workbuddy_gateway_config.json`。
/// Default 为手工实现（各字段有非零业务默认值，不能全零派生）。
#[derive(Clone, Debug, PartialEq)]
pub struct GatewayConfig {
    /// 选号模式："balance"（均衡，默认）| "rotate"（烧号）| "pinned"（指定）。
    pub mode: String,
    /// pinned 模式指定的 uid。
    pub pinned_uid: String,
    /// 软冷却（429）基础时长（秒）。
    pub soft_rate_base_secs: u64,
    /// 软冷却指数退避封顶（秒）。
    pub soft_rate_max_secs: u64,
    /// 连续失败触发熔断阈值。
    pub breaker_threshold: i64,
    /// 熔断基础时长（秒）。
    pub breaker_base_secs: u64,
    /// 熔断指数退避封顶（秒）。
    pub breaker_max_secs: u64,
    /// 单账号最大在途请求数；0 = 不限。
    pub max_in_flight: i64,
    /// 会话粘性 TTL（ms，滚动续期）。
    pub sticky_ttl_ms: i64,
    /// 出站脱敏开关（P3 默认开）。
    pub sanitize_enabled: bool,
    /// 余额巡检间隔（分钟，驱动分层选号）。
    pub balance_refresh_minutes: u64,
}

impl Default for GatewayConfig {
    /// 默认口径：均衡模式、软冷却 600s 起封顶 1h、熔断 3 次起 30m 封顶 6h、
    /// 在途 3、粘性 60min、脱敏开、巡检 15min（契约 §4/§5 + 参考实现默认值）。
    fn default() -> Self {
        Self {
            mode: "balance".to_string(),
            pinned_uid: String::new(),
            soft_rate_base_secs: 600,
            soft_rate_max_secs: 3600,
            breaker_threshold: 3,
            breaker_base_secs: 1800,
            breaker_max_secs: 21600,
            max_in_flight: 3,
            sticky_ttl_ms: DEFAULT_STICKY_TTL_MS,
            sanitize_enabled: true,
            balance_refresh_minutes: 15,
        }
    }
}

impl GatewayConfig {
    /// 钳制非法值（PATCH 配置与读盘共用）：mode 白名单外回落 balance、
    /// `<=0` 的阈值/时长视为未设置走默认、封顶不小于基础值。
    fn normalized(mut self) -> Self {
        let d = GatewayConfig::default();
        self.mode = match self.mode.trim() {
            "rotate" => "rotate",
            "pinned" => "pinned",
            _ => "balance",
        }
        .to_string();
        self.pinned_uid = self.pinned_uid.trim().to_string();
        self.soft_rate_base_secs = clamp_u64(self.soft_rate_base_secs, d.soft_rate_base_secs, 1, 86_400);
        self.soft_rate_max_secs =
            clamp_u64(self.soft_rate_max_secs, d.soft_rate_max_secs, 1, 7 * 86_400).max(self.soft_rate_base_secs);
        self.breaker_threshold = clamp_i64(self.breaker_threshold, d.breaker_threshold, 1, 1000);
        self.breaker_base_secs = clamp_u64(self.breaker_base_secs, d.breaker_base_secs, 1, 86_400);
        self.breaker_max_secs =
            clamp_u64(self.breaker_max_secs, d.breaker_max_secs, 1, 7 * 86_400).max(self.breaker_base_secs);
        // max_in_flight 的 0 是合法值（不限），不回落默认。
        self.max_in_flight = self.max_in_flight.clamp(0, 1000);
        self.sticky_ttl_ms = if self.sticky_ttl_ms > 0 {
            self.sticky_ttl_ms.min(7 * 24 * 3600 * 1000)
        } else {
            d.sticky_ttl_ms
        };
        self.balance_refresh_minutes =
            clamp_u64(self.balance_refresh_minutes, d.balance_refresh_minutes, 1, 24 * 60);
        self
    }
}

/// `0 = 未设置 → 走默认`，其余钳到 [lo, hi]。
fn clamp_u64(v: u64, default: u64, lo: u64, hi: u64) -> u64 {
    if v == 0 {
        default
    } else {
        v.clamp(lo, hi)
    }
}

/// `<=0 = 未设置 → 走默认`，其余钳到 [lo, hi]。
fn clamp_i64(v: i64, default: i64, lo: i64, hi: i64) -> i64 {
    if v <= 0 {
        default
    } else {
        v.clamp(lo, hi)
    }
}

// ---------------------------------------------------------------------------
// 配置：load_config / save_config
// ---------------------------------------------------------------------------

/// 池配置文件路径。
pub fn config_path() -> PathBuf {
    store_dir().join("workbuddy_gateway_config.json")
}

/// 读取池配置；缺失/损坏/字段缺席一律回落默认值（对照 workbuddy_proxy 的容错读盘）。
pub fn load_config() -> GatewayConfig {
    load_config_at(&config_path())
}

/// 读盘核心（路径可注入，供测试用临时目录）。
fn load_config_at(path: &Path) -> GatewayConfig {
    let d = GatewayConfig::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return d;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return d;
    };
    GatewayConfig {
        mode: v.get("mode").and_then(Value::as_str).unwrap_or(&d.mode).to_string(),
        pinned_uid: v
            .get("pinned_uid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        soft_rate_base_secs: v
            .get("soft_rate_base_secs")
            .and_then(Value::as_u64)
            .unwrap_or(d.soft_rate_base_secs),
        soft_rate_max_secs: v
            .get("soft_rate_max_secs")
            .and_then(Value::as_u64)
            .unwrap_or(d.soft_rate_max_secs),
        breaker_threshold: v
            .get("breaker_threshold")
            .and_then(Value::as_i64)
            .unwrap_or(d.breaker_threshold),
        breaker_base_secs: v
            .get("breaker_base_secs")
            .and_then(Value::as_u64)
            .unwrap_or(d.breaker_base_secs),
        breaker_max_secs: v
            .get("breaker_max_secs")
            .and_then(Value::as_u64)
            .unwrap_or(d.breaker_max_secs),
        max_in_flight: v
            .get("max_in_flight")
            .and_then(Value::as_i64)
            .unwrap_or(d.max_in_flight),
        sticky_ttl_ms: v
            .get("sticky_ttl_ms")
            .and_then(Value::as_i64)
            .unwrap_or(d.sticky_ttl_ms),
        sanitize_enabled: v
            .get("sanitize_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(d.sanitize_enabled),
        balance_refresh_minutes: v
            .get("balance_refresh_minutes")
            .and_then(Value::as_u64)
            .unwrap_or(d.balance_refresh_minutes),
    }
    .normalized()
}

/// 保存池配置（原子写 + 钳制），并热更新推进活池（契约 §5：PATCH 即写即生效）。
pub fn save_config(cfg: &GatewayConfig) -> Result<(), String> {
    let cfg = cfg.clone().normalized();
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "mode": cfg.mode,
        "pinned_uid": cfg.pinned_uid,
        "soft_rate_base_secs": cfg.soft_rate_base_secs,
        "soft_rate_max_secs": cfg.soft_rate_max_secs,
        "breaker_threshold": cfg.breaker_threshold,
        "breaker_base_secs": cfg.breaker_base_secs,
        "breaker_max_secs": cfg.breaker_max_secs,
        "max_in_flight": cfg.max_in_flight,
        "sticky_ttl_ms": cfg.sticky_ttl_ms,
        "sanitize_enabled": cfg.sanitize_enabled,
        "balance_refresh_minutes": cfg.balance_refresh_minutes,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())?;
    // 热更新：活池参数与粘性 TTL 立即生效（重启只是再次读取同一文件）。
    let mut p = pool();
    p.apply_config(&cfg);
    Ok(())
}

/// 落盘核心（路径可注入，供测试）。
fn save_config_at(path: &Path, cfg: &GatewayConfig) -> Result<(), String> {
    let cfg = cfg.clone().normalized();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let content = serde_json::to_string_pretty(&json!({
        "mode": cfg.mode,
        "pinned_uid": cfg.pinned_uid,
        "soft_rate_base_secs": cfg.soft_rate_base_secs,
        "soft_rate_max_secs": cfg.soft_rate_max_secs,
        "breaker_threshold": cfg.breaker_threshold,
        "breaker_base_secs": cfg.breaker_base_secs,
        "breaker_max_secs": cfg.breaker_max_secs,
        "max_in_flight": cfg.max_in_flight,
        "sticky_ttl_ms": cfg.sticky_ttl_ms,
        "sanitize_enabled": cfg.sanitize_enabled,
        "balance_refresh_minutes": cfg.balance_refresh_minutes,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(path, &content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 凭证桥（accounts.json → 池条目，契约 §11）
// ---------------------------------------------------------------------------

/// 从 `~/.wb-switch/accounts.json` 桥接池条目。**accounts.json 只读**；
/// 文件缺失/损坏返回空列表。
pub fn load_pool_accounts() -> Vec<PoolAccount> {
    load_pool_accounts_at(&accounts_file()).unwrap_or_default()
}

/// 读盘核心（路径可注入，供测试）；返回 `None` 表示文件不可读/不是数组
/// （调用方据此保留内存池，不清空）。
fn load_pool_accounts_at(path: &Path) -> Option<Vec<PoolAccount>> {
    let text = std::fs::read_to_string(path).ok()?;
    pool_accounts_from_json(&text)
}

/// 凭证桥纯计算核心：解析 accounts.json 文本，过滤出 WorkBuddy 账号并映射字段。
///
/// 映射（契约 §11）：uid←auth_raw.uid（缺省回落顶层 uid/id）、region←domain 后缀
/// （.ai→intl，其余→cn）、token←access_token、refresh←refresh_token、
/// expires_at/refresh_expires_at←expiresAt/refreshExpiresAt（秒/毫秒自动归一）。
/// credits/credit_expires_at 首轮巡检后回填，桥接时为 0（未知）。
/// 无 access_token 的账号无法 chat，跳过；非 WorkBuddy 平台账号不进来。
fn pool_accounts_from_json(text: &str) -> Option<Vec<PoolAccount>> {
    let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(text) else {
        return None;
    };
    Some(arr.iter().filter_map(pool_account_from_value).collect())
}

/// 单条账号库记录 → 池条目（纯函数）。
fn pool_account_from_value(acc: &Value) -> Option<PoolAccount> {
    if account_platform(acc) != PLATFORM_WORKBUDDY {
        return None;
    }
    let access_token = get_str(acc, "access_token")?;
    let uid = acc
        .get("auth_raw")
        .and_then(|r| r.get("uid"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| get_str(acc, "uid"))
        .or_else(|| get_str(acc, "id"))?;
    let domain = get_str(acc, "domain").unwrap_or_default();
    let region = if domain.ends_with(".ai") { "intl" } else { "cn" };
    Some(PoolAccount {
        uid,
        nickname: get_str(acc, "nickname").unwrap_or_default(),
        region: region.to_string(),
        access_token,
        refresh_token: get_str(acc, "refresh_token").unwrap_or_default(),
        expires_at: norm_ts(acc.get("expiresAt")).unwrap_or(0),
        refresh_expires_at: norm_ts(acc.get("refreshExpiresAt")).unwrap_or(0),
        credits: 0,
        credit_expires_at: 0,
    })
}

/// 记录是否匹配 uid（auth_raw.uid → 顶层 uid → id，与桥接口径一致）。
fn account_matches_uid(acc: &Value, uid: &str) -> bool {
    if uid.is_empty() {
        return false;
    }
    acc.get("auth_raw")
        .and_then(|r| r.get("uid"))
        .and_then(Value::as_str)
        == Some(uid)
        || get_str(acc, "uid").as_deref() == Some(uid)
        || get_str(acc, "id").as_deref() == Some(uid)
}

/// token 刷新成功后回写账号库（契约 §11：按「过期时间较新者」回写；
/// 空 refresh token 不覆盖已有值）。回写失败只记日志，不阻断请求。
pub fn writeback_tokens(uid: &str, access_token: &str, expires_at: i64) {
    if let Err(e) = writeback_tokens_at(&accounts_file(), uid, access_token, expires_at) {
        eprintln!("[wb-gateway] writeback_tokens FAILED: {e}");
    }
}

/// 回写落盘核心（路径可注入，供测试）。
fn writeback_tokens_at(path: &Path, uid: &str, access_token: &str, expires_at: i64) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut accounts: Vec<Value> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut changed = false;
    for acc in accounts.iter_mut() {
        if account_matches_uid(acc, uid) {
            // 公共入口只回写 access_token + expires_at（refresh 相关字段留空 → 不覆盖）。
            changed |= apply_token_writeback(acc, access_token, "", expires_at, 0);
        }
    }
    if !changed {
        return Ok(());
    }
    let content = serde_json::to_string_pretty(&accounts).map_err(|e| e.to_string())?;
    atomic_write(path, &content).map_err(|e| e.to_string())
}

/// 凭证回写纯计算核心：把刷新得到的新凭证合并进账号库条目。
///
/// 规则（契约 §11）：
/// - 按「过期时间较新者」回写：现有 `expiresAt` 不早于新值时整体跳过（不倒退）；
/// - `access_token` 仅在写入 `expiresAt` 时一并更新；
/// - 空 `refresh_token` 不覆盖已有值；`refresh_expires_at` 只在新值更优时写。
///
/// 返回是否真的改动了条目。
fn apply_token_writeback(
    acc: &mut Value,
    access_token: &str,
    refresh_token: &str,
    expires_at: i64,
    refresh_expires_at: i64,
) -> bool {
    if access_token.trim().is_empty() || expires_at <= 0 {
        return false;
    }
    let cur_exp = norm_ts(acc.get("expiresAt")).unwrap_or(0);
    if cur_exp >= expires_at {
        return false; // 现有的更晚 → 不倒退
    }
    // 先读旧值再拿可变借用：as_object_mut 之后不能再不可变借用 acc。
    let cur_refresh_exp = norm_ts(acc.get("refreshExpiresAt")).unwrap_or(0);
    let Some(obj) = acc.as_object_mut() else {
        return false;
    };
    obj.insert("access_token".to_string(), json!(access_token));
    obj.insert("expiresAt".to_string(), json!(expires_at));
    if !refresh_token.trim().is_empty() {
        obj.insert("refresh_token".to_string(), json!(refresh_token));
    }
    if refresh_expires_at > 0 && refresh_expires_at > cur_refresh_exp {
        obj.insert("refreshExpiresAt".to_string(), json!(refresh_expires_at));
    }
    true
}

// ---------------------------------------------------------------------------
// 账号池内部结构
// ---------------------------------------------------------------------------

/// 冷却类型（账号级即时冷却）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CoolKind {
    /// 429 软限流 / 404 → `"soft_rate"`。
    #[default]
    Soft,
    /// 余额耗尽（402 / 14018）→ `"hard_credit"`，冷却到次日 04:00 等签到。
    Hard,
}

impl CoolKind {
    fn as_str(&self) -> &'static str {
        match self {
            CoolKind::Soft => "soft_rate",
            CoolKind::Hard => "hard_credit",
        }
    }

    /// 持久化编号（与 Go 侧 CoolKind 的 iota 一致：0=soft, 1=hard）。
    fn code(&self) -> i32 {
        match self {
            CoolKind::Soft => 0,
            CoolKind::Hard => 1,
        }
    }
}

/// 单条模型级冷却记录（uid+model）。对照 Go pool.go:1126 `modelCool`。
#[derive(Debug, Clone)]
struct ModelCool {
    /// 冷却截止（ms epoch）：上游给的重置时刻，或解析失败时的回退软冷却。
    until_ms: i64,
    /// 面向用户的说明（含模型名与重置时间）。
    reason: String,
    /// true=到期时间取自上游文案；false=回退固定软冷却。
    reset_parsed: bool,
}

/// 池内部账号条目（凭证 + 运行态 + 持久态）。
#[derive(Debug, Clone)]
struct Entry {
    /// 凭证与元数据（credits/credit_expires_at 是运行态权威值，巡检回填）。
    acc: PoolAccount,
    success_count: i64,
    err_total: i64,
    last_err_ms: i64,
    last_success_ms: i64,
    /// 账号级即时冷却类型。
    cool_kind: Option<CoolKind>,
    /// 账号级即时冷却截止（ms epoch）；0 = 不在冷却期。
    until_ms: i64,
    /// session 死亡（12153）→ 永久禁用，需人工重登。
    disabled: bool,
    reason: String,
    /// 最近被选中时刻（防并发撞号）；0 = 从未使用。
    last_used_ms: i64,
    /// 单调序号：打破 last_used 的时钟粒度平局（Windows 约 511µs 分辨率）。
    /// 对照 pool.rs:912 `older_last_used` / pool.go:836 `olderLastUsed`。
    last_used_seq: i64,
    /// 熔断截止（ms epoch）；0 = 未熔断。
    breaker_until_ms: i64,
    /// 连续失败计数（熔断用，唯一权威）。
    fails: i64,
    /// 已熔断次数（指数退避的指数）。
    retry_count: i64,
    /// 连续软限流次数（驱动软冷却指数退避，成功清零）。
    soft_fails: i64,
    /// 单账号在途请求数（运行态，不持久化）。
    in_flight: i64,
    /// 模型级冷却表（模型名 → 记录）。持久化。
    model_cools: HashMap<String, ModelCool>,
}

impl Entry {
    fn new(acc: PoolAccount) -> Self {
        Self {
            acc,
            success_count: 0,
            err_total: 0,
            last_err_ms: 0,
            last_success_ms: 0,
            cool_kind: None,
            until_ms: 0,
            disabled: false,
            reason: String::new(),
            last_used_ms: 0,
            last_used_seq: 0,
            breaker_until_ms: 0,
            fails: 0,
            retry_count: 0,
            soft_fails: 0,
            in_flight: 0,
            model_cools: HashMap::new(),
        }
    }

    /// 报告账号当前是否可选（未禁用、未处于任一冷却/熔断期）。
    fn healthy(&self, now: i64) -> bool {
        !self.disabled && self.until_ms <= now && self.breaker_until_ms <= now
    }

    /// 当前仍在生效的最近冷却/熔断截止（两个截止取较早者）；不在冷却期返回 0。
    /// 对照 Go pool.go:181 `expiry`（比 Rust 迁移版的 until.or(breaker_until)
    /// 更准：until 已过期时仍会看 breaker_until）。
    fn expiry(&self, now: i64) -> i64 {
        let mut t = 0i64;
        if self.until_ms > now {
            t = self.until_ms;
        }
        if self.breaker_until_ms > now && (t == 0 || self.breaker_until_ms < t) {
            t = self.breaker_until_ms;
        }
        t
    }

    /// 「最近到期积分」的到期日（本地时区 YYYY-MM-DD），空串表示未知。
    /// 用「日」而非精确时刻做分层键：上游额度按天失效，同一天到期视为同一档。
    fn expiry_day_key(&self) -> String {
        if self.acc.credit_expires_at <= 0 {
            return String::new();
        }
        match Local.timestamp_millis_opt(self.acc.credit_expires_at).single() {
            Some(dt) => dt.format("%Y-%m-%d").to_string(),
            None => String::new(),
        }
    }

    /// 报告该账号的指定模型当前是否处于模型级冷却中。
    /// 对照 Go pool.go:1166 `modelCooled`。
    fn model_cooled(&self, model: &str, now: i64) -> bool {
        if model.is_empty() {
            return false;
        }
        match self.model_cools.get(model) {
            Some(mc) => now < mc.until_ms,
            None => false,
        }
    }

    /// 当前仍在生效的模型冷却列表（按到期时间升序，供 /status 透出）。
    /// 对照 Go pool.go:1175 `modelCoolingList`。
    fn model_cooling_list(&self, now: i64) -> Vec<Value> {
        let mut out: Vec<(i64, &str, &ModelCool)> = self
            .model_cools
            .iter()
            .filter(|(_, mc)| mc.until_ms > now)
            .map(|(m, mc)| (mc.until_ms, m.as_str(), mc))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        out.into_iter()
            .map(|(_, m, mc)| {
                json!({
                    "model": m,
                    "until": mc.until_ms,
                    "remaining_sec": ((mc.until_ms - now + 999) / 1000).max(0),
                    "reason": mc.reason,
                    "reset_at_parsed": mc.reset_parsed,
                })
            })
            .collect()
    }

    /// 清掉已过期的模型冷却，避免 map 无限增长。返回是否真的删除了条目。
    /// 对照 Go pool.go:1207 `pruneModelCoolsLocked`。
    fn prune_model_cools(&mut self, now: i64) -> bool {
        let before = self.model_cools.len();
        self.model_cools.retain(|_, mc| mc.until_ms > now);
        self.model_cools.len() != before
    }
}

/// 状态文件中的单个账号（毫秒 epoch，0 = None）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateAccount {
    #[serde(default)]
    credits: i64,
    #[serde(default)]
    credit_expires_at: i64,
    #[serde(default)]
    disabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    reason: String,
    #[serde(default)]
    until_ms: i64,
    #[serde(default)]
    cool_kind: i32,
    #[serde(default)]
    soft_fails: i64,
    #[serde(default)]
    success_count: i64,
    #[serde(default)]
    err_total: i64,
    #[serde(default)]
    last_success_ms: i64,
    #[serde(default)]
    last_err_ms: i64,
    #[serde(default)]
    breaker_until_ms: i64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    model_cools: HashMap<String, StateModelCool>,
}

/// 状态文件中的单条模型冷却。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateModelCool {
    #[serde(default)]
    until_ms: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    reason: String,
    #[serde(default)]
    reset_parsed: bool,
}

/// 状态文件顶层结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    accounts: HashMap<String, StateAccount>,
}

/// 把状态里的时间戳归一为毫秒（兼容秒级旧值；0 = None 原样返回）。
fn norm_state_ms(v: i64) -> i64 {
    norm_ts(Some(&json!(v))).unwrap_or(v)
}

// ---------------------------------------------------------------------------
// 账号池
// ---------------------------------------------------------------------------

/// 账号池：选号（到期分层→Top5 加权→防惊群→LRU 兜底）+ 冷却/熔断/模型冷却/在途租约。
pub struct Pool {
    by_uid: HashMap<String, usize>,
    entries: Vec<Entry>,
    /// 状态文件路径（空 = 不持久化）。
    state_fp: PathBuf,
    dirty: bool,

    // 可调参数（来自 GatewayConfig，apply_config 推进）
    mode: String,
    pinned_uid: String,
    /// 软冷却基础时长（ms）。
    soft_rate_base_ms: i64,
    /// 软冷却指数退避封顶（ms）。
    soft_rate_max_ms: i64,
    breaker_threshold: i64,
    breaker_base_ms: i64,
    breaker_max_ms: i64,
    max_in_flight: i64,

    /// 轮转（烧号）模式下当前正在用的账号（运行态，不持久化）。
    rotate_uid: String,
    /// 选号单调序号（仅由选号路径自增）。
    pick_seq: i64,
    /// 防撞号窗口（ms）；测试可置 0 退化为纯加权分布（对照 Go minPickGap 测试手法）。
    min_pick_gap_ms: i64,
    /// 可选注入的确定性随机源（测试用）；None 用进程级 xorshift。
    rand: Option<Box<dyn Fn(i64) -> i64 + Send + Sync>>,
}

impl Pool {
    /// 构建空池并加载状态文件。`state_fp` 为空表示不持久化。
    pub fn new(state_fp: PathBuf, cfg: &GatewayConfig) -> Self {
        let mut p = Self {
            by_uid: HashMap::new(),
            entries: Vec::new(),
            state_fp,
            dirty: false,
            mode: String::new(),
            pinned_uid: String::new(),
            soft_rate_base_ms: 0,
            soft_rate_max_ms: 0,
            breaker_threshold: 0,
            breaker_base_ms: 0,
            breaker_max_ms: 0,
            max_in_flight: 0,
            rotate_uid: String::new(),
            pick_seq: 0,
            min_pick_gap_ms: MIN_PICK_GAP_MS,
            rand: None,
        };
        p.load_state();
        p.apply_config(cfg);
        p
    }

    /// 推进配置参数（load/save config 与测试共用）。
    pub fn apply_config(&mut self, cfg: &GatewayConfig) {
        let cfg = cfg.clone().normalized();
        self.mode = cfg.mode;
        self.pinned_uid = cfg.pinned_uid;
        self.soft_rate_base_ms = cfg.soft_rate_base_secs as i64 * 1000;
        self.soft_rate_max_ms = cfg.soft_rate_max_secs as i64 * 1000;
        self.breaker_threshold = cfg.breaker_threshold;
        self.breaker_base_ms = cfg.breaker_base_secs as i64 * 1000;
        self.breaker_max_ms = cfg.breaker_max_secs as i64 * 1000;
        self.max_in_flight = cfg.max_in_flight;
        STICKY_TTL_MS.store(cfg.sticky_ttl_ms, Ordering::Relaxed);
    }

    /// 注入确定性随机源（仅供测试）；传 `None` 恢复真实随机。
    pub fn set_random_source(&mut self, f: Option<Box<dyn Fn(i64) -> i64 + Send + Sync>>) {
        self.rand = f;
    }

    /// 设置防撞号窗口（ms）；测试置 0 可退化为纯加权分布。
    pub fn set_min_pick_gap_ms(&mut self, ms: i64) {
        self.min_pick_gap_ms = ms.max(0);
    }

    // -----------------------------------------------------------------------
    // 凭证同步与状态持久化
    // -----------------------------------------------------------------------

    /// 新增或更新账号（保留运行态：冷却/熔断/统计/credits，只刷新凭证与元数据）。
    /// 对照 pool.rs:534 `add`。
    pub fn upsert(&mut self, acc: PoolAccount) {
        if let Some(&idx) = self.by_uid.get(&acc.uid) {
            let e = &mut self.entries[idx];
            // 巡检回填的余额不被凭证同步清掉（accounts.json 不带余额，桥接值为 0）。
            let credits = if acc.credits == 0 { e.acc.credits } else { acc.credits };
            let credit_expires_at = if acc.credit_expires_at == 0 {
                e.acc.credit_expires_at
            } else {
                acc.credit_expires_at
            };
            e.acc = PoolAccount {
                credits,
                credit_expires_at,
                ..acc
            };
            return;
        }
        let uid = acc.uid.clone();
        self.entries.push(Entry::new(acc));
        self.by_uid.insert(uid, self.entries.len() - 1);
    }

    /// 把 accounts.json 的凭证同步进池。读盘失败时原样返回（不清池——
    /// 防止文件被锁/损坏的瞬间把整个池抹掉）。
    pub fn sync_accounts(&mut self) {
        let Some(accounts) = load_pool_accounts_at(&accounts_file()) else {
            return;
        };
        let mut seen: HashMap<String, ()> = HashMap::new();
        for acc in accounts {
            seen.insert(acc.uid.clone(), ());
            self.upsert(acc);
        }
        // 丢掉已不在账号库的条目（state 里的占位凭证 / 已删除账号）。
        self.entries
            .retain(|e| seen.contains_key(&e.acc.uid) && !e.acc.access_token.is_empty());
        self.reindex();
    }

    fn reindex(&mut self) {
        self.by_uid.clear();
        for (i, e) in self.entries.iter().enumerate() {
            self.by_uid.insert(e.acc.uid.clone(), i);
        }
    }

    /// 从状态文件载入持久化状态（覆盖/插入占位凭证）。
    /// 文件缺失或解析失败时静默返回（首次启动属正常情况），不阻断启动。
    pub fn load_state(&mut self) {
        if self.state_fp.as_os_str().is_empty() {
            return;
        }
        let Ok(raw) = std::fs::read_to_string(&self.state_fp) else {
            return;
        };
        let Ok(sf) = serde_json::from_str::<StateFile>(&raw) else {
            return;
        };
        for (uid, s) in sf.accounts {
            let until = norm_state_ms(s.until_ms);
            let breaker = norm_state_ms(s.breaker_until_ms);
            let cool_kind = match s.cool_kind {
                1 => Some(CoolKind::Hard),
                0 if until > 0 => Some(CoolKind::Soft),
                _ => None,
            };
            let model_cools: HashMap<String, ModelCool> = s
                .model_cools
                .into_iter()
                .map(|(m, mc)| {
                    (
                        m,
                        ModelCool {
                            until_ms: norm_state_ms(mc.until_ms),
                            reason: mc.reason,
                            reset_parsed: mc.reset_parsed,
                        },
                    )
                })
                .collect();
            let acc = PoolAccount {
                uid: uid.clone(),
                credits: s.credits,
                credit_expires_at: norm_state_ms(s.credit_expires_at),
                ..Default::default()
            };
            let mut entry = Entry::new(acc);
            entry.success_count = s.success_count;
            entry.err_total = s.err_total;
            entry.last_err_ms = norm_state_ms(s.last_err_ms);
            entry.last_success_ms = norm_state_ms(s.last_success_ms);
            entry.cool_kind = cool_kind;
            entry.until_ms = until;
            entry.disabled = s.disabled;
            entry.reason = s.reason;
            entry.breaker_until_ms = breaker;
            entry.soft_fails = s.soft_fails;
            entry.model_cools = model_cools;
            if let Some(&idx) = self.by_uid.get(&uid) {
                self.entries[idx] = entry;
            } else {
                self.by_uid.insert(uid, self.entries.len());
                self.entries.push(entry);
            }
        }
    }

    /// 把当前状态落盘（原子写）。仅在 dirty 时写，返回是否真的写了盘。
    pub fn flush(&mut self) -> bool {
        if !self.dirty || self.state_fp.as_os_str().is_empty() {
            return false;
        }
        let mut accounts: HashMap<String, StateAccount> = HashMap::new();
        for e in &self.entries {
            let model_cools: HashMap<String, StateModelCool> = e
                .model_cools
                .iter()
                .map(|(m, mc)| {
                    (
                        m.clone(),
                        StateModelCool {
                            until_ms: mc.until_ms,
                            reason: mc.reason.clone(),
                            reset_parsed: mc.reset_parsed,
                        },
                    )
                })
                .collect();
            accounts.insert(
                e.acc.uid.clone(),
                StateAccount {
                    credits: e.acc.credits,
                    credit_expires_at: e.acc.credit_expires_at,
                    disabled: e.disabled,
                    reason: e.reason.clone(),
                    until_ms: e.until_ms,
                    cool_kind: e.cool_kind.map(|k| k.code()).unwrap_or(0),
                    soft_fails: e.soft_fails,
                    success_count: e.success_count,
                    err_total: e.err_total,
                    last_success_ms: e.last_success_ms,
                    last_err_ms: e.last_err_ms,
                    breaker_until_ms: e.breaker_until_ms,
                    model_cools,
                },
            );
        }
        let sf = StateFile { accounts };
        let Ok(raw) = serde_json::to_string_pretty(&sf) else {
            return false;
        };
        if let Some(parent) = self.state_fp.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if atomic_write(&self.state_fp, &raw).is_ok() {
            self.dirty = false;
            true
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------
    // 在途租约
    // -----------------------------------------------------------------------

    /// 报告账号是否已占满在途名额（max=0 不限 → 恒 false）。
    fn in_flight_full(&self, e: &Entry) -> bool {
        self.max_in_flight > 0 && e.in_flight >= self.max_in_flight
    }

    /// 占用在途名额；已满或账号不存在返回 false。
    pub fn acquire(&mut self, uid: &str) -> bool {
        let Some(&idx) = self.by_uid.get(uid) else {
            return false;
        };
        if self.in_flight_full(&self.entries[idx]) {
            return false;
        }
        self.entries[idx].in_flight += 1;
        true
    }

    /// 释放在途名额（不下溢）。
    pub fn release(&mut self, uid: &str) {
        if let Some(&idx) = self.by_uid.get(uid) {
            let e = &mut self.entries[idx];
            if e.in_flight > 0 {
                e.in_flight -= 1;
            }
        }
    }

    // -----------------------------------------------------------------------
    // 结果记账：成功 / 错误分级
    // -----------------------------------------------------------------------

    /// 记录一次成功：累加成功计数、清连续失败/熔断/软冷却退避运行态。
    /// 对照 pool.rs:606 `note_success` / pool.go:1349 `NoteSuccess`
    /// （不碰 until/cool_kind——那些是即时冷却，各自到期）。
    pub fn note_success(&mut self, uid: &str) {
        let Some(&idx) = self.by_uid.get(uid) else {
            return;
        };
        let e = &mut self.entries[idx];
        e.success_count += 1;
        e.last_success_ms = now_ms();
        e.fails = 0;
        e.retry_count = 0;
        e.soft_fails = 0;
        e.breaker_until_ms = 0;
        self.dirty = true;
    }

    /// 记录一次 5xx 失败：累计错误 + 喂熔断（不设账号级冷却，由换号重试兜底）。
    /// 对照 pool.go:1336 `NoteError`。
    pub fn note_server_error(&mut self, uid: &str) {
        let Some(&idx) = self.by_uid.get(uid) else {
            return;
        };
        let e = &mut self.entries[idx];
        e.err_total += 1;
        e.last_err_ms = now_ms();
        self.record_breaker_failure(idx, true);
        self.dirty = true;
    }

    /// 按 ErrKind 分流冷却（契约 §4 表 + 参考实现 handler.go:593 `applyErrorPolicy`）。
    ///
    /// - HardCredit → 硬冷却到次日 04:00（等签到），喂熔断（放大指数）；
    /// - ModelRate → 模型级冷却到 `parse_reset_time`（解析不出回退 600s），**不喂熔断**；
    /// - SoftRate → 软冷却 600s 起指数退避封顶 soft_rate_max，喂熔断但不放大指数；
    /// - NotFound → 软冷却 60s（防雪崩），喂熔断但不放大指数；
    /// - SessionDead → 永久禁用（需人工重登），不喂熔断；
    /// - Server → 累计错误 + 喂熔断（放大指数）；
    /// - ContextTooLong/Client/None/Transport → 只换号不罚（传输层错误不喂熔断）。
    pub fn note_error(&mut self, uid: &str, model: &str, kind: ErrKind, body: &str) {
        let Some(&idx) = self.by_uid.get(uid) else {
            return;
        };
        let now = now_ms();
        match kind {
            ErrKind::HardCredit => {
                let until = next_local_4am_ms(now);
                self.set_cooldown(idx, CoolKind::Hard, until, "余额不足", true);
            }
            ErrKind::ModelRate => {
                // 模型级限流（429 code=6004）：只冷却该账号的这个模型。
                // 上游文案给出确切重置时刻，优先用它；解析不出回退固定软冷却。
                if model.is_empty() {
                    return;
                }
                let (until, parsed) = match workbuddy_errors::parse_reset_time(body, now) {
                    Some(t) if t > now => (t, true),
                    _ => (now + MODEL_RATE_FALLBACK_MS, false),
                };
                let reason = model_rate_reason(model, until, parsed);
                self.cooldown_model(idx, model, until, &reason, parsed);
            }
            ErrKind::SoftRate => {
                // 软冷却指数退避：600s 起，每次连续软限流 ×2，封顶 soft_rate_max。
                let soft_fails = self.entries[idx].soft_fails + 1;
                self.entries[idx].soft_fails = soft_fails;
                let mut d = self.soft_rate_base_ms;
                for _ in 1..soft_fails {
                    d = d.saturating_mul(2);
                    if d >= self.soft_rate_max_ms {
                        d = self.soft_rate_max_ms;
                        break;
                    }
                }
                let d = d.clamp(1, self.soft_rate_max_ms.max(1));
                self.set_cooldown(idx, CoolKind::Soft, now + d, "429 rate limit", false);
            }
            ErrKind::NotFound => {
                // 404 短冷却（软冷却），防雪崩。
                self.set_cooldown(
                    idx,
                    CoolKind::Soft,
                    now + NOT_FOUND_COOLDOWN_MS,
                    "upstream 404",
                    false,
                );
            }
            ErrKind::SessionDead => {
                // session 死亡：永久禁用，需人工重登（不喂熔断）。
                let e = &mut self.entries[idx];
                e.disabled = true;
                e.reason = "12153 session dead".to_string();
                self.dirty = true;
            }
            ErrKind::Server => {
                // 5xx 上游故障：喂熔断计数（不设账号级冷却，由换号重试兜底）。
                self.note_server_error(uid);
            }
            // 其余（ContextTooLong 请求级 400 / Client / None / Transport）：只换号不罚。
            _ => {}
        }
    }

    /// 即时冷却（账号级）。冷却入口同时是熔断器的失败信号：喂入 fails，
    /// 达到阈值按指数退避熔断；`escalate=false` 时只累计 fails、不放大退避指数
    /// （429 是瞬时过载信号，对照 Go pool.go:1237 的 issue #5 修复）。
    fn set_cooldown(&mut self, idx: usize, kind: CoolKind, until_ms: i64, reason: &str, escalate: bool) {
        let e = &mut self.entries[idx];
        e.until_ms = until_ms;
        e.cool_kind = Some(kind);
        e.reason = reason.to_string();
        self.record_breaker_failure(idx, escalate);
        self.dirty = true;
    }

    /// 累计一次熔断失败；达到阈值则按指数退避熔断。
    /// 熔断与冷却（until）解耦：冷却按错误类别给固定时长，
    /// 熔断则对「反复失败」逐次加长封禁。对照 pool.go:1254
    /// `recordBreakerFailureLockedKind`（`fails++` 在本函数内完成）。
    fn record_breaker_failure(&mut self, idx: usize, escalate: bool) {
        self.entries[idx].fails += 1;
        let fails = self.entries[idx].fails;
        let retry = self.entries[idx].retry_count;
        if fails < self.breaker_threshold {
            return;
        }
        let mut d = self.breaker_base_ms;
        if escalate {
            for _ in 0..retry {
                d = d.saturating_mul(2);
                if d >= self.breaker_max_ms {
                    d = self.breaker_max_ms;
                    break;
                }
            }
        }
        let e = &mut self.entries[idx];
        e.fails = 0;
        if escalate {
            e.retry_count += 1;
        }
        e.breaker_until_ms = now_ms() + d.clamp(1, self.breaker_max_ms.max(1));
    }

    /// 把某个「账号+模型」冷却到指定时刻。**不喂熔断器**：模型级限流是配额信号
    /// 而非账号故障，反复命中说明该模型额度确实用尽（换模型仍可用）。
    /// 对照 Go pool.go:1148 `CooldownModel`。
    fn cooldown_model(&mut self, idx: usize, model: &str, until_ms: i64, reason: &str, reset_parsed: bool) {
        if model.is_empty() || until_ms <= 0 {
            return;
        }
        self.entries[idx].model_cools.insert(
            model.to_string(),
            ModelCool {
                until_ms,
                reason: reason.to_string(),
                reset_parsed,
            },
        );
        self.dirty = true;
    }

    /// 巡检回填余额与到期日（驱动分层选号）；`credit_expires_at` 为 0 表示未知。
    pub fn set_credits(&mut self, uid: &str, credits: i64, credit_expires_at: i64) {
        let Some(&idx) = self.by_uid.get(uid) else {
            return;
        };
        let e = &mut self.entries[idx];
        e.acc.credits = credits;
        if credit_expires_at > 0 {
            e.acc.credit_expires_at = credit_expires_at;
        }
        self.dirty = true;
    }

    // -----------------------------------------------------------------------
    // 选号
    // -----------------------------------------------------------------------

    /// 当前是否有账号可受理（healthy 且未占满在途）。
    pub fn servable_now(&self) -> bool {
        let now = now_ms();
        self.entries
            .iter()
            .any(|e| e.healthy(now) && !self.in_flight_full(e))
    }

    /// 按模式选号（balance/rotate/pinned），跳过 `tried` 中的 UID。
    pub fn pick_account(&mut self, model: &str, tried: &[String]) -> Option<PoolAccount> {
        match self.mode.as_str() {
            "pinned" => {
                // 指定模式： pinned 号可用就用它；不可用（冷却/熔断/模型限流/在途/tried）
                // 回落普通选号，避免指定号故障时整个池不可用。
                let pinned = self.pinned_uid.clone();
                if !pinned.is_empty() && !tried.iter().any(|t| t == &pinned) {
                    if let Some(acc) = self.pick_by_uid(&pinned, model) {
                        return Some(acc);
                    }
                }
                self.pick(model, tried)
            }
            "rotate" => {
                // 烧号模式：当前号仍可用就继续用，不可用才换下一个（换的仍是选号算法挑的）。
                let current = self.rotate_uid.clone();
                if !current.is_empty() && !tried.iter().any(|t| t == &current) {
                    if let Some(acc) = self.pick_by_uid(&current, model) {
                        return Some(acc);
                    }
                }
                let acc = self.pick(model, tried)?;
                self.rotate_uid = acc.uid.clone();
                Some(acc)
            }
            _ => self.pick(model, tried),
        }
    }

    /// 直取指定 uid（若 healthy、该模型未被限流且未占满在途），记录使用时间防撞号。
    fn pick_by_uid(&mut self, uid: &str, model: &str) -> Option<PoolAccount> {
        let idx = *self.by_uid.get(uid)?;
        let now = now_ms();
        let e = &self.entries[idx];
        if !e.healthy(now) || self.in_flight_full(e) || e.model_cooled(model, now) {
            return None;
        }
        self.mark_used(idx);
        Some(self.entries[idx].acc.clone())
    }

    /// 挑号，跳过 `tried` 中的 UID。到期分层→Top5 加权→防惊群→LRU 兜底。
    /// 对照 pool.rs:840 `pick_excluding` + pool.go:725 `pick`（model 过滤照 Go 版）。
    fn pick(&mut self, model: &str, tried: &[String]) -> Option<PoolAccount> {
        let now = now_ms();

        // 顺手清理已过期的模型冷却，避免 map 随模型种类无限增长（Go pick 同款）。
        let mut pruned = false;
        for e in self.entries.iter_mut() {
            if e.prune_model_cools(now) {
                pruned = true;
            }
        }
        if pruned {
            self.dirty = true;
        }

        // 1) 收集 healthy 且未在途占满、不在 tried、该模型未被模型级冷却的候选
        // （按 UID 排序保证确定性）
        let mut cands: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                !tried.iter().any(|t| t == &e.acc.uid)
                    && e.healthy(now)
                    && !self.in_flight_full(e)
                    && !e.model_cooled(model, now)
            })
            .collect();
        cands.sort_by(|&a, &b| self.entries[a].acc.uid.cmp(&self.entries[b].acc.uid));

        if cands.is_empty() {
            // 全冷却兜底（soft/breaker 号可参与，hard 冷却排除）。
            return self.pick_earliest_expiry(tried, model, now);
        }

        // 2) 到期分层：只保留最早到期的一档
        let (cands, tiered) = self.earliest_expiry_tier(cands);

        // 3) top5 短名单：按权重降序截断（平局按 UID 升序，保证确定性）
        let max_credits = cands
            .iter()
            .map(|&i| self.entries[i].acc.credits)
            .max()
            .unwrap_or(0);
        let mut weighted: Vec<(usize, f64)> = cands
            .iter()
            .map(|&i| (i, self.weight_of(i, max_credits, now, tiered)))
            .collect();
        weighted.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.entries[a.0].acc.uid.cmp(&self.entries[b.0].acc.uid))
        });
        let mut cands: Vec<usize> = weighted.into_iter().map(|(i, _)| i).collect();
        if cands.len() > SHORTLIST_SIZE {
            cands.truncate(SHORTLIST_SIZE);
        }

        // 4) 并发防雪崩：跳过 last_used 距今 < min_pick_gap 的账号
        let eligible: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| {
                let lu = self.entries[i].last_used_ms;
                lu == 0 || now.saturating_sub(lu) >= self.min_pick_gap_ms
            })
            .collect();

        let chosen = if eligible.is_empty() {
            // 候选全部刚被用过：LRU 兜底。
            // 必须用「时刻 + 单调序号」双键比较：时钟粒度会让并发写入的 last_used
            // 完全相同，单靠时刻比较会因平局恒 false 而永远停在第一个候选，
            // 退化成惊群（Go 侧已修复，此处沿用）。
            let mut best = cands[0];
            for &c in &cands[1..] {
                if self.older_last_used(c, best) {
                    best = c;
                }
            }
            best
        } else {
            self.pick_weighted(&eligible, tiered)
        };

        self.mark_used(chosen);
        Some(self.entries[chosen].acc.clone())
    }

    /// 全冷却兜底：在非禁用的软冷却/熔断账号中选截止最早的一个。
    /// 分级：disabled 永不参与；硬冷却（余额耗尽）账号若仍处于有效冷却期也排除
    /// （调了必 402）；软冷却与熔断号允许参与（可能已恢复）。被 tried / 在途占满
    /// / 该模型被限流的账号同样跳过。对照 pool.rs:934 `pick_earliest_expiry`
    /// + pool.go:886 `pickEarliestExpiryLocked`（model 过滤照 Go 版）。
    fn pick_earliest_expiry(&mut self, tried: &[String], model: &str, now: i64) -> Option<PoolAccount> {
        let mut best: Option<usize> = None;
        let mut best_exp = 0i64;
        for i in 0..self.entries.len() {
            let e = &self.entries[i];
            if tried.iter().any(|t| t == &e.acc.uid) {
                continue;
            }
            if e.disabled {
                continue;
            }
            // 余额耗尽号处于有效 hard 冷却期 → 不参与兜底
            if e.cool_kind == Some(CoolKind::Hard) && e.until_ms > now {
                continue;
            }
            // 该模型正被限流：换了也是同一个 6004
            if e.model_cooled(model, now) {
                continue;
            }
            if self.in_flight_full(e) {
                continue;
            }
            let exp = e.expiry(now);
            if exp == 0 {
                continue; // 不在任何冷却/熔断期（理论上进不到这里）
            }
            if best.is_none() || exp < best_exp {
                best = Some(i);
                best_exp = exp;
            }
        }
        let idx = best?;
        self.mark_used(idx);
        Some(self.entries[idx].acc.clone())
    }

    /// 到期分层：返回最早到期那一档，以及是否真的分了档。
    /// 到期日未知的账号视为最晚（排最后）：只有全部候选都没有到期信息时，
    /// 它们才会成为唯一的一档。对照 pool.rs:971 `earliest_expiry_tier`。
    fn earliest_expiry_tier(&self, cands: Vec<usize>) -> (Vec<usize>, bool) {
        if cands.len() <= 1 {
            return (cands, false);
        }
        // 找出最小的非空 day key（字典序比较即日期先后，因格式固定为 YYYY-MM-DD）
        let mut best: Option<String> = None;
        for &i in &cands {
            let k = self.entries[i].expiry_day_key();
            if k.is_empty() {
                continue; // 到期未知视为最晚，不参与选档
            }
            if best.as_ref().map(|b| k < *b).unwrap_or(true) {
                best = Some(k);
            }
        }
        let Some(best) = best else {
            // 全员无到期信息 → 不分层
            return (cands, false);
        };
        let out: Vec<usize> = cands
            .into_iter()
            .filter(|&i| self.entries[i].expiry_day_key() == best)
            .collect();
        (out, true)
    }

    /// 权重 = credits 比例 ×10 + 闲置补偿 + 成功率 ×3；`tiered` 时去掉 credits 项。
    /// 对照 pool.rs:998 `weight_of_mode` / pool.go `weightOf` + `tierWeightOf`。
    fn weight_of(&self, idx: usize, max_credits: i64, now: i64, tiered: bool) -> f64 {
        let e = &self.entries[idx];
        let mut w = 1.0;

        // 1. credits 比例 ×10（tiered 时跳过：同档代表同样紧迫，按积分分配会让
        //    高积分账号长期吃掉大部分流量）
        if !tiered && max_credits > 0 {
            w += e.acc.credits as f64 / max_credits as f64 * 10.0;
        }

        // 2. 闲置补偿
        if e.last_used_ms == 0 {
            w += IDLE_WEIGHT_MAX; // 从未使用 → 满分
        } else {
            let hours = now.saturating_sub(e.last_used_ms) as f64 / 3_600_000.0;
            let mut idle_w = hours * IDLE_WEIGHT_PER_HOUR;
            if idle_w > IDLE_WEIGHT_MAX {
                idle_w = IDLE_WEIGHT_MAX;
            }
            if idle_w < 0.0 {
                idle_w = 0.0; // 时钟回拨钳 0
            }
            w += idle_w;
        }

        // 3. 成功率 ×3
        let total_req = e.success_count + e.err_total;
        if total_req > 0 {
            w += e.success_count as f64 / total_req as f64 * 3.0;
        } else {
            w += 1.5; // 无请求记录 → 中性偏信任
        }
        w
    }

    /// 加权随机抽签（定点 ×1e6 累加，与 Go 侧同口径）。
    fn pick_weighted(&self, cands: &[usize], tiered: bool) -> usize {
        let now = now_ms();
        let max_credits = cands
            .iter()
            .map(|&i| self.entries[i].acc.credits)
            .max()
            .unwrap_or(0);

        const SCALE: f64 = 1_000_000.0;
        let weights: Vec<i64> = cands
            .iter()
            .map(|&i| (self.weight_of(i, max_credits, now, tiered) * SCALE) as i64)
            .collect();
        let total: i64 = weights.iter().sum();

        if total <= 0 {
            let n = cands.len() as i64;
            let r = self.rand_n(n).max(0) as usize;
            return cands[r.min(cands.len() - 1)];
        }
        let r = self.rand_n(total);
        let mut acc: i64 = 0;
        for (k, &i) in cands.iter().enumerate() {
            acc += weights[k];
            if r < acc {
                return i;
            }
        }
        cands[cands.len() - 1]
    }

    /// 内部随机源：优先注入源，否则用线程本地 xorshift64*（无需密码学强度）。
    fn rand_n(&self, n: i64) -> i64 {
        if let Some(f) = &self.rand {
            return f(n);
        }
        thread_local! {
            static STATE: std::cell::Cell<u64> = std::cell::Cell::new(seed_once());
        }
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            s.set(x);
            let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
            if n <= 0 {
                return 0;
            }
            ((v >> 33) as i64) % n
        })
    }

    /// 记录一次选中使用（时刻 + 单调序号）。
    fn mark_used(&mut self, idx: usize) {
        self.pick_seq += 1;
        let e = &mut self.entries[idx];
        e.last_used_seq = self.pick_seq;
        e.last_used_ms = now_ms();
    }

    /// 报告 a 是否比 b "更久未被选中"（时刻优先，平局用单调序号）。
    /// 对照 pool.rs:913 `older_last_used`。
    fn older_last_used(&self, a: usize, b: usize) -> bool {
        let ea = &self.entries[a];
        let eb = &self.entries[b];
        if ea.last_used_ms != eb.last_used_ms {
            return ea.last_used_ms < eb.last_used_ms;
        }
        ea.last_used_seq < eb.last_used_seq
    }

    // -----------------------------------------------------------------------
    // 状态透出（脱敏）
    // -----------------------------------------------------------------------

    /// 池状态 JSON（total/healthy/cooling/model_cooling/breaker/accounts[]，脱敏）。
    /// token 不出现；uid 仅保留前 8 位。
    pub fn status_value(&self) -> Value {
        let now = now_ms();
        let mut total = 0i64;
        let mut healthy = 0i64;
        let mut cooling = 0i64;
        let mut breaker = 0i64;
        let mut model_cooling = 0i64;
        let mut entries: Vec<&Entry> = self.entries.iter().collect();
        entries.sort_by(|a, b| a.acc.uid.cmp(&b.acc.uid));
        let mut accounts = Vec::with_capacity(entries.len());
        for e in entries {
            total += 1;
            let is_cooling = e.until_ms > now;
            let is_breaker = e.breaker_until_ms > now;
            let mcs = e.model_cooling_list(now);
            if is_cooling {
                cooling += 1;
            }
            if is_breaker {
                breaker += 1;
            }
            if !mcs.is_empty() {
                model_cooling += 1;
            }
            if !e.disabled && !is_cooling && !is_breaker {
                healthy += 1;
            }
            accounts.push(json!({
                "uid": mask_uid(&e.acc.uid),
                "nickname": e.acc.nickname,
                "region": e.acc.region,
                "credits": e.acc.credits,
                "credit_expires_at": e.acc.credit_expires_at,
                "cooldown_kind": e.cool_kind.map(|k| k.as_str()).unwrap_or(""),
                "until": e.until_ms,
                "breaker_until": e.breaker_until_ms,
                "in_flight": e.in_flight,
                "model_cools": mcs,
                "last_used": e.last_used_ms,
                "disabled": e.disabled,
                "reason": e.reason,
                "success_count": e.success_count,
                "err_total": e.err_total,
            }));
        }
        json!({
            "total": total,
            "healthy": healthy,
            "cooling": cooling,
            "model_cooling": model_cooling,
            "breaker": breaker,
            "accounts": accounts,
        })
    }
}

/// uid 脱敏：仅保留前 8 位（状态透出不泄露完整身份）。
fn mask_uid(uid: &str) -> String {
    let chars: Vec<char> = uid.chars().collect();
    if chars.len() <= 8 {
        return uid.to_string();
    }
    let prefix: String = chars[..8].iter().collect();
    format!("{prefix}…")
}

/// 计算 `now_ms` 之后最近的一个本地 04:00 的绝对时刻（ms epoch）。
/// 对照 pool.rs:1104 `next_day_4am_duration` / pool.go:1290 `nextDay4AM`：
/// now 在当天 04:00 之前返回当天 04:00（此时签到尚未执行，等当天签到即可）；
/// 04:00 整及之后返回次日 04:00（time.Date 的日溢出自动进位覆盖跨月/跨年）。
fn next_local_4am_ms(now: i64) -> i64 {
    let now = Local
        .timestamp_millis_opt(now)
        .single()
        .unwrap_or_else(Local::now);
    let today4am = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 4, 0, 0)
        .single();
    let target = match today4am {
        Some(t) if now.hour() < 4 => t,
        Some(t) => t + chrono::Duration::days(1),
        None => now + chrono::Duration::hours(24),
    };
    target.timestamp_millis()
}

/// 模型冷却的展示文案（含模型名与重置时间）。对照 Go handler.go:629 `modelRateReason`。
fn model_rate_reason(model: &str, until_ms: i64, parsed: bool) -> String {
    let name = if model.is_empty() { "（未知模型）" } else { model };
    let ts = Local
        .timestamp_millis_opt(until_ms)
        .single()
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_default();
    if parsed {
        format!("{name} 已达频率上限，{ts} 重置")
    } else {
        format!("{name} 已达频率上限（未取到重置时间，按软冷却 {ts} 处理）")
    }
}

/// 进程级随机种子（一次性）。
fn seed_once() -> u64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    base ^ (n.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

// ---------------------------------------------------------------------------
// 全局池（进程级单例）+ 契约 §13 钉定入口
// ---------------------------------------------------------------------------

/// 池状态文件路径。
pub fn state_file() -> PathBuf {
    store_dir().join("workbuddy_gateway_state.json")
}

static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();

fn pool() -> MutexGuard<'static, Pool> {
    POOL.get_or_init(|| {
        let cfg = load_config();
        Mutex::new(Pool::new(state_file(), &cfg))
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// 统一入口：同步凭证 → 执行操作 → 按需落盘。
fn with_pool<T>(f: impl FnOnce(&mut Pool) -> T) -> T {
    let mut guard = pool();
    let p = &mut *guard;
    p.sync_accounts();
    let out = f(p);
    p.flush();
    out
}

/// 选号：到期分层→档内加权 Top5→防惊群→LRU 兜底；tried 跳过；无可用返回 None。
pub fn pick_account(model: &str, tried: &[String]) -> Option<PoolAccount> {
    with_pool(|p| p.pick_account(model, tried))
}

/// 记录一次成功（清连续失败/熔断/软冷却退避）。
pub fn note_success(uid: &str) {
    with_pool(|p| p.note_success(uid));
}

/// 按 ErrKind 分流冷却（ModelRate 走 uid+model，重置时间从 body 解析）。
pub fn note_error(uid: &str, model: &str, kind: ErrKind, body: &str) {
    with_pool(|p| p.note_error(uid, model, kind, body));
}

/// 池状态 JSON（total/healthy/cooling/model_cooling/breaker/accounts[]，脱敏）。
pub fn pool_status() -> Value {
    with_pool(|p| p.status_value())
}

/// 占用在途租约（请求级）；已满返回 false。
pub fn acquire(uid: &str) -> bool {
    with_pool(|p| p.acquire(uid))
}

/// 释放在途租约（请求级）。
pub fn release(uid: &str) {
    with_pool(|p| p.release(uid));
}

/// 巡检回填余额与到期日（驱动分层选号）。
pub fn set_credits(uid: &str, credits: i64, credit_expires_at: i64) {
    with_pool(|p| p.set_credits(uid, credits, credit_expires_at));
}

// ---------------------------------------------------------------------------
// 会话粘性（key = conversation_id 或其派生哈希）
// ---------------------------------------------------------------------------

/// 单条会话绑定。
#[derive(Debug, Clone)]
struct StickyBinding {
    uid: String,
    last_active_ms: i64,
}

static STICKY: OnceLock<Mutex<HashMap<String, StickyBinding>>> = OnceLock::new();
/// 粘性 TTL 活值（配置热更新时随 apply_config 推进），避免每次读盘。
static STICKY_TTL_MS: AtomicI64 = AtomicI64::new(DEFAULT_STICKY_TTL_MS);

fn sticky_map() -> MutexGuard<'static, HashMap<String, StickyBinding>> {
    STICKY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 粘性纯计算核心：命中且未过期 → 滚动续期并返回 uid；过期/缺失 → 清除并返回 None。
/// TTL 滚动续期（契约 P1 §3：TTL 60min 滚动）；`ttl_ms <= 0` 回落默认值。
fn sticky_resolve(
    map: &mut HashMap<String, StickyBinding>,
    key: &str,
    now: i64,
    ttl_ms: i64,
) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let ttl = if ttl_ms > 0 { ttl_ms } else { DEFAULT_STICKY_TTL_MS };
    // 先取 uid 再改写表：避免 get 的不可变借用与 insert 的可变借用重叠。
    let hit = match map.get(key) {
        Some(b) if now.saturating_sub(b.last_active_ms) <= ttl => Some(b.uid.clone()),
        _ => None,
    };
    match hit {
        Some(uid) => {
            map.insert(
                key.to_string(),
                StickyBinding {
                    uid: uid.clone(),
                    last_active_ms: now,
                },
            );
            Some(uid)
        }
        None => {
            map.remove(key);
            None
        }
    }
}

/// 会话粘性查询：命中返回绑定 uid 并滚动续期；未命中/已过期返回 None。
pub fn sticky_get(key: &str) -> Option<String> {
    let ttl = STICKY_TTL_MS.load(Ordering::Relaxed);
    sticky_resolve(&mut sticky_map(), key, now_ms(), ttl)
}

/// 会话粘性绑定（幂等覆盖旧值）。空 key / 空 uid 直接返回。
pub fn sticky_set(key: &str, uid: &str) {
    if key.is_empty() || uid.is_empty() {
        return;
    }
    sticky_map().insert(
        key.to_string(),
        StickyBinding {
            uid: uid.to_string(),
            last_active_ms: now_ms(),
        },
    );
}

/// 解除会话绑定（请求失败时调用，让该会话下次重新分配）。
pub fn sticky_clear(key: &str) {
    sticky_map().remove(key);
}

// ---------------------------------------------------------------------------
// 测试（无网络：选号矩阵 / 冷却 / 熔断 / 模型冷却 / 粘性 / round-trip / 凭证桥）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn mk_acc(uid: &str) -> PoolAccount {
        PoolAccount {
            uid: uid.to_string(),
            nickname: format!("nick-{uid}"),
            region: "cn".to_string(),
            access_token: format!("at-{uid}"),
            refresh_token: format!("rt-{uid}"),
            expires_at: 4_000_000_000_000,
            refresh_expires_at: 4_100_000_000_000,
            credits: 0,
            credit_expires_at: 0,
        }
    }

    /// 空状态文件路径的池（不落盘），配置走默认。
    fn mk_pool() -> Pool {
        Pool::new(PathBuf::new(), &GatewayConfig::default())
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-gw-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -----------------------------------------------------------------------
    // 凭证桥
    // -----------------------------------------------------------------------

    #[test]
    fn bridge_maps_fields_and_filters_platform() {
        let text = json!([
            {"id":"id-1","nickname":"猫猫","domain":"www.workbuddy.cn","access_token":"at1","refresh_token":"rt1",
             "expiresAt":1792814920000i64,"refreshExpiresAt":1795406920000i64,"auth_raw":{"uid":"uid-cn"}},
            {"id":"id-2","nickname":"cat","domain":"www.workbuddy.ai","access_token":"at2","refresh_token":"rt2",
             "expiresAt":"1792814920","auth_raw":{"uid":"uid-intl"}},
            {"id":"id-3","platform":"autoclaw","domain":"x","access_token":"at3","auth_raw":{"uid":"uid-other"}}
        ])
        .to_string();
        let accs = pool_accounts_from_json(&text).unwrap();
        assert_eq!(accs.len(), 2, "非 WorkBuddy 平台账号不进池");
        assert_eq!(accs[0].uid, "uid-cn", "uid←auth_raw.uid");
        assert_eq!(accs[0].region, "cn", ".cn → cn");
        assert_eq!(accs[0].nickname, "猫猫");
        assert_eq!(accs[0].access_token, "at1");
        assert_eq!(accs[0].refresh_token, "rt1");
        assert_eq!(accs[0].expires_at, 1792814920000i64);
        assert_eq!(accs[0].refresh_expires_at, 1795406920000i64);
        assert_eq!(accs[1].uid, "uid-intl");
        assert_eq!(accs[1].region, "intl", ".ai → intl");
        assert_eq!(accs[1].expires_at, 1792814920000i64, "秒级时间戳应归一为毫秒");
        assert_eq!(accs[0].credits, 0, "桥接时余额未知");
        assert_eq!(accs[0].credit_expires_at, 0);
    }

    #[test]
    fn bridge_skips_account_without_token() {
        let text = json!([
            {"id":"id-1","domain":"www.workbuddy.cn","access_token":"","auth_raw":{"uid":"uid-empty"}},
            {"id":"id-2","domain":"www.workbuddy.cn","auth_raw":{"uid":"uid-missing"}}
        ])
        .to_string();
        assert!(pool_accounts_from_json(&text).unwrap().is_empty());
    }

    #[test]
    fn bridge_rejects_unreadable_json() {
        assert!(pool_accounts_from_json("not json").is_none());
        assert!(pool_accounts_from_json("{}").is_none(), "非数组视为不可读");
    }

    #[test]
    fn bridge_reads_temp_accounts_file() {
        let dir = tmp_dir("acc");
        let fp = dir.join("accounts.json");
        std::fs::write(
            &fp,
            json!([
                {"id":"id-1","nickname":"猫猫","domain":"www.workbuddy.cn","access_token":"at1","refresh_token":"rt1",
                 "expiresAt":1792814920000i64,"refreshExpiresAt":1795406920000i64,"auth_raw":{"uid":"uid-cn"}},
                {"id":"id-2","domain":"www.workbuddy.ai","access_token":"at2","auth_raw":{"uid":"uid-intl"}}
            ])
            .to_string(),
        )
        .unwrap();
        let accs = load_pool_accounts_at(&fp).unwrap();
        assert_eq!(accs.len(), 2);
        assert_eq!(accs[0].uid, "uid-cn");
        assert_eq!(accs[1].region, "intl");
        assert_eq!(accs[1].nickname, "");
        // 缺文件 → None（同步层据此不清池）
        assert!(load_pool_accounts_at(&dir.join("nope.json")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writeback_prefers_newer_expiry_and_keeps_refresh() {
        // 用毫秒 epoch 量级的时间戳（norm_ts 会把 < 10^10 的值当秒缩放）。
        let mut acc = json!({
            "id":"a","access_token":"old","refresh_token":"rt-old",
            "expiresAt":1_700_000_000_000i64,"refreshExpiresAt":1_700_000_000_000i64,"auth_raw":{"uid":"u1"}
        });
        // 更旧的过期时间 → 不倒退
        assert!(!apply_token_writeback(&mut acc, "new", "", 1_600_000_000_000i64, 0));
        assert_eq!(acc["access_token"], "old");
        // 更新 access_token + expiresAt；空 refresh 不覆盖
        assert!(apply_token_writeback(&mut acc, "new", "", 1_800_000_000_000i64, 0));
        assert_eq!(acc["access_token"], "new");
        assert_eq!(acc["expiresAt"], 1_800_000_000_000i64);
        assert_eq!(acc["refresh_token"], "rt-old", "空 refresh 不覆盖已有值");
        // 带 refresh + 更新的 refreshExpiresAt
        assert!(apply_token_writeback(&mut acc, "new2", "rt-new", 1_900_000_000_000i64, 1_950_000_000_000i64));
        assert_eq!(acc["refresh_token"], "rt-new");
        assert_eq!(acc["refreshExpiresAt"], 1_950_000_000_000i64);
        // refreshExpiresAt 更旧不写
        assert!(apply_token_writeback(&mut acc, "new3", "rt-new3", 2_000_000_000_000i64, 1_800_000_000_000i64));
        assert_eq!(acc["refreshExpiresAt"], 1_950_000_000_000i64);
        // 空 token / 非正 expires_at 直接拒绝
        assert!(!apply_token_writeback(&mut acc, "", "rt", 2_100_000_000_000i64, 0));
        assert!(!apply_token_writeback(&mut acc, "new4", "rt", 0, 0));
    }

    #[test]
    fn writeback_through_temp_dir() {
        let dir = tmp_dir("wb");
        let fp = dir.join("accounts.json");
        let original = json!([
            {"id":"id-1","access_token":"at-old","refresh_token":"rt-1","expiresAt":1_700_000_000_000i64,"auth_raw":{"uid":"u1"}},
            {"id":"id-2","access_token":"at-2","expiresAt":1_900_000_000_000i64,"auth_raw":{"uid":"u2"}}
        ])
        .to_string();
        std::fs::write(&fp, &original).unwrap();

        // u1 的过期时间更旧 → 不写，文件内容不变
        writeback_tokens_at(&fp, "u1", "at-new", 1_600_000_000_000i64).unwrap();
        assert_eq!(std::fs::read_to_string(&fp).unwrap(), original);

        // u1 刷新成功 → 回写；u2 不动
        let new_exp = now_ms() + 3_600_000;
        writeback_tokens_at(&fp, "u1", "at-new", new_exp).unwrap();
        let arr: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&fp).unwrap()).unwrap();
        assert_eq!(arr[0]["access_token"], "at-new");
        assert_eq!(arr[0]["expiresAt"], new_exp);
        assert_eq!(arr[0]["refresh_token"], "rt-1", "公共入口不带 refresh，不覆盖");
        assert_eq!(arr[1]["access_token"], "at-2", "其他账号不受影响");

        // 未知 uid → 无改动
        writeback_tokens_at(&fp, "nope", "at-x", new_exp + 1000).unwrap();
        let arr2: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&fp).unwrap()).unwrap();
        assert_eq!(arr2[0]["access_token"], "at-new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 选号矩阵
    // -----------------------------------------------------------------------

    #[test]
    fn pick_prefers_earliest_expiry_tier() {
        // 分层优先于 credits：晚到期档积分再多也不选。
        let mut p = mk_pool();
        let now = now_ms();
        let mut early = mk_acc("early");
        early.credits = 1;
        early.credit_expires_at = now - 3 * 86_400_000;
        let mut late = mk_acc("late");
        late.credits = 10000;
        late.credit_expires_at = now + 10 * 86_400_000;
        p.upsert(early);
        p.upsert(late);

        let mut counts: HashMap<String, i64> = HashMap::new();
        for _ in 0..50 {
            if let Some(a) = p.pick("m", &[]) {
                *counts.entry(a.uid).or_insert(0) += 1;
            }
        }
        assert_eq!(counts.get("early").copied().unwrap_or(0), 50, "应固定选最早到期档: {counts:?}");
        assert!(!counts.contains_key("late"), "较晚档不应被选中: {counts:?}");
    }

    #[test]
    fn tiered_weight_drops_credits_term() {
        // 同档内（tiered）权重去掉 credits 项 → 三个号均摊。
        let mut p = mk_pool();
        p.set_min_pick_gap_ms(0); // 关闭防撞号，走纯加权分布
        let exp = now_ms() + 5 * 86_400_000;
        for (uid, c) in [("c100", 100i64), ("c50", 50), ("c10", 10)] {
            let mut a = mk_acc(uid);
            a.credits = c;
            a.credit_expires_at = exp;
            p.upsert(a);
        }
        let calls = AtomicI64::new(0);
        p.set_random_source(Some(Box::new(move |n| {
            if n <= 0 {
                return 0;
            }
            calls.fetch_add(1, Ordering::Relaxed) % n
        })));
        let mut counts: HashMap<String, i64> = HashMap::new();
        for _ in 0..90 {
            if let Some(a) = p.pick("m", &[]) {
                *counts.entry(a.uid).or_insert(0) += 1;
            }
        }
        assert_eq!(counts.len(), 3, "同档内应均摊（不含 credits 项）: {counts:?}");
    }

    #[test]
    fn untiered_weight_uses_credits_term() {
        // 无到期信息（不分层）→ 三因子权重含 credits 项，高积分号优先。
        let mut p = mk_pool();
        p.set_min_pick_gap_ms(0);
        for (uid, c) in [("c100", 100i64), ("c50", 50), ("c10", 10)] {
            let mut a = mk_acc(uid);
            a.credits = c;
            p.upsert(a);
        }
        // 恒抽签 0 → 取权重最高者
        p.set_random_source(Some(Box::new(|_| 0)));
        for _ in 0..20 {
            assert_eq!(p.pick("m", &[]).unwrap().uid, "c100", "credits 项应起作用");
        }
    }

    #[test]
    fn min_pick_gap_forces_lru_fallback() {
        // 防惊群：候选全部刚被用过 → LRU 双键兜底，连续两次不撞同一号。
        let mut p = mk_pool();
        p.upsert(mk_acc("a"));
        p.upsert(mk_acc("b"));
        p.set_random_source(Some(Box::new(|_| 0)));
        let mut prev = String::new();
        for i in 0..10 {
            let uid = p.pick("m", &[]).unwrap().uid;
            if i > 0 {
                assert_ne!(uid, prev, "惊群：连续两次选中同一账号");
            }
            prev = uid;
        }
        assert_eq!(prev, "b", "双键 LRU 应严格交替（a,b,a,b,…）");
    }

    #[test]
    fn anti_thundering_herd_diverges() {
        // 复刻参考实现 TestPickAntiThunderingHerd：串行高频 Pick 不应落在少数字号上。
        let mut p = mk_pool();
        for i in 0..10 {
            p.upsert(mk_acc(&format!("c{i:02}")));
        }
        let calls = AtomicI64::new(0);
        p.set_random_source(Some(Box::new(move |n| {
            if n <= 0 {
                return 0;
            }
            calls.fetch_add(1, Ordering::Relaxed) % n
        })));
        let mut counts: HashMap<String, i64> = HashMap::new();
        const N: i64 = 100;
        for _ in 0..N {
            if let Some(a) = p.pick("m", &[]) {
                *counts.entry(a.uid).or_insert(0) += 1;
            }
        }
        assert!(counts.len() >= 2, "应覆盖多个账号: {counts:?}");
        for (uid, n) in &counts {
            assert!(*n <= N / 2, "惊群：{uid} 被选中 {n}/{N} 次");
        }
    }

    #[test]
    fn fallback_excludes_hard_cooling() {
        // 全冷却兜底：软冷却号可参与，硬冷却（余额耗尽）排除。
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        p.upsert(mk_acc("u2"));
        p.note_error("u1", "", ErrKind::SoftRate, "");
        p.note_error("u2", "", ErrKind::HardCredit, "");
        assert_eq!(p.pick("m", &[]).unwrap().uid, "u1", "兜底应选软冷却号");

        // 两个都硬冷却 → 无可用
        let mut p2 = mk_pool();
        p2.upsert(mk_acc("u1"));
        p2.upsert(mk_acc("u2"));
        p2.note_error("u1", "", ErrKind::HardCredit, "");
        p2.note_error("u2", "", ErrKind::HardCredit, "");
        assert!(p2.pick("m", &[]).is_none(), "硬冷却号不参与兜底");

        // 兜底也尊重 tried
        let mut p3 = mk_pool();
        p3.upsert(mk_acc("u1"));
        p3.note_error("u1", "", ErrKind::SoftRate, "");
        assert!(p3.pick("m", &["u1".to_string()]).is_none(), "tried 跳过兜底");
    }

    #[test]
    fn pick_modes_pinned_rotate_balance() {
        // pinned：指定号优先；指定号不可用 → 回落普通选号。
        let cfg = GatewayConfig {
            mode: "pinned".to_string(),
            pinned_uid: "b".to_string(),
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("a"));
        p.upsert(mk_acc("b"));
        assert_eq!(p.pick_account("m", &[]).unwrap().uid, "b");
        p.note_error("b", "", ErrKind::SoftRate, "");
        assert_eq!(p.pick_account("m", &[]).unwrap().uid, "a", "指定号冷却后回落");

        // rotate：烧号——同一号持续用到不可用才换。
        let cfg = GatewayConfig {
            mode: "rotate".to_string(),
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("a"));
        p.upsert(mk_acc("b"));
        p.set_random_source(Some(Box::new(|_| 0)));
        let first = p.pick_account("m", &[]).unwrap().uid;
        for _ in 0..5 {
            assert_eq!(p.pick_account("m", &[]).unwrap().uid, first, "轮转模式应持续用同一号");
        }
        p.note_error(&first, "", ErrKind::SoftRate, "");
        let second = p.pick_account("m", &[]).unwrap().uid;
        assert_ne!(first, second, "当前号冷却后应换下一个");
        for _ in 0..3 {
            assert_eq!(p.pick_account("m", &[]).unwrap().uid, second);
        }

        // balance：tried 跳过。
        let mut p = mk_pool();
        p.upsert(mk_acc("a"));
        p.upsert(mk_acc("b"));
        assert_eq!(p.pick_account("m", &["a".to_string()]).unwrap().uid, "b");
    }

    #[test]
    fn in_flight_lease_limits_pick() {
        let cfg = GatewayConfig {
            max_in_flight: 2,
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("u1"));
        assert!(p.acquire("u1"));
        assert!(p.acquire("u1"));
        assert!(!p.acquire("u1"), "超过上限应拒绝");
        assert!(p.pick("m", &[]).is_none(), "占满在途不可选（兜底同样跳过）");
        p.release("u1");
        assert!(p.pick("m", &[]).is_some());
        p.release("u1");
        p.release("u1"); // 多 release 不应下溢
        assert_eq!(p.entries[0].in_flight, 0);
        assert!(!p.acquire("nope"), "未知 uid 租约失败");
    }

    // -----------------------------------------------------------------------
    // 冷却恢复时序 / 错误分级
    // -----------------------------------------------------------------------

    #[test]
    fn cooldown_recovers_after_expiry() {
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        let idx = 0;
        p.set_cooldown(idx, CoolKind::Soft, now_ms() + 50, "test", false);
        assert!(!p.servable_now(), "冷却期内 servable_now 应为假");
        // 契约 §4/§2-7：软冷却号参与全冷却兜底（可能已恢复，失败成本仅一轮换），
        // 故 pick 仍返回它；调用方用 tried 排除实现换号（接线层 ≤3 次）。
        assert!(p.pick("m", &[]).is_some(), "软冷却号按契约参与兜底");
        std::thread::sleep(Duration::from_millis(80));
        assert!(p.servable_now(), "冷却到期后应恢复");
        assert!(p.pick("m", &[]).is_some());
    }

    #[test]
    fn soft_rate_exponential_backoff() {
        // 软冷却 600s 起，每次连续软限流 ×2，封顶 soft_rate_max。
        let cfg = GatewayConfig {
            soft_rate_base_secs: 600,
            soft_rate_max_secs: 3600,
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("u1"));
        for (round, expect) in [(1, 600i64), (2, 1200), (3, 2400), (4, 3600), (5, 3600)] {
            p.note_error("u1", "", ErrKind::SoftRate, "");
            let d = (p.entries[0].until_ms - now_ms()) / 1000;
            assert!((d - expect).abs() <= 1, "第 {round} 次软冷却应约 {expect}s，实际 {d}s");
        }
        // 成功后清零，再从 600s 起
        p.note_success("u1");
        p.note_error("u1", "", ErrKind::SoftRate, "");
        let d = (p.entries[0].until_ms - now_ms()) / 1000;
        assert!((d - 600).abs() <= 1, "成功后软冷却退避应清零");
    }

    #[test]
    fn hard_credit_cools_until_tomorrow_4am() {
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        p.note_error("u1", "", ErrKind::HardCredit, "");
        let e = &p.entries[0];
        assert_eq!(e.cool_kind, Some(CoolKind::Hard));
        assert_eq!(e.reason, "余额不足");
        let d = e.until_ms - now_ms();
        assert!(d > 0, "硬冷却必须在未来");
        assert!(d <= 28 * 3600 * 1000, "最晚不超过次日 04:00（≤28h），实际 {}h", d / 3_600_000);
    }

    #[test]
    fn session_dead_disables_permanently() {
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        p.note_error("u1", "", ErrKind::SessionDead, "");
        assert!(p.entries[0].disabled);
        assert_eq!(p.entries[0].reason, "12153 session dead");
        assert!(p.pick("m", &[]).is_none(), "禁用号永不参与（含兜底）");
        assert_eq!(p.entries[0].fails, 0, "禁用不喂熔断");
    }

    #[test]
    fn request_level_errors_do_not_penalize() {
        // ContextTooLong（请求级 400）/ Client / None / Transport：只换号不罚，
        // 传输层错误不喂熔断。
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        for kind in [
            ErrKind::ContextTooLong,
            ErrKind::Transport,
            ErrKind::Client,
            ErrKind::None,
        ] {
            p.note_error("u1", "m", kind, "");
        }
        assert_eq!(p.entries[0].err_total, 0);
        assert_eq!(p.entries[0].fails, 0);
        assert_eq!(p.entries[0].until_ms, 0);
        assert!(p.pick("m", &[]).is_some());
    }

    #[test]
    fn not_found_short_cools_60s() {
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        p.note_error("u1", "", ErrKind::NotFound, "");
        let d = (p.entries[0].until_ms - now_ms()) / 1000;
        assert!((d - 60).abs() <= 1, "404 应短冷却 60s，实际 {d}s");
        assert_eq!(p.entries[0].cool_kind, Some(CoolKind::Soft));
        assert_eq!(p.entries[0].reason, "upstream 404");
    }

    // -----------------------------------------------------------------------
    // 熔断升降
    // -----------------------------------------------------------------------

    #[test]
    fn breaker_trips_exponential_and_success_clears() {
        let cfg = GatewayConfig {
            breaker_threshold: 3,
            breaker_base_secs: 1800,
            breaker_max_secs: 21600,
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("u1"));
        let trip = |p: &Pool| (p.entries[0].breaker_until_ms - now_ms()) / 1000;

        // 前两次失败不熔断
        p.note_error("u1", "", ErrKind::Server, "");
        p.note_error("u1", "", ErrKind::Server, "");
        assert_eq!(p.entries[0].breaker_until_ms, 0);
        assert_eq!(p.entries[0].err_total, 2);
        assert!(p.servable_now());

        // 第三次 → 熔断 1800s
        p.note_error("u1", "", ErrKind::Server, "");
        assert!((trip(&p) - 1800).abs() <= 2, "首次熔断应 1800s");
        assert!(!p.servable_now(), "熔断期不可受理");
        assert_eq!(p.entries[0].fails, 0, "触发后 fails 清零");

        // 成功清零（降）
        p.note_success("u1");
        assert_eq!(p.entries[0].breaker_until_ms, 0);
        assert!(p.servable_now());

        // 指数退避：1800 → 3600 → 7200 → 14400 → 21600（封顶）→ 持续封顶
        for expect in [1800i64, 3600, 7200, 14400, 21600, 21600] {
            for _ in 0..3 {
                p.note_error("u1", "", ErrKind::Server, "");
            }
            assert!((trip(&p) - expect).abs() <= 2, "熔断时长应约 {expect}s");
        }
    }

    #[test]
    fn breaker_recovers_after_cooldown_expires() {
        // 熔断降级：退避到期后自动恢复受理。
        let cfg = GatewayConfig {
            breaker_threshold: 3,
            breaker_base_secs: 1,
            breaker_max_secs: 4,
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("u1"));
        for _ in 0..3 {
            p.note_error("u1", "", ErrKind::Server, "");
        }
        assert!(!p.servable_now(), "熔断期不可受理");
        std::thread::sleep(Duration::from_millis(1100));
        assert!(p.servable_now(), "退避到期应恢复");
    }

    #[test]
    fn soft_rate_does_not_escalate_breaker() {
        // issue #5 口径：软冷却（429）正常累计 fails、照常触发熔断，但不放大退避指数
        // （否则一次流量高峰会把账号按 30m→1h→2h→4h→6h 封死）。对照 Go pool.go:1237。
        let cfg = GatewayConfig {
            breaker_threshold: 3,
            breaker_base_secs: 1800,
            breaker_max_secs: 21600,
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("u1"));
        for _ in 0..3 {
            for _ in 0..3 {
                p.note_error("u1", "", ErrKind::SoftRate, "");
            }
            let d = (p.entries[0].breaker_until_ms - now_ms()) / 1000;
            assert!((d - 1800).abs() <= 2, "软冷却不应放大熔断时长，实际 {d}s");
        }
    }

    // -----------------------------------------------------------------------
    // 模型级冷却（uid+model，直接抄 Go 版口径）
    // -----------------------------------------------------------------------

    /// 上游 6004 实测文案（带 UTC+8 重置时刻）。
    fn model_rate_body() -> String {
        r#"{"message":"您的使用量已超出频率限制，将在 2099-01-01 10:00:00 UTC+8 重置，您也可以切换其他模型继续使用。"}"#
            .to_string()
    }

    #[test]
    fn model_cool_filters_all_three_entries() {
        // 选号 / 兜底两个入口都按 model 过滤；粘性入口由路由层把 sticky uid 放入
        // tried 实现（pick_account 的 model 过滤对粘性号一视同仁，见模块文档）。
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        p.upsert(mk_acc("u2"));
        p.note_error("u1", "m1", ErrKind::ModelRate, &model_rate_body());

        // 入口 1：选号跳过该模型被限流的账号
        assert_eq!(p.pick("m1", &[]).unwrap().uid, "u2");
        // u1 的其他模型不受影响
        assert!(p.pick("m2", &[]).is_some());

        // 入口 2：兜底也按 model 过滤——u1 账号级软冷却 + m1 模型冷却；u2 硬冷却（排除）
        p.note_error("u1", "", ErrKind::SoftRate, "");
        p.note_error("u2", "", ErrKind::HardCredit, "");
        assert!(
            p.pick("m1", &[]).is_none(),
            "u1 被 m1 模型冷却挡住、u2 硬冷却排除 → 无候选"
        );
        assert_eq!(p.pick("m2", &[]).unwrap().uid, "u1", "m2 不受影响，u1 可参与兜底");
    }

    #[test]
    fn model_cool_prunes_expired_and_status_lists_active() {
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        p.upsert(mk_acc("u2"));
        let idx1 = *p.by_uid.get("u1").unwrap();
        // 直接插入一条已过期的模型冷却
        p.cooldown_model(idx1, "m1", now_ms() - 1000, "expired", false);
        // 选号顺手清理
        assert!(p.pick("m1", &[]).is_some());
        assert!(p.entries[idx1].model_cools.is_empty(), "过期模型冷却应被清理");

        // 未过期的进 status 列表
        let idx2 = *p.by_uid.get("u2").unwrap();
        p.cooldown_model(idx2, "m9", now_ms() + 600_000, "m9 限流", false);
        let st = p.status_value();
        let acc = st["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["nickname"] == "nick-u2")
            .unwrap();
        let mcs = acc["model_cools"].as_array().unwrap();
        assert_eq!(mcs.len(), 1);
        assert_eq!(mcs[0]["model"], "m9");
        assert!(mcs[0]["remaining_sec"].as_i64().unwrap() > 500);
        assert_eq!(mcs[0]["reset_at_parsed"], false);
    }

    #[test]
    fn model_rate_does_not_feed_breaker() {
        // 模型级限流是配额信号而非账号故障：不喂熔断、不记累计错误。
        let cfg = GatewayConfig {
            breaker_threshold: 3,
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("u1"));
        for _ in 0..10 {
            p.note_error("u1", "m1", ErrKind::ModelRate, &model_rate_body());
        }
        assert_eq!(p.entries[0].fails, 0);
        assert_eq!(p.entries[0].breaker_until_ms, 0);
        assert_eq!(p.entries[0].err_total, 0);
        assert!(p.servable_now(), "账号其他模型仍可用");
        assert!(p.entries[0].model_cools.contains_key("m1"));

        // model 为空 → 不记模型冷却（无模型上下文无法按 uid+model 记账）
        p.note_error("u1", "", ErrKind::ModelRate, &model_rate_body());
        assert_eq!(p.entries[0].model_cools.len(), 1);
    }

    #[test]
    fn model_rate_parses_reset_time_with_fallback() {
        // 解析出上游重置时刻（UTC+8）
        let expected = chrono::FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2099, 1, 1, 10, 0, 0)
            .unwrap()
            .timestamp_millis();
        let mut p = mk_pool();
        p.upsert(mk_acc("u1"));
        p.note_error("u1", "m1", ErrKind::ModelRate, &model_rate_body());
        let mc = &p.entries[0].model_cools["m1"];
        assert!(mc.reset_parsed, "取到重置时间应标记 parsed");
        assert_eq!(mc.until_ms, expected);

        // 解析不出 → 回退 600s
        p.note_error("u1", "m2", ErrKind::ModelRate, r#"{"message":"rate limited"}"#);
        let mc2 = &p.entries[0].model_cools["m2"];
        assert!(!mc2.reset_parsed);
        assert!(
            (mc2.until_ms - now_ms() - MODEL_RATE_FALLBACK_MS).abs() < 2000,
            "解析不出应回退 600s"
        );
    }

    // -----------------------------------------------------------------------
    // 状态持久化 round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn state_round_trip_preserves_runtime() {
        let dir = tmp_dir("state");
        let fp = dir.join("workbuddy_gateway_state.json");
        let cfg = GatewayConfig::default();
        let mut p = Pool::new(fp.clone(), &cfg);
        p.upsert(mk_acc("u1"));
        p.upsert(mk_acc("u2"));
        p.set_credits("u1", 500, now_ms() + 5 * 86_400_000);
        p.note_error("u1", "", ErrKind::Server, "");
        p.note_success("u1");
        p.note_error("u2", "", ErrKind::SoftRate, "");
        p.note_error("u2", "m1", ErrKind::ModelRate, &model_rate_body());
        assert!(p.flush(), "dirty 后应落盘");

        // 毫秒 epoch 落盘
        let text = std::fs::read_to_string(&fp).unwrap();
        assert!(text.contains("until_ms"), "状态应为毫秒 epoch 字段: {text}");

        // 重载：冷却/熔断/模型冷却/credits/统计不丢失
        let mut p2 = Pool::new(fp.clone(), &cfg);
        assert_eq!(p2.entries.len(), 2);
        let e1 = &p2.entries[*p2.by_uid.get("u1").unwrap()];
        assert_eq!(e1.acc.credits, 500);
        assert_eq!(e1.success_count, 1);
        assert_eq!(e1.err_total, 1);
        assert!(e1.acc.credit_expires_at > 0);
        let e2 = &p2.entries[*p2.by_uid.get("u2").unwrap()];
        assert!(e2.until_ms > now_ms(), "软冷却应恢复");
        assert_eq!(e2.cool_kind, Some(CoolKind::Soft));
        assert!(e2.model_cools.contains_key("m1"), "模型冷却应恢复");

        // upsert 凭证后运行态保留（credits/统计不丢）
        p2.upsert(mk_acc("u1"));
        let e1 = &p2.entries[*p2.by_uid.get("u1").unwrap()];
        assert_eq!(e1.acc.credits, 500, "凭证同步不清 credits");
        assert_eq!(e1.success_count, 1);

        // 熔断截止同样持久化
        let mut p3 = Pool::new(fp.clone(), &cfg);
        for _ in 0..3 {
            p3.note_error("u1", "", ErrKind::Server, "");
        }
        assert!(p3.flush());
        let p4 = Pool::new(fp.clone(), &cfg);
        let e = &p4.entries[*p4.by_uid.get("u1").unwrap()];
        assert!(e.breaker_until_ms > now_ms(), "熔断截止应持久化");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_zero_values_and_seconds_legacy() {
        let dir = tmp_dir("zero");
        let fp = dir.join("state.json");
        std::fs::write(
            &fp,
            br#"{"accounts":{
                "u9":{"credits":7,"until_ms":0,"breaker_until_ms":0,"model_cools":{}},
                "u8":{"until_ms":1792814920}
            }}"#,
        )
        .unwrap();
        let p = Pool::new(fp, &GatewayConfig::default());
        let e9 = &p.entries[*p.by_uid.get("u9").unwrap()];
        assert_eq!(e9.acc.credits, 7);
        assert_eq!(e9.until_ms, 0, "零值 = None（不在冷却期）");
        assert_eq!(e9.breaker_until_ms, 0);
        assert!(e9.model_cools.is_empty());
        let e8 = &p.entries[*p.by_uid.get("u8").unwrap()];
        assert_eq!(e8.until_ms, 1792814920000i64, "秒级旧值应归一为毫秒");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_skips_when_clean_or_no_path() {
        let mut p = mk_pool(); // 空状态路径
        p.upsert(mk_acc("u1"));
        p.note_success("u1");
        assert!(!p.flush(), "无状态路径不落盘");
    }

    // -----------------------------------------------------------------------
    // 粘性
    // -----------------------------------------------------------------------

    #[test]
    fn sticky_ttl_rolling_and_expiry() {
        let mut map: HashMap<String, StickyBinding> = HashMap::new();
        let ttl = 1000i64;
        map.insert(
            "k".to_string(),
            StickyBinding {
                uid: "u1".to_string(),
                last_active_ms: 0,
            },
        );
        // 命中滚动续期
        assert_eq!(sticky_resolve(&mut map, "k", 999, ttl).as_deref(), Some("u1"));
        assert_eq!(map["k"].last_active_ms, 999, "命中应滚动续期");
        assert_eq!(sticky_resolve(&mut map, "k", 1998, ttl).as_deref(), Some("u1"));
        // 超过 TTL 失效并清除
        assert_eq!(sticky_resolve(&mut map, "k", 2999, ttl).as_deref(), None);
        assert!(!map.contains_key("k"), "失效应清除绑定");
        // 空 key 不粘性
        assert_eq!(sticky_resolve(&mut map, "", 0, ttl), None);
        // ttl<=0 回落默认（60min）
        map.insert(
            "k2".to_string(),
            StickyBinding {
                uid: "u2".to_string(),
                last_active_ms: 0,
            },
        );
        assert_eq!(
            sticky_resolve(&mut map, "k2", DEFAULT_STICKY_TTL_MS - 1, 0).as_deref(),
            Some("u2")
        );
        // 第一次命中已把 last_active 滚到 DEFAULT-1，失效需再等满一个 TTL
        assert_eq!(sticky_resolve(&mut map, "k2", DEFAULT_STICKY_TTL_MS * 2, 0), None);
    }

    #[test]
    fn sticky_global_set_get_clear() {
        // 全局表是进程级共享的，用唯一 key 避免测试间串扰。
        let key = format!("conv-{}", uuid::Uuid::new_v4());
        assert_eq!(sticky_get(&key), None);
        sticky_set(&key, "uid-x");
        assert_eq!(sticky_get(&key).as_deref(), Some("uid-x"));
        sticky_clear(&key);
        assert_eq!(sticky_get(&key), None);
        // 空 key / 空 uid 忽略
        sticky_set("", "uid-x");
        sticky_set(&key, "");
        assert_eq!(sticky_get(&key), None);
    }

    // -----------------------------------------------------------------------
    // 状态透出（脱敏）
    // -----------------------------------------------------------------------

    #[test]
    fn pool_status_masks_tokens_and_counts() {
        let mut p = mk_pool();
        let mut u1 = mk_acc("05efd60f-3079-41a8-ab31-745eb9c09a40");
        u1.nickname = "猫猫".to_string();
        p.upsert(u1);
        p.upsert(mk_acc("u2"));
        p.upsert(mk_acc("u3"));
        p.note_error("u2", "", ErrKind::SoftRate, "");
        for _ in 0..3 {
            p.note_error("u3", "", ErrKind::Server, ""); // u3 熔断
        }
        p.note_error("05efd60f-3079-41a8-ab31-745eb9c09a40", "m1", ErrKind::ModelRate, &model_rate_body()); // u1 模型冷却

        let st = p.status_value();
        assert_eq!(st["total"], 3);
        assert_eq!(st["healthy"], 1);
        assert_eq!(st["cooling"], 1);
        assert_eq!(st["breaker"], 1);
        assert_eq!(st["model_cooling"], 1);

        let raw = serde_json::to_string(&st).unwrap();
        assert!(!raw.contains("at-u"), "状态不得泄露 token");
        assert!(!raw.contains("05efd60f-3079"), "uid 应脱敏");
        assert!(raw.contains("05efd60f…"), "uid 应保留前 8 位");

        let accs = st["accounts"].as_array().unwrap();
        assert_eq!(accs.len(), 3);
        assert_eq!(accs[0]["uid"], "05efd60f…", "按 uid 排序稳定输出");
        assert_eq!(accs[0]["nickname"], "猫猫");
        assert_eq!(accs[0]["region"], "cn");
        assert_eq!(accs[0]["model_cools"].as_array().unwrap().len(), 1);
    }

    // -----------------------------------------------------------------------
    // 配置
    // -----------------------------------------------------------------------

    #[test]
    fn config_round_trip_and_clamp() {
        let dir = tmp_dir("cfg");
        let fp = dir.join("workbuddy_gateway_config.json");
        let cfg = GatewayConfig {
            mode: "rotate".to_string(),
            pinned_uid: " u1 ".to_string(),
            soft_rate_base_secs: 300,
            soft_rate_max_secs: 1200,
            breaker_threshold: 5,
            breaker_base_secs: 60,
            breaker_max_secs: 600,
            max_in_flight: 4,
            sticky_ttl_ms: 1_800_000,
            sanitize_enabled: false,
            balance_refresh_minutes: 30,
        };
        save_config_at(&fp, &cfg).unwrap();
        let back = load_config_at(&fp);
        assert_eq!(back.mode, "rotate");
        assert_eq!(back.pinned_uid, "u1", "pinned_uid 应去空格");
        assert_eq!(back.soft_rate_base_secs, 300);
        assert_eq!(back.soft_rate_max_secs, 1200);
        assert_eq!(back.breaker_threshold, 5);
        assert_eq!(back.max_in_flight, 4);
        assert_eq!(back.sticky_ttl_ms, 1_800_000);
        assert!(!back.sanitize_enabled);
        assert_eq!(back.balance_refresh_minutes, 30);

        // 脏值钳制
        let dirty = GatewayConfig {
            mode: "bogus".to_string(),
            soft_rate_base_secs: 0,
            soft_rate_max_secs: 1,
            breaker_threshold: 0,
            breaker_base_secs: 0,
            breaker_max_secs: 1,
            max_in_flight: -5,
            sticky_ttl_ms: 0,
            balance_refresh_minutes: 0,
            ..Default::default()
        };
        save_config_at(&fp, &dirty).unwrap();
        let c = load_config_at(&fp);
        let d = GatewayConfig::default();
        assert_eq!(c.mode, "balance", "白名单外模式回落");
        assert_eq!(c.soft_rate_base_secs, d.soft_rate_base_secs, "0 = 未设置走默认");
        assert!(c.soft_rate_max_secs >= c.soft_rate_base_secs, "封顶不小于基础");
        assert_eq!(c.breaker_threshold, d.breaker_threshold);
        assert!(c.breaker_max_secs >= c.breaker_base_secs);
        assert_eq!(c.max_in_flight, 0, "0 = 不限，不钳到默认");
        assert_eq!(c.sticky_ttl_ms, d.sticky_ttl_ms);
        assert_eq!(c.balance_refresh_minutes, d.balance_refresh_minutes);

        // 缺失 / 损坏文件 → 默认
        assert_eq!(load_config_at(&dir.join("nope.json")), d);
        std::fs::write(&fp, "not json").unwrap();
        assert_eq!(load_config_at(&fp), d);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_config_pushes_sticky_ttl() {
        // 配置热更新：粘性 TTL 活值跟随 apply_config。
        let prev = STICKY_TTL_MS.load(Ordering::Relaxed);
        let mut p = mk_pool();
        p.apply_config(&GatewayConfig {
            sticky_ttl_ms: 123_456,
            ..Default::default()
        });
        assert_eq!(STICKY_TTL_MS.load(Ordering::Relaxed), 123_456);
        p.apply_config(&GatewayConfig::default());
        assert_eq!(STICKY_TTL_MS.load(Ordering::Relaxed), DEFAULT_STICKY_TTL_MS);
        STICKY_TTL_MS.store(prev, Ordering::Relaxed);
    }
}
