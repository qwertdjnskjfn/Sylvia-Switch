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
//! # 余额巡检（增补 §2.1）
//!
//! 只读维护循环：启动后 60s 首轮，之后每 `balance_refresh_minutes` 分钟
//! （0 = 关闭）对池内账号逐个调 `workbuddy_upstream::user_resource` 回填
//! `credits` / `credit_expires_at`（驱动到期分层选号）。不写 accounts.json、
//! 不改账号状态；单账号失败静默跳过（巡检失败 ≠ 账号故障，不罚号）；
//! 禁用 / error 账号跳过；账号间隔 300ms。
//!
//! # 路由层接线约定（供 api.rs 薄接线参考）
//!
//! 粘性入口的「按 model 过滤」由路由层把 sticky uid 放进 `tried` 实现：
//! `sticky_get(key)` → 命中则 `pick_account(model, &[])` 后比对 uid，成功即
//! `sticky_set(key, 实际 uid)` 重绑（对齐参考实现 session.Bind 的「粘性跟随最终成功号」），
//! 失败则 `sticky_clear(key)` 解绑；未命中直接 `pick_account(model, &[])`。
//! 因为 `pick_account` 的统一候选过滤（健康 / 模型冷却 / 在途 / tried）对粘性号一视同仁。
//!
//! # 区域隔离（workbuddy-pool-region 契约 §3.1/§3.2）
//!
//! 账号 region 由登录域后缀派生（`.cn→cn` / `.ai→intl`，见 `pool_account_from_value`），
//! 上游端点已按区域分域——缺口纯在路由与聚合，本文件补三件：
//! ① 区域目录缓存 [`region_catalog`]（TTL 10min，两区独立成败，intl 无账号回落静态表）；
//! ② 解析 [`resolve_model_region`]（hint 优先 > 唯一命中 > 无约束）与区域约束选号
//!    [`pick_account_for_region`]（候选收集与全冷却兜底同步过滤，pinned/rotate 前置比对）；
//! ③ 11102 跨区翻转 [`region_flip`] 与粘性区域一致性 [`sticky_region_mismatch`]
//!    （判定在 core，接线在 api.rs dispatch）。
//! 旧签名 `pick_account(model, tried)` 保留为 region=Unknown 委托（回归红线 3）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use chrono::{Datelike, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::modules::account::get_str;
use crate::modules::config::{accounts_file, atomic_write, norm_ts, now_ms, store_dir};
use crate::modules::platform_checkin::{account_platform, PLATFORM_WORKBUDDY};
use crate::modules::workbuddy_errors::{self, ErrKind};
use crate::modules::workbuddy_upstream;

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
/// 巡检间隔下限（分钟）：非 0 值最小 5 分钟（增补 §2.1：clamp 放开 0，其余 >=5）。
const BALANCE_REFRESH_MIN_MINUTES: u64 = 5;

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
    /// 余额巡检间隔（分钟，驱动分层选号）；0 = 关闭巡检（增补 §2.1）。
    pub balance_refresh_minutes: u64,
    /// 国际区（www.workbuddy.ai）出站代理，如 `http://127.0.0.1:18502`。
    /// **空 = 直连**（当前默认，也是直连可达时的正确取值）；非空 = 国际区 chat/rpc 走它。
    ///
    /// 🔴 这不是「配了就更好」的字段——它只在**直连不可达**时才有意义，且一旦所指向的
    /// 出口失效会变成**唯一**故障点（2026-10-13 实翻车：直连已恢复而代理全挂，
    /// 导致国际区连续 11 小时 503）。**改动前先实测直连**，详见
    /// `workbuddy_upstream.rs::intl_proxy_config` 的取值对照表。国内区**不走**代理——
    /// Clash 抖动掐 SSE 的历史口径（workbuddy_upstream.rs 2026-10-12 注释）。
    pub intl_proxy: String,
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
            intl_proxy: String::new(),
        }
    }
}

