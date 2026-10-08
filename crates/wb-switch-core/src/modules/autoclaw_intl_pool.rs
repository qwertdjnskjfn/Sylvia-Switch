//! AutoClaw 国际号轮换账号池（M1：状态机与存储）。
//!
//! 契约唯一事实来源：`docs/autoclaw-intl-pool-contract.md`（§2.2 数据模型、
//! §3.1-3.4 关键流程、§4 风控边界）。本模块只管 region=international：
//!
//! - 轮换状态落盘 `~/.wb-switch/autoclaw_intl_pool.json`（新文件，与凭据
//!   `autoclaw_accounts.json` 分离：高频回写不碰凭据文件）；
//! - PoolManager 内存单例（OnceLock + std Mutex）：锁内决策并 clone 数据、
//!   锁外 IO，MutexGuard 绝不跨 await（kimi.rs 教训）；落盘事件驱动 +
//!   debounce 合并写（tokio spawn，默认 2s）；
//! - acquire/release 两段式 lease：数据面消费方（glm_proxy/glm_anthropic
//!   国际 lane，M2 接入）按 §3.2 六重过滤 + 四种策略取号，按 §3.3 错误族
//!   回写结果（2xx / 429 冷却退避 / 401 续期 / 410004 banned / 810001
//!   quota-out 零点复活）；
//! - TokenKeeper（§3.4）续期实现（M3）：到期扫描（15min）与 401 即时触发都
//!   走注入的回调，续期复用 `autoclaw_accounts::refresh_stored_account`
//!   （按 region 路由到国际网关 autoglm-api.autoglm.ai；intl 端点形态为契约
//!   §7 同构假设待实测，见 refresh_credentials 注释）；
//! - watch 订阅（§3.7）：轮询读 autoclaw_watch.json 探测结果，把 intl 号的
//!   banned/invalid 单向映射进轮换状态机（watch 不感知池存在，零侵入）。
//!
//! 测试纪律：状态机纯逻辑（PoolState，now 由参数注入）不碰磁盘；持久化
//! 测试用临时目录注入存储路径，禁止写真实 `~/.wb-switch`。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::modules::config::{atomic_write, now_secs, store_dir};
use crate::modules::notify;

// ---------------------------------------------------------------------------
// 常量（契约 §2.2 / §3.2-3.4 / §4）
// ---------------------------------------------------------------------------

/// 轮换状态文件（契约 §2.2；与凭据 autoclaw_accounts.json 分离）。
const POOL_FILE: &str = "autoclaw_intl_pool.json";
/// events 环形上限（契约 §2.2：100 条）。
const EVENTS_CAP: usize = 100;
/// 落盘 debounce：事件驱动合并写（契约 §2.2 读写纪律：debounce 2s）。
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
/// 选号过滤：token 剩余寿命须 > 300s（契约 §3.2 过滤 3）。
const TOKEN_MIN_TTL_SECS: i64 = 300;
/// TokenKeeper 扫描间隔 15 分钟（契约 §3.4）。
const TOKEN_SCAN_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// token 剩余 < 2h 触发续期（契约 §3.4）。
const TOKEN_REFRESH_AHEAD_SECS: i64 = 2 * 3600;
/// 冷却退避基值 60s × 1.5^n（契约 §3.3）。
const COOLDOWN_BASE_SECS: f64 = 60.0;
/// 冷却退避帽 15 分钟（契约 §3.3）。
const COOLDOWN_CAP_SECS: i64 = 15 * 60;
/// 续期连续失败 3 次 → invalid 摘出轮换（契约 §3.4）。
const REFRESH_FAIL_LIMIT: u32 = 3;
/// watch 订阅轮询间隔（契约 §3.7：watch 每 30min 探测一轮，订阅侧 60s 轮询
/// 其落盘结果即可准实时跟进，且不增加任何上游探测流量）。
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// stats.byDay 最多保留天数（契约未约束；防止文件随日期无限膨胀，实现自定）。
const BY_DAY_KEEP: usize = 30;

// ---------------------------------------------------------------------------
// 数据模型（契约 §2.2，serde camelCase 与文档字段一一对应）
// ---------------------------------------------------------------------------

/// 轮换策略（契约 §2.2 policy.strategy）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    RoundRobin,
    Random,
    LeastUsed,
    Sticky,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::RoundRobin => "round-robin",
            Strategy::Random => "random",
            Strategy::LeastUsed => "least-used",
            Strategy::Sticky => "sticky",
        }
    }

    /// 解析策略名（契约 §2.2 取值；含下划线等常见变体）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "round-robin" | "round_robin" | "roundrobin" | "rr" => Some(Strategy::RoundRobin),
            "random" => Some(Strategy::Random),
            "least-used" | "least_used" | "leastused" => Some(Strategy::LeastUsed),
            "sticky" => Some(Strategy::Sticky),
            _ => None,
        }
    }
}

/// 单号运行态（契约 §2.2 accounts.<id>）。inFlight 仅内存态，落盘时清零。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountRuntime {
    pub enabled: bool,
    pub weight: u32,
    /// 仅内存态：落盘清零、加载清零（契约 §2.2）。
    pub in_flight: u32,
    pub today_calls: u32,
    /// 今日计数的本地日期（YYYY-MM-DD），跨零点重置的比对键（契约 §3.3）。
    /// 同时是 quota-out 复活判断的重置点参照。
    pub today_date: String,
    pub total_calls: u64,
    pub consecutive_fails: u32,
    /// 429/繁忙 冷却到期（秒级时间戳）。
    pub cooldown_until: i64,
    #[serde(deserialize_with = "deserialize_state_lenient")]
    pub state: AccountState,
    /// 410004 上游原始 code（证据保留，契约 §2.2）。
    pub ban_code: Option<Value>,
    pub banned_at: Option<i64>,
    pub token_expire_at: Option<i64>,
    pub last_refresh_at: Option<i64>,
    pub refresh_fails: u32,
    pub last_used_at: Option<i64>,
    pub last_error: Option<String>,
}

impl Default for AccountRuntime {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: 1,
            in_flight: 0,
            today_calls: 0,
            today_date: String::new(),
            total_calls: 0,
            consecutive_fails: 0,
            cooldown_until: 0,
            state: AccountState::Active,
            ban_code: None,
            banned_at: None,
            token_expire_at: None,
            last_refresh_at: None,
            refresh_fails: 0,
            last_used_at: None,
            last_error: None,
        }
    }
}

/// 单号状态（契约 §2.2 state 枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountState {
    Active,
    QuotaOut,
    Cooldown,
    Invalid,
    Banned,
    Disabled,
}

impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountState::Active => "active",
            AccountState::QuotaOut => "quota-out",
            AccountState::Cooldown => "cooldown",
            AccountState::Invalid => "invalid",
            AccountState::Banned => "banned",
            AccountState::Disabled => "disabled",
        }
    }

    /// 解析状态串（契约 §2.2 state 枚举取值）。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(AccountState::Active),
            "quota-out" => Some(AccountState::QuotaOut),
            "cooldown" => Some(AccountState::Cooldown),
            "invalid" => Some(AccountState::Invalid),
            "banned" => Some(AccountState::Banned),
            "disabled" => Some(AccountState::Disabled),
            _ => None,
        }
    }
}

/// 轮换策略参数（契约 §2.2 policy；默认值与文档一致）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PoolPolicy {
    #[serde(deserialize_with = "deserialize_strategy_lenient")]
    pub strategy: Strategy,
    /// 单号每分钟调用上限（风控参数，勿随意调大）。M1 仅落盘/热更新，
    /// 限速执行点在 M2 数据面接入时实现（契约 §3.2 六重过滤未含此项）。
    pub per_account_rate_per_min: u32,
    pub per_account_daily_cap: u32,
    pub pool_concurrent_cap: u32,
    pub account_concurrent_cap: u32,
    /// 810001 当日额度的重置时刻（本地时区小时）。
    pub quota_reset_hour: u32,
    pub auto_replace: bool,
    pub min_healthy: u32,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            strategy: Strategy::RoundRobin,
            per_account_rate_per_min: 12,
            per_account_daily_cap: 6000,
            pool_concurrent_cap: 4,
            account_concurrent_cap: 2,
            quota_reset_hour: 0,
            auto_replace: true,
            min_healthy: 2,
        }
    }
}

/// 游标（契约 §2.2 cursor）：round-robin 与 sticky 兜底的选择基准。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cursor {
    pub lane: String,
    pub offset: u64,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            lane: "international".to_string(),
            offset: 0,
        }
    }
}

/// 轮换事件（契约 §2.2 events，环形 100 条）。
/// kind: join | rotate | cooldown | ban | quota | refresh | replace | disable
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PoolEvent {
    pub ts: i64,
    pub kind: String,
    pub account_id: String,
    pub detail: String,
}

/// 累计统计（契约 §2.2 stats）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PoolStats {
    pub rotations: u64,
    pub replacements: u64,
    pub refreshes: u64,
    /// 本地日期 → 调用数。
    pub by_day: HashMap<String, u64>,
}

/// 轮换状态机（内存态；持久化经 PoolManager 的 debounce 写）。
/// 纯逻辑、now 由参数注入 —— 单测不碰磁盘、完全确定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolState {
    pub policy: PoolPolicy,
    pub cursor: Cursor,
    /// BTreeMap 保证遍历序稳定（选号/落盘/rows 可复现）。
    pub accounts: BTreeMap<String, AccountRuntime>,
    pub events: Vec<PoolEvent>,
    pub stats: PoolStats,
    /// 在途 lease 登记（lease_id → account_id）：release 幂等去重。
    /// 仅内存态，不落盘。
    #[serde(skip)]
    pub leases: HashMap<String, String>,
    /// random 策略的轻量随机源（xorshift；避免引入 rand 依赖）。仅内存态。
    #[serde(skip)]
    pub rng: u64,
}

impl Default for PoolState {
    fn default() -> Self {
        Self {
            policy: PoolPolicy::default(),
            cursor: Cursor::default(),
            accounts: BTreeMap::new(),
            events: Vec::new(),
            stats: PoolStats::default(),
            leases: HashMap::new(),
            rng: now_secs() as u64,
        }
    }
}

/// lease 句柄（契约 §3.2：Lease{id, accountId, acquiredAt}）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub id: String,
    pub account_id: String,
    pub lane: String,
    pub acquired_at: i64,
}

