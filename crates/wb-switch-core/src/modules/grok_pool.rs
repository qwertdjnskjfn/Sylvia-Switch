//! Grok 账号池管理（v1.5.0）
//!
//! 四层职责，模块化、易扩展：
//! - 策略层：`PoolPolicy` 全部阈值/间隔可调，落盘 `grok_pool_policy.json`
//! - 健康层：healthScore(0-100) 随调用结果增减，风险等级由策略阈值推导
//! - 检查层：低频轮转体检（复用 A2 注入链路，串行于 UI_CHAT_LOCK）
//! - 生命层：异常账号自动降权；致命失效走 error 退避（见 grok.rs）
//!
//! 设计取舍：
//! - 主动行为越少越好。体检默认每 2 小时 1 个账号（轮转），行为接近真人。
//! - 不做自动封号式"硬刚"重试；触发限流一律退避。
//! - 无法保证"不封号"，只保证行为合规 + 尽早止损。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use crate::modules::config::{atomic_write, now_ms, store_dir};

// ---------------------------------------------------------------------------
// 策略层
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PoolPolicy {
    /// 是否启用自动体检循环
    pub auto_check_enabled: bool,
    /// 体检间隔（分钟）
    pub check_interval_minutes: u64,
    /// 每轮体检账号数（轮转）
    pub batch_size: usize,
    /// health >= 此值 = 低风险
    pub risk_low_threshold: i64,
    /// health < 此值 = 高风险
    pub risk_high_threshold: i64,
    /// 高风险账号是否自动禁用（熔断）
    pub pause_high_risk: bool,
    /// 成功一次的健康分增量
    pub success_delta: i64,
    /// 限流一次的健康分扣分
    pub rate_limit_penalty: i64,
    /// 超时一次的健康分扣分
    pub timeout_penalty: i64,
    /// 致命失效一次的健康分扣分
    pub fatal_penalty: i64,
    /// 池容量下限：可用账号数低于该值即触发容量守卫（告警 + 可选暂停调度）
    pub min_pool_size: usize,
    /// 低于容量下限时暂停调度（chat/test 选号直接失败并给出可读原因）
    pub pause_when_below_min: bool,
    /// 驻留时长（分钟，D4）：同一账号在该时段内不切号（grok_sso RESIDENT 语义，
    /// 原 grok_sso::RESIDENT_TTL_MS 常量迁入）。范围 5-120，默认 30。
    pub resident_ttl_min: u64,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            auto_check_enabled: true,
            check_interval_minutes: 120,
            batch_size: 1,
            risk_low_threshold: 80,
            risk_high_threshold: 50,
            pause_high_risk: false,
            success_delta: 5,
            rate_limit_penalty: 10,
            timeout_penalty: 8,
            fatal_penalty: 30,
            // 容量守卫默认值与现状语义兼容：min=1 时仅「无可用账号」触发。
            min_pool_size: 1,
            pause_when_below_min: true,
            // 驻留时长默认 30 分钟（与原 grok_sso::RESIDENT_TTL_MS 一致）。
            resident_ttl_min: 30,
        }
    }
}

fn policy_path() -> PathBuf {
    store_dir().join("grok_pool_policy.json")
}

/// 驻留时长（分钟）合法范围：5-120（D4）。
pub fn clamp_resident_ttl_min(v: u64) -> u64 {
    v.clamp(5, 120)
}