impl GatewayConfig {
    /// 钳制非法值（PATCH 配置与读盘共用）：mode 白名单外回落 balance、
    /// `<=0` 的阈值/时长视为未设置走默认、封顶不小于基础值。
    /// 例外：`balance_refresh_minutes` 的 0 = 关闭巡检（增补 §2.1，合法值不回落），
    /// 其余钳到 [5, 24*60] 分钟。
    fn normalized(mut self) -> Self {
        let d = GatewayConfig::default();
        self.mode = match self.mode.trim() {
            "rotate" => "rotate",
            "pinned" => "pinned",
            _ => "balance",
        }
        .to_string();
        self.pinned_uid = self.pinned_uid.trim().to_string();
        self.intl_proxy = self.intl_proxy.trim().to_string();
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
        // 增补 §2.1：0 = 关闭巡检（不回落默认）；其余钳到 [5, 24*60] 分钟。
        self.balance_refresh_minutes = if self.balance_refresh_minutes == 0 {
            0
        } else {
            self.balance_refresh_minutes
                .clamp(BALANCE_REFRESH_MIN_MINUTES, 24 * 60)
        };
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
        intl_proxy: v
            .get("intl_proxy")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
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
        "intl_proxy": cfg.intl_proxy,
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
        "intl_proxy": cfg.intl_proxy,
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

/// 账号库记录的 uid 口径（auth_raw.uid → 顶层 uid → id；桥接 / 回写 / 巡检共用）。
fn account_uid(acc: &Value) -> Option<String> {
    acc.get("auth_raw")
        .and_then(|r| r.get("uid"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| get_str(acc, "uid"))
        .or_else(|| get_str(acc, "id"))
}

/// 单条账号库记录 → 池条目（纯函数）。
fn pool_account_from_value(acc: &Value) -> Option<PoolAccount> {
    if account_platform(acc) != PLATFORM_WORKBUDDY {
        return None;
    }
    let access_token = get_str(acc, "access_token")?;
    let uid = account_uid(acc)?;
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

/// 一条记录**未能进池的原因**；`None` = 可以进池。与 [`pool_account_from_value`]
/// 的拒绝条件**逐条对应**（改那边必须同步改这里，单测
/// `reject_reason_matches_admission` 会守住这条不变量）。
fn pool_account_reject_reason(acc: &Value) -> Option<&'static str> {
    if account_platform(acc) != PLATFORM_WORKBUDDY {
        return Some("非 WorkBuddy 平台账号（platform 指向其他渠道）");
    }
    if get_str(acc, "access_token").is_none() {
        return Some("缺少 access_token（导入文件多为脱敏导出，不含凭证）");
    }
    if account_uid(acc).is_none() {
        return Some("缺少 uid（auth_raw.uid / uid / id 三者都没有）");
    }
    None
}

/// 账号库中**存在但没能进池**的记录及原因（诊断用）。
///
/// # 为什么需要
///
/// [`pool_account_from_value`] 对不合格记录**静默返回 `None`** —— 于是
/// 「导入成功、账号池里却看不到」时，用户既不知道该看哪里，也分不清是
/// 「被拒了」还是「根本没导进来」。2026-10-13 用户报障即为此。
/// 本函数把「被拒」这件事显式暴露出来（进 `/workbuddy-pool/v1/status` 的 `rejected`）。
pub fn rejected_accounts() -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(accounts_file()) else {
        return Vec::new();
    };
    let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    arr.iter()
        .enumerate()
        .filter_map(|(index, a)| {
            let reason = pool_account_reject_reason(a)?;
            Some(json!({
                "index": index,
                "nickname": get_str(a, "nickname").unwrap_or_default(),
                "id": get_str(a, "id").unwrap_or_default(),
                "domain": get_str(a, "domain").unwrap_or_default(),
                "platform": account_platform(a),
                "reason": reason,
            }))
        })
        .collect()
}

/// 把「池 uid」映射回**账号库记录 id**（编辑 / 删除按 `id` 定位）。
///
/// 为什么需要中间一步：池的 uid 取自 `auth_raw.uid → uid → id`（见 [`account_uid`]），
/// 与账号库的 `id` 字段**不保证相同** —— 直接拿池 uid 当 id 去改会「找不到账号」。
/// 复用 [`account_matches_uid`]（同一份 uid 口径），不另写一套。
pub fn account_library_id_for_uid(uid: &str) -> Option<String> {
    let text = std::fs::read_to_string(accounts_file()).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let arr = parsed.as_array()?;
    arr.iter()
        .find(|a| account_matches_uid(a, uid))
        .and_then(|a| get_str(a, "id"))
}

/// 记录是否匹配 uid（口径与桥接一致，见 account_uid）。
fn account_matches_uid(acc: &Value, uid: &str) -> bool {
    if uid.is_empty() {
        return false;
    }
    account_uid(acc).as_deref() == Some(uid)
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
    ///
    /// ⚠️ **系统状态，非用户开关**：只由错误路径置位（`ErrKind::SessionDead`），
    /// 表示上游判定该凭证已失效。用户想临时停用某账号请用 [`Entry::paused`]。
    disabled: bool,
    /// **用户意图**：手动暂停该账号（不参与选号）。
    ///
    /// 与 `disabled` 严格分离——`disabled` 是「系统说这号废了」，`paused` 是
    /// 「我想先不用它」。两者都让账号退出选号，但**恢复路径不同**：
    /// `disabled` 需重新登录换凭证，`paused` 一个开关即可撤销。
    /// 混用会导致「用户手动停用 → 被误报成 session 失效 → 让人去重登」。
    paused: bool,
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
            paused: false,
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

    /// 报告账号当前是否可选（未禁用、未暂停、未处于任一冷却/熔断期）。
    fn healthy(&self, now: i64) -> bool {
        !self.disabled && !self.paused && self.until_ms <= now && self.breaker_until_ms <= now
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
    /// 用户手动暂停（2026-10-13 新增）。`serde(default)` ⇒ 旧状态文件无此字段时
    /// 读为 false，**向后兼容**（老版本写的 state 不会被误判成全暂停）。
    #[serde(default)]
    paused: bool,
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
            entry.paused = s.paused;
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
                    paused: e.paused,
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

    /// 巡检目标快照：过滤禁用（session 死亡出池）与账号库 error 标记后的凭证列表。
    /// 纯过滤无 IO，且**不在锁内 await**：快照完即放锁，HTTP 巡检期间不阻塞
    /// chat 选号路径（pool 锁是请求路径的热锁）。
    fn patrol_targets(&self, errored: &HashSet<String>) -> Vec<PoolAccount> {
        self.entries
            .iter()
            .filter(|e| patrol_should_check(e.disabled, errored.contains(&e.acc.uid)))
            .map(|e| e.acc.clone())
            .collect()
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
    ///
    /// 旧签名保留（workbuddy-pool-region 契约回归红线 3）：无区域约束，
    /// 委托 [`Pool::pick_account_for_region`] 的 Unknown 分支，语义逐字节等价。
    pub fn pick_account(&mut self, model: &str, tried: &[String]) -> Option<PoolAccount> {
        self.pick_account_for_region(model, tried, RegionFilter::Unknown)
    }

    /// 区域约束版选号（workbuddy-pool-region 契约 §3.2）：只在 wanted 区候选里挑。
    /// pinned / rotate 的 `pick_by_uid` 前先比对区域，不匹配直接跳过该路径回落
    /// 区域化 pick（不硬塞跨区请求）；wanted = Unknown 时与旧签名完全等价。
    pub fn pick_account_for_region(
        &mut self,
        model: &str,
        tried: &[String],
        wanted: RegionFilter,
    ) -> Option<PoolAccount> {
        match self.mode.as_str() {
            "pinned" => {
                // 指定模式： pinned 号可用就用它；不可用（冷却/熔断/模型限流/在途/tried/
                // 区域不匹配）回落普通选号，避免指定号故障时整个池不可用。
                let pinned = self.pinned_uid.clone();
                if !pinned.is_empty()
                    && !tried.iter().any(|t| t == &pinned)
                    && self.uid_in_region(&pinned, wanted)
                {
                    if let Some(acc) = self.pick_by_uid(&pinned, model) {
                        return Some(acc);
                    }
                }
                self.pick_region(model, tried, wanted)
            }
            "rotate" => {
                // 烧号模式：当前号仍可用就继续用，不可用（含区域不匹配）才换下一个
                // （换的仍是选号算法挑的，且同样受区域约束）。
                let current = self.rotate_uid.clone();
                if !current.is_empty()
                    && !tried.iter().any(|t| t == &current)
                    && self.uid_in_region(&current, wanted)
                {
                    if let Some(acc) = self.pick_by_uid(&current, model) {
                        return Some(acc);
                    }
                }
                let acc = self.pick_region(model, tried, wanted)?;
                self.rotate_uid = acc.uid.clone();
                Some(acc)
            }
            _ => self.pick_region(model, tried, wanted),
        }
    }

    /// uid 对应条目的区域是否满足 wanted（Unknown 恒 true；uid 不在池内 = 不满足，
    /// 与 `pick_by_uid` 查不到返回 None 的口径一致）。
    fn uid_in_region(&self, uid: &str, wanted: RegionFilter) -> bool {
        match self.by_uid.get(uid) {
            Some(&idx) => wanted.allows(&self.entries[idx].acc.region),
            None => false,
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

    /// 挑号（无区域约束；旧形态，仅测试矩阵使用——生产路径一律走
    /// [`Pool::pick_region`]，无约束等价于 wanted=Unknown）。
    #[cfg(test)]
    fn pick(&mut self, model: &str, tried: &[String]) -> Option<PoolAccount> {
        self.pick_region(model, tried, RegionFilter::Unknown)
    }

    /// 诊断：`wanted` 区内的账号**为什么都没被选中**（逐条给原因，最多 3 条）。
    ///
    /// 🔴 存在意义（2026-10-13 用户报障）：选号返回 None 时，调用方对外只发一句
    /// 「区内无可用账号」——**看不出为什么**。尤其「**模型级冷却**」（对某模型达上游
    /// 频率上限）会被误读成「整个区都挂了」，用户拿到 503 以为网关故障，
    /// 实际只是单个模型被限流、同账号其他模型仍可用。
    ///
    /// 判据与 [`Pool::pick_region`] 的闸门**一一对应**（healthy / 模型冷却 / 在途），
    /// 改选号逻辑时必须同步改这里，否则诊断会说谎。
    fn region_block_reasons(&self, model: &str, wanted: RegionFilter) -> Vec<String> {
        let now = now_ms();
        let mut out: Vec<String> = Vec::new();
        for e in &self.entries {
            if !wanted.allows(&e.acc.region) {
                continue;
            }
            let label = if e.acc.nickname.trim().is_empty() {
                e.acc.uid.chars().take(8).collect::<String>()
            } else {
                e.acc.nickname.clone()
            };
            // 顺序即优先级：模型级冷却排最前，它最容易被误读成整体故障。
            let why = if e.disabled {
                Some(format!("已禁用（{}）", e.reason))
            } else if e.paused {
                // 用户主动暂停：既不是故障也不是配额，**必须说清楚**，
                // 否则会被当成「服务故障」去查链路（白白浪费时间）。
                Some("已被手动暂停（可在账号池页面恢复）".to_string())
            } else if e.model_cooled(model, now) {
                Some(
                    e.model_cools
                        .get(model)
                        .map(|mc| mc.reason.clone())
                        .unwrap_or_else(|| format!("模型 {model} 已被限流")),
                )
            } else if e.until_ms > now {
                Some(format!(
                    "冷却中至 {}（{}）",
                    fmt_ms(e.until_ms),
                    e.reason
                ))
            } else if e.breaker_until_ms > now {
                Some(format!(
                    "熔断中至 {}（{}）",
                    fmt_ms(e.breaker_until_ms),
                    e.reason
                ))
            } else if self.in_flight_full(e) {
                Some("在途并发已占满".to_string())
            } else {
                // 健康账号：选不中只可能是被 tried 排空，无需解释。
                None
            };
            if let Some(w) = why {
                out.push(format!("{label}：{w}"));
                if out.len() >= 3 {
                    break;
                }
            }
        }
        out
    }

    /// 按「完整 uid 或**唯一前缀**」解析出真实 uid。
    ///
    /// 为什么需要它：`pool_status()` 输出的 uid 是**脱敏**的（`mask_uid` 只留前 8 位 + "…"），
    /// 前端拿到的就是这个脱敏值——写操作必须能把它映射回真 uid，否则
    /// 「界面看得到、点不动」。池内账号量级很小（个位数），唯一前缀足够安全；
    /// **多命中即拒绝**（不猜），避免误停用别的账号。
    fn resolve_uid(&self, key: &str) -> Result<String, String> {
        let k = key.trim().trim_end_matches('…').trim_end_matches("...");
        if k.is_empty() {
            return Err("uid 不能为空".to_string());
        }
        if self.by_uid.contains_key(k) {
            return Ok(k.to_string());
        }
        let hits: Vec<&String> = self.by_uid.keys().filter(|u| u.starts_with(k)).collect();
        match hits.len() {
            1 => Ok(hits[0].clone()),
            0 => Err(format!("账号不存在：{key}")),
            n => Err(format!("uid 前缀「{key}」命中 {n} 个账号，请提供更长的前缀")),
        }
    }

    /// 设置账号的**用户暂停**标记。uid 支持完整值或唯一前缀（见 [`Pool::resolve_uid`]）。
    ///
    /// 🔴 只动 `paused`，**绝不碰** `disabled`（系统状态）与任何冷却字段 ——
    /// 暂停是「临时不用」，恢复时必须让账号回到暂停**之前**的状态；
    /// 顺手清冷却会让一个已熔断/被限流的号立刻重新入池，把问题从「我不用它」
    /// 变成「它又在报错」。
    pub fn set_paused(&mut self, key: &str, paused: bool) -> Result<String, String> {
        let uid = self.resolve_uid(key)?;
        let idx = *self.by_uid.get(&uid).expect("resolve_uid 已保证存在");
        self.entries[idx].paused = paused;
        self.dirty = true;
        Ok(uid)
    }

    /// 清除账号的**冷却类**运行态（手动干预）。
    ///
    /// 清的是：账号级冷却、熔断、模型级冷却、以及喂熔断用的失败计数。
    ///
    /// 🔴 **绝不动** `disabled`（凭证失效的**事实**）与 `paused`（**用户意图**）——
    /// 清冷却只解决「不等它自己退避」这件事：伪造凭证有效会让请求立刻再失败，
    /// 替用户改主意则违背他的显式选择。
    pub fn clear_cooldown(&mut self, key: &str) -> Result<String, String> {
        let uid = self.resolve_uid(key)?;
        let idx = *self.by_uid.get(&uid).expect("resolve_uid 已保证存在");
        let e = &mut self.entries[idx];
        e.until_ms = 0;
        e.cool_kind = None;
        e.breaker_until_ms = 0;
        e.fails = 0;
        e.retry_count = 0;
        e.soft_fails = 0;
        e.model_cools.clear();
        // 出池（disabled）的 reason 描述的是凭证事实，保留；其余是冷却备注，清掉
        // 以免界面显示已不存在的「429 rate limit」之类陈旧原因。
        if !e.disabled {
            e.reason.clear();
        }
        self.dirty = true;
        Ok(uid)
    }

    /// 挑号（区域约束版），跳过 `tried` 中的 UID。到期分层→Top5 加权→防惊群→LRU 兜底。
    /// 对照 pool.rs:840 `pick_excluding` + pool.go:725 `pick`（model 过滤照 Go 版）。
    /// wanted ≠ Unknown 时候选收集与全冷却兜底同步按区域过滤（契约 §3.2）。
    fn pick_region(
        &mut self,
        model: &str,
        tried: &[String],
        wanted: RegionFilter,
    ) -> Option<PoolAccount> {
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

        // 1) 收集 healthy 且未在途占满、不在 tried、该模型未被模型级冷却、区域满足
        // wanted 的候选（按 UID 排序保证确定性）
        let mut cands: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                !tried.iter().any(|t| t == &e.acc.uid)
                    && wanted.allows(&e.acc.region)
                    && e.healthy(now)
                    && !self.in_flight_full(e)
                    && !e.model_cooled(model, now)
            })
            .collect();
        cands.sort_by(|&a, &b| self.entries[a].acc.uid.cmp(&self.entries[b].acc.uid));

        if cands.is_empty() {
            // 全冷却兜底（soft/breaker 号可参与，hard 冷却排除；区域约束同步生效）。
            return self.pick_earliest_expiry(tried, model, now, wanted);
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
    /// / 该模型被限流 / **区域不满足 wanted** 的账号同样跳过。对照 pool.rs:934
    /// `pick_earliest_expiry` + pool.go:886 `pickEarliestExpiryLocked`（model 过滤照 Go 版）。
    fn pick_earliest_expiry(
        &mut self,
        tried: &[String],
        model: &str,
        now: i64,
        wanted: RegionFilter,
    ) -> Option<PoolAccount> {
        let mut best: Option<usize> = None;
        let mut best_exp = 0i64;
        for i in 0..self.entries.len() {
            let e = &self.entries[i];
            if tried.iter().any(|t| t == &e.acc.uid) {
                continue;
            }
            // 区域约束（§3.2）：对侧账号不参与兜底——兜底选了它照样 11102。
            if !wanted.allows(&e.acc.region) {
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
    ///
    /// 增 regions 聚合（workbuddy-pool-region 契约 §3.5）：
    /// `{cn:{accounts,healthy,models?},intl:{...}}`——models 数取自区域目录缓存，
    /// 缓存未建/过期为 null（status 是只读透出，不因查询而触发上游探测）。
    pub fn status_value(&self) -> Value {
        let now = now_ms();
        // 区域聚合（§3.5）：accounts/healthy 按条目区域计数（healthy 口径 = `Entry::healthy`：
        // 未禁用、未暂停、不在任一冷却/熔断期）。
        let catalog = region_catalog_cached();
        let mut regions = serde_json::Map::new();
        for region in ["cn", "intl"] {
            let mut accounts = 0i64;
            let mut healthy = 0i64;
            for e in &self.entries {
                if e.acc.region != region {
                    continue;
                }
                accounts += 1;
                if e.healthy(now) {
                    healthy += 1;
                }
            }
            let models = catalog.as_ref().map(|c| {
                if region == "cn" {
                    c.cn_ids.len() as i64
                } else {
                    c.intl_ids.len() as i64
                }
            });
            regions.insert(
                region.to_string(),
                json!({"accounts": accounts, "healthy": healthy, "models": models}),
            );
        }
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
            // 🔴 必须走 `Entry::healthy()`，**不要**在这里重新推导条件。
            // 2026-10-13 实翻车：新增 `paused` 时只改了 `healthy()`，这里仍是
            // `!disabled && !cooling && !breaker` ⇒ 暂停后池级 `healthy` 仍报满值，
            // 与账号级判定不一致（界面显示「3/3 可用」，实际选号已排除该号）。
            // 单一事实来源，杜绝此类漂移。
            if e.healthy(now) {
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
                "paused": e.paused,
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
            "regions": Value::Object(regions),
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

/// ms epoch → `MM-DD HH:MM`（本地时区；非法值回落空串）。
/// 冷却原因与区域诊断共用同一格式，避免两处时间文案不一致。
fn fmt_ms(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 模型冷却的展示文案（含模型名与重置时间）。对照 Go handler.go:629 `modelRateReason`。
fn model_rate_reason(model: &str, until_ms: i64, parsed: bool) -> String {
    let name = if model.is_empty() { "（未知模型）" } else { model };
    let ts = fmt_ms(until_ms);
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

/// 选号（区域约束版，workbuddy-pool-region 契约 §3.2）：只在 wanted 区候选里挑
/// （含全冷却兜底同步过滤）。`pick_account`（无约束旧签名）保留为 Unknown 委托，
/// 粘性等既有调用点语义不变；本期 dispatch 全部走本入口。
pub fn pick_account_for_region(
    model: &str,
    tried: &[String],
    wanted: RegionFilter,
) -> Option<PoolAccount> {
    with_pool(|p| p.pick_account_for_region(model, tried, wanted))
}

/// 诊断：`wanted` 区内账号不可用的原因清单（最多 3 条，空 = 无法解释）。
///
/// **仅在选号返回 None 之后调用**，用于把「为什么」一并告知客户端，
/// 把不可行动的 `503 无可用账号` 变成可自行处置的提示
/// （详见 `Pool::region_block_reasons` 的存在意义）。
pub fn region_block_reasons(model: &str, wanted: RegionFilter) -> Vec<String> {
    with_pool(|p| p.region_block_reasons(model, wanted))
}

/// 设置账号的用户暂停标记；返回被解析出的真实 uid。
///
/// `key` 可为完整 uid或**唯一前缀**（状态接口输出的是脱敏 uid，见 `Pool::resolve_uid`）。
/// 错误（不存在 / 前缀歧义）以 `Err` 返回，由调用方转成 4xx。
pub fn set_paused(key: &str, paused: bool) -> Result<String, String> {
    with_pool(|p| p.set_paused(key, paused))
}

/// 清除账号的冷却/熔断/模型限流状态（手动干预）。返回真实 uid。
///
/// ⚠️ 这是**手动覆盖**：清掉熔断计数后，同一故障会重新累积。仅用于确认是误判
/// （如出口抖动已恢复）时提前放行；**不动** `disabled` 与 `paused`。
pub fn clear_cooldown(key: &str) -> Result<String, String> {
    with_pool(|p| p.clear_cooldown(key))
}

/// 把「完整 uid 或唯一前缀」解析成真实 uid（供按 uid 操作账号库的调用方复用）。
///
/// 状态接口输出的是**脱敏 uid**（前 8 位 + `…`），前端只能拿到该值；
/// 凡「界面看得到就得点得动」的写操作都要先过这里。前缀歧义 → `Err`（不猜）。
pub fn resolve_account_uid(key: &str) -> Result<String, String> {
    with_pool(|p| p.resolve_uid(key))
}

// ---------------------------------------------------------------------------
// 区域隔离（契约 docs/workbuddy-pool-region-contract.md §3.1/§3.2/§3.4）
// ---------------------------------------------------------------------------

/// 区域过滤（§3.1）：选号只在 wanted 区候选里挑；[`RegionFilter::Unknown`] = 无约束
/// （两区同名 / 两边都没有 / 缓存不可用降级——维持现状交上游校验）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegionFilter {
    /// 无约束（默认；等价旧签名的 region=None 语义）。
    #[default]
    Unknown,
    /// 只挑国服（.cn 派生）账号。
    Cn,
    /// 只挑国际版（.ai 派生）账号。
    Intl,
}

impl RegionFilter {
    /// 请求体显式 `region` 字段（"cn"/"intl"，大小写/首尾空白不敏感）→ 过滤；
    /// 缺席 / 非法值返回 None（交区域目录解析，§3.3 优先级①）。
    pub fn from_hint(hint: Option<&str>) -> Option<RegionFilter> {
        match hint.map(str::trim).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("cn") => Some(RegionFilter::Cn),
            Some("intl") => Some(RegionFilter::Intl),
            _ => None,
        }
    }

    /// wanted 区域名（None = 无约束）；日志与错误文案用。
    pub fn wanted_region(self) -> Option<&'static str> {
        match self {
            RegionFilter::Cn => Some("cn"),
            RegionFilter::Intl => Some("intl"),
            RegionFilter::Unknown => None,
        }
    }

    /// 纯判定：账号区域是否满足过滤（Unknown 恒 true）。
    fn allows(self, acc_region: &str) -> bool {
        match self {
            RegionFilter::Unknown => true,
            RegionFilter::Cn => acc_region == "cn",
            RegionFilter::Intl => acc_region == "intl",
        }
    }
}

/// 11102 跨区翻转（§3.3）：上游「model service info not found」说明本轮实际使用的
/// 账号区域与模型区域不匹配 → 把 filter 翻到对侧继续重试。已是明确约束 → 直接取
/// 对侧；无约束（Unknown）→ 从本轮实际使用的 acc.region 反推对侧。
pub fn region_flip(current: RegionFilter, used_region: &str) -> RegionFilter {
    match current {
        RegionFilter::Cn => RegionFilter::Intl,
        RegionFilter::Intl => RegionFilter::Cn,
        RegionFilter::Unknown => {
            if used_region.trim().eq_ignore_ascii_case("intl") {
                RegionFilter::Cn
            } else {
                RegionFilter::Intl
            }
        }
    }
}

/// T4 粘性区域一致性判定：wanted 有约束且粘性号区域不满足 → 应解绑粘性走常规
/// （不硬塞跨区请求）。`sticky_region` = 粘性 uid 在账号库里的 region；查不到视为
/// 不匹配（绑定的号已不在池内，粘性本就该重选）。wanted 无约束恒 false（现状不变）。
pub fn sticky_region_mismatch(sticky_region: Option<&str>, wanted: RegionFilter) -> bool {
    match wanted.wanted_region() {
        None => false,
        Some(w) => sticky_region != Some(w),
    }
}

/// 区域目录（§3.1）：两区模型 id 集（区域路由判据）+ 构建时刻（TTL 判据）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionCatalog {
    pub cn_ids: HashSet<String>,
    pub intl_ids: HashSet<String>,
    pub probed_at: i64,
}

impl RegionCatalog {
    /// 模型落在哪区（(in_cn, in_intl)）；解析唯一判据。
    fn locate(&self, model: &str) -> (bool, bool) {
        (self.cn_ids.contains(model), self.intl_ids.contains(model))
    }
}

/// 区域目录 TTL：10 分钟（§3.2 T2：chat 时零额外上游调用）。
const REGION_CATALOG_TTL_MS: i64 = 10 * 60 * 1000;

/// 区域目录 TTL 缓存（进程内共享）。仿 trae_acp `NATIVE_CATALOG_CACHE` 范式：
/// 锁只护快照读写，重建（网络）在锁外做。
static REGION_CATALOG_CACHE: Mutex<Option<RegionCatalog>> = Mutex::new(None);

/// 区域目录快照（TTL 内有效才返回；未建/过期返回 None——解析与 status 共用）。
fn region_catalog_cached() -> Option<RegionCatalog> {
    let guard = REGION_CATALOG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .filter(|c| now_ms() - c.probed_at < REGION_CATALOG_TTL_MS)
        .cloned()
}

/// 区域目录（§3.1）：TTL 内直接返回缓存；过期/未建则重建一次。
/// 重建失败返回 Err——调用方（dispatch）降级为无约束路由，绝不让 chat 挂死。
pub async fn region_catalog() -> Result<RegionCatalog, String> {
    if let Some(c) = region_catalog_cached() {
        return Ok(c);
    }
    region_catalog_refresh().await
}

/// 强制重建区域目录（绕过 TTL；成功才写缓存）。
pub async fn region_catalog_refresh() -> Result<RegionCatalog, String> {
    let catalog = build_region_catalog().await?;
    let mut guard = REGION_CATALOG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *guard = Some(catalog.clone());
    Ok(catalog)
}

/// 区域目录重建（§3.1）：cn 区 = 首个成功 cn 账号 `fetch_models` 的 id 集；
/// intl 区 = 首个成功 intl 账号，无 intl 账号或全失败 → `intl_static_models()` 的
/// id 集（静态表就是为「本机无国际版账号」场景准备的，契约 §五-6）。两区独立成败，
/// 互不阻塞。**不在任何锁内 await**：账号快照直接读 accounts.json（不经池锁），
/// fetch_models 是纯网络调用。
async fn build_region_catalog() -> Result<RegionCatalog, String> {
    let accounts = load_pool_accounts();
    let mut cn_ids = HashSet::new();
    for acc in accounts.iter().filter(|a| a.region == "cn") {
        if let Ok(payload) = workbuddy_upstream::fetch_models(acc).await {
            cn_ids = ids_from_models_payload(&payload);
            break;
        }
    }
    let mut intl_ids = HashSet::new();
    for acc in accounts.iter().filter(|a| a.region == "intl") {
        if let Ok(payload) = workbuddy_upstream::fetch_models(acc).await {
            intl_ids = ids_from_models_payload(&payload);
            break;
        }
    }
    if intl_ids.is_empty() {
        // 无 intl 账号或 intl 动态目录全失败：静态表兜底（§3.1）。
        intl_ids = workbuddy_upstream::intl_static_models()
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
            .filter(|s| !s.is_empty())
            .collect();
    }
    if cn_ids.is_empty() && intl_ids.is_empty() {
        return Err("区域目录构建失败：cn/intl 两区均无可判定的模型目录".to_string());
    }
    Ok(RegionCatalog {
        cn_ids,
        intl_ids,
        probed_at: now_ms(),
    })
}

/// 纯函数：fetch_models 的 OpenAI models list 载荷 → 模型 id 集（目录构建与测试共用）。
pub fn ids_from_models_payload(payload: &Value) -> HashSet<String> {
    payload
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 模型 → 区域解析（§3.1 契约签名）：hint 优先（§3.3①）；区域目录唯一命中 → 该区；
/// 两区同名或两边都没有 → 无约束；缓存不可用（TTL 内未成功）→ 无约束并写降级日志
/// （可用性优先于隔离；R1：TTL 内新上架模型同样落 Unknown，上游校验兜底）。
pub fn resolve_model_region(model: &str, hint: Option<&str>) -> RegionFilter {
    let catalog = region_catalog_cached();
    if catalog.is_none() && RegionFilter::from_hint(hint).is_none() {
        eprintln!(
            "[wb-gateway] region catalog unavailable（TTL 内未构建成功）→ {model} 区域解析降级为无约束（可用性优先）"
        );
    }
    resolve_model_region_with(catalog.as_ref(), model, hint)
}

/// 解析纯函数核心（目录注入，供无网络矩阵测试；无日志副作用）。
pub fn resolve_model_region_with(
    catalog: Option<&RegionCatalog>,
    model: &str,
    hint: Option<&str>,
) -> RegionFilter {
    if let Some(f) = RegionFilter::from_hint(hint) {
        return f;
    }
    let Some(c) = catalog else {
        return RegionFilter::Unknown;
    };
    match c.locate(model) {
        (true, false) => RegionFilter::Cn,
        (false, true) => RegionFilter::Intl,
        // 两区同名（glm-5.3）或两边都没有（全新模型）：不约束，上游校验兜底（§3.1③）。
        _ => RegionFilter::Unknown,
    }
}

// ---------------------------------------------------------------------------
// models 端点按区聚合纯函数（契约 §3.4；fetch 注入，IO 全在 api.rs 接线层）
// ---------------------------------------------------------------------------

/// 单区模型目录聚合结果：source 标记 + 动态目录来源账号 + 已带 region 字段的条目
/// （区内已按 id 去重）。
#[derive(Debug, Clone)]
pub struct RegionModels {
    pub region: &'static str,
    /// "dynamic"（该区账号动态目录）| "fallback"（cn 内置表回落）| "static"（intl 静态表回落）。
    pub source: &'static str,
    /// 动态目录来源账号 uid；回落为空串（载荷里不落 account 字段）。
    pub account: String,
    pub items: Vec<Value>,
}

/// 纯函数：单区目录聚合（§3.4）。`fetch` 注入：None = 该区无账号；
/// `Some((uid, Err))` = 该区账号全失败（uid 为末次尝试账号，供日志）。两区回落
/// source 语义不同（§3.4）：cn → "fallback"（builtin 内置表）、intl → "static"
/// （intl_static_models——「本机无国际版账号」是正常态，静态表显性化，R2）。
pub fn region_models_part(
    wanted: RegionFilter,
    fetch: Option<(String, Result<Value, String>)>,
    fallback_items: Vec<Value>,
) -> RegionModels {
    let region = match wanted {
        RegionFilter::Cn => "cn",
        _ => "intl",
    };
    let fallback_source = if region == "cn" { "fallback" } else { "static" };
    let (source, account, items) = match fetch {
        Some((uid, Ok(payload))) => {
            let data = payload
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let items = entries_with_region(&data, region);
            if items.is_empty() {
                // 动态目录空表按回落处理（fetch_models 本身会拒绝空列表，防御为主）。
                (fallback_source, String::new(), entries_with_region(&fallback_items, region))
            } else {
                ("dynamic", uid, items)
            }
        }
        _ => (fallback_source, String::new(), entries_with_region(&fallback_items, region)),
    };
    RegionModels {
        region,
        source,
        account,
        items,
    }
}

/// 纯函数：两区部分 → 最终 models 载荷（§3.4）。`only` 为单区过滤（?region=cn|intl，
/// Unknown = 两区全量）；`regions` 元数据只含纳入的区。**跨区同名（glm-5.3）保留
/// 两条**（region 字段区分）——「同名模型两区各有一套后端映射」的诚实表达；
/// 区内去重在 [`region_models_part`] 已完成。
pub fn assemble_region_models(cn: RegionModels, intl: RegionModels, only: RegionFilter) -> Value {
    let mut regions = serde_json::Map::new();
    let mut data = Vec::new();
    for part in [&cn, &intl] {
        let part_filter = if part.region == "cn" {
            RegionFilter::Cn
        } else {
            RegionFilter::Intl
        };
        if only != RegionFilter::Unknown && only != part_filter {
            continue;
        }
        let mut meta = serde_json::Map::new();
        meta.insert("source".to_string(), json!(part.source));
        if !part.account.is_empty() {
            meta.insert("account".to_string(), json!(part.account));
        }
        regions.insert(part.region.to_string(), Value::Object(meta));
        data.extend(part.items.iter().cloned());
    }
    json!({
        "object": "list",
        "regions": Value::Object(regions),
        "data": data,
    })
}

/// 条目集加 `region` 字段（区内按 id 去重、剔除空 id，保持原顺序）。
fn entries_with_region(items: &[Value], region: &str) -> Vec<Value> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for m in items {
        let id = m.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        let mut entry = m.clone();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("region".to_string(), json!(region));
        }
        out.push(entry);
    }
    out
}

/// 区域内逐账号尝试 `fetch_models`，首个成功即返回 `(uid, Ok)`；全败返回
/// 末个 `(uid, Err)`。无该区账号返回 `None`。
/// （自 api.rs `workbuddy_pool_fetch_region_models` 上移为 pub，供总览复用。）
pub async fn fetch_region_models(
    accounts: &[PoolAccount],
    region: &str,
) -> Option<(String, Result<Value, String>)> {
    let mut last: Option<(String, Result<Value, String>)> = None;
    for acc in accounts.iter().filter(|a| a.region == region) {
        match crate::modules::workbuddy_upstream::fetch_models(acc).await {
            Ok(v) => return Some((acc.uid.clone(), Ok(v))),
            Err(e) => last = Some((acc.uid.clone(), Err(e))),
        }
    }
    last
}

/// 总览用：两区目录聚合（条目带 region 字段，跨区同名各留一条；cn 回落内置表、
/// intl 回落静态表，与 `/workbuddy-pool/v1/models` 同口径，不含观测 note）。
/// 两区全空返回 Err（总览按 health=error 降级）。
pub async fn overview_models_catalog() -> Result<Vec<Value>, String> {
    let accounts = load_pool_accounts();
    let (cn_fetch, intl_fetch) = tokio::join!(
        fetch_region_models(&accounts, "cn"),
        fetch_region_models(&accounts, "intl"),
    );
    let builtin = crate::modules::workbuddy_proxy::builtin_models_payload();
    let builtin_items = builtin
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let intl_static = crate::modules::workbuddy_upstream::intl_static_models();
    let cn_part = region_models_part(RegionFilter::Cn, cn_fetch, builtin_items);
    let intl_part = region_models_part(RegionFilter::Intl, intl_fetch, intl_static);
    let payload = assemble_region_models(cn_part, intl_part, RegionFilter::Unknown);
    let items = payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        Err("池两区目录均为空（账号库无 WorkBuddy 账号或上游不可达）".to_string())
    } else {
        Ok(items)
    }
}

// ---------------------------------------------------------------------------
// 余额巡检（增补契约 §2.1）
// ---------------------------------------------------------------------------

/// 巡检单账号间隔（ms）：对上游计费 RPC 限速，避免突发（增补 §2.1）。
const PATROL_ACCOUNT_GAP_MS: u64 = 300;
/// 巡检首轮延迟（s）：启动后 60s 再做首轮（避开启动 / 登录同步高峰，增补 §2.1）。
const PATROL_FIRST_DELAY_SECS: u64 = 60;
/// 巡检关闭（配置 0）时的空转探测间隔（s）：循环不退出，按此间隔重读配置，
/// PATCH 改回非 0 后下一轮即恢复，无需重启进程。
const PATROL_IDLE_POLL_SECS: u64 = 60;

/// 巡检账号过滤纯计算核心：禁用（session 死亡出池）与账号库标记 error 的账号跳过。
/// 巡检是只读维护，对已知坏号发请求无谓；单账号请求失败本身不罚号（见
/// [`balance_patrol_once`] 的静默跳过）。
fn patrol_should_check(disabled: bool, errored: bool) -> bool {
    !disabled && !errored
}

/// 巡检间隔纯计算核心：`0 = 关闭`（返回 None）；其余钳到 [5, 24*60] 分钟后转 ms。
/// `load_config` 已做同款钳制，这里防御性重钳（配置可能来自未经 normalized 的来源）。
fn patrol_interval_ms(minutes: u64) -> Option<u64> {
    if minutes == 0 {
        return None;
    }
    Some(minutes.clamp(BALANCE_REFRESH_MIN_MINUTES, 24 * 60) * 60_000)
}

/// 账号库中标记 `status == "error"` 的 WorkBuddy uid 集合（纯计算核心）。
/// 口径对齐账号库错误标记约定（grok.rs:474 / grok_sso.rs:788）；非数组 / 损坏
/// 输入返回空集（巡检不因此中断，只放过）。
fn errored_uids_from_json(text: &str) -> HashSet<String> {
    let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(text) else {
        return HashSet::new();
    };
    arr.iter()
        .filter(|acc| {
            account_platform(acc) == PLATFORM_WORKBUDDY
                && acc.get("status").and_then(Value::as_str) == Some("error")
        })
        .filter_map(account_uid)
        .collect()
}

/// 读账号库取 error 标记 uid 集合（文件缺失 / 损坏 → 空集）。
fn patrol_errored_uids() -> HashSet<String> {
    std::fs::read_to_string(accounts_file())
        .ok()
        .map(|t| errored_uids_from_json(&t))
        .unwrap_or_default()
}

/// 对池内每个账号刷新余额与到期日（增补 §2.1：只读上游，不写 accounts.json、
/// 不改账号状态）。跳过禁用 / error 账号；单账号失败静默跳过（巡检失败 ≠
/// 账号故障，不罚号、不喂熔断）；账号间隔 300ms 防突发。
///
/// 返回 `{checked, updated, failed}`：checked = 实际请求过的账号数（跳过的不计），
/// updated = 回填成功数，failed = 静默失败数（checked = updated + failed）。
pub async fn balance_patrol_once() -> Value {
    let errored = patrol_errored_uids();
    // 快照在锁内完成（sync + 过滤 + clone），HTTP 期间不持锁。
    let targets = with_pool(|p| p.patrol_targets(&errored));
    let mut checked = 0i64;
    let mut updated = 0i64;
    let mut failed = 0i64;
    let mut errors: Vec<String> = Vec::new();
    for (i, acc) in targets.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(PATROL_ACCOUNT_GAP_MS)).await;
        }
        checked += 1;
        match workbuddy_upstream::user_resource(acc).await {
            // set_credits 内部 with_pool：sync + 回填 + flush（内存 + 状态文件）。
            Ok((credits, expires_at)) => {
                set_credits(&acc.uid, credits, expires_at);
                updated += 1;
            }
            // 不罚号、不记熔断（巡逻失败≠账号故障）；原因进 errors[] 供观测。
            Err(e) => {
                failed += 1;
                errors.push(format!("{}: {}", acc.uid.chars().take(8).collect::<String>(), e));
            }
        }
    }
    json!({
        "checked": checked,
        "updated": updated,
        "failed": failed,
        "errors": errors,
    })
}

/// 巡检循环（增补 §2.1）：启动后 60s 首轮，之后每 `balance_refresh_minutes` 分钟；
/// 配置为 0 = 关闭。间隔每轮现读（PATCH 热更新即写即生效，对齐 save_config 口径）。
pub fn spawn_balance_patrol_loop() {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(PATROL_FIRST_DELAY_SECS)).await;
        loop {
            match patrol_interval_ms(load_config().balance_refresh_minutes) {
                Some(interval) => {
                    let r = balance_patrol_once().await;
                    // 观测：走 gateway_admin 记录（eprintln 会被 launcher 的 stdio:ignore
                    // 吞掉，线上等于瞎子——2026-09-24 E2E 排查实测）。每轮一条汇总 +
                    // 每失败账号一条原因（failed>0 时），默认 15 分钟一轮不刷屏。
                    let g = |k: &str| r.get(k).and_then(Value::as_i64).unwrap_or(0);
                    crate::modules::gateway_admin::record_request(json!({
                        "gateway": "workbuddy-pool",
                        "kind": "balance_patrol",
                        "ok": g("failed") == 0,
                        "at": now_ms(),
                        "latencyMs": 0,
                        "error": format!(
                            "[wb-gateway] balance patrol: checked {}, updated {}, failed {}",
                            g("checked"), g("updated"), g("failed")
                        ),
                    }));
                    for e in r.get("errors").and_then(Value::as_array).cloned().unwrap_or_default() {
                        crate::modules::gateway_admin::record_request(json!({
                            "gateway": "workbuddy-pool",
                            "kind": "balance_patrol_error",
                            "ok": false,
                            "at": now_ms(),
                            "latencyMs": 0,
                            "error": e.as_str().unwrap_or("").to_string(),
                        }));
                    }
                    tokio::time::sleep(Duration::from_millis(interval)).await;
                }
                None => {
                    // 0 = 关闭：循环不退出（配置可 PATCH 热改回），空转重读配置。
                    tokio::time::sleep(Duration::from_secs(PATROL_IDLE_POLL_SECS)).await;
                }
            }
        }
    });
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
    fn reject_reason_matches_admission() {
        // 🔴 不变量：`pool_account_reject_reason` 判「能进池」⇔ `pool_account_from_value`
        // 返回 `Some`。两者是**同一套准入判据的两份表述**，必须恒等 ——
        // 否则诊断（`rejected_accounts`）会说谎，而它存在的理由恰恰是
        // 「导入成功但池里看不到、且没有任何线索」（2026-10-13 用户报障）。
        let cases = vec![
            // 合格：三个条件齐备
            json!({"uid":"u1","access_token":"at","domain":"www.workbuddy.cn"}),
            // 缺 access_token（导入文件多为脱敏导出）
            json!({"uid":"u2","domain":"www.workbuddy.cn"}),
            // 缺 uid
            json!({"access_token":"at","domain":"www.workbuddy.cn"}),
            // 非 WorkBuddy 平台（会被静默丢弃的典型）
            json!({"uid":"u3","access_token":"at","platform":"minimax"}),
            json!({"uid":"u7","access_token":"at","platform":"chatglm"}),
            json!({"uid":"u8","access_token":"at","platform":"autoclaw"}),
            // uid 走 auth_raw / id 回落 —— 都应算合格
            json!({"auth_raw":{"uid":"u4"},"access_token":"at"}),
            json!({"id":"u5","access_token":"at"}),
            // platform 缺失或未知 → 兜底 workbuddy，算合格
            json!({"uid":"u6","access_token":"at","platform":"totally-unknown"}),
        ];
        for c in cases {
            let admitted = pool_account_from_value(&c).is_some();
            let rejected = pool_account_reject_reason(&c).is_some();
            assert_eq!(
                admitted, !rejected,
                "两份准入判据不一致（admitted={admitted} rejected={rejected}）：{c}"
            );
        }

        // 明确断言几个关键分类，防止「两边一起错」
        assert!(pool_account_from_value(&json!({"uid":"u3","access_token":"at","platform":"minimax"})).is_none());
        assert!(pool_account_reject_reason(&json!({"uid":"u3","access_token":"at","platform":"minimax"})).is_some());
        assert!(pool_account_from_value(&json!({"uid":"u1","access_token":"at"})).is_some());
        assert!(pool_account_reject_reason(&json!({"uid":"u1","access_token":"at"})).is_none());
    }

    #[test]
    fn pool_status_healthy_count_never_drifts_from_entry_healthy() {
        // 🔴 不变量：`status_value()` 的 healthy 计数必须**恒等于**逐条 `Entry::healthy()`。
        //
        // 2026-10-13 实翻车：新增 `paused` 时只给 `Entry::healthy()` 加了闸门，
        // 池级计数仍在**重复推导旧条件**（`!disabled && !cooling && !breaker`）
        // ⇒ 暂停账号后仍上报「3/3 可用」，与账号级判定不一致
        // （界面说全可用，实际选号已排除该号）。
        //
        // 本用例即防该漂移复发：**将来再给 healthy 加任何新维度，这条会立刻红**。
        let mut p = mk_pool();
        p.upsert(mk_acc("aa-1"));
        p.upsert(mk_acc_region("bb-2", "intl"));

        let expect = |p: &Pool| -> i64 {
            let now = now_ms();
            p.entries.iter().filter(|e| e.healthy(now)).count() as i64
        };
        let reported = |p: &Pool| -> i64 {
            p.status_value()
                .get("healthy")
                .and_then(Value::as_i64)
                .unwrap_or(-1)
        };

        assert_eq!(reported(&p), expect(&p), "初始状态两口径须一致");
        assert_eq!(reported(&p), 2);

        // 暂停一个 → 池级计数必须跟着掉（而非仍报满值）
        p.set_paused("aa-1", true).unwrap();
        assert_eq!(reported(&p), expect(&p), "暂停后两口径须一致");
        assert_eq!(reported(&p), 1, "暂停必须体现在池级 healthy 上");

        // 恢复 → 回升
        p.set_paused("aa-1", false).unwrap();
        assert_eq!(reported(&p), expect(&p), "恢复后两口径须一致");
        assert_eq!(reported(&p), 2);

        // region 聚合同样必须一致（cn 区此刻 1/1）
        let st = p.status_value();
        let cn = st.get("regions").and_then(|r| r.get("cn")).cloned().unwrap_or(Value::Null);
        assert_eq!(
            cn.get("healthy").and_then(Value::as_i64),
            Some(1),
            "region 聚合的 healthy 口径须与池级一致"
        );
    }

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
            intl_proxy: " http://127.0.0.1:18502 ".to_string(),
        };
        save_config_at(&fp, &cfg).unwrap();
        let back = load_config_at(&fp);
        assert_eq!(back.mode, "rotate");
        assert_eq!(back.pinned_uid, "u1", "pinned_uid 应去空格");
        assert_eq!(
            back.intl_proxy, "http://127.0.0.1:18502",
            "intl_proxy 应去空格并往返一致（2026-10-12 intl 可达性修复）"
        );
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
        assert_eq!(
            c.balance_refresh_minutes, 0,
            "增补 §2.1：0 = 关闭巡检，不回落默认"
        );

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

    // -----------------------------------------------------------------------
    // 余额巡检（增补 §2.1；无网络：只测纯函数化核心与状态迁移）
    // -----------------------------------------------------------------------

    #[test]
    fn patrol_filters_disabled_and_errored_accounts() {
        // 纯函数核心：disabled / error 跳过，正常账号巡检。
        assert!(patrol_should_check(false, false), "正常账号应巡检");
        assert!(!patrol_should_check(true, false), "禁用号跳过");
        assert!(!patrol_should_check(false, true), "error 标记账号跳过");
        assert!(!patrol_should_check(true, true));

        // 池快照过滤：disabled 条目（session 死亡）与 errored uid 都不进 targets。
        let mut p = mk_pool();
        p.upsert(mk_acc("u-ok"));
        p.upsert(mk_acc("u-dead"));
        p.upsert(mk_acc("u-err"));
        p.note_error("u-dead", "", ErrKind::SessionDead, "");
        let errored: HashSet<String> = ["u-err".to_string()].into_iter().collect();
        let targets = p.patrol_targets(&errored);
        let uids: Vec<&str> = targets.iter().map(|a| a.uid.as_str()).collect();
        assert_eq!(uids, vec!["u-ok"], "只有正常账号进巡检: {uids:?}");
        // 无 error 标记 → 不过滤
        assert_eq!(p.patrol_targets(&HashSet::new()).len(), 2, "无 error 标记只滤 disabled");

        // error 标记来自账号库文本（uid 口径与桥接一致，只认 WorkBuddy 平台）。
        let set = errored_uids_from_json(
            &json!([
                {"id":"a","access_token":"at","status":"error","auth_raw":{"uid":"u-err"}},
                {"id":"b","access_token":"at","status":"ok","auth_raw":{"uid":"u-ok"}},
                {"id":"c","access_token":"at","domain":"www.workbuddy.cn","status":"error","uid":"u-top"},
                {"id":"d","platform":"autoclaw","access_token":"at","status":"error","auth_raw":{"uid":"u-other"}}
            ])
            .to_string(),
        );
        assert!(set.contains("u-err"), "status==error 的 WorkBuddy 账号进跳过集");
        assert!(!set.contains("u-ok"), "非 error 账号不进跳过集");
        assert!(set.contains("u-top"), "顶层 uid 口径兼容");
        assert!(!set.contains("u-other"), "非 WorkBuddy 平台账号不进跳过集");
        assert!(errored_uids_from_json("not json").is_empty(), "损坏输入 → 空集");
        assert!(errored_uids_from_json("{}").is_empty(), "非数组 → 空集");
    }

    #[test]
    fn patrol_interval_zero_disables_and_clamps() {
        // 增补 §2.1：0 = 关闭；其余钳 [5, 24*60] 分钟。
        assert_eq!(patrol_interval_ms(0), None, "0 = 关闭巡检");
        assert_eq!(patrol_interval_ms(15), Some(15 * 60_000));
        assert_eq!(patrol_interval_ms(5), Some(5 * 60_000), "下限 5 分钟");
        assert_eq!(patrol_interval_ms(3), Some(5 * 60_000), "<5 钳到下限");
        assert_eq!(patrol_interval_ms(99_999), Some(24 * 60 * 60_000), "封顶 24h");
        // load_config 的 normalized 同口径：0 保持 0（不回落默认 15），非 0 钳 [5, 1440]。
        let cfg = GatewayConfig {
            balance_refresh_minutes: 0,
            ..Default::default()
        }
        .normalized();
        assert_eq!(cfg.balance_refresh_minutes, 0, "0 = 关闭，不回落默认");
        let cfg = GatewayConfig {
            balance_refresh_minutes: 2,
            ..Default::default()
        }
        .normalized();
        assert_eq!(cfg.balance_refresh_minutes, 5, "非 0 钳到下限 5");
        let cfg = GatewayConfig {
            balance_refresh_minutes: 30,
            ..Default::default()
        }
        .normalized();
        assert_eq!(cfg.balance_refresh_minutes, 30, "合法值原样");
    }

    #[test]
    fn patrol_backfill_changes_pick_tier() {
        // 巡检回填前后选号分层的变化（到期分层键 = credit_expires_at 所在日）。
        let mut p = mk_pool();
        p.set_min_pick_gap_ms(0); // 关闭防撞号，走纯加权
        let mut poor = mk_acc("poor");
        poor.credits = 10;
        let mut rich = mk_acc("rich");
        rich.credits = 1000;
        p.upsert(poor);
        p.upsert(rich);
        // 恒抽签 0 → 取权重最高者
        p.set_random_source(Some(Box::new(|_| 0)));

        // 回填前：无到期信息 → 不分层，权重含 credits 项 → 高积分号优先。
        assert_eq!(p.pick("m", &[]).unwrap().uid, "rich", "回填前按 credits 加权选号");

        // 巡检回填：poor 的额度最早到期 → 到期分层生效，优先烧快过期的。
        let now = now_ms();
        p.set_credits("poor", 10, now + 2 * 86_400_000);
        p.set_credits("rich", 1000, now + 30 * 86_400_000);
        let mut counts: HashMap<String, i64> = HashMap::new();
        for _ in 0..20 {
            if let Some(a) = p.pick("m", &[]) {
                *counts.entry(a.uid).or_insert(0) += 1;
            }
        }
        assert_eq!(
            counts.get("poor").copied().unwrap_or(0),
            20,
            "回填后应固定选最早到期档: {counts:?}"
        );
        assert!(!counts.contains_key("rich"), "较晚档不应被选中: {counts:?}");

        // user_resource 可能只返回 credits（到期未知）：0 不清已有到期日。
        p.set_credits("poor", 999, 0);
        let e = &p.entries[*p.by_uid.get("poor").unwrap()];
        assert_eq!(e.acc.credits, 999);
        assert_eq!(
            e.acc.credit_expires_at,
            now + 2 * 86_400_000,
            "credit_expires_at 0 = 未知，不清到期日"
        );
    }

    // -----------------------------------------------------------------------
    // 区域隔离（workbuddy-pool-region 契约 §3.1/§3.2/§3.4；无网络：纯函数矩阵）
    // -----------------------------------------------------------------------

    /// 指定区域的池账号（mk_acc 默认 cn，intl 用本助手）。
    fn mk_acc_region(uid: &str, region: &str) -> PoolAccount {
        let mut a = mk_acc(uid);
        a.region = region.to_string();
        a
    }

    /// 解析矩阵测试目录：cn=deepseek-v4-flash + 同名 glm-5.3；intl=gpt-5.6-sol + 同名 glm-5.3
    /// （对照真实分布：同名模型两区各有一套后端映射，契约 §3.1③）。
    fn mk_catalog() -> RegionCatalog {
        RegionCatalog {
            cn_ids: ["deepseek-v4-flash".to_string(), "glm-5.3".to_string()]
                .into_iter()
                .collect(),
            intl_ids: ["gpt-5.6-sol".to_string(), "glm-5.3".to_string()]
                .into_iter()
                .collect(),
            probed_at: now_ms(),
        }
    }

    #[test]
    fn region_filter_from_hint_matrix() {
        // 显式 region 字段（§3.3①）：大小写/空白不敏感；缺席/非法 → None 交目录解析。
        assert_eq!(RegionFilter::from_hint(Some("cn")), Some(RegionFilter::Cn));
        assert_eq!(RegionFilter::from_hint(Some("intl")), Some(RegionFilter::Intl));
        assert_eq!(RegionFilter::from_hint(Some(" INTL ")), Some(RegionFilter::Intl));
        assert_eq!(RegionFilter::from_hint(Some("CN")), Some(RegionFilter::Cn));
        assert_eq!(RegionFilter::from_hint(Some("")), None, "空串 = 未提供");
        assert_eq!(RegionFilter::from_hint(Some("bogus")), None, "非法值不约束");
        assert_eq!(RegionFilter::from_hint(None), None);
        // wanted_region：日志/文案取值。
        assert_eq!(RegionFilter::Cn.wanted_region(), Some("cn"));
        assert_eq!(RegionFilter::Unknown.wanted_region(), None);
    }

    #[test]
    fn region_resolve_matrix() {
        let cat = mk_catalog();
        // 唯一命中 → 该区。
        assert_eq!(
            resolve_model_region_with(Some(&cat), "deepseek-v4-flash", None),
            RegionFilter::Cn,
            "cn 独有模型 → 唯一命中 cn"
        );
        assert_eq!(
            resolve_model_region_with(Some(&cat), "gpt-5.6-sol", None),
            RegionFilter::Intl,
            "intl 独有模型 → 唯一命中 intl"
        );
        // 两区同名 → 无约束（§3.1③：上游校验兜底）。
        assert_eq!(
            resolve_model_region_with(Some(&cat), "glm-5.3", None),
            RegionFilter::Unknown,
            "两区同名模型不约束"
        );
        // 两边都没有（全新模型）→ 无约束。
        assert_eq!(
            resolve_model_region_with(Some(&cat), "brand-new-model", None),
            RegionFilter::Unknown
        );
        // hint 覆盖目录：显式 region 优先（§3.3① > ②）。
        assert_eq!(
            resolve_model_region_with(Some(&cat), "deepseek-v4-flash", Some("intl")),
            RegionFilter::Intl,
            "hint 应覆盖目录唯一命中"
        );
        assert_eq!(
            resolve_model_region_with(Some(&cat), "brand-new-model", Some("cn")),
            RegionFilter::Cn,
            "hint 对目录未知模型仍生效"
        );
        // 缓存不可用（None）→ 无约束（降级路径，§3.1）。
        assert_eq!(
            resolve_model_region_with(None, "deepseek-v4-flash", None),
            RegionFilter::Unknown,
            "目录不可用 → 无约束（可用性优先）"
        );
        // 目录不可用但 hint 在 → 仍按 hint 约束。
        assert_eq!(resolve_model_region_with(None, "m", Some("intl")), RegionFilter::Intl);
        // 空目录（两区 id 集全空）→ 一律无约束。
        let empty = RegionCatalog::default();
        assert_eq!(resolve_model_region_with(Some(&empty), "m", None), RegionFilter::Unknown);
    }

    #[test]
    fn region_catalog_cache_ttl_and_global_resolve() {
        // 全局静态缓存（进程级）：本测试是测试模块里唯一触碰 REGION_CATALOG_CACHE 的
        // 用例（无并行竞争），结束前复位防止污染。
        *REGION_CATALOG_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        assert_eq!(
            resolve_model_region("deepseek-v4-flash", None),
            RegionFilter::Unknown,
            "缓存未建 → 无约束（TTL 内未成功，§3.1）"
        );

        let fresh = mk_catalog();
        *REGION_CATALOG_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(fresh.clone());
        assert_eq!(
            resolve_model_region("gpt-5.6-sol", None),
            RegionFilter::Intl,
            "TTL 内缓存生效：intl 独有模型命中 intl"
        );
        assert_eq!(region_catalog_cached(), Some(fresh), "快照与缓存一致");

        // 过期（TTL 外）→ 视为不可用 → 无约束。
        let stale = RegionCatalog {
            probed_at: now_ms() - REGION_CATALOG_TTL_MS - 1,
            ..mk_catalog()
        };
        *REGION_CATALOG_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(stale);
        assert_eq!(
            resolve_model_region("deepseek-v4-flash", None),
            RegionFilter::Unknown,
            "过期缓存视为不可用"
        );
        // hint 不依赖目录：缓存过期仍按 hint 约束。
        assert_eq!(
            resolve_model_region("deepseek-v4-flash", Some("cn")),
            RegionFilter::Cn,
            "显式 region 不受缓存可用性影响"
        );

        *REGION_CATALOG_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        assert_eq!(region_catalog_cached(), None, "复位后不可见");
    }

    #[test]
    fn pick_region_filter_matrix() {
        // cn a + intl b：区域约束把候选切成两个互不相交的集合。
        let mut p = mk_pool();
        p.upsert(mk_acc("a-cn"));
        p.upsert(mk_acc_region("b-intl", "intl"));
        assert_eq!(
            p.pick_account_for_region("m", &[], RegionFilter::Cn).unwrap().uid,
            "a-cn",
            "cn 约束只出 cn 号"
        );
        assert_eq!(
            p.pick_account_for_region("m", &[], RegionFilter::Intl).unwrap().uid,
            "b-intl",
            "intl 约束只出 intl 号"
        );
        // Unknown：两区都可选（选中号必属其一）。
        let uid = p
            .pick_account_for_region("m", &[], RegionFilter::Unknown)
            .unwrap()
            .uid;
        assert!(uid == "a-cn" || uid == "b-intl");
        // tried 与区域约束叠加。
        assert!(
            p.pick_account_for_region("m", &["a-cn".to_string()], RegionFilter::Cn).is_none(),
            "cn 区唯一账号进 tried → 无候选"
        );
        // 旧签名等价性：pick_account == for_region(Unknown)。
        assert_eq!(
            p.pick_account("m", &["b-intl".to_string()]).unwrap().uid,
            "a-cn",
            "旧签名排除 intl 号（tried）后选 cn"
        );
    }

    #[test]
    fn clear_cooldown_clears_cooling_but_never_disabled_or_paused() {
        // 🔴 固化：清冷却只动**冷却类**运行态，绝不碰 `disabled`（凭证失效的事实）
        // 与 `paused`（用户意图）。伪造凭证有效会让请求立刻再失败；
        // 替用户改主意则违背他的显式选择。
        let mut p = mk_pool();
        let uid = "aaaa1111-bbbb-2222-cccc-333344445555";
        p.upsert(mk_acc(uid));
        let idx = *p.by_uid.get(uid).unwrap();

        // 造出「各种冷却 + 用户暂停 + 系统出池」的复合状态
        p.set_cooldown(idx, CoolKind::Soft, now_ms() + 600_000, "429 rate limit", false);
        p.entries[idx].breaker_until_ms = now_ms() + 900_000;
        p.entries[idx].fails = 3;
        p.entries[idx].soft_fails = 2;
        p.cooldown_model(idx, "m", now_ms() + 600_000, "m 已达频率上限", true);
        p.set_paused(uid, true).unwrap();
        p.entries[idx].disabled = true;
        p.entries[idx].reason = "12153 session dead".to_string();

        let resolved = p.clear_cooldown(uid).expect("应能按 uid 解析");
        assert_eq!(resolved, uid);

        let e = &p.entries[idx];
        // 冷却类应全部清空
        assert_eq!(e.until_ms, 0, "账号级冷却应清");
        assert!(e.cool_kind.is_none(), "冷却类型应清");
        assert_eq!(e.breaker_until_ms, 0, "熔断应清");
        assert_eq!(e.fails, 0, "熔断计数应清");
        assert_eq!(e.soft_fails, 0, "软限流计数应清");
        assert!(e.model_cools.is_empty(), "模型级冷却应清");

        // ✅ 两条不可动的状态
        assert!(e.disabled, "清冷却绝不能让凭证失效的账号复活");
        assert!(e.paused, "清冷却绝不能不问自取地替用户恢复");
        assert_eq!(
            e.reason, "12153 session dead",
            "出池原因描述的是凭证事实，清冷却不得抹掉"
        );

        // 非出池账号：冷却备注随冷却一起清（避免界面显示已不存在的旧原因）
        let mut p2 = mk_pool();
        p2.upsert(mk_acc("bb-2"));
        let i2 = *p2.by_uid.get("bb-2").unwrap();
        p2.set_cooldown(i2, CoolKind::Soft, now_ms() + 1000, "429 rate limit", false);
        p2.clear_cooldown("bb-2").unwrap();
        assert!(p2.entries[i2].reason.is_empty(), "非出池账号的冷却备注应随冷却清掉");

        // 前缀歧义 / 不存在 → 拒绝，且不得改动任何账号
        let mut p3 = mk_pool();
        p3.upsert(mk_acc("cccc-1"));
        p3.upsert(mk_acc("cccc-2"));
        assert!(p3.clear_cooldown("cccc").is_err(), "歧义前缀必须拒绝");
        assert!(p3.clear_cooldown("nosuch").is_err());
    }

    #[test]
    fn user_pause_is_separate_from_system_disabled() {
        // 🔴 固化「用户暂停(`paused`)」与「系统禁用(`disabled`)」的**严格分离**：
        // 两者都让账号退出选号，但恢复路径不同（暂停一个开关即可，禁用需重登）。
        // 混用会导致「用户手动停用 → 被误报成 session 失效 → 让人去重登」。
        let mut p = mk_pool();
        let uid = "cbea9317-a9df-4e81-809b-1162a4e6be40";
        p.upsert(mk_acc(uid));
        assert!(p.pick_account_for_region("m", &[], RegionFilter::Unknown).is_some());

        // 用**脱敏形式**（前 8 位 + …，即 status 接口给前端的那个值）暂停
        let resolved = p.set_paused("cbea9317…", true).expect("脱敏前缀应能解析");
        assert_eq!(resolved, uid);
        assert!(p.entries[0].paused);
        assert!(!p.entries[0].disabled, "暂停绝不能写成 disabled");
        assert!(!p.entries[0].healthy(now_ms()));
        assert!(
            p.pick_account_for_region("m", &[], RegionFilter::Unknown).is_none(),
            "暂停后不得参与选号"
        );

        // 暂停期间叠加一个模型冷却 → 恢复后该冷却**必须仍在**
        let idx = *p.by_uid.get(uid).unwrap();
        p.cooldown_model(idx, "m", now_ms() + 600_000, "m 已达频率上限", true);
        p.set_paused(uid, false).unwrap();
        assert!(!p.entries[0].paused);
        assert!(
            p.pick_account_for_region("other-model", &[], RegionFilter::Unknown).is_some(),
            "恢复后其他模型应可选"
        );
        assert!(
            p.pick_account_for_region("m", &[], RegionFilter::Unknown).is_none(),
            "恢复不得顺手清掉模型冷却——那会把限流问题重新引爆"
        );

        // 前缀歧义 → 拒绝（不猜，避免误停用别的账号）
        let mut p2 = mk_pool();
        p2.upsert(mk_acc("aaaa1111-one"));
        p2.upsert(mk_acc("aaaa1111-two"));
        assert!(p2.set_paused("aaaa1111", true).is_err(), "歧义前缀必须拒绝");
        // 不存在的 uid
        assert!(p2.set_paused("nosuch-uid", true).is_err());
        // 空值
        assert!(p2.set_paused("   ", true).is_err());
        // 命中歧义时账号状态不得被改动
        assert!(p2.entries.iter().all(|e| !e.paused));
    }

    #[test]
    fn region_block_reasons_surface_model_cool_not_region_down() {
        // 🔴 2026-10-13 用户报障场景：intl 区**唯一**账号对 deepseek-v4.1-flash
        // 达上游频率上限。客户端只看到「区内无可用账号」⇒ 误判成整区/网关故障。
        // 本诊断必须**指名原因**，且必须证明「只限该模型」（同账号其他模型仍可用），
        // 否则调用方会做出「整区不可用」的错误处置。
        let mut p = mk_pool();
        p.upsert(mk_acc_region("b-intl", "intl"));
        let idx = *p.by_uid.get("b-intl").unwrap();

        // 健康时：可选中，且诊断为空（健康账号不该出现在诊断里，否则误导）。
        assert!(
            p.pick_account_for_region("m", &[], RegionFilter::Intl).is_some(),
            "健康 intl 账号应可选中"
        );
        assert!(
            p.region_block_reasons("m", RegionFilter::Intl).is_empty(),
            "健康账号不得出现在诊断里"
        );

        // 给 m 打模型级冷却（未过期）→ 选不中，诊断须给出上游原因与账号标识。
        p.cooldown_model(
            idx,
            "m",
            now_ms() + 600_000,
            "m 已达频率上限，10-13 22:59 重置",
            true,
        );
        assert!(
            p.pick_account_for_region("m", &[], RegionFilter::Intl).is_none(),
            "模型冷却中应选不中"
        );
        let reasons = p.region_block_reasons("m", RegionFilter::Intl);
        assert_eq!(reasons.len(), 1, "intl 区仅 1 个账号，诊断应恰为 1 条：{reasons:?}");
        assert!(reasons[0].contains("已达频率上限"), "须带上游原因：{reasons:?}");
        assert!(reasons[0].contains("22:59"), "须带重置时间：{reasons:?}");
        assert!(reasons[0].contains("nick-b-intl"), "须带账号标识：{reasons:?}");

        // 区域隔离：诊断只看 wanted 区，绝不把 cn 区账号混进来。
        p.upsert(mk_acc("a-cn"));
        assert_eq!(
            p.region_block_reasons("m", RegionFilter::Intl).len(),
            1,
            "cn 账号不进 intl 区诊断"
        );

        // 🎯 核心性质：模型级冷却**只限该模型** —— 其他模型照常可选、诊断为空。
        assert!(
            p.pick_account_for_region("other-model", &[], RegionFilter::Intl).is_some(),
            "模型冷却不得波及同账号其他模型"
        );
        assert!(
            p.region_block_reasons("other-model", RegionFilter::Intl).is_empty(),
            "其他模型下不该有阻塞原因"
        );
    }

    #[test]
    fn pick_region_rejects_missing_region() {
        // G4 前提：池内只有 cn 账号时，intl 约束选不到号（返回 None，
        // 由 dispatch 报「intl 区无可用账号」，绝不误用 cn 号打上游）。
        let mut p = mk_pool();
        p.upsert(mk_acc("a-cn"));
        assert!(p.pick_account_for_region("m", &[], RegionFilter::Intl).is_none());
        assert!(p.pick_account_for_region("m", &[], RegionFilter::Cn).is_some());
    }

    #[test]
    fn pick_region_fallback_respects_region() {
        // 全冷却兜底路径的区域过滤（§3.2）：cn 区全员软冷却时，cn 约束经兜底选回
        // 软冷却 cn 号；intl 约束不受 cn 冷却影响选 intl 号；对侧健康号绝不被
        // 借来硬塞。
        let mut p = mk_pool();
        p.upsert(mk_acc("a-cn"));
        p.upsert(mk_acc_region("b-intl", "intl"));
        p.note_error("a-cn", "", ErrKind::SoftRate, "");
        assert_eq!(
            p.pick_account_for_region("m", &[], RegionFilter::Cn).unwrap().uid,
            "a-cn",
            "cn 全冷却 → 兜底选软冷却 cn 号（区域内）"
        );
        assert_eq!(
            p.pick_account_for_region("m", &[], RegionFilter::Intl).unwrap().uid,
            "b-intl",
            "intl 约束无视 cn 冷却选 intl"
        );

        // 对比：cn 区唯一账号硬冷却（余额耗尽）→ cn 约束兜底也拿不到 → None。
        let mut p2 = mk_pool();
        p2.upsert(mk_acc("a-cn"));
        p2.upsert(mk_acc_region("b-intl", "intl"));
        p2.note_error("a-cn", "", ErrKind::HardCredit, "");
        assert!(
            p2.pick_account_for_region("m", &[], RegionFilter::Cn).is_none(),
            "硬冷却不参与兜底，且不得借 intl 号"
        );
        assert_eq!(
            p2.pick_account_for_region("m", &[], RegionFilter::Intl).unwrap().uid,
            "b-intl"
        );
        // Unknown 维持旧行为：硬冷却排除后兜底软冷却/熔断号或健康号均可。
        assert!(p2.pick_account("m", &[]).is_some(), "无约束时 intl 号可选");
    }

    #[test]
    fn pick_region_modes_pinned_rotate_mismatch_falls_back() {
        // pinned：指定号区域与 wanted 不匹配 → 跳过 pick_by_uid 回落区域化 pick
        // （§3.2：不硬塞跨区请求）。
        let cfg = GatewayConfig {
            mode: "pinned".to_string(),
            pinned_uid: "b-intl".to_string(),
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("a-cn"));
        p.upsert(mk_acc_region("b-intl", "intl"));
        assert_eq!(
            p.pick_account_for_region("m", &[], RegionFilter::Cn).unwrap().uid,
            "a-cn",
            "pinned 是 intl 号 → cn 请求跳过指定号"
        );
        assert_eq!(
            p.pick_account_for_region("m", &[], RegionFilter::Intl).unwrap().uid,
            "b-intl",
            "pinned 区域匹配时优先指定号"
        );
        // Unknown 维持 pinned 原语义。
        assert_eq!(p.pick_account("m", &[]).unwrap().uid, "b-intl");

        // rotate：当前号区域不匹配 → 换区域内账号并推进 rotate_uid。
        let cfg = GatewayConfig {
            mode: "rotate".to_string(),
            ..Default::default()
        };
        let mut p = Pool::new(PathBuf::new(), &cfg);
        p.upsert(mk_acc("a-cn"));
        p.upsert(mk_acc_region("b-intl", "intl"));
        p.set_random_source(Some(Box::new(|_| 0)));
        let first = p.pick_account("m", &[]).unwrap().uid; // 烧号起步
        let second = p
            .pick_account_for_region("m", &[], RegionFilter::Intl)
            .unwrap()
            .uid;
        assert_ne!(first, second, "区域约束应把烧号轮转到 intl 号");
        assert_eq!(p.rotate_uid, second, "rotate_uid 跟随区域化选号");
        assert_eq!(
            p.pick_account_for_region("m", &[], RegionFilter::Intl).unwrap().uid,
            second,
            "intl 请求粘在 intl 烧号上"
        );
    }

    #[test]
    fn region_flip_matrix() {
        // 已是明确约束 → 取对侧。
        assert_eq!(region_flip(RegionFilter::Cn, "cn"), RegionFilter::Intl);
        assert_eq!(region_flip(RegionFilter::Intl, "intl"), RegionFilter::Cn);
        // 无约束 → 从本轮实际使用的 acc.region 反推对侧（§3.3）。
        assert_eq!(region_flip(RegionFilter::Unknown, "cn"), RegionFilter::Intl);
        assert_eq!(region_flip(RegionFilter::Unknown, "intl"), RegionFilter::Cn);
        // used_region 大小写/空白不敏感。
        assert_eq!(region_flip(RegionFilter::Unknown, " INTL "), RegionFilter::Cn);
        // 未知区域串按非 intl 处理（与 is_intl 口径一致）。
        assert_eq!(region_flip(RegionFilter::Unknown, ""), RegionFilter::Intl);
    }

    #[test]
    fn sticky_region_mismatch_matrix() {
        // T4：wanted 有约束且粘性号区域不满足 → 解绑。
        assert!(!sticky_region_mismatch(Some("cn"), RegionFilter::Cn), "区域匹配不解绑");
        assert!(!sticky_region_mismatch(Some("intl"), RegionFilter::Intl));
        assert!(sticky_region_mismatch(Some("intl"), RegionFilter::Cn), "跨区 → 解绑");
        assert!(sticky_region_mismatch(Some("cn"), RegionFilter::Intl));
        assert!(
            sticky_region_mismatch(None, RegionFilter::Cn),
            "粘性号已不在账号库 → 解绑重选"
        );
        // wanted 无约束：粘性区域永远不算不匹配（维持现状）。
        assert!(!sticky_region_mismatch(Some("cn"), RegionFilter::Unknown));
        assert!(!sticky_region_mismatch(None, RegionFilter::Unknown));
    }

    #[test]
    fn ids_from_models_payload_extracts_ids() {
        let payload = json!({"object":"list","data":[
            {"id":"m1","object":"model"},
            {"id":"m2"},
            {"id":"m1"},          // 重复 id → 集合自然去重
            {"id":""},            // 空 id 剔除
            {"object":"model"}    // 无 id 剔除
        ]});
        let ids = ids_from_models_payload(&payload);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("m1") && ids.contains("m2"));
        // 非 list 载荷 → 空集（防御）。
        assert!(ids_from_models_payload(&json!({})).is_empty());
    }