/// release 的错误族（契约 §3.3）。
#[derive(Debug, Clone)]
pub enum ReleaseOutcome {
    /// 2xx。
    Ok,
    /// 429 或「繁忙」→ 冷却退避 60s×1.5^n，帽 15min。
    RateLimited,
    /// 401 → state 不变，触发 TokenKeeper 立即续期（回调在锁外执行）。
    AuthExpired,
    /// 410004 → banned + 证据 + 水位检查。
    Banned { code: Value, detail: String },
    /// 810001 → quota-out（quotaResetHour 后自动复活）。
    QuotaExhausted,
    /// 其他错误：只记 last_error / consecutive_fails。
    Failed(String),
}

/// release 产生的锁外动作（回调/通知在 MutexGuard 释放后执行）。
#[derive(Debug, Default)]
pub struct ReleaseEffects {
    /// 401 → 需立即续期的账号 id。
    pub refresh_account_id: Option<String>,
    /// banned 后水位不足 → toast 文案。
    pub low_water_notice: Option<String>,
}

/// HTTP 状态 + 响应体 → 错误族（契约 §3.3；口径与 autoclaw_watch::classify
/// 一致：410004=banned、810001=quota、401=auth、429/「繁忙」=限流）。
pub fn outcome_from_http(status: u16, body: &str) -> ReleaseOutcome {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("code").cloned());
    if (200..300).contains(&status) {
        ReleaseOutcome::Ok
    } else if status == 429 || body.contains("繁忙") {
        ReleaseOutcome::RateLimited
    } else if status == 401 {
        ReleaseOutcome::AuthExpired
    } else if body.contains("410004") {
        let detail = body.chars().take(160).collect::<String>();
        ReleaseOutcome::Banned {
            code: code.unwrap_or(Value::Null),
            detail: format!("上游封禁(410004): {detail}"),
        }
    } else if body.contains("810001") {
        ReleaseOutcome::QuotaExhausted
    } else {
        let detail = body.chars().take(160).collect::<String>();
        ReleaseOutcome::Failed(format!("HTTP {status}: {detail}"))
    }
}

// ---------------------------------------------------------------------------
// 时间工具（今日计数/日预算/810001 复活均按本地时区，系统时区 Asia/Shanghai）
// ---------------------------------------------------------------------------