/// 从 JSON 对象解析策略，缺字段用 `d` 的默认值补齐（D4：旧配置文件无
/// `residentTtlMin` 字段时自然落到默认 30，天然向后兼容）。
/// 抽出为纯函数便于单测（不读盘、无环境依赖）。
fn policy_from_json(v: &Value, d: &PoolPolicy) -> PoolPolicy {
    PoolPolicy {
        auto_check_enabled: v.get("autoCheckEnabled").and_then(Value::as_bool).unwrap_or(d.auto_check_enabled),
        check_interval_minutes: v.get("checkIntervalMinutes").and_then(Value::as_u64).unwrap_or(d.check_interval_minutes),
        batch_size: v.get("batchSize").and_then(Value::as_u64).unwrap_or(d.batch_size as u64) as usize,
        risk_low_threshold: v.get("riskLowThreshold").and_then(Value::as_i64).unwrap_or(d.risk_low_threshold),
        risk_high_threshold: v.get("riskHighThreshold").and_then(Value::as_i64).unwrap_or(d.risk_high_threshold),
        pause_high_risk: v.get("pauseHighRisk").and_then(Value::as_bool).unwrap_or(d.pause_high_risk),
        success_delta: v.get("successDelta").and_then(Value::as_i64).unwrap_or(d.success_delta),
        rate_limit_penalty: v.get("rateLimitPenalty").and_then(Value::as_i64).unwrap_or(d.rate_limit_penalty),
        timeout_penalty: v.get("timeoutPenalty").and_then(Value::as_i64).unwrap_or(d.timeout_penalty),
        fatal_penalty: v.get("fatalPenalty").and_then(Value::as_i64).unwrap_or(d.fatal_penalty),
        min_pool_size: v.get("minPoolSize").and_then(Value::as_u64).unwrap_or(d.min_pool_size as u64) as usize,
        pause_when_below_min: v.get("pauseWhenBelowMin").and_then(Value::as_bool).unwrap_or(d.pause_when_below_min),
        resident_ttl_min: clamp_resident_ttl_min(
            v.get("residentTtlMin").and_then(Value::as_u64).unwrap_or(d.resident_ttl_min),
        ),
    }
}

/// 策略序列化为落盘 JSON（camelCase，与既有字段命名一致）。抽出为纯函数
/// 便于单测（save/load 键名往返不漂移）。
fn policy_to_json(p: &PoolPolicy, updated_at: i64) -> Value {
    json!({
        "autoCheckEnabled": p.auto_check_enabled,
        "checkIntervalMinutes": p.check_interval_minutes,
        "batchSize": p.batch_size,
        "riskLowThreshold": p.risk_low_threshold,
        "riskHighThreshold": p.risk_high_threshold,
        "pauseHighRisk": p.pause_high_risk,
        "successDelta": p.success_delta,
        "rateLimitPenalty": p.rate_limit_penalty,
        "timeoutPenalty": p.timeout_penalty,
        "fatalPenalty": p.fatal_penalty,
        "minPoolSize": p.min_pool_size,
        "pauseWhenBelowMin": p.pause_when_below_min,
        "residentTtlMin": p.resident_ttl_min,
        "updatedAt": updated_at,
    })
}

pub fn load_policy() -> PoolPolicy {
    let d = PoolPolicy::default();
    match std::fs::read_to_string(policy_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(v) => policy_from_json(&v, &d),
        None => d,
    }
}

pub fn save_policy(p: &PoolPolicy) -> Result<(), String> {
    let path = policy_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&policy_to_json(p, now_ms()))
        .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 健康层：评分与风险等级
// ---------------------------------------------------------------------------

/// 健康分增减（clamp 到 0-100）。
pub fn next_health(current: i64, delta: i64) -> i64 {
    (current + delta).clamp(0, 100)
}

/// 由健康分推导风险等级。
pub fn risk_level(health: i64, policy: &PoolPolicy) -> &'static str {
    if health >= policy.risk_low_threshold {
        "low"
    } else if health >= policy.risk_high_threshold {
        "medium"
    } else {
        "high"
    }
}

/// 确保账号带有健康字段（新账号/旧账号迁移）。返回是否做过修改。
pub fn ensure_health_fields(acc: &mut Value) {
    if acc.get("healthScore").is_none() {
        acc["healthScore"] = json!(100);
    }
    if acc.get("consecutiveFailures").is_none() {
        acc["consecutiveFailures"] = json!(0);
    }
    if acc.get("rateLimitCount").is_none() {
        acc["rateLimitCount"] = json!(0);
    }
    if acc.get("riskLevel").is_none() {
        acc["riskLevel"] = json!("low");
    }
}