    #[test]
    fn region_models_part_dynamic_tags_region_and_dedups() {
        // 动态目录：条目加 region 字段；区内按 id 去重。
        let payload = json!({"object":"list","data":[
            {"id":"glm-5.3","object":"model","owned_by":"workbuddy"},
            {"id":"deepseek-v4-flash","object":"model"},
            {"id":"glm-5.3","object":"model"}   // 区内重复 → 只保留一条
        ]});
        let part = region_models_part(
            RegionFilter::Cn,
            Some(("uid-cn-1".to_string(), Ok(payload))),
            vec![json!({"id":"builtin-fallback"})],
        );
        assert_eq!(part.region, "cn");
        assert_eq!(part.source, "dynamic");
        assert_eq!(part.account, "uid-cn-1");
        assert_eq!(part.items.len(), 2, "区内重复 id 去重");
        assert!(
            part.items.iter().all(|m| m["region"] == "cn"),
            "每条目必须带 region=cn"
        );
        assert_eq!(part.items[0]["owned_by"], "workbuddy", "原有字段保留");
    }

    #[test]
    fn region_models_part_fallback_sources() {
        // cn 全失败 → builtin 回落，source=fallback（§3.4）。
        let cn_fallback = vec![
            json!({"id":"builtin-1","object":"model"}),
            json!({"id":"builtin-2","object":"model"}),
        ];
        let cn = region_models_part(
            RegionFilter::Cn,
            Some(("uid-cn-1".to_string(), Err("models api status 401".to_string()))),
            cn_fallback,
        );
        assert_eq!(cn.source, "fallback");
        assert!(cn.account.is_empty(), "回落不落 account 字段");
        assert_eq!(cn.items.len(), 2);
        assert!(cn.items.iter().all(|m| m["region"] == "cn"));

        // intl 无账号（None）→ 静态表回落，source=static（§五-6 正常态）。
        let intl_static = workbuddy_upstream::intl_static_models();
        let intl = region_models_part(RegionFilter::Intl, None, intl_static.clone());
        assert_eq!(intl.source, "static");
        assert_eq!(intl.items.len(), intl_static.len(), "静态表条目全量带出");
        assert!(intl.items.iter().all(|m| m["region"] == "intl"));
        assert_eq!(intl.items[0]["owned_by"], "workbuddy-intl", "静态表来源标记保留");

        // 动态 200 但 data 为空 → 同样按回落处理。
        let empty = region_models_part(
            RegionFilter::Cn,
            Some(("uid".to_string(), Ok(json!({"object":"list","data":[]})))),
            vec![json!({"id":"builtin"})],
        );
        assert_eq!(empty.source, "fallback", "空动态目录按回落");
    }