fn local_date_of(now_secs: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(now_secs, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn local_hour_of(now_secs: i64) -> u32 {
    use chrono::{TimeZone, Timelike};
    chrono::Local
        .timestamp_opt(now_secs, 0)
        .single()
        .map(|d| d.hour())
        .unwrap_or(0)
}

/// 冷却退避：60s × 1.5^n（n = 此前连续失败次数），帽 15min（契约 §3.3）。
fn cooldown_backoff_secs(fails_before: u32) -> i64 {
    let raw = COOLDOWN_BASE_SECS * 1.5f64.powi(fails_before.min(60) as i32);
    (raw as i64).clamp(1, COOLDOWN_CAP_SECS)
}

// ---------------------------------------------------------------------------
// 状态机核心（纯逻辑，now 注入）
// ---------------------------------------------------------------------------

impl PoolState {
    /// events 环形写入：超过 100 条挤出最旧（契约 §2.2）。
    fn push_event(&mut self, now: i64, kind: &str, account_id: &str, detail: &str) {
        self.events.push(PoolEvent {
            ts: now,
            kind: kind.to_string(),
            account_id: account_id.to_string(),
            detail: detail.to_string(),
        });
        if self.events.len() > EVENTS_CAP {
            self.events.drain(..self.events.len() - EVENTS_CAP);
        }
    }

    /// 轻量 xorshift（random 策略用）。
    fn next_rand(&mut self) -> u64 {
        let mut x = self.rng ^ 0x9E37_79B9_7F4A_7C15;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// 惰性状态迁移（读/选号路径前先跑）：
    ///
    /// 1. 冷却到期 → active；
    /// 2. quota-out 跨过 quotaResetHour 重置点 → 复活 active。todayDate 保留
    ///    quota-out 发生当日作为重置点参照（该状态下方跳过每日零点重置），
    ///    复活当次随即补做重置；
    /// 3. 今日计数跨零点重置（todayDate 比对，契约 §3.3）。
    ///
    /// 返回是否有变更（调用方决定是否安排落盘）。
    pub fn refresh_transient(&mut self, now: i64) -> bool {
        let today = local_date_of(now);
        let now_hour = local_hour_of(now);
        let mut changed = false;
        for rt in self.accounts.values_mut() {
            if rt.state == AccountState::Cooldown && rt.cooldown_until < now {
                rt.state = AccountState::Active;
                changed = true;
            }
            if rt.state == AccountState::QuotaOut
                && !rt.today_date.is_empty()
                && rt.today_date != today
                && now_hour >= self.policy.quota_reset_hour
            {
                rt.state = AccountState::Active;
                changed = true;
            }
            if rt.state != AccountState::QuotaOut && rt.today_date != today {
                rt.today_calls = 0;
                rt.today_date = today.clone();
                changed = true;
            }
        }
        changed
    }

    /// 健康水位（契约 §3.4/§3.5）：enabled ∧ active ∧ 冷却已过 ∧
    /// token 剩余 >300s ∧ 今日未到日预算（容量口径，不含瞬时并发占用）。
    pub fn healthy_count(&self, now: i64) -> usize {
        self.accounts
            .values()
            .filter(|rt| {
                rt.enabled
                    && rt.state == AccountState::Active
                    && rt.cooldown_until < now
                    && rt
                        .token_expire_at
                        .is_some_and(|te| te - now > TOKEN_MIN_TTL_SECS)
                    && rt.today_calls < self.policy.per_account_daily_cap
            })
            .count()
    }

    /// 选号（契约 §3.2）：六重过滤 + 四种策略。无可用号 → None
    /// （调用方返回 503 + 池水位原因文案，与 glmplan 空池语义一致）。
    pub fn acquire(&mut self, lane: &str, now: i64) -> Option<Lease> {
        // 本池只服务国际 lane（契约 §1：glm_proxy/glm_anthropic 国际 lane 接入）。
        if !matches!(lane, "international" | "intl") {
            return None;
        }
        self.refresh_transient(now);
        // 过滤 6：整池同时在途上限。
        let pool_in_flight: u32 = self.accounts.values().map(|rt| rt.in_flight).sum();
        if pool_in_flight >= self.policy.pool_concurrent_cap {
            return None;
        }
        // 过滤 1-5（单号维度）。tokenExpireAt 缺失 = 无法确认剩余寿命，保守摘除。
        let eligible: Vec<String> = self
            .accounts
            .iter()
            .filter(|(_, rt)| {
                rt.enabled                                        // 过滤 1
                    && rt.state == AccountState::Active           // 过滤 1
                    && rt.cooldown_until < now                    // 过滤 2
                    && rt.token_expire_at                         // 过滤 3
                        .is_some_and(|te| te - now > TOKEN_MIN_TTL_SECS)
                    && rt.today_calls < self.policy.per_account_daily_cap // 过滤 4
                    && rt.in_flight < self.policy.account_concurrent_cap  // 过滤 5
            })
            .map(|(id, _)| id.clone())
            .collect();
        if eligible.is_empty() {
            return None;
        }
        let n = eligible.len();
        let picked: String = match self.policy.strategy {
            // 游标推进，跳过不可用（不可用号根本不在 eligible 里）。
            Strategy::RoundRobin => {
                let i = (self.cursor.offset % n as u64) as usize;
                self.cursor.offset = self.cursor.offset.wrapping_add(1);
                eligible[i].clone()
            }
            Strategy::Random => {
                let i = (self.next_rand() % n as u64) as usize;
                eligible[i].clone()
            }
            // todayCalls 最小（契约 §3.2）。
            Strategy::LeastUsed => {
                let mut best = 0usize;
                for (i, id) in eligible.iter().enumerate() {
                    if self.accounts[id].today_calls < self.accounts[eligible[best].as_str()].today_calls {
                        best = i;
                    }
                }
                eligible[best].clone()
            }
            // 优先上次成功号（lastUsedAt 最新）直到失效；无人成功过 → 游标兜底。
            Strategy::Sticky => {
                let mut best: Option<(usize, i64)> = None;
                for (i, id) in eligible.iter().enumerate() {
                    if let Some(ts) = self.accounts[id.as_str()].last_used_at {
                        if best.is_none_or(|(_, bts)| ts > bts) {
                            best = Some((i, ts));
                        }
                    }
                }
                match best {
                    Some((i, _)) => eligible[i].clone(),
                    None => {
                        let i = (self.cursor.offset % n as u64) as usize;
                        self.cursor.offset = self.cursor.offset.wrapping_add(1);
                        eligible[i].clone()
                    }
                }
            }
        };
        // 锁内记账：inFlight+1、cursor 推进（契约 §3.2 步骤 3）。
        let rt = self.accounts.get_mut(&picked)?;
        rt.in_flight += 1;
        let lease = Lease {
            id: format!("lease-{}", uuid::Uuid::new_v4().simple()),
            account_id: picked,
            lane: "international".to_string(),
            acquired_at: now,
        };
        self.leases.insert(lease.id.clone(), lease.account_id.clone());
        self.stats.rotations += 1;
        Some(lease)
    }

    /// 回写（契约 §3.3）：按错误族映射状态；inFlight−1；今日计数跨零点重置。
    /// 重复 release（lease 未登记）幂等忽略，防 in_flight 双减。
    pub fn release(&mut self, lease: &Lease, outcome: ReleaseOutcome, now: i64) -> ReleaseEffects {
        let mut effects = ReleaseEffects::default();
        self.refresh_transient(now);
        let Some(account_id) = self.leases.remove(&lease.id) else {
            return effects;
        };
        if !self.accounts.contains_key(&account_id) {
            return effects;
        }
        let today = local_date_of(now);
        let mut event: Option<(&'static str, String)> = None;
        {
            let rt = self.accounts.get_mut(&account_id).unwrap();
            rt.in_flight = rt.in_flight.saturating_sub(1);
            if rt.today_date != today {
                rt.today_calls = 0;
                rt.today_date = today.clone();
            }
            match outcome {
                ReleaseOutcome::Ok => {
                    rt.today_calls += 1;
                    rt.total_calls += 1;
                    rt.last_used_at = Some(now);
                    rt.consecutive_fails = 0;
                    *self.stats.by_day.entry(today).or_insert(0) += 1;
                }
                ReleaseOutcome::RateLimited => {
                    let n = rt.consecutive_fails;
                    rt.consecutive_fails = n.saturating_add(1);
                    let secs = cooldown_backoff_secs(n);
                    rt.cooldown_until = now + secs;
                    rt.state = AccountState::Cooldown;
                    rt.last_error = Some("429/繁忙，进入冷却退避".to_string());
                    event = Some(("cooldown", format!("429/繁忙，冷却 {secs}s（退避第 {} 档）", n + 1)));
                }
                ReleaseOutcome::AuthExpired => {
                    // state 不变，触发 TokenKeeper 立即续期（契约 §3.3）；
                    // 回调在锁外执行（ReleaseEffects）。
                    rt.last_error = Some("401 登录态过期，触发 TokenKeeper 续期".to_string());
                    effects.refresh_account_id = Some(account_id.clone());
                }
                ReleaseOutcome::Banned { code, detail } => {
                    rt.state = AccountState::Banned;
                    rt.ban_code = Some(code);
                    rt.banned_at = Some(now);
                    rt.last_error = Some(detail.clone());
                    event = Some(("ban", format!("410004 上游封禁，摘出轮换: {detail}")));
                }
                ReleaseOutcome::QuotaExhausted => {
                    rt.state = AccountState::QuotaOut;
                    rt.last_error = Some("810001 当日额度用尽".to_string());
                    event = Some((
                        "quota",
                        format!("810001 额度用尽，等待 quotaResetHour={} 点复活", self.policy.quota_reset_hour),
                    ));
                }
                ReleaseOutcome::Failed(msg) => {
                    rt.consecutive_fails = rt.consecutive_fails.saturating_add(1);
                    rt.last_error = Some(msg);
                }
            }
        }
        if let Some((kind, detail)) = event {
            self.push_event(now, kind, &account_id, &detail);
        }
        // 410004 摘出后立即水位检查（契约 §3.3）。
        if self
            .accounts
            .get(&account_id)
            .is_some_and(|rt| rt.state == AccountState::Banned)
        {
            let healthy = self.healthy_count(now);
            if healthy < self.policy.min_healthy as usize {
                effects.low_water_notice = Some(format!(
                    "国际号池健康水位不足：healthy={healthy} < minHealthy={}，请运行注册管线扩号",
                    self.policy.min_healthy
                ));
            }
        }
        effects
    }

    /// 入池（契约 §3.1）：注册管线换码成功 → add_account_v2 intl 分支 → 本方法。
    /// 幂等：同账号重复入池 = 更新令牌（tokenExpireAt 刷新 + 重新激活）。
    /// 新号入池若处于「有号被剔后的缺位」且 healthy < minHealthy，视为自动
    /// 补位（§3.5：stats.replacements+1 + events「replace」）；建池初期的
    /// 首批号不计补位。
    pub fn join(
        &mut self,
        account_id: &str,
        source_email: &str,
        token_expire_at: Option<i64>,
        device_isolated: bool,
        now: i64,
    ) -> Value {
        let before_healthy = self.healthy_count(now);
        let had_removed = self.accounts.values().any(|rt| {
            matches!(
                rt.state,
                AccountState::Banned | AccountState::Invalid | AccountState::QuotaOut | AccountState::Disabled
            )
        });
        let today = local_date_of(now);
        let mut joined = true;
        match self.accounts.get_mut(account_id) {
            Some(rt) => {
                joined = false;
                rt.enabled = true;
                rt.state = AccountState::Active;
                rt.refresh_fails = 0;
                if token_expire_at.is_some() {
                    rt.token_expire_at = token_expire_at;
                }
                self.push_event(now, "join", account_id, &format!("重复入池=更新令牌（{source_email}）"));
            }
            None => {
                self.accounts.insert(
                    account_id.to_string(),
                    AccountRuntime {
                        today_date: today,
                        token_expire_at,
                        ..AccountRuntime::default()
                    },
                );
                let iso = if device_isolated { "，独立虚拟设备" } else { "" };
                self.push_event(now, "join", account_id, &format!("新号入池（{source_email}{iso}）"));
            }
        }
        if joined && had_removed && before_healthy < self.policy.min_healthy as usize {
            self.stats.replacements += 1;
            self.push_event(now, "replace", account_id, "新号入池自动补位（水位不足）");
        }
        json!({ "joined": joined, "id": account_id })
    }

    /// 启用/停用（契约 §3.6 toggle）。停用 → disabled 摘出轮换；
    /// 启用 → disabled 回 active（其余状态不动，等各自的恢复路径）。
    pub fn set_enabled(&mut self, account_id: &str, enabled: bool, now: i64) -> bool {
        let state_changed = {
            let Some(rt) = self.accounts.get_mut(account_id) else {
                return false;
            };
            rt.enabled = enabled;
            if enabled {
                if rt.state == AccountState::Disabled {
                    rt.state = AccountState::Active;
                }
            } else {
                rt.state = AccountState::Disabled;
            }
            true
        };
        if state_changed {
            self.push_event(now, "disable", account_id, if enabled { "启用" } else { "停用" });
        }
        state_changed
    }

    /// 热更新策略（契约 §3.6 policy；camelCase 字段）。全量校验通过才应用，
    /// 避免半套配置生效。
    pub fn update_policy(&mut self, patch: &Value) -> Result<(), String> {
        let Some(obj) = patch.as_object() else {
            return Err("策略补丁必须是 JSON 对象".to_string());
        };
        let mut next = self.policy.clone();
        for (key, value) in obj {
            match key.as_str() {
                "strategy" => {
                    let s = value
                        .as_str()
                        .ok_or_else(|| "strategy 必须是字符串".to_string())?;
                    next.strategy = Strategy::parse(s).ok_or_else(|| {
                        format!("未知策略: {s}（可选 round-robin/random/least-used/sticky）")
                    })?;
                }
                "perAccountRatePerMin" => next.per_account_rate_per_min = json_u32(value, 1)?,
                "perAccountDailyCap" => next.per_account_daily_cap = json_u32(value, 1)?,
                "poolConcurrentCap" => next.pool_concurrent_cap = json_u32(value, 1)?,
                "accountConcurrentCap" => next.account_concurrent_cap = json_u32(value, 1)?,
                "quotaResetHour" => {
                    let h = json_u32(value, 0)?;
                    if h > 23 {
                        return Err("quotaResetHour 必须在 0-23".to_string());
                    }
                    next.quota_reset_hour = h;
                }
                "autoReplace" => {
                    next.auto_replace = value
                        .as_bool()
                        .ok_or_else(|| "autoReplace 必须是布尔".to_string())?;
                }
                "minHealthy" => next.min_healthy = json_u32(value, 0)?,
                other => return Err(format!("未知策略字段: {other}")),
            }
        }
        self.policy = next;
        Ok(())
    }

    /// TokenKeeper 扫描（契约 §3.4）：tokenExpireAt − now < 2h 的在役账号
    /// （banned/invalid/disabled 不扫）。返回账号 id 列表，回调由外层执行。
    pub fn due_for_refresh(&self, now: i64) -> Vec<String> {
        self.accounts
            .iter()
            .filter(|(_, rt)| {
                rt.enabled
                    && !matches!(
                        rt.state,
                        AccountState::Banned | AccountState::Invalid | AccountState::Disabled
                    )
                    && rt
                        .token_expire_at
                        .is_some_and(|te| te - now < TOKEN_REFRESH_AHEAD_SECS)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 续期成功回写（契约 §3.4）：tokenExpireAt/lastRefreshAt 更新 +
    /// events「refresh」+ stats.refreshes+1；invalid 视为手动/重注册复活。
    pub fn mark_refresh_success(&mut self, account_id: &str, token_expire_at: Option<i64>, now: i64) -> bool {
        let state_changed = {
            let Some(rt) = self.accounts.get_mut(account_id) else {
                return false;
            };
            rt.last_refresh_at = Some(now);
            rt.refresh_fails = 0;
            if token_expire_at.is_some() {
                rt.token_expire_at = token_expire_at;
            }
            if rt.state == AccountState::Invalid {
                rt.state = AccountState::Active;
            }
            true
        };
        if state_changed {
            self.stats.refreshes += 1;
            self.push_event(now, "refresh", account_id, "token 续期成功");
        }
        state_changed
    }

    /// 续期失败回写（契约 §3.4）：连续 3 次失败 → invalid 摘出轮换
    /// （等待重新注册/人工）。返回本次是否触发摘除。
    pub fn mark_refresh_failure(&mut self, account_id: &str, err: &str, now: i64) -> bool {
        let (failed_out, fails) = {
            let Some(rt) = self.accounts.get_mut(account_id) else {
                return false;
            };
            rt.refresh_fails = rt.refresh_fails.saturating_add(1);
            rt.last_error = Some(format!("续期失败: {err}"));
            let failed_out = rt.refresh_fails >= REFRESH_FAIL_LIMIT;
            if failed_out && rt.state != AccountState::Banned {
                // banned 证据优先保留，不降级覆盖。
                rt.state = AccountState::Invalid;
            }
            (failed_out, rt.refresh_fails)
        };
        self.push_event(
            now,
            "refresh",
            account_id,
            &format!("token 续期失败（第 {fails} 次）: {err}"),
        );
        failed_out
    }

    /// watch 探测结果映射（契约 §3.7）：autoclaw_watch 的分类
    /// ok / banned 410004 / quota 810001 / invalid 401 / error → 本状态机。
    /// 单向数据流：watch 不感知池存在；banned/invalid 摘出轮换（banned 保留
    /// 上游原始 code 证据），quota 映射 quota-out（零点复活路径复用 §3.3），
    /// ok/error 不做状态迁移（error 只更新 last_error 观测，避免网络抖动把
    /// 正常号踢出轮换）。返回 true 表示有状态变更（调用方安排落盘）。
    pub fn apply_watch_result(&mut self, account_id: &str, watch_state: &str, code: &Value, detail: &str, now: i64) -> bool {
        let Some(rt) = self.accounts.get_mut(account_id) else {
            return false;
        };
        // disabled 是管理员手动停用，watch 不得越权复活；banned 证据优先，
        // 不被后续 invalid/quota 结果降级覆盖。
        if rt.state == AccountState::Disabled || rt.state == AccountState::Banned {
            return false;
        }
        match watch_state {
            "banned" => {
                if rt.state == AccountState::Banned {
                    return false;
                }
                rt.state = AccountState::Banned;
                rt.ban_code = Some(code.clone());
                rt.banned_at = Some(now);
                rt.last_error = Some(format!("watch 探测封禁: {detail}"));
                self.push_event(now, "ban", account_id, &format!("watch 探测 410004 封禁，摘出轮换: {detail}"));
                true
            }
            "invalid" => {
                if rt.state == AccountState::Invalid {
                    return false;
                }
                rt.state = AccountState::Invalid;
                rt.last_error = Some(format!("watch 探测登录态失效(401): {detail}"));
                self.push_event(now, "refresh", account_id, "watch 探测 401，摘出轮换等待续期/人工");
                true
            }
            "quota" => {
                if rt.state == AccountState::QuotaOut {
                    return false;
                }
                rt.state = AccountState::QuotaOut;
                rt.last_error = Some(format!("watch 探测额度用尽(810001): {detail}"));
                self.push_event(now, "quota", account_id, "watch 探测 810001，等待 quotaResetHour 复活");
                true
            }
            "ok" => {
                // invalid 号被 watch 判 ok = 上游已恢复（如续期生效），放回轮换。
                if rt.state == AccountState::Invalid {
                    rt.state = AccountState::Active;
                    rt.refresh_fails = 0;
                    self.push_event(now, "refresh", account_id, "watch 探测恢复 200，重新入役");
                    return true;
                }
                false
            }
            _ => false, // error：只观测不迁移
        }
    }

    /// 手动移除（契约 §3.6 remove）：连 PoolState 条目一并清理（§3.5 purge
    /// 语义）。返回是否存在过该条目（供 API 层区分「不存在」）。
    pub fn purge(&mut self, account_id: &str, now: i64) -> bool {
        if self.accounts.remove(account_id).is_none() {
            return false;
        }
        self.push_event(now, "disable", account_id, "手动移除，轮换池条目一并清理");
        true
    }

    /// 手动推进/重置游标（契约 §3.6 rotate）。action=reset 归零；advance 推进
    /// 指定步数（缺省 1）。返回游标偏移；池为空时返回 None。
    pub fn rotate_cursor(&mut self, reset: bool, advance: u64, _now: i64) -> Option<u64> {
        if self.accounts.is_empty() {
            return None;
        }
        if reset {
            self.cursor.offset = 0;
        } else if advance > 0 {
            self.cursor.offset = self.cursor.offset.wrapping_add(advance);
        }
        self.push_event(
            _now,
            "rotate",
            "",
            &if reset {
                "手动重置游标".to_string()
            } else {
                format!("手动推进游标 +{advance}")
            },
        );
        Some(self.cursor.offset)
    }

    /// 池总览（契约 §3.6 summary）。
    pub fn summary(&self, now: i64) -> Value {
        let mut counts: HashMap<&'static str, u32> = HashMap::new();
        for rt in self.accounts.values() {
            *counts.entry(rt.state.as_str()).or_insert(0) += 1;
        }
        let today = local_date_of(now);
        let today_calls: u64 = self
            .accounts
            .values()
            .filter(|rt| rt.today_date == today)
            .map(|rt| rt.today_calls as u64)
            .sum();
        let pool_in_flight: u32 = self.accounts.values().map(|rt| rt.in_flight).sum();
        let healthy = self.healthy_count(now);
        json!({
            "lane": self.cursor.lane,
            "strategy": self.policy.strategy.as_str(),
            "cursorOffset": self.cursor.offset,
            "total": self.accounts.len(),
            "healthy": healthy,
            "cooldown": counts.get("cooldown").copied().unwrap_or(0),
            "quotaOut": counts.get("quota-out").copied().unwrap_or(0),
            "banned": counts.get("banned").copied().unwrap_or(0),
            "invalid": counts.get("invalid").copied().unwrap_or(0),
            "disabled": counts.get("disabled").copied().unwrap_or(0),
            "poolInFlight": pool_in_flight,
            "poolConcurrentCap": self.policy.pool_concurrent_cap,
            "perAccountDailyCap": self.policy.per_account_daily_cap,
            "todayCalls": today_calls,
            "minHealthy": self.policy.min_healthy,
            "lowWater": healthy < self.policy.min_healthy as usize,
            "autoReplace": self.policy.auto_replace,
            "rotations": self.stats.rotations,
            "replacements": self.stats.replacements,
            "refreshes": self.stats.refreshes,
        })
    }

    /// 账号行（契约 §3.6 list 的数据源；脱敏由 API 层负责）。
    pub fn rows(&self, now: i64) -> Vec<Value> {
        self.accounts
            .iter()
            .map(|(id, rt)| {
                json!({
                    "id": id,
                    "enabled": rt.enabled,
                    "weight": rt.weight,
                    "state": rt.state.as_str(),
                    "inFlight": rt.in_flight,
                    "todayCalls": rt.today_calls,
                    "totalCalls": rt.total_calls,
                    "consecutiveFails": rt.consecutive_fails,
                    "cooldownUntil": rt.cooldown_until,
                    "cooldownRemainSecs": (rt.cooldown_until - now).max(0),
                    "banCode": rt.ban_code,
                    "bannedAt": rt.banned_at,
                    "tokenExpireAt": rt.token_expire_at,
                    "tokenTtlSecs": rt.token_expire_at.map(|te| te - now),
                    "lastRefreshAt": rt.last_refresh_at,
                    "refreshFails": rt.refresh_fails,
                    "lastUsedAt": rt.last_used_at,
                    "lastError": rt.last_error,
                })
            })
            .collect()
    }
}

/// 未知状态串容错：保守按 invalid 摘出轮换（宁可不选号，不可误选坏号）。
fn deserialize_state_lenient<'de, D>(deserializer: D) -> Result<AccountState, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(AccountState::parse(&s).unwrap_or(AccountState::Invalid))
}

/// 未知策略串容错：回退默认 round-robin（契约 §2.2 默认值）。
fn deserialize_strategy_lenient<'de, D>(deserializer: D) -> Result<Strategy, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(Strategy::parse(&s).unwrap_or(Strategy::RoundRobin))
}

fn json_u32(value: &Value, min: u32) -> Result<u32, String> {
    let n = value
        .as_i64()
        .or_else(|| value.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .ok_or_else(|| format!("数字字段非法: {value}"))?;
    if n < min as i64 || n > u32::MAX as i64 {
        return Err(format!("数值超出范围 [{min}, {}]: {n}", u32::MAX));
    }
    Ok(n as u32)
}

// ---------------------------------------------------------------------------
// 持久化（契约 §2.2：atomic_write；inFlight 落盘清零）
// ---------------------------------------------------------------------------

/// 序列化落盘快照：inFlight 清零（仅内存态，契约 §2.2）；byDay 只保留最近
/// 30 天（键 YYYY-MM-DD 字典序 = 时间序）。
fn state_to_disk_json(st: &PoolState) -> Value {
    let mut v = serde_json::to_value(st).unwrap_or_else(|_| json!({}));
    if let Some(accounts) = v.get_mut("accounts").and_then(Value::as_object_mut) {
        for entry in accounts.values_mut() {
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("inFlight".to_string(), json!(0));
            }
        }
    }
    if let Some(by_day) = v.pointer_mut("/stats/byDay").and_then(Value::as_object_mut) {
        if by_day.len() > BY_DAY_KEEP {
            let mut keys: Vec<String> = by_day.keys().cloned().collect();
            keys.sort();
            let overflow = keys.len() - BY_DAY_KEEP;
            for key in keys.into_iter().take(overflow) {
                by_day.remove(&key);
            }
        }
    }
    v
}

/// 落盘一个状态快照（测试与 persist_inner 共用；atomic_write）。
fn write_state_file(path: &Path, st: &PoolState) {
    let content = match serde_json::to_string_pretty(&state_to_disk_json(st)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[intl-pool] 状态序列化失败: {e}");
            return;
        }
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = atomic_write(path, &content) {
        eprintln!("[intl-pool] 状态落盘失败: {e}");
    }
}

/// 读状态文件；缺失/损坏 → 全新默认状态（沿用仓库既有容错惯例）。
fn read_state_file(path: &Path) -> PoolState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<PoolState>(&t).ok())
        .map(|mut st| {
            st.events.truncate(EVENTS_CAP);
            // 内存态不从磁盘恢复（契约 §2.2）。
            for rt in st.accounts.values_mut() {
                rt.in_flight = 0;
            }
            st.leases.clear();
            st
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// PoolManager 单例（OnceLock + std Mutex；锁内决策、锁外 IO/回调）
// ---------------------------------------------------------------------------

struct PoolInner {
    path: PathBuf,
    debounce: Duration,
    state: Mutex<PoolState>,
    /// 有未落盘变更。
    dirty: AtomicBool,
    /// 已有落盘任务在跑（合并写去重）。
    saving: AtomicBool,
    /// TokenKeeper 续期回调（M3 注入实现）。
    refresh_cb: Mutex<Option<RefreshCallback>>,
}

/// 续期回调（契约 §3.4，M3 注入实现）：入参账号 id。实现内部自行
/// tokio::spawn 做网络 IO，避免阻塞调用线程。
pub type RefreshCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// 内存单例（OnceLock + std Mutex）。所有方法锁内决策并 clone 数据、
/// 锁外做 IO/回调 —— MutexGuard 绝不跨 await（kimi.rs 教训）。
#[derive(Clone)]
pub struct PoolManager {
    inner: Arc<PoolInner>,
}

static POOL_MANAGER: OnceLock<PoolManager> = OnceLock::new();

/// 全局单例（惰性初始化；落盘 ~/.wb-switch/autoclaw_intl_pool.json）。
pub fn pool_manager() -> &'static PoolManager {
    POOL_MANAGER.get_or_init(|| PoolManager::new(store_dir().join(POOL_FILE)))
}

impl PoolManager {
    pub fn new(path: PathBuf) -> Self {
        Self::with_debounce(path, SAVE_DEBOUNCE)
    }

    /// 测试用：自定义落盘路径 + debounce 时长（临时目录注入）。
    pub fn with_debounce(path: PathBuf, debounce: Duration) -> Self {
        let state = read_state_file(&path);
        Self {
            inner: Arc::new(PoolInner {
                path,
                debounce,
                state: Mutex::new(state),
                dirty: AtomicBool::new(false),
                saving: AtomicBool::new(false),
                refresh_cb: Mutex::new(None),
            }),
        }
    }

    /// 选号（契约 §3.2）。lane 仅接受 international/intl。
    pub fn acquire(&self, lane: &str) -> Option<Lease> {
        let now = now_secs();
        // 锁内决策：acquire 返回 owned Lease，锁随即释放。
        let lease = {
            let mut st = self.inner.state.lock().unwrap();
            st.acquire(lane, now)
        };
        if lease.is_some() {
            self.schedule_save();
        }
        lease
    }

    /// 回写（契约 §3.3）。401 → 锁外触发续期回调；banned 后水位不足 →
    /// 锁外 toast（notify 自带节流）。
    pub fn release(&self, lease: &Lease, outcome: ReleaseOutcome) {
        let now = now_secs();
        let effects = {
            let mut st = self.inner.state.lock().unwrap();
            st.release(lease, outcome, now)
        };
        self.schedule_save();
        if let Some(id) = &effects.refresh_account_id {
            Self::fire_refresh_inner(&self.inner, id);
        }
        if let Some(msg) = &effects.low_water_notice {
            notify::notify("autoclaw-intl-pool-low", "AutoClaw 国际号池水位告警", msg);
        }
    }

    /// HTTP 状态便捷回写：内部走 outcome_from_http 错误族映射。
    pub fn release_status(&self, lease: &Lease, status: u16, body: &str) {
        self.release(lease, outcome_from_http(status, body));
    }

    /// 入池（契约 §3.1；autoclaw_accounts 的 intl 分支调用）。
    pub fn join(
        &self,
        account_id: &str,
        source_email: &str,
        token_expire_at: Option<i64>,
        device_isolated: bool,
    ) -> Value {
        let now = now_secs();
        let out = {
            let mut st = self.inner.state.lock().unwrap();
            st.join(account_id, source_email, token_expire_at, device_isolated, now)
        };
        self.schedule_save();
        out
    }

    /// 启用/停用（契约 §3.6 toggle）。
    pub fn set_enabled(&self, account_id: &str, enabled: bool) -> bool {
        let now = now_secs();
        let changed = {
            let mut st = self.inner.state.lock().unwrap();
            st.set_enabled(account_id, enabled, now)
        };
        if changed {
            self.schedule_save();
        }
        changed
    }

    /// 热更新策略（契约 §3.6 policy）；返回应用后的策略快照。
    pub fn update_policy(&self, patch: &Value) -> Result<Value, String> {
        let applied = {
            let mut st = self.inner.state.lock().unwrap();
            st.update_policy(patch)?;
            serde_json::to_value(&st.policy).map_err(|e| e.to_string())?
        };
        self.schedule_save();
        Ok(applied)
    }

    pub fn policy_value(&self) -> Value {
        let st = self.inner.state.lock().unwrap();
        serde_json::to_value(&st.policy).unwrap_or(Value::Null)
    }

    /// 健康水位计数。
    pub fn healthy_count(&self) -> usize {
        let st = self.inner.state.lock().unwrap();
        st.healthy_count(now_secs())
    }

    /// 启动对账（契约 §3.1 幂等入池的服务端兜底）：凭据文件里有 intl 条目
    /// 但池状态缺失（池文件被清/首次部署/断言清理）时，自动 join 重建，
    /// 使 list 不落 "missing" 自愈视图。重复调用无害（join 幂等）。
    pub fn reconcile_from_credentials(&self, intl_credentials: &[Value]) -> usize {
        let mut rejoined = 0usize;
        for cred in intl_credentials {
            let Some(id) = cred.get("id").and_then(Value::as_str) else {
                continue;
            };
            let source_email = cred
                .pointer("/intlMeta/sourceEmail")
                .and_then(Value::as_str)
                .unwrap_or("");
            let token_expire_at = cred.pointer("/intlMeta/tokenExpireAt").and_then(Value::as_i64);
            let device_isolated = cred
                .pointer("/intlMeta/deviceIsolated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let existed = {
                let st = self.inner.state.lock().unwrap();
                st.accounts.contains_key(id)
            };
            if !existed {
                self.join(id, source_email, token_expire_at, device_isolated);
                rejoined += 1;
            }
        }
        rejoined
    }

    /// 池总览（含惰性状态迁移：复活/冷却到期/跨零点重置）。
    pub fn summary(&self) -> Value {
        let now = now_secs();
        let changed = {
            let mut st = self.inner.state.lock().unwrap();
            let changed = st.refresh_transient(now);
            (changed, st.summary(now))
        };
        if changed.0 {
            self.schedule_save();
        }
        changed.1
    }

    /// 轮换事件流（最新在前，取 limit 条）。
    pub fn events(&self, limit: usize) -> Vec<Value> {
        let st = self.inner.state.lock().unwrap();
        st.events
            .iter()
            .rev()
            .take(limit)
            .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
            .collect()
    }

    /// 账号行（list 数据源）。
    pub fn rows(&self) -> Vec<Value> {
        let st = self.inner.state.lock().unwrap();
        st.rows(now_secs())
    }

    /// 注入 TokenKeeper 续期回调（M3 实现；重复注入覆盖旧回调）。
    pub fn set_refresh_callback(&self, cb: RefreshCallback) {
        *self.inner.refresh_cb.lock().unwrap() = Some(cb);
    }

    /// 续期成功回写（契约 §3.4；M3 回调完成后调用）。
    pub fn mark_refresh_success(&self, account_id: &str, token_expire_at: Option<i64>) {
        let now = now_secs();
        {
            let mut st = self.inner.state.lock().unwrap();
            st.mark_refresh_success(account_id, token_expire_at, now);
        }
        self.schedule_save();
    }

    /// 续期失败回写；返回本次是否触发 invalid 摘除。
    pub fn mark_refresh_failure(&self, account_id: &str, err: &str) -> bool {
        let now = now_secs();
        let failed_out = {
            let mut st = self.inner.state.lock().unwrap();
            st.mark_refresh_failure(account_id, err, now)
        };
        self.schedule_save();
        failed_out
    }

    /// 立即落盘（绕过 debounce；测试与退出钩子用）。
    pub fn persist_now(&self) {
        persist_inner(&self.inner);
    }

    /// watch 探测结果映射（契约 §3.7）：锁内决策，有变更时锁外安排落盘。
    /// 返回本次映射是否产生状态变更。
    pub fn apply_watch_result(&self, account_id: &str, watch_state: &str, code: &Value, detail: &str) -> bool {
        let now = now_secs();
        let changed = {
            let mut st = self.inner.state.lock().unwrap();
            st.apply_watch_result(account_id, watch_state, code, detail, now)
        };
        if changed {
            self.schedule_save();
        }
        changed
    }

    /// 手动移除（契约 §3.6 remove）：轮换池条目清理（§3.5 purge）。
    /// 返回池中是否存在过该条目。
    pub fn purge(&self, account_id: &str) -> bool {
        let now = now_secs();
        let existed = {
            let mut st = self.inner.state.lock().unwrap();
            st.purge(account_id, now)
        };
        if existed {
            self.schedule_save();
        }
        existed
    }

    /// 手动推进/重置游标（契约 §3.6 rotate）。池为空返回 None。
    pub fn rotate_cursor(&self, reset: bool, advance: u64) -> Option<u64> {
        let now = now_secs();
        let out = {
            let mut st = self.inner.state.lock().unwrap();
            st.rotate_cursor(reset, advance, now)
        };
        if out.is_some() {
            self.schedule_save();
        }
        out
    }

    /// TokenKeeper 扫描循环骨架（契约 §3.4）：每 15 分钟扫描
    /// tokenExpireAt−now<2h 的在役账号并触发续期回调；401 事件的即时触发
    /// 见 release()。续期实现归 M3（回调注入）。
    pub fn spawn_token_keeper(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            eprintln!("[intl-pool] spawn_token_keeper 需要 tokio 运行时，已跳过");
            return;
        }
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            // 启动后 90s 先跑一轮（等服务初始化，对齐 autoclaw_watch 惯例）。
            tokio::time::sleep(Duration::from_secs(90)).await;
            loop {
                let due = {
                    let st = inner.state.lock().unwrap();
                    st.due_for_refresh(now_secs())
                };
                for id in &due {
                    Self::fire_refresh_inner(&inner, id);
                }
                tokio::time::sleep(TOKEN_SCAN_INTERVAL).await;
            }
        });
    }

    /// 锁外触发续期回调：回调本身只拿账号 id，网络 IO 由实现内部 spawn。
    fn fire_refresh_inner(inner: &PoolInner, account_id: &str) {
        let cb = inner.refresh_cb.lock().unwrap().clone();
        if let Some(cb) = cb {
            cb(account_id);
        }
    }

    /// 注入默认 TokenKeeper 续期回调（M3 契约 §3.4 实现接线）。
    ///
    /// 续期动作 = `autoclaw_accounts::refresh_stored_account`：
    /// - `POST /userapi/v1/refresh {source_id:"autoclaw", device_id, refresh_token}`
    ///   打账号所属区域的网关（intl 号 → `autoglm-api.autoglm.ai`，契约 §2.1/
    ///   autoclaw_accounts::AccountRegion::api_host）；头体系沿用
    ///   `platform_checkin::autoclaw_headers`（X-Auth-Sign 签名头）；
    /// - 外网请求走仓库共享 reqwest 客户端（config::http_request），
    ///   系统时区/代理环境与国内号续期完全同构，无需 no_proxy；
    /// - 成功回写凭据文件 accessToken + intlMeta.tokenExpireAt（契约 §2.1），
    ///   再回写轮换状态 lastRefreshAt/tokenExpireAt + events「refresh」；
    /// - 失败按 §3.4 连败 3 次 → invalid 摘出轮换。
    ///
    /// 回调只拿 id，网络 IO 内部 tokio::spawn，绝不阻塞 TokenKeeper 扫描循环
    /// 与 MutexGuard（kimi.rs 教训）。
    pub fn install_default_refresh_callback(&self) {
        self.set_refresh_callback(Arc::new(|account_id| {
            let id = account_id.to_string();
            tokio::spawn(async move {
                match crate::modules::autoclaw_accounts::refresh_stored_account(&id).await {
                    Ok(new_access) => {
                        // 凭据文件已由 refresh_stored_account 写回（accessToken +
                        // intlMeta.tokenExpireAt，autoclaw_accounts.rs:644）；
                        // 轮换状态以新 JWT exp 为准回写。
                        let expire =
                            crate::modules::autoclaw_accounts::jwt_exp_from_access_token(&new_access);
                        pool_manager().mark_refresh_success(&id, expire);
                    }
                    Err(e) => {
                        pool_manager().mark_refresh_failure(&id, &e);
                    }
                }
            });
        }));
    }

    /// watch 订阅循环（契约 §3.7 / §1 职责边界）：每 60s 读
    /// `~/.wb-switch/autoclaw_watch.json`（autoclaw_watch 每 30min 全池探测后
    /// 落盘），把 region=international 条目的 banned/invalid/quota 结果单向
    /// 映射进轮换状态机。watch 不感知池存在，本订阅零侵入、零新增上游流量。
    ///
    /// 注：探测请求本身是最小 chat 计费请求（watch 已有），探测流量计入日预算
    /// 由 release 记账承担（watch 直发不计入池计数 —— 已知与预算口径的偏差，
    /// 契约 §3.7「探测流量计入日预算」在 M2 release 记账侧生效）。
    pub fn spawn_watch_subscriber() {
        if tokio::runtime::Handle::try_current().is_err() {
            eprintln!("[intl-pool] spawn_watch_subscriber 需要 tokio 运行时，已跳过");
            return;
        }
        tokio::spawn(async move {
            // 启动后 2 分钟先跑一轮（避开启动风暴：watch 首轮在 90s）。
            tokio::time::sleep(Duration::from_secs(120)).await;
            loop {
                subscribe_watch_once();
                tokio::time::sleep(WATCH_POLL_INTERVAL).await;
            }
        });
    }

    /// 事件驱动 + debounce 落盘（合并写）：
    /// - dirty 标记本次变更；saving 确保同一时刻只有一个写任务；
    /// - 写任务醒后若窗口内又有新变更则继续合并，落盘期间的新变更由
    ///   「醒后无条件落盘一次」兜底；
    /// - 退出瞬间的新变更：能接管就补落一轮；接管失败说明新写任务已接手，
    ///   而写任务醒后必无条件落盘一次，变更不会丢。
    fn schedule_save(&self) {
        self.inner.dirty.store(true, Ordering::SeqCst);
        if self.inner.saving.swap(true, Ordering::SeqCst) {
            return; // 已有写任务，由其收尾检查兜底合并
        }
        // 非 tokio 上下文（测试兜底）：直接同步落盘。
        if tokio::runtime::Handle::try_current().is_err() {
            self.persist_now();
            self.inner.dirty.store(false, Ordering::SeqCst);
            self.inner.saving.store(false, Ordering::SeqCst);
            return;
        }
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            'writer: loop {
                tokio::time::sleep(inner.debounce).await;
                // 合并窗口：醒后清脏标记 → 窗口内又有新变更则继续等。
                loop {
                    if !inner.dirty.swap(false, Ordering::SeqCst) {
                        break;
                    }
                    tokio::time::sleep(inner.debounce).await;
                }
                persist_inner(&inner); // 醒后无条件落盘一次（含最新状态）
                if !inner.dirty.load(Ordering::SeqCst) {
                    break 'writer;
                }
            }
            inner.saving.store(false, Ordering::SeqCst);
            // 收尾竞态兜底：退出瞬间若有新变更且无人接管，补落一轮。
            if inner.dirty.swap(false, Ordering::SeqCst) && !inner.saving.swap(true, Ordering::SeqCst) {
                tokio::time::sleep(inner.debounce).await;
                persist_inner(&inner);
                inner.saving.store(false, Ordering::SeqCst);
            }
        });
    }
}