// ---------------------------------------------------------------------------
// 检查层：轮转体检
// ---------------------------------------------------------------------------

/// 选出一个"最久未被体检"的 web 账号（按 lastCheckAt 升序，未体检者优先）。
fn pick_next_for_check(accounts: &[Value]) -> Option<String> {
    accounts
        .iter()
        .filter(|a| {
            a.get("provider").and_then(Value::as_str) == Some("web")
                && a.get("enabled").and_then(Value::as_bool).unwrap_or(true)
        })
        .min_by_key(|a| a.get("lastCheckAt").and_then(Value::as_i64).unwrap_or(0))
        .and_then(|a| a.get("id").and_then(Value::as_str).map(String::from))
}

/// 对单个账号做体检并回写健康字段。
pub async fn check_one_account(account_id: &str) -> Result<Value, String> {
    let accounts = crate::modules::grok::load_accounts();
    let acc = accounts
        .iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(account_id))
        .ok_or_else(|| format!("账号不存在: {account_id}"))?
        .clone();
    let provider = acc.get("provider").and_then(Value::as_str).unwrap_or("web");
    if provider != "web" {
        return Ok(json!({"ok": true, "skipped": "非 web 账号暂不体检", "accountId": account_id}));
    }
    let token = acc.get("token").and_then(Value::as_str).unwrap_or("");
    if token.is_empty() {
        return Err("账号 token 为空".to_string());
    }

    // D2：传入账号 ID —— 体检成功路径要写 RESIDENT 驻留（需要账号身份）。
    let probe = crate::modules::grok_sso::probe_account(account_id, token).await;
    let (ok, err_kind) = match probe {
        Ok(outcome) => (outcome.logged_in, if outcome.logged_in { "" } else { "not_logged_in" }),
        Err(_) => (false, "infra"),
    };

    let policy = load_policy();
    let now = now_ms();
    let mut accounts = crate::modules::grok::load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(account_id))
    {
        ensure_health_fields(item);
        item["lastCheckAt"] = json!(now);
        let cur = item.get("healthScore").and_then(Value::as_i64).unwrap_or(100);
        if ok {
            let h = next_health(cur, policy.success_delta);
            item["healthScore"] = json!(h);
            item["consecutiveFailures"] = json!(0);
            item["lastSuccessAt"] = json!(now);
            item["riskLevel"] = json!(risk_level(h, &policy));
            // 体检成功 = 登录态有效，清除 error 状态（自愈）。
            if item.get("status").and_then(Value::as_str) == Some("error") {
                item["status"] = json!("ok");
                item["error"] = Value::Null;
                item["errorRetryLevel"] = Value::Null;
                item["errorRetryAt"] = Value::Null;
            }
        } else {
            let penalty = match err_kind {
                "not_logged_in" => policy.fatal_penalty,
                "rate_limit" => policy.rate_limit_penalty,
                "timeout" => policy.timeout_penalty,
                _ => policy.success_delta,
            };
            let h = next_health(cur, -penalty);
            item["healthScore"] = json!(h);
            let cf = item.get("consecutiveFailures").and_then(Value::as_i64).unwrap_or(0) + 1;
            item["consecutiveFailures"] = json!(cf);
            item["lastErrorKind"] = json!(err_kind);
            let rl = risk_level(h, &policy);
            item["riskLevel"] = json!(rl);
            // 体检失败不立即标 error —— 探针本身可能因页面慢而误判。
            // 连续 2 次体检失败才升级为 error（进入退避），降低误伤。
            if err_kind == "not_logged_in" && cf >= 2 {
                item["status"] = json!("error");
                item["error"] = json!("体检连续失败：登录态疑似失效");
                let lvl = item.get("errorRetryLevel").and_then(Value::as_i64).unwrap_or(0) + 1;
                item["errorRetryLevel"] = json!(lvl.min(4));
                item["errorRetryAt"] = json!(now + 30 * 60 * 1000);
            }
            // 高风险熔断（策略可关）。
            if policy.pause_high_risk && rl == "high" {
                item["enabled"] = json!(false);
                item["error"] = json!("健康分过低，已自动熔断（可在策略中关闭）");
            }
        }
        item["updatedAt"] = json!(now);
    }
    crate::modules::grok::save_accounts(&accounts)?;

    Ok(json!({
        "ok": ok,
        "accountId": account_id,
        "errorKind": if ok { Value::Null } else { json!(err_kind) },
    }))
}