    #[test]
    fn assemble_region_models_cross_region_and_filter() {
        // 跨区同名（glm-5.3）保留两条（§3.4：region 字段区分，不跨区去重）。
        let cn = region_models_part(
            RegionFilter::Cn,
            Some(("uid-cn".to_string(), Ok(json!({"object":"list","data":[
                {"id":"glm-5.3"},{"id":"deepseek-v4-flash"}
            ]})))),
            vec![],
        );
        let intl = region_models_part(
            RegionFilter::Intl,
            Some(("uid-intl".to_string(), Ok(json!({"object":"list","data":[
                {"id":"glm-5.3"},{"id":"gpt-5.6-sol"}
            ]})))),
            vec![],
        );
        let all = assemble_region_models(cn.clone(), intl.clone(), RegionFilter::Unknown);
        let data = all["data"].as_array().unwrap();
        assert_eq!(data.len(), 4, "跨区同名保留两条，两区共 4 条");
        assert_eq!(
            data.iter().filter(|m| m["id"] == "glm-5.3").count(),
            2,
            "glm-5.3 两区各一条"
        );
        assert_eq!(all["object"], "list", "OpenAI list 形状保持");
        assert_eq!(all["regions"]["cn"]["source"], "dynamic");
        assert_eq!(all["regions"]["cn"]["account"], "uid-cn");
        assert_eq!(all["regions"]["intl"]["source"], "dynamic");
        assert_eq!(all["regions"]["intl"]["account"], "uid-intl");

        // ?region=cn → 只含 cn 条目，regions 元数据只含 cn。
        let only_cn = assemble_region_models(cn.clone(), intl.clone(), RegionFilter::Cn);
        let data = only_cn["data"].as_array().unwrap();
        assert!(data.iter().all(|m| m["region"] == "cn"), "?region=cn 只出 cn 条目");
        assert_eq!(data.len(), 2);
        assert!(only_cn["regions"].get("intl").is_none(), "过滤后不落 intl 元数据");

        // ?region=intl 同理。
        let only_intl = assemble_region_models(cn, intl, RegionFilter::Intl);
        let data = only_intl["data"].as_array().unwrap();
        assert!(data.iter().all(|m| m["region"] == "intl"));
        assert!(only_intl["regions"].get("cn").is_none());
    }
}