/// 落盘一次：锁内 clone 快照，锁外序列化 + 写文件（MutexGuard 不跨 IO）。
fn persist_inner(inner: &PoolInner) {
    let snapshot = {
        let st = inner.state.lock().unwrap();
        st.clone()
    };
    write_state_file(&inner.path, &snapshot);
}

/// watch 订阅单轮：读 autoclaw_watch.json，把 intl 号的探测结果映射进状态机。
/// 独立自由函数便于单测注入临时文件路径。
fn subscribe_watch_once() {
    subscribe_watch_from(watch_file(), pool_manager());
}

fn watch_file() -> PathBuf {
    crate::modules::config::store_dir().join("autoclaw_watch.json")
}

fn subscribe_watch_from(path: PathBuf, mgr: &PoolManager) {
    let Some(root) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return; // watch 文件缺失/损坏：静默跳过本轮（watch 未运行属正常态）
    };
    let Some(entries) = root.get("accounts").and_then(Value::as_array) else {
        return;
    };
    for entry in entries {
        // 只订阅国际号（契约 §1：本模块只管 region=international）。
        if entry.get("region").and_then(Value::as_str) != Some("international") {
            continue;
        }
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let state = entry.get("state").and_then(Value::as_str).unwrap_or("ok");
        if state == "ok" || state == "error" {
            // ok：仅 invalid 复活路径需要映射；error：只观测不迁移。
            if state == "ok" {
                mgr.apply_watch_result(id, state, &Value::Null, "");
            }
            continue;
        }
        let code = entry.get("code").cloned().unwrap_or(Value::Null);
        let detail = entry
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let changed = mgr.apply_watch_result(id, state, &code, &detail);
        // 摘出轮换后水位检查（契约 §3.3/§3.5：healthy < minHealthy → 告警）。
        if changed && matches!(state, "banned" | "invalid") {
            let healthy = mgr.healthy_count();
            let min_healthy = mgr
                .policy_value()
                .get("minHealthy")
                .and_then(Value::as_u64)
                .unwrap_or(2) as usize;
            if healthy < min_healthy {
                notify::notify(
                    "autoclaw-intl-pool-low",
                    "AutoClaw 国际号池水位告警",
                    &format!(
                        "watch 探测 {state} 摘出后 healthy={healthy} < minHealthy={min_healthy}，请运行注册管线扩号"
                    ),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 单测：状态机纯逻辑（now 注入，不碰磁盘）；持久化用临时目录注入路径，
// 禁止写真实 ~/.wb-switch。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定测试基准时刻（任意历史时刻即可，日期/小时由 Local 换算）。
    const T0: i64 = 1_760_000_000;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-intl-pool-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 批量入池（token 剩余 24h，超过 300s 过滤与 2h 续期线）。
    fn join_n(st: &mut PoolState, ids: &[&str], now: i64) {
        for id in ids {
            st.join(id, &format!("{id}@x.io"), Some(T0 + 86_400), false, now);
        }
    }

    /// 取本地时区「次日 hour 点」的时间戳（与机器时区无关的确定性构造）。
    fn next_day_at(base: i64, hour: u32) -> i64 {
        use chrono::{TimeZone, Timelike};
        chrono::Local
            .timestamp_opt(base, 0)
            .single()
            .unwrap()
            .checked_add_signed(chrono::Duration::days(1))
            .unwrap()
            .with_hour(hour)
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_second(0))
            .unwrap()
            .timestamp()
    }

    // ---- 四种策略选号与跳过不可用 -------------------------------------

    #[test]
    fn round_robin_rotates_and_skips_unavailable() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a", "acct-b", "acct-c"], T0);
        let mut seen = Vec::new();
        for i in 0..3 {
            let lease = st.acquire("international", T0 + i).unwrap();
            seen.push(lease.account_id.clone());
            st.release(&lease, ReleaseOutcome::Ok, T0 + i);
        }
        assert_eq!(seen, ["acct-a", "acct-b", "acct-c"]); // BTreeMap 稳定序
        // 绕一圈回到 a（游标推进）。
        let lease = st.acquire("international", T0 + 10).unwrap();
        assert_eq!(lease.account_id, "acct-a");
        st.release(&lease, ReleaseOutcome::Ok, T0 + 10);
        // 停用 b → 游标跳过不可用号。
        assert!(st.set_enabled("acct-b", false, T0 + 11));
        let mut seen2 = Vec::new();
        for i in 0..4 {
            let lease = st.acquire("international", T0 + 12 + i).unwrap();
            seen2.push(lease.account_id.clone());
            st.release(&lease, ReleaseOutcome::Ok, T0 + 12 + i);
        }
        assert_eq!(seen2, ["acct-a", "acct-c", "acct-a", "acct-c"]);
    }

    #[test]
    fn random_only_picks_eligible() {
        let mut st = PoolState::default();
        st.policy.strategy = Strategy::Random;
        st.rng = 7;
        join_n(&mut st, &["acct-a", "acct-b", "acct-c"], T0);
        st.accounts.get_mut("acct-c").unwrap().state = AccountState::Banned;
        let mut picked = std::collections::HashSet::new();
        for i in 0..20 {
            let lease = st.acquire("international", T0 + i).unwrap();
            assert_ne!(lease.account_id, "acct-c"); // 不可用号绝不出现
            picked.insert(lease.account_id.clone());
            st.release(&lease, ReleaseOutcome::Ok, T0 + i);
        }
        assert!(picked.contains("acct-a") && picked.contains("acct-b"));
    }

    #[test]
    fn least_used_picks_min_today_calls() {
        let mut st = PoolState::default();
        st.policy.strategy = Strategy::LeastUsed;
        join_n(&mut st, &["acct-a", "acct-b", "acct-c"], T0);
        st.accounts.get_mut("acct-a").unwrap().today_calls = 5;
        st.accounts.get_mut("acct-b").unwrap().today_calls = 1;
        st.accounts.get_mut("acct-c").unwrap().today_calls = 3;
        let lease = st.acquire("international", T0).unwrap();
        assert_eq!(lease.account_id, "acct-b");
        st.release(&lease, ReleaseOutcome::Ok, T0); // b → 2
        let lease = st.acquire("international", T0 + 1).unwrap();
        assert_eq!(lease.account_id, "acct-b"); // b=2 仍最小
    }

    #[test]
    fn sticky_prefers_last_success_until_failure() {
        let mut st = PoolState::default();
        st.policy.strategy = Strategy::Sticky;
        join_n(&mut st, &["acct-a", "acct-b", "acct-c"], T0);
        // 无人成功过 → 游标兜底选 a；此后 sticky 停在 a。
        let lease = st.acquire("international", T0).unwrap();
        assert_eq!(lease.account_id, "acct-a");
        st.release(&lease, ReleaseOutcome::Ok, T0);
        for i in 1..3 {
            let l = st.acquire("international", T0 + i).unwrap();
            assert_eq!(l.account_id, "acct-a");
            st.release(&l, ReleaseOutcome::Ok, T0 + i);
        }
        // a 被封禁（失效）→ sticky 迁移到剩余可用号，并停在其上。
        let lease = st.acquire("international", T0 + 10).unwrap();
        st.release(
            &lease,
            ReleaseOutcome::Banned {
                code: json!(410004),
                detail: "banned".into(),
            },
            T0 + 10,
        );
        let l = st.acquire("international", T0 + 11).unwrap();
        let picked = l.account_id.clone();
        assert_ne!(picked, "acct-a");
        st.release(&l, ReleaseOutcome::Ok, T0 + 11);
        let l = st.acquire("international", T0 + 12).unwrap();
        assert_eq!(l.account_id, picked);
        st.release(&l, ReleaseOutcome::Ok, T0 + 12);
    }

    #[test]
    fn acquire_rejects_non_intl_lane() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        assert!(st.acquire("domestic", T0).is_none());
        let lease = st.acquire("intl", T0).unwrap(); // 别名接受
        assert_eq!(lease.lane, "international");
    }

    // ---- 六重过滤 -----------------------------------------------------

    #[test]
    fn acquire_applies_six_filters() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a", "acct-b", "acct-c", "acct-d", "acct-e"], T0);
        // 过滤 1：停用。
        st.accounts.get_mut("acct-a").unwrap().enabled = false;
        // 过滤 2：冷却未到期。
        let b = st.accounts.get_mut("acct-b").unwrap();
        b.cooldown_until = T0 + 60;
        b.state = AccountState::Cooldown;
        // 过滤 3：token 剩余 < 300s。
        st.accounts.get_mut("acct-c").unwrap().token_expire_at = Some(T0 + 200);
        // 过滤 4：日预算打满。
        st.accounts.get_mut("acct-d").unwrap().today_calls = st.policy.per_account_daily_cap;
        // acct-e 保持健康 → 唯一可选。
        let lease = st.acquire("international", T0).unwrap();
        assert_eq!(lease.account_id, "acct-e");
        st.release(&lease, ReleaseOutcome::Ok, T0);
        // 过滤 5：单号并发满（cap=2）→ 无可用号。
        st.accounts.get_mut("acct-e").unwrap().in_flight = st.policy.account_concurrent_cap;
        assert!(st.acquire("international", T0 + 1).is_none());
        // 冷却到期（过滤 2 解除）→ b 回到可选池。
        let lease = st.acquire("international", T0 + 61).unwrap();
        assert_eq!(lease.account_id, "acct-b");
        st.release(&lease, ReleaseOutcome::Ok, T0 + 61);
    }

    #[test]
    fn pool_concurrent_cap_blocks_acquire() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a", "acct-b"], T0);
        // 提高单号并发上限，隔离验证过滤 6（整池 cap=4）。
        st.policy.account_concurrent_cap = 5;
        st.accounts.get_mut("acct-a").unwrap().in_flight = 2;
        st.accounts.get_mut("acct-b").unwrap().in_flight = 2;
        assert!(st.acquire("international", T0).is_none());
    }

    // ---- 冷却退避与帽 ---------------------------------------------------

    #[test]
    fn rate_limit_backoff_caps_at_15min() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        let mut now = T0;
        let mut last_expected = 0i64;
        for fails in 0..9u32 {
            let lease = st.acquire("international", now).expect("冷却到期后应重新可选");
            st.release(&lease, ReleaseOutcome::RateLimited, now);
            let rt = st.accounts.get("acct-a").unwrap();
            assert_eq!(rt.state, AccountState::Cooldown);
            let expected = ((COOLDOWN_BASE_SECS * 1.5f64.powi(fails as i32)) as i64).min(COOLDOWN_CAP_SECS);
            assert_eq!(rt.cooldown_until - now, expected);
            last_expected = expected;
            now = rt.cooldown_until + 1;
        }
        // 第 7 档起触帽（60×1.5^7≈1025s > 900s），之后恒为 900s。
        assert_eq!(last_expected, COOLDOWN_CAP_SECS);
    }

    // ---- 810001 quota-out 零点复活 -------------------------------------

    #[test]
    fn quota_out_revives_after_reset_hour() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        let lease = st.acquire("international", T0).unwrap();
        st.release(&lease, ReleaseOutcome::QuotaExhausted, T0);
        assert_eq!(st.accounts["acct-a"].state, AccountState::QuotaOut);
        // 当天稍后：不复活。
        assert!(st.acquire("international", T0 + 3600).is_none());
        // 次日凌晨 1 点（默认 quotaResetHour=0 已过）→ 复活为 active。
        let next_1am = next_day_at(T0, 1);
        let lease = st.acquire("international", next_1am).unwrap();
        assert_eq!(lease.account_id, "acct-a");
        assert_eq!(st.accounts["acct-a"].state, AccountState::Active);
        st.release(&lease, ReleaseOutcome::Ok, next_1am);
        // quotaResetHour=5：次日 1 点不复活，7 点复活。
        let mut st2 = PoolState::default();
        st2.policy.quota_reset_hour = 5;
        join_n(&mut st2, &["acct-a"], T0);
        let lease = st2.acquire("international", T0).unwrap();
        st2.release(&lease, ReleaseOutcome::QuotaExhausted, T0);
        assert!(st2.acquire("international", next_day_at(T0, 1)).is_none());
        let lease = st2.acquire("international", next_day_at(T0, 7)).unwrap();
        assert_eq!(lease.account_id, "acct-a");
    }

    // ---- banned 摘除与水位 ---------------------------------------------

    #[test]
    fn banned_removed_from_rotation_and_water_level_notified() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a", "acct-b"], T0); // minHealthy 默认 2
        let lease = st.acquire("international", T0).unwrap();
        let effects = st.release(
            &lease,
            ReleaseOutcome::Banned {
                code: json!(410004),
                detail: "账号已被封禁".into(),
            },
            T0,
        );
        let rt = &st.accounts["acct-a"];
        assert_eq!(rt.state, AccountState::Banned);
        assert_eq!(rt.ban_code, Some(json!(410004))); // 证据保留
        assert_eq!(rt.banned_at, Some(T0));
        assert!(effects.low_water_notice.is_some()); // healthy 1 < 2
        assert!(st.events.iter().any(|e| e.kind == "ban" && e.account_id == "acct-a"));
        // banned 不再参与选号。
        for i in 0..3 {
            let lease = st.acquire("international", T0 + 1 + i).unwrap();
            assert_eq!(lease.account_id, "acct-b");
            st.release(&lease, ReleaseOutcome::Ok, T0 + 1 + i);
        }
        // 新号入池自动补位 → replacements+1，水位恢复。
        st.join("acct-c", "c@x.io", Some(T0 + 86_400), false, T0 + 10);
        assert_eq!(st.stats.replacements, 1);
        assert!(st.events.iter().any(|e| e.kind == "replace" && e.account_id == "acct-c"));
        let s = st.summary(T0 + 11);
        assert_eq!(s["healthy"], json!(2));
        assert_eq!(s["lowWater"], json!(false));
    }

    // ---- token 到期过滤与 TokenKeeper 扫描 ------------------------------

    #[test]
    fn token_expiry_filter_and_keeper_scan() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a", "acct-b", "acct-c"], T0);
        st.accounts.get_mut("acct-a").unwrap().token_expire_at = Some(T0 + 100); // <300s → 摘除
        // b 给冷却（隔离验证过滤 3；token 剩余 1h 在 2h 续期线内 → 扫描命中）。
        let b = st.accounts.get_mut("acct-b").unwrap();
        b.cooldown_until = T0 + 60;
        b.state = AccountState::Cooldown;
        b.token_expire_at = Some(T0 + 3600);
        st.accounts.get_mut("acct-c").unwrap().token_expire_at = Some(T0 + 6 * 3600); // >2h → 不命中
        let lease = st.acquire("international", T0).unwrap();
        assert_eq!(lease.account_id, "acct-c"); // a 被 token 到期过滤，b 被冷却过滤
        st.release(&lease, ReleaseOutcome::Ok, T0);
        let due = st.due_for_refresh(T0);
        assert_eq!(due, ["acct-a", "acct-b"]); // <2h 的在役号进入续期扫描
    }

    #[test]
    fn refresh_fails_three_times_marks_invalid_and_success_revives() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        assert!(!st.mark_refresh_failure("acct-a", "refresh 404", T0));
        assert!(!st.mark_refresh_failure("acct-a", "refresh 404", T0 + 1));
        assert!(st.mark_refresh_failure("acct-a", "refresh 404", T0 + 2)); // 第 3 次摘除
        assert_eq!(st.accounts["acct-a"].state, AccountState::Invalid);
        assert!(st.acquire("international", T0 + 3).is_none());
        // 续期成功 → invalid 复活。
        assert!(st.mark_refresh_success("acct-a", Some(T0 + 86_400), T0 + 4));
        assert_eq!(st.accounts["acct-a"].state, AccountState::Active);
        assert!(st.acquire("international", T0 + 5).is_some());
        assert_eq!(st.stats.refreshes, 1);
    }

    // ---- lease inFlight 计数 -------------------------------------------

    #[test]
    fn lease_in_flight_accounting_and_double_release() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        let l1 = st.acquire("international", T0).unwrap();
        let l2 = st.acquire("international", T0).unwrap();
        assert_eq!(st.accounts["acct-a"].in_flight, 2);
        // 单号并发 cap=2 → 第三次 None。
        assert!(st.acquire("international", T0).is_none());
        st.release(&l1, ReleaseOutcome::Ok, T0);
        assert_eq!(st.accounts["acct-a"].in_flight, 1);
        st.release(&l1, ReleaseOutcome::Ok, T0 + 1); // 重复 release 幂等忽略
        assert_eq!(st.accounts["acct-a"].in_flight, 1);
        st.release(&l2, ReleaseOutcome::Ok, T0 + 1);
        assert_eq!(st.accounts["acct-a"].in_flight, 0);
        // 今日计数只按成功 release 各记一次。
        assert_eq!(st.accounts["acct-a"].today_calls, 2);
    }

    #[test]
    fn daily_counter_resets_across_midnight() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        let lease = st.acquire("international", T0).unwrap();
        st.release(&lease, ReleaseOutcome::Ok, T0);
        assert_eq!(st.accounts["acct-a"].today_calls, 1);
        let next_day = next_day_at(T0, 10);
        let lease = st.acquire("international", next_day).unwrap();
        st.release(&lease, ReleaseOutcome::Ok, next_day);
        assert_eq!(st.accounts["acct-a"].today_calls, 1); // 跨零点重置后重新计数
        assert_eq!(st.stats.by_day.len(), 2);
        assert_eq!(st.stats.by_day.values().sum::<u64>(), 2);
    }

    // ---- events 环形上限 -------------------------------------------------

    #[test]
    fn events_ring_caps_at_100() {
        let mut st = PoolState::default();
        st.policy.min_healthy = 0; // 关掉补位事件，保证每号恰好 1 条 join 事件
        for i in 0..150 {
            st.join(&format!("acct-{i}"), "x@x.io", Some(T0 + 86_400), false, T0 + i as i64);
        }
        assert_eq!(st.events.len(), 100);
        assert_eq!(st.events[0].account_id, "acct-50"); // 最旧的 50 条被挤出
        assert_eq!(st.events[99].account_id, "acct-149");
        // 落盘/回读保持环形上限。
        let dir = temp_dir("ring");
        let path = dir.join("pool.json");
        write_state_file(&path, &st);
        let st2 = read_state_file(&path);
        assert_eq!(st2.events.len(), 100);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- 策略热更新 -------------------------------------------------------

    #[test]
    fn policy_hot_update_validates_and_applies() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        st.update_policy(&json!({
            "strategy": "sticky",
            "perAccountDailyCap": 100,
            "quotaResetHour": 5,
        }))
        .unwrap();
        assert_eq!(st.policy.strategy, Strategy::Sticky);
        assert_eq!(st.policy.per_account_daily_cap, 100);
        assert_eq!(st.policy.quota_reset_hour, 5);
        // 新日预算立即生效。
        st.accounts.get_mut("acct-a").unwrap().today_calls = 100;
        assert!(st.acquire("international", T0).is_none());
        // 非法补丁：报错且不应用。
        assert!(st.update_policy(&json!({"strategy": "bogus"})).is_err());
        assert!(st.update_policy(&json!({"quotaResetHour": 24})).is_err());
        assert!(st.update_policy(&json!({"unknown": 1})).is_err());
        assert_eq!(st.policy.strategy, Strategy::Sticky);
        assert_eq!(st.policy.per_account_daily_cap, 100);
    }

    // ---- 入池幂等 ---------------------------------------------------------

    #[test]
    fn join_is_idempotent_and_updates_token() {
        let mut st = PoolState::default();
        let r1 = st.join("acct-a", "a@x.io", Some(T0 + 1000), false, T0);
        assert_eq!(r1["joined"], json!(true));
        let r2 = st.join("acct-a", "a@x.io", Some(T0 + 200_000), true, T0 + 1);
        assert_eq!(r2["joined"], json!(false)); // 重复入池=更新令牌（§3.1）
        assert_eq!(st.accounts.len(), 1);
        assert_eq!(st.accounts["acct-a"].token_expire_at, Some(T0 + 200_000));
        assert_eq!(st.accounts["acct-a"].state, AccountState::Active);
    }

    // ---- 错误族映射 ---------------------------------------------------------

    #[test]
    fn outcome_from_http_maps_error_families() {
        assert!(matches!(outcome_from_http(200, "{}"), ReleaseOutcome::Ok));
        assert!(matches!(outcome_from_http(429, "rate"), ReleaseOutcome::RateLimited));
        assert!(matches!(outcome_from_http(503, "服务繁忙"), ReleaseOutcome::RateLimited));
        assert!(matches!(outcome_from_http(401, "{}"), ReleaseOutcome::AuthExpired));
        match outcome_from_http(403, r#"{"code":410004,"message":"banned"}"#) {
            ReleaseOutcome::Banned { code, .. } => assert_eq!(code, json!(410004)),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            outcome_from_http(403, r#"{"action":{"kind":"pay-view"},"code":810001}"#),
            ReleaseOutcome::QuotaExhausted
        ));
        assert!(matches!(outcome_from_http(500, "boom"), ReleaseOutcome::Failed(_)));
    }

    // ---- 持久化（临时目录注入；PoolManager 单例不被测试触碰） ---------------

    #[tokio::test]
    async fn persist_roundtrip_zeroes_inflight() {
        let dir = temp_dir("round");
        let path = dir.join("pool.json");
        let exp = now_secs() + 86_400;
        let mgr = PoolManager::with_debounce(path.clone(), Duration::from_millis(20));
        mgr.join("acct-a", "a@x.io", Some(exp), false);
        let lease = mgr.acquire("international").unwrap();
        mgr.persist_now();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["accounts"]["acct-a"]["inFlight"], json!(0)); // 落盘清零（§2.2）
        assert_eq!(raw["policy"]["strategy"], json!("round-robin"));
        assert_eq!(raw["cursor"]["lane"], json!("international"));
        assert_eq!(raw["cursor"]["offset"], json!(1)); // acquire 推进游标
        let st = read_state_file(&path);
        assert_eq!(st.accounts["acct-a"].in_flight, 0);
        assert_eq!(st.accounts["acct-a"].token_expire_at, Some(exp));
        // 回写后计数与事件落盘。
        mgr.release(&lease, ReleaseOutcome::Ok);
        mgr.persist_now();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["accounts"]["acct-a"]["todayCalls"], json!(1));
        assert_eq!(raw["stats"]["byDay"].as_object().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn debounce_save_writes_merged() {
        let dir = temp_dir("deb");
        let path = dir.join("pool.json");
        let mgr = PoolManager::with_debounce(path.clone(), Duration::from_millis(100));
        mgr.join("acct-a", "a@x.io", Some(now_secs() + 86_400), false);
        assert!(!path.exists()); // debounce 未到期，未写盘
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(path.exists());
        // 连续变更合并为一轮写（最终状态完整）。
        mgr.join("acct-b", "b@x.io", Some(now_secs() + 86_400), false);
        mgr.join("acct-c", "c@x.io", Some(now_secs() + 86_400), false);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let st = read_state_file(&path);
        assert_eq!(st.accounts.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn auth_expired_triggers_refresh_callback() {
        let dir = temp_dir("cb");
        let mgr = PoolManager::with_debounce(dir.join("pool.json"), Duration::from_millis(20));
        mgr.join("acct-a", "a@x.io", Some(now_secs() + 86_400), false);
        let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hits2 = hits.clone();
        mgr.set_refresh_callback(Arc::new(move |id| hits2.lock().unwrap().push(id.to_string())));
        let lease = mgr.acquire("international").unwrap();
        mgr.release(&lease, ReleaseOutcome::AuthExpired);
        assert_eq!(hits.lock().unwrap().as_slice(), ["acct-a"]); // 401 → 立即续期（§3.3）
        // 连续 3 次续期失败 → invalid 摘出轮换（§3.4）。
        assert!(!mgr.mark_refresh_failure("acct-a", "refresh 404"));
        assert!(!mgr.mark_refresh_failure("acct-a", "refresh 404"));
        assert!(mgr.mark_refresh_failure("acct-a", "refresh 404"));
        assert!(mgr.acquire("international").is_none());
        assert_eq!(mgr.rows()[0]["state"], json!("invalid"));
        // 续期成功 → 复活并回池。
        mgr.mark_refresh_success("acct-a", Some(now_secs() + 86_400));
        assert!(mgr.acquire("international").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn manager_policy_toggle_and_summary() {
        let dir = temp_dir("mgr");
        let mgr = PoolManager::with_debounce(dir.join("pool.json"), Duration::from_millis(20));
        let exp = now_secs() + 86_400;
        mgr.join("acct-a", "a@x.io", Some(exp), false);
        mgr.join("acct-b", "b@x.io", Some(exp), false);
        // 热更新策略（§3.6 policy）。
        let applied = mgr.update_policy(&json!({"strategy": "least-used"})).unwrap();
        assert_eq!(applied["strategy"], json!("least-used"));
        assert_eq!(mgr.policy_value()["strategy"], json!("least-used"));
        // toggle 停用 → 选不到；启用 → 恢复。
        assert!(mgr.set_enabled("acct-a", false));
        assert!(mgr.set_enabled("acct-b", false));
        assert!(mgr.acquire("international").is_none());
        let s = mgr.summary();
        assert_eq!(s["disabled"], json!(2));
        assert_eq!(s["lowWater"], json!(true)); // healthy 0 < 2
        assert!(mgr.set_enabled("acct-a", true));
        assert!(mgr.acquire("international").is_some());
        // 事件流（最新在前）。
        let events = mgr.events(50);
        assert!(events.iter().any(|e| e["kind"] == json!("disable")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- watch 订阅映射（契约 §3.7） -------------------------------------

    #[test]
    fn watch_banned_invalid_quota_map_into_state() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a", "acct-b"], T0);
        // banned：摘出 + 证据保留。
        assert!(st.apply_watch_result("acct-a", "banned", &json!(410004), "上游封禁", T0));
        let rt = &st.accounts["acct-a"];
        assert_eq!(rt.state, AccountState::Banned);
        assert_eq!(rt.ban_code, Some(json!(410004)));
        assert_eq!(rt.banned_at, Some(T0));
        assert!(st.events.iter().any(|e| e.kind == "ban" && e.account_id == "acct-a"));
        // banned 证据优先：后续 invalid/quota 不得降级覆盖。
        assert!(!st.apply_watch_result("acct-a", "invalid", &json!("401"), "x", T0 + 1));
        assert!(!st.apply_watch_result("acct-a", "quota", &json!(810001), "x", T0 + 1));
        assert_eq!(st.accounts["acct-a"].state, AccountState::Banned);
        // invalid：摘出。
        assert!(st.apply_watch_result("acct-b", "invalid", &json!("401"), "登录态失效", T0 + 2));
        assert_eq!(st.accounts["acct-b"].state, AccountState::Invalid);
    }

    #[test]
    fn watch_quota_maps_and_ok_revives_invalid() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        assert!(st.apply_watch_result("acct-a", "quota", &json!(810001), "pay-view", T0));
        assert_eq!(st.accounts["acct-a"].state, AccountState::QuotaOut);
        // ok 不把 quota-out 直接复活（quota-out 走 quotaResetHour 零点复活路径）。
        assert!(!st.apply_watch_result("acct-a", "ok", &Value::Null, "", T0 + 1));
        assert_eq!(st.accounts["acct-a"].state, AccountState::QuotaOut);
        // invalid 号被 watch 判 ok → 复活回役。
        st.accounts.get_mut("acct-a").unwrap().state = AccountState::Invalid;
        assert!(st.apply_watch_result("acct-a", "ok", &Value::Null, "", T0 + 2));
        assert_eq!(st.accounts["acct-a"].state, AccountState::Active);
    }

    #[test]
    fn watch_never_touches_disabled_or_unknown_accounts() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a"], T0);
        st.set_enabled("acct-a", false, T0);
        // disabled：管理员手动停用，watch 不越权。
        assert!(!st.apply_watch_result("acct-a", "banned", &json!(410004), "x", T0 + 1));
        assert_eq!(st.accounts["acct-a"].state, AccountState::Disabled);
        // 未知账号：静默忽略。
        assert!(!st.apply_watch_result("nope", "banned", &json!(410004), "x", T0 + 1));
        // error：只观测不迁移。
        st.set_enabled("acct-a", true, T0 + 2);
        assert!(!st.apply_watch_result("acct-a", "error", &json!(500), "boom", T0 + 3));
        assert_eq!(st.accounts["acct-a"].state, AccountState::Active);
    }

    // ---- purge 与 rotate（契约 §3.6 remove/rotate） -----------------------

    #[test]
    fn purge_removes_pool_entry_and_rotate_moves_cursor() {
        let mut st = PoolState::default();
        join_n(&mut st, &["acct-a", "acct-b"], T0);
        assert!(st.purge("acct-a", T0));
        assert!(!st.accounts.contains_key("acct-a"));
        assert!(!st.purge("acct-a", T0 + 1)); // 二次 purge 幂等
        // rotate 推进/重置。
        assert_eq!(st.rotate_cursor(false, 5, T0 + 2), Some(5));
        assert_eq!(st.rotate_cursor(true, 0, T0 + 3), Some(0));
        assert!(st.events.iter().any(|e| e.kind == "rotate"));
        // 池空 → rotate 返回 None。
        assert!(st.purge("acct-b", T0 + 4));
        assert!(st.rotate_cursor(false, 1, T0 + 5).is_none());
    }
}