/// 体检一轮（batch_size 个账号）。
pub async fn run_check_batch() -> Value {
    let policy = load_policy();
    let accounts = crate::modules::grok::load_accounts();
    let web_count = accounts
        .iter()
        .filter(|a| a.get("provider").and_then(Value::as_str) == Some("web"))
        .count();
    if web_count == 0 {
        return json!({"status": "empty", "message": "账号池中没有 web 账号"});
    }

    let mut checked = Vec::new();
    for _ in 0..policy.batch_size.max(1) {
        let accounts = crate::modules::grok::load_accounts();
        let Some(id) = pick_next_for_check(&accounts) else { break };
        match check_one_account(&id).await {
            Ok(r) => checked.push(r),
            Err(e) => checked.push(json!({"accountId": id, "error": e})),
        }
    }
    // 无论手动还是自动触发，都记录快照 + 评估告警（供趋势图与及时预警）。
    let ov = pool_overview();
    record_snapshot(&ov);
    evaluate_alerts(&ov);
    json!({"status": "ok", "checked": checked, "webAccounts": web_count})
}

/// 体检循环：每 check_interval_minutes 触发一轮（策略可关）。
pub fn spawn_health_check_loop() {
    tokio::spawn(async move {
        // 启动后 15 分钟做首轮（避开同步/签到/保活高峰）。
        tokio::time::sleep(Duration::from_secs(900)).await;
        loop {
            let policy = load_policy();
            if policy.auto_check_enabled {
                // run_check_batch 内部已记录快照 + 评估告警。
                let _ = run_check_batch().await;
            }
            tokio::time::sleep(Duration::from_secs(policy.check_interval_minutes.max(15) * 60)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// C1 趋势记录层
// ---------------------------------------------------------------------------

const HISTORY_KEEP_MS: i64 = 7 * 24 * 3600 * 1000;

fn history_path() -> PathBuf {
    store_dir().join("grok_pool_history.json")
}

/// 追加一条池健康快照（供趋势图）。超过 7 天的记录自动裁剪。
pub fn record_snapshot(overview: &Value) {
    let path = history_path();
    let mut list: Vec<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("snapshots").and_then(Value::as_array).cloned())
        .unwrap_or_default();

    let total = overview.get("total").and_then(Value::as_i64).unwrap_or(0);
    let ok = overview.get("ok").and_then(Value::as_i64).unwrap_or(0);
    let errored = overview.get("errored").and_then(Value::as_i64).unwrap_or(0);
    let cooling = overview.get("cooling").and_then(Value::as_i64).unwrap_or(0);
    let risk = overview.get("risk").cloned().unwrap_or(json!({}));
    let avg_health = overview
        .get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            if arr.is_empty() {
                return 100;
            }
            let sum: i64 = arr
                .iter()
                .filter_map(|a| a.get("healthScore").and_then(Value::as_i64))
                .sum();
            sum / arr.len() as i64
        })
        .unwrap_or(100);

    let now = now_ms();
    list.push(json!({
        "at": now,
        "total": total,
        "ok": ok,
        "errored": errored,
        "cooling": cooling,
        "risk": risk,
        "avgHealth": avg_health,
    }));

    // 裁剪 7 天前的记录。
    list.retain(|s| {
        s.get("at")
            .and_then(Value::as_i64)
            .map(|t| now - t <= HISTORY_KEEP_MS)
            .unwrap_or(false)
    });

    let content = serde_json::to_string_pretty(&json!({"snapshots": list})).unwrap_or_default();
    let _ = atomic_write(&path, &content);
}

/// 读取趋势记录。
pub fn history() -> Value {
    std::fs::read_to_string(history_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({"snapshots": []}))
}

// ---------------------------------------------------------------------------
// C2 容量守卫层
// ---------------------------------------------------------------------------

/// 纯计算：给定账号列表/策略/时刻，返回 (可用数, 是否低于下限)。
/// 抽出为纯函数以便单测（不读盘、无环境依赖）。
pub(crate) fn compute_capacity(accounts: &[Value], policy: &PoolPolicy, now: i64) -> (usize, bool) {
    let available = accounts
        .iter()
        .filter(|a| crate::modules::grok::is_account_available(a, now))
        .count();
    let floor = policy.min_pool_size.max(1);
    (available, policy.pause_when_below_min && available < floor)
}

/// 容量守卫状态（供总览展示与选号层裁决）。
///
/// 可用性判定复用 `grok::is_account_available`（与选号层同一事实来源，
/// 避免两处漂移）。`belowMin` 仅在 `pauseWhenBelowMin` 开启时才算真——
/// 关闭时池浅只告警不暂停。
pub fn capacity_guard_state() -> Value {
    let policy = load_policy();
    let accounts = crate::modules::grok::load_accounts();
    let (available, below_min) = compute_capacity(&accounts, &policy, now_ms());
    json!({
        "minPoolSize": policy.min_pool_size.max(1),
        "pauseWhenBelowMin": policy.pause_when_below_min,
        "available": available,
        "belowMin": below_min,
    })
}

// ---------------------------------------------------------------------------
// C2 告警层
// ---------------------------------------------------------------------------

/// 告警状态去重：记录上次告警时的关键指标快照，避免重复告警。
/// 仅在"状态发生变化"（如可用数进一步下降）时才再次通知。
static LAST_ALERT: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

/// 评估池状态并按需发出桌面告警（notify 内部已按类别 10 分钟节流）。
///
/// 触发条件：
/// - 可用账号数低于策略阈值（默认 1，即无可用账号）
/// - 可用账号数低于容量下限 minPoolSize（pauseWhenBelowMin 开启时）
/// - 高风险账号占比超阈值
/// - 单账号被限流（由调用方直接在 mark 时告警，此处不重复）
pub fn evaluate_alerts(overview: &Value) {
    let total = overview.get("total").and_then(Value::as_i64).unwrap_or(0);
    let ok = overview.get("ok").and_then(Value::as_i64).unwrap_or(0);
    let high = overview
        .get("risk")
        .and_then(|r| r.get("high"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let errored = overview.get("errored").and_then(Value::as_i64).unwrap_or(0);

    if total == 0 {
        return;
    }

    // 条件 1：无可用账号（最严重）。
    if ok == 0 {
        crate::modules::notify::notify(
            "grok-pool",
            "Grok 账号池告急",
            &format!("当前无可用账号（共 {total} 个，{errored} 个异常），请检查账号池或补充新账号。"),
        );
    }

    // 条件 2：低于容量下限（pauseWhenBelowMin 开启时调度已暂停，需尽快补充）。
    if let Some(cap) = overview.get("capacity") {
        if cap.get("belowMin").and_then(Value::as_bool) == Some(true) {
            let floor = cap.get("minPoolSize").and_then(Value::as_i64).unwrap_or(1);
            let available = cap.get("available").and_then(Value::as_i64).unwrap_or(0);
            crate::modules::notify::notify(
                "grok-pool-capacity",
                "Grok 账号池低于容量下限",
                &format!("当前可用账号 {available} 个，低于容量下限 {floor}，调度已暂停；请补充账号或在策略中调整下限。"),
            );
        }
    }

    // 条件 2：高风险账号过半。
    if high > 0 && high * 2 >= total {
        crate::modules::notify::notify(
            "grok-pool-risk",
            "Grok 账号池风险升高",
            &format!("{high}/{total} 个账号处于高风险状态，建议降低调用频率或补充新账号。"),
        );
    }

    // 状态快照（供未来扩展：仅在指标变化时告警）。
    let mut guard = LAST_ALERT.lock().unwrap();
    *guard = Some(json!({"ok": ok, "high": high, "errored": errored, "at": now_ms()}));
}

// ---------------------------------------------------------------------------
// 总览
// ---------------------------------------------------------------------------

/// 账号池健康总览（供前端展示）。
pub fn pool_overview() -> Value {
    let policy = load_policy();
    let accounts = crate::modules::grok::load_accounts();
    let now = now_ms();

    let mut total = 0usize;
    let mut ok = 0usize;
    let mut errored = 0usize;
    let mut cooling = 0usize;
    let mut low = 0usize;
    let mut medium = 0usize;
    let mut high = 0usize;
    let mut details = Vec::new();

    for acc in &accounts {
        total += 1;
        let status = acc.get("status").and_then(Value::as_str).unwrap_or("ok");
        let cooldown = acc.get("cooldownUntil").and_then(Value::as_i64).unwrap_or(0);
        if status == "error" {
            errored += 1;
        } else if cooldown > now {
            cooling += 1;
        } else {
            ok += 1;
        }
        let health = acc.get("healthScore").and_then(Value::as_i64).unwrap_or(100);
        let rl = acc
            .get("riskLevel")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| risk_level(health, &policy).to_string());
        match rl.as_str() {
            "high" => high += 1,
            "medium" => medium += 1,
            _ => low += 1,
        }
        details.push(json!({
            "id": acc.get("id"),
            "name": acc.get("name"),
            "provider": acc.get("provider"),
            "status": status,
            "healthScore": health,
            "riskLevel": rl,
            "consecutiveFailures": acc.get("consecutiveFailures").and_then(Value::as_i64).unwrap_or(0),
            "rateLimitCount": acc.get("rateLimitCount").and_then(Value::as_i64).unwrap_or(0),
            "lastSuccessAt": acc.get("lastSuccessAt"),
            "lastCheckAt": acc.get("lastCheckAt"),
            "lastErrorKind": acc.get("lastErrorKind"),
            "cooldownUntil": cooldown,
        }));
    }

    json!({
        "total": total,
        "ok": ok,
        "errored": errored,
        "cooling": cooling,
        "risk": {"low": low, "medium": medium, "high": high},
        "capacity": capacity_guard_state(),
        "policy": {
            "autoCheckEnabled": policy.auto_check_enabled,
            "checkIntervalMinutes": policy.check_interval_minutes,
            "batchSize": policy.batch_size,
            "riskLowThreshold": policy.risk_low_threshold,
            "riskHighThreshold": policy.risk_high_threshold,
            "pauseHighRisk": policy.pause_high_risk,
        },
        "accounts": details,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(id: &str, enabled: bool, cooldown_until: i64, status: &str, error_retry_at: i64) -> Value {
        json!({
            "id": id,
            "enabled": enabled,
            "cooldownUntil": cooldown_until,
            "status": status,
            "errorRetryAt": error_retry_at,
        })
    }

    /// 容量矩阵：可用性判定必须与选号层一致（启用+未冷却+error 退避期已过），
    /// 且 belowMin 只在 pauseWhenBelowMin 开启时才为真。
    #[test]
    fn capacity_matrix_matches_availability() {
        let now = 1_000_000i64;
        let accounts = vec![
            acc("a", true, 0, "ok", 0),                    // 可用
            acc("b", true, now + 60_000, "cooldown", 0),   // 冷却中 → 不可用
            acc("c", false, 0, "ok", 0),                   // 手动禁用 → 不可用
            acc("d", true, 0, "error", now + 60_000),      // error 退避期内 → 不可用
            acc("e", true, 0, "error", now - 1),           // 退避期已过 → 可用（假 error 自愈）
        ];
        let mut p = PoolPolicy::default();

        // 开启暂停：2 个可用 < 下限 3 → 触发守卫。
        p.min_pool_size = 3;
        p.pause_when_below_min = true;
        let (available, below) = compute_capacity(&accounts, &p, now);
        assert_eq!(available, 2);
        assert!(below);

        // 关闭暂停：池浅只告警，不判 belowMin。
        p.pause_when_below_min = false;
        let (available2, below2) = compute_capacity(&accounts, &p, now);
        assert_eq!(available2, 2);
        assert!(!below2);

        // 下限降到 2：恰好达标 → 不触发。
        p.min_pool_size = 2;
        p.pause_when_below_min = true;
        let (_, below3) = compute_capacity(&accounts, &p, now);
        assert!(!below3);

        // 空池 + min=0（被钳到 1）→ 必然低于下限。
        p.min_pool_size = 0;
        let (av0, below0) = compute_capacity(&[], &p, now);
        assert_eq!(av0, 0);
        assert!(below0);

        // 默认策略与旧语义兼容：min=1 时仅「无可用账号」触发。
        let d = PoolPolicy::default();
        assert_eq!(d.min_pool_size, 1);
        assert!(d.pause_when_below_min);
        let (av_ok, below_ok) = compute_capacity(&accounts, &d, now);
        assert_eq!(av_ok, 2);
        assert!(!below_ok);
    }

    /// D4：驻留时长默认 30 分钟，clamp 范围 5-120。
    #[test]
    fn resident_ttl_default_is_30_and_clamps_to_5_120() {
        assert_eq!(PoolPolicy::default().resident_ttl_min, 30);
        assert_eq!(clamp_resident_ttl_min(0), 5);
        assert_eq!(clamp_resident_ttl_min(4), 5);
        assert_eq!(clamp_resident_ttl_min(5), 5);
        assert_eq!(clamp_resident_ttl_min(30), 30);
        assert_eq!(clamp_resident_ttl_min(45), 45);
        assert_eq!(clamp_resident_ttl_min(120), 120);
        assert_eq!(clamp_resident_ttl_min(121), 120);
        assert_eq!(clamp_resident_ttl_min(u64::MAX), 120);
    }

    /// D4：旧配置文件缺 residentTtlMin 字段 → 用默认 30（向后兼容）；
    /// 新文件带值 → 落盘前 clamp 到 5-120。
    #[test]
    fn policy_json_missing_resident_ttl_uses_default_and_clamps() {
        let d = PoolPolicy::default();
        // 旧文件：除 residentTtlMin 外的字段照常解析，缺失字段补默认。
        let old = policy_from_json(&json!({"batchSize": 2, "checkIntervalMinutes": 90}), &d);
        assert_eq!(old.resident_ttl_min, 30);
        assert_eq!(old.batch_size, 2);
        assert_eq!(old.check_interval_minutes, 90);
        // 带值但越界 → clamp。
        assert_eq!(
            policy_from_json(&json!({"residentTtlMin": 999}), &d).resident_ttl_min,
            120
        );
        assert_eq!(policy_from_json(&json!({"residentTtlMin": 1}), &d).resident_ttl_min, 5);
        assert_eq!(
            policy_from_json(&json!({"residentTtlMin": 45}), &d).resident_ttl_min,
            45
        );
        // 字段类型异常（字符串）→ 用默认。
        assert_eq!(
            policy_from_json(&json!({"residentTtlMin": "45"}), &d).resident_ttl_min,
            30
        );
    }

    /// D4：save 的键名与 load 的解析往返一致（residentTtlMin 不漂移）。
    #[test]
    fn policy_json_roundtrip_keeps_resident_ttl() {
        let mut p = PoolPolicy::default();
        p.resident_ttl_min = 45;
        let v = policy_to_json(&p, 1_000);
        assert_eq!(v.get("residentTtlMin").and_then(Value::as_u64), Some(45));
        let back = policy_from_json(&v, &PoolPolicy::default());
        assert_eq!(back.resident_ttl_min, 45);
        // 默认策略往返不变形。
        let d = policy_from_json(&policy_to_json(&PoolPolicy::default(), 0), &PoolPolicy::default());
        assert_eq!(d.resident_ttl_min, 30);
    }
}
