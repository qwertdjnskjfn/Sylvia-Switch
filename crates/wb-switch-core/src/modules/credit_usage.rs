//! 本地积分观察快照与统计投影。
//!
//! WorkBuddy 只返回当前资源余额，没有可复用的历史账单序列。因此这里把
//! 成功查询到的余额保存为本地观察值，再用相邻快照的正向下降量推导“观察到
//! 的消耗”。首次快照和余额增加只建立新的基线，不产生负数消耗。

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, TimeZone};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::modules::account::account_display_name;
use crate::modules::config::{
    atomic_write, credit_usage_snapshots_file, load_checkin_logs, now_ms, store_dir,
    CHECKIN_LOG_KEEP_DAYS,
};
use crate::modules::official_usage;

pub const CREDIT_SNAPSHOT_RETENTION_DAYS: i64 = 90;
pub const CREDIT_SNAPSHOT_MAX_RECORDS: usize = 5_000;
pub const CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS: i64 = 5 * 60 * 1000;
const CREDIT_STATS_MAX_EVENTS: usize = 200;

static SNAPSHOT_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
struct Snapshot {
    ts: i64,
    account_id: String,
    account_name: String,
    total: f64,
    remaining: f64,
}

#[derive(Clone, Debug)]
struct UsageEvent {
    ts: i64,
    date: String,
    account_id: String,
    amount: f64,
}

impl UsageEvent {
    fn new(ts: i64, account_id: String, amount: f64) -> Self {
        Self {
            ts,
            date: local_date_string(ts).unwrap_or_default(),
            account_id,
            amount,
        }
    }
}

#[derive(Clone, Debug)]
struct CheckinEvent {
    ts: i64,
    date: String,
    account_id: Option<String>,
    account_name: String,
    result: String,
    error: Option<String>,
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(value)) => value.as_f64(),
        Some(Value::String(value)) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn snapshot_from_value(value: &Value) -> Option<Snapshot> {
    let account_id = non_empty_string(value.get("accountId"))?;
    let ts = value.get("ts").and_then(Value::as_i64)?;
    let total = number(value.get("total"))?.max(0.0);
    let remaining = number(value.get("remaining"))?.max(0.0);
    Some(Snapshot {
        ts,
        account_id,
        account_name: non_empty_string(value.get("accountName"))
            .unwrap_or_else(|| "unknown".to_string()),
        total,
        remaining,
    })
}

fn snapshot_value(
    ts: i64,
    account_id: &str,
    account_name: &str,
    total: f64,
    remaining: f64,
) -> Value {
    json!({
        "ts": ts,
        "accountId": account_id,
        "accountName": account_name,
        "total": total.max(0.0),
        "remaining": remaining.max(0.0),
    })
}

/// 读取本地观察快照；文件缺失或损坏时返回空列表。
pub fn load_snapshots() -> Vec<Value> {
    let path = credit_usage_snapshots_file();
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) {
            return items;
        }
    }
    vec![]
}

fn should_suppress_duplicate(
    snapshots: &[Value],
    account_id: &str,
    total: f64,
    remaining: f64,
    at_ms: i64,
) -> bool {
    let Some(latest) = snapshots
        .iter()
        .filter_map(snapshot_from_value)
        .filter(|snapshot| snapshot.account_id == account_id)
        .max_by_key(|snapshot| snapshot.ts)
    else {
        return false;
    };

    latest.ts <= at_ms
        && at_ms - latest.ts <= CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS
        && (latest.total - total).abs() < f64::EPSILON
        && (latest.remaining - remaining).abs() < f64::EPSILON
}

fn normalize_snapshots(snapshots: &[Value], at_ms: i64) -> Vec<Value> {
    let cutoff = at_ms.saturating_sub(CREDIT_SNAPSHOT_RETENTION_DAYS * 24 * 3600 * 1000);
    let mut kept: Vec<Value> = snapshots
        .iter()
        .filter_map(|value| {
            let snapshot = snapshot_from_value(value)?;
            (snapshot.ts >= cutoff && snapshot.ts <= at_ms).then_some(value.clone())
        })
        .collect();
    if kept.len() > CREDIT_SNAPSHOT_MAX_RECORDS {
        kept.drain(..kept.len() - CREDIT_SNAPSHOT_MAX_RECORDS);
    }
    kept
}

/// 写入一个成功的资源观察值。
///
/// 同一账号同一资源值在短时间内只保留一条；资源值发生变化时立即保留，
/// 这样余额下降可以归因到新快照。返回值表示本次是否实际写入。
pub fn record_snapshot(account_id: &str, account_name: &str, total: f64, remaining: f64) -> bool {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return false;
    }

    let at_ms = now_ms();
    let _guard = SNAPSHOT_WRITE_LOCK.lock().unwrap();
    let snapshots = load_snapshots();
    if should_suppress_duplicate(&snapshots, account_id, total, remaining, at_ms) {
        return false;
    }

    let mut kept = normalize_snapshots(&snapshots, at_ms);
    kept.push(snapshot_value(
        at_ms,
        account_id,
        account_name.trim(),
        total,
        remaining,
    ));
    if kept.len() > CREDIT_SNAPSHOT_MAX_RECORDS {
        kept.drain(..kept.len() - CREDIT_SNAPSHOT_MAX_RECORDS);
    }

    if let Err(error) = std::fs::create_dir_all(store_dir()).and_then(|_| {
        let content = serde_json::to_string_pretty(&kept).unwrap_or_default();
        atomic_write(&credit_usage_snapshots_file(), &content)
    }) {
        eprintln!("[积分统计] 保存积分快照失败: {error}");
        return false;
    }
    true
}

fn local_date(ts: i64) -> Option<NaiveDate> {
    Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|date| date.date_naive())
}

fn local_date_string(ts: i64) -> Option<String> {
    local_date(ts).map(|date| date.format("%Y-%m-%d").to_string())
}

/// 生成逐日观察序列（daily_start..=today，无数据的天补 0）；无快照起点时返回空数组。
fn local_daily_series(
    daily: &HashMap<String, f64>,
    daily_start: Option<NaiveDate>,
    today: NaiveDate,
) -> Vec<Value> {
    let Some(start) = daily_start else {
        return Vec::new();
    };
    let mut points = Vec::new();
    let mut date = start;
    while date <= today {
        let key = date.format("%Y-%m-%d").to_string();
        points.push(json!({
            "date": key,
            "usage": daily.get(&key).copied().unwrap_or(0.0),
        }));
        date += ChronoDuration::days(1);
    }
    points
}

fn parse_checkin_event(value: &Value) -> Option<CheckinEvent> {
    let ts = value
        .get("ts")
        .and_then(Value::as_i64)
        .or_else(|| crate::modules::config::norm_ts(value.get("ts")))?;
    Some(CheckinEvent {
        ts,
        date: local_date_string(ts)?,
        account_id: non_empty_string(value.get("accountId")),
        account_name: non_empty_string(value.get("email")).unwrap_or_else(|| "unknown".to_string()),
        result: non_empty_string(value.get("result")).unwrap_or_else(|| "error".to_string()),
        error: non_empty_string(value.get("error")),
    })
}

fn parse_snapshots(values: &[Value], at_ms: i64) -> Vec<Snapshot> {
    let cutoff = at_ms.saturating_sub(CREDIT_SNAPSHOT_RETENTION_DAYS * 24 * 3600 * 1000);
    let mut snapshots: Vec<Snapshot> = values
        .iter()
        .filter_map(snapshot_from_value)
        .filter(|snapshot| snapshot.ts >= cutoff && snapshot.ts <= at_ms)
        .collect();
    snapshots.sort_by_key(|snapshot| snapshot.ts);
    snapshots
}

fn usage_in_windows(date: &str, today: NaiveDate) -> (bool, bool, bool) {
    let Some(date) = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok() else {
        return (false, false, false);
    };
    let distance = (today - date).num_days();
    (
        distance == 0,
        (0..7).contains(&distance),
        date.year() == today.year() && date.month() == today.month(),
    )
}

fn add_account_name(
    account_ids: &mut Vec<String>,
    account_names: &mut HashMap<String, String>,
    account_id: String,
    account_name: String,
) {
    if !account_ids.contains(&account_id) {
        account_ids.push(account_id.clone());
    }
    let should_replace = account_names
        .get(&account_id)
        .map(|name| name == "unknown" && account_name != "unknown")
        .unwrap_or(true);
    if should_replace {
        account_names.insert(account_id, account_name);
    }
}

fn checkin_identity(event: &CheckinEvent) -> Option<String> {
    if let Some(account_id) = event.account_id.as_ref() {
        return Some(format!("account:{account_id}"));
    }
    (event.account_name != "unknown").then(|| format!("legacy:{}", event.account_name))
}

fn build_statistics(
    snapshot_values: &[Value],
    checkin_values: &[Value],
    accounts: &[Value],
    at_ms: i64,
    official_events: &[UsageEvent],
    official_windows: &HashMap<String, (f64, f64, f64)>,
) -> Value {
    let snapshots = parse_snapshots(snapshot_values, at_ms);
    let today = local_date(at_ms).unwrap_or_else(|| Local::now().date_naive());
    let checkin_cutoff = at_ms.saturating_sub(CHECKIN_LOG_KEEP_DAYS * 24 * 3600 * 1000);
    let checkins: Vec<CheckinEvent> = checkin_values
        .iter()
        .filter_map(parse_checkin_event)
        .filter(|event| event.ts >= checkin_cutoff && event.ts <= at_ms)
        .collect();

    let mut account_ids = Vec::new();
    let mut current_account_ids = Vec::new();
    let mut account_names = HashMap::new();
    let mut account_platforms = HashMap::new();
    for account in accounts {
        if let Some(id) = non_empty_string(account.get("id")) {
            if !current_account_ids.contains(&id) {
                current_account_ids.push(id.clone());
            }
            add_account_name(
                &mut account_ids,
                &mut account_names,
                id.clone(),
                account_display_name(account),
            );
            account_platforms.insert(
                id,
                crate::modules::platform_checkin::account_platform(account).to_string(),
            );
        }
    }
    for snapshot in &snapshots {
        add_account_name(
            &mut account_ids,
            &mut account_names,
            snapshot.account_id.clone(),
            snapshot.account_name.clone(),
        );
    }
    for event in &checkins {
        if let Some(account_id) = &event.account_id {
            add_account_name(
                &mut account_ids,
                &mut account_names,
                account_id.clone(),
                event.account_name.clone(),
            );
        }
    }

    let mut by_account: HashMap<String, Vec<Snapshot>> = HashMap::new();
    for snapshot in snapshots {
        by_account
            .entry(snapshot.account_id.clone())
            .or_default()
            .push(snapshot);
    }
    let coverage_start_at = by_account
        .values()
        .flat_map(|snapshots| snapshots.iter().map(|snapshot| snapshot.ts))
        .min();

    let mut usage_events = Vec::new();
    let mut usage_totals: HashMap<String, (f64, f64, f64)> = HashMap::new();
    let mut daily_usage: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut latest_snapshots = HashMap::new();
    for (account_id, mut snapshots) in by_account {
        snapshots.sort_by_key(|snapshot: &Snapshot| snapshot.ts);
        if let Some(latest) = snapshots.last() {
            latest_snapshots.insert(account_id.clone(), latest.clone());
        }
        // 消耗口径分平台（见 get_statistics 文档）：
        // - WorkBuddy / AutoClaw 的消耗来自官方逐请求明细与流水账（已在
        //   `official_events` 注入），余额差会把包到期、积分发放、资源包
        //   重组统统误判为消耗，彻底弃用；
        // - TRAE 没有逐请求官方接口，保留快照差值，但剔除总额度变动的
        //   窗口（包到期/新包发放时的余额变化不是消耗）。
        // 无平台信息的快照账号历史上就是 WorkBuddy 账号，同样不适用余额差。
        if account_platforms.get(&account_id).map(String::as_str) != Some("trae_work_cn") {
            continue;
        }
        for pair in snapshots.windows(2) {
            let previous = &pair[0];
            let current = &pair[1];
            let amount = previous.remaining - current.remaining;
            if amount <= f64::EPSILON {
                continue;
            }
            // 总额度变了 = 包到期/发放/调整窗口，余额变化≠消耗，整窗剔除。
            if (previous.total - current.total).abs() > f64::EPSILON {
                continue;
            }
            let Some(date) = local_date_string(current.ts) else {
                continue;
            };
            let entry = usage_totals.entry(account_id.clone()).or_default();
            let (today_usage, week_usage, month_usage) = usage_in_windows(&date, today);
            if today_usage {
                entry.0 += amount;
            }
            if week_usage {
                entry.1 += amount;
            }
            if month_usage {
                entry.2 += amount;
            }
            *daily_usage
                .entry(account_id.clone())
                .or_default()
                .entry(date.clone())
                .or_default() += amount;
            usage_events.push(UsageEvent {
                ts: current.ts,
                date,
                account_id: account_id.clone(),
                amount,
            });
        }
    }

    // WorkBuddy（官方逐请求明细）与 AutoClaw（官方流水 debit）的消耗事件：
    // 时间、金额、账号全部来自官方数据。
    for event in official_events {
        let entry = usage_totals.entry(event.account_id.clone()).or_default();
        let (today_usage, week_usage, month_usage) = usage_in_windows(&event.date, today);
        if today_usage {
            entry.0 += event.amount;
        }
        if week_usage {
            entry.1 += event.amount;
        }
        if month_usage {
            entry.2 += event.amount;
        }
        *daily_usage
            .entry(event.account_id.clone())
            .or_default()
            .entry(event.date.clone())
            .or_default() += event.amount;
        usage_events.push(event.clone());
        add_account_name(
            &mut account_ids,
            &mut account_names,
            event.account_id.clone(),
            event.account_id.clone(),
        );
    }

    // WorkBuddy 账号的窗口合计改用官方账号行（全量聚合）覆盖。
    //
    // 事件流来自官方「请求明细」，而官方每账号只返回最近 100 条明细；用它反推
    // 近 7 天 / 本月会把超过 100 条的历史请求漏掉，表现为同一页面上「积分总览」
    // 与「按模型分类」两个口径对不上。官方账号行的 usageToday / usage7Days /
    // usageThisMonth 是不受明细条数限制的全量聚合，因此以它为准。
    for (account_id, (today_usage, week_usage, month_usage)) in official_windows {
        add_account_name(
            &mut account_ids,
            &mut account_names,
            account_id.clone(),
            account_id.clone(),
        );
        usage_totals.insert(
            account_id.clone(),
            (*today_usage, *week_usage, *month_usage),
        );
    }

    let mut checkin_today_latest: HashMap<String, (i64, String)> = HashMap::new();
    let mut today_success = 0;
    let mut today_already = 0;
    let mut today_failed = 0;
    for event in &checkins {
        if event.date != today.format("%Y-%m-%d").to_string() {
            continue;
        }
        match event.result.as_str() {
            "success" => today_success += 1,
            "already" => today_already += 1,
            _ => today_failed += 1,
        }
        if let Some(identity) = checkin_identity(event) {
            if checkin_today_latest
                .get(&identity)
                .map(|(ts, _)| *ts <= event.ts)
                .unwrap_or(true)
            {
                checkin_today_latest.insert(identity, (event.ts, event.result.clone()));
            }
        }
    }

    let today_checked_in_accounts = checkin_today_latest
        .values()
        .filter(|(_, result)| result == "success" || result == "already")
        .count();
    let today_key = today.format("%Y-%m-%d").to_string();

    let mut current_remaining = 0.0;
    let mut current_capacity = 0.0;
    for account_id in &current_account_ids {
        if let Some(snapshot) = latest_snapshots.get(account_id) {
            current_remaining += snapshot.remaining;
            current_capacity += snapshot.total;
        }
    }

    let mut last_checkins: HashMap<String, CheckinEvent> = HashMap::new();
    for event in &checkins {
        let Some(account_id) = &event.account_id else {
            continue;
        };
        if last_checkins
            .get(account_id)
            .map(|current: &CheckinEvent| current.ts <= event.ts)
            .unwrap_or(true)
        {
            last_checkins.insert(account_id.clone(), event.clone());
        }
    }

    // 逐日序列起点：全局最早快照与保留窗口下界的较大者；无快照时为 None（返回空序列）
    let daily_start = coverage_start_at.and_then(local_date).map(|coverage_date| {
        let earliest = today - ChronoDuration::days(CREDIT_SNAPSHOT_RETENTION_DAYS - 1);
        coverage_date.max(earliest)
    });
    let empty_account_daily: HashMap<String, f64> = HashMap::new();

    let account_summaries: Vec<Value> = account_ids
        .iter()
        .map(|account_id| {
            let latest = latest_snapshots.get(account_id);
            let is_current = current_account_ids.contains(account_id);
            let (usage_today, usage_week, usage_month) = usage_totals
                .get(account_id)
                .copied()
                .unwrap_or_default();
            let today_checkin = checkins
                .iter()
                .filter(|event| {
                    event.account_id.as_deref() == Some(account_id.as_str())
                        && event.date == today_key
                })
                .max_by_key(|event| event.ts);
            let last_checkin = last_checkins.get(account_id);
            json!({
                "accountId": account_id,
                "accountName": account_names.get(account_id).cloned().unwrap_or_else(|| "unknown".to_string()),
                "platform": account_platforms.get(account_id).map(String::as_str),
                "isCurrent": is_current,
                "currentRemaining": is_current.then(|| latest.map(|snapshot| snapshot.remaining)).flatten(),
                "totalCapacity": is_current.then(|| latest.map(|snapshot| snapshot.total)).flatten(),
                "lastSnapshotAt": latest.map(|snapshot| snapshot.ts),
                "usageToday": usage_today,
                "usage7Days": usage_week,
                "usageThisMonth": usage_month,
                "checkedInToday": today_checkin.map(|event| event.result == "success" || event.result == "already"),
                "checkinStatusToday": today_checkin.map(|event| event.result.clone()),
                "lastCheckinAt": last_checkin.map(|event| event.ts),
                "lastCheckinResult": last_checkin.map(|event| event.result.clone()),
                "daily": local_daily_series(
                    daily_usage.get(account_id).unwrap_or(&empty_account_daily),
                    daily_start,
                    today,
                ),
            })
        })
        .collect();

    let mut aggregate_daily: HashMap<String, f64> = HashMap::new();
    for account_daily in daily_usage.values() {
        for (date, amount) in account_daily {
            *aggregate_daily.entry(date.clone()).or_insert(0.0) += amount;
        }
    }
    let daily = local_daily_series(&aggregate_daily, daily_start, today);

    let mut events: Vec<(i64, Value)> = usage_events
        .iter()
        .map(|event| {
            (
                event.ts,
                json!({
                    "kind": "usage",
                    "ts": event.ts,
                    "date": event.date,
                    "accountId": event.account_id,
                    "accountName": account_names.get(&event.account_id).cloned().unwrap_or_else(|| "unknown".to_string()),
                    "amount": event.amount,
                }),
            )
        })
        .collect();
    events.extend(checkins.iter().map(|event| {
        (
            event.ts,
            json!({
                "kind": "checkin",
                "ts": event.ts,
                "date": event.date,
                "accountId": event.account_id,
                "accountName": event.account_id.as_ref().and_then(|id| account_names.get(id)).cloned().unwrap_or_else(|| event.account_name.clone()),
                "result": event.result,
                "error": event.error,
            }),
        )
    }));
    events.sort_by_key(|event| Reverse(event.0));
    let recent_events: Vec<Value> = events
        .into_iter()
        .take(CREDIT_STATS_MAX_EVENTS)
        .map(|(_, event)| event)
        .collect();

    json!({
        "generatedAt": at_ms,
        "retentionDays": CREDIT_SNAPSHOT_RETENTION_DAYS,
        "coverageStartAt": coverage_start_at,
        "summary": {
            "currentRemaining": current_remaining,
            "currentCapacity": current_capacity,
            "usageToday": usage_totals.values().map(|value| value.0).sum::<f64>(),
            "usage7Days": usage_totals.values().map(|value| value.1).sum::<f64>(),
            "usageThisMonth": usage_totals.values().map(|value| value.2).sum::<f64>(),
            "todayCheckedInAccounts": today_checked_in_accounts,
            "todaySuccess": today_success,
            "todayAlready": today_already,
            "todayFailed": today_failed,
        },
        "daily": daily,
        "accounts": account_summaries,
        "events": recent_events,
    })
}

/// 返回本地快照、账号列表、签到日志与官方请求用量的统一统计投影。
///
/// 消耗口径（每个平台用最可信的官方数据，不再用余额差猜）：
/// - WorkBuddy：官方逐请求明细（get-user-request-usage，精确到每次调用）；
/// - AutoClaw：官方流水账"模型消耗积分" debit 条目（按账号 token 查账）；
/// - TRAE：无逐请求官方数据，保留快照差值（剔除总额度变动的窗口）。
///
/// `refresh = false` 时官方数据读本地缓存，不打官方接口；
/// 只有统计页「刷新统计」传入 `refresh = true` 才会重新采集。
pub async fn get_statistics(refresh: bool) -> Value {
    let at_ms = now_ms();
    let accounts = crate::modules::platform_checkin::all_checkin_accounts();
    // 平台账号（AutoClaw / TRAE Work CN）的积分/额度也纳入统计：
    // 查询过程本身会写快照（用于"剩余"展示与 TRAE 的消耗推导）。
    for account in &accounts {
        if crate::modules::platform_checkin::account_platform(account)
            != crate::modules::platform_checkin::PLATFORM_WORKBUDDY
        {
            let _ = crate::modules::platform_checkin::get_platform_credits(account).await;
        }
    }
    // 官方请求用量先取（缓存或按 refresh 重采），供 WorkBuddy 消耗事件使用。
    let official = official_usage::official_usage_for_statistics(&accounts, at_ms, refresh).await;
    let mut official_events: Vec<UsageEvent> = Vec::new();
    // WorkBuddy：逐请求明细 → 消耗事件（替代会被包到期/发放污染的余额差）。
    official_events.extend(official_workbuddy_events(&official));
    // AutoClaw：官方流水账"模型消耗积分" debit（按账号 token 各自查账）。
    // 读取实现已统一到 stats::source::ledger（与模型用量的流水对账共用同一套解析）。
    for account in crate::modules::autoclaw_accounts::pool_accounts() {
        for entry in crate::modules::stats::source::ledger::model_consumption(
            &account,
            at_ms - 31 * 24 * 3600 * 1000,
        )
        .await
        {
            official_events.push(UsageEvent::new(
                entry.ts,
                account.id.clone(),
                entry.amount,
            ));
        }
    }
    let mut statistics = build_statistics(
        &load_snapshots(),
        &load_checkin_logs(),
        &accounts,
        at_ms,
        &official_events,
        &official_window_totals(&official),
    );
    statistics["officialUsage"] = official;
    statistics
}

/// 把官方账号行里的窗口合计抽成 `accountId → (今日, 近 7 天, 本月)`。
///
/// 只接受 `ok = true` 的账号：官方失败时对应数值是 null，回退到本地事件流口径，
/// 不能用 0 覆盖掉已有的观察数据。
fn official_window_totals(official: &Value) -> HashMap<String, (f64, f64, f64)> {
    let mut totals = HashMap::new();
    let Some(accounts) = official.get("accounts").and_then(Value::as_array) else {
        return totals;
    };
    for row in accounts {
        if row.get("ok").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(account_id) = non_empty_string(row.get("accountId")) else {
            continue;
        };
        let read = |key: &str| number(row.get(key)).unwrap_or(0.0);
        totals.insert(
            account_id,
            (read("usageToday"), read("usage7Days"), read("usageThisMonth")),
        );
    }
    totals
}

/// 把官方请求明细（WorkBuddy 账号）转换成消耗事件。
/// 每次 API 调用一条（credit > 0），时间取官方 requestTime。
fn official_workbuddy_events(official: &Value) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    let Some(accounts) = official.get("accounts").and_then(Value::as_array) else {
        return events;
    };
    let Some(requests) = official.get("requests").and_then(Value::as_array) else {
        return events;
    };
    for row in accounts {
        if row.get("ok").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(account_id) = row.get("accountId").and_then(Value::as_str) else {
            continue;
        };
        for request in requests {
            if request.get("accountId").and_then(Value::as_str) != Some(account_id) {
                continue;
            }
            let Some((_, ts, _)) =
                official_usage::parse_request_time(request.get("requestTime"))
            else {
                continue;
            };
            let amount = request
                .get("credit")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if amount <= 0.0 {
                continue;
            }
            events.push(UsageEvent::new(ts, account_id.to_string(), amount));
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_local_date(days_ago: i64, hour: u32) -> i64 {
        let today = Local::now().date_naive() - ChronoDuration::days(days_ago);
        Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), hour, 0, 0)
            .single()
            .expect("valid local test date")
            .timestamp_millis()
    }

    fn trae_account() -> Value {
        json!({"id": "account-1", "email": "one@example.com", "platform": "trae_work_cn"})
    }

    fn snap(ts: i64, remaining: f64) -> Value {
        snapshot_value(ts, "account-1", "one@example.com", 100.0, remaining)
    }

    fn no_official_windows() -> HashMap<String, (f64, f64, f64)> {
        HashMap::new()
    }

    #[test]
    fn official_account_windows_override_detail_limited_events() {
        // 官方每账号只返回 100 条请求明细，用它反推近 7 天 / 本月会漏掉更早的请求；
        // 官方账号行的全量聚合值必须覆盖事件流口径，否则总览与按模型分类对不上。
        let now = at_local_date(0, 12);
        let accounts = vec![json!({"id": "account-1", "email": "one@example.com"})];
        let event = UsageEvent::new(now - 60_000, "account-1".to_string(), 4.06);
        let mut windows = HashMap::new();
        windows.insert("account-1".to_string(), (179.58, 948.07, 3144.36));

        let stats = build_statistics(&[], &[], &accounts, now, &[event], &windows);

        assert_eq!(stats["summary"]["usageToday"], 179.58);
        assert_eq!(stats["summary"]["usage7Days"], 948.07);
        assert_eq!(stats["summary"]["usageThisMonth"], 3144.36);
        let account = &stats["accounts"].as_array().unwrap()[0];
        assert_eq!(account["usageToday"], 179.58);
        assert_eq!(account["usage7Days"], 948.07);
        // 事件流仍保留（明细用于曲线），只是不再反推窗口合计。
        assert_eq!(stats["events"][0]["kind"], "usage");
    }

    #[test]
    fn failed_official_accounts_do_not_zero_out_observed_usage() {
        let official = json!({
            "accounts": [
                {"accountId": "account-1", "ok": false, "usageToday": null},
                {"accountId": "account-2", "ok": true, "usageToday": 12.5,
                 "usage7Days": 20.0, "usageThisMonth": 20.0}
            ]
        });
        let windows = official_window_totals(&official);

        assert!(!windows.contains_key("account-1"));
        assert_eq!(windows["account-2"], (12.5, 20.0, 20.0));
    }

    #[test]
    fn first_observation_does_not_create_usage() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(&[snap(now, 100.0)], &[], &[], now, &[], &no_official_windows());

        assert_eq!(stats["summary"]["usageToday"], 0.0);
        assert_eq!(stats["daily"][0]["usage"], 0.0);
    }

    #[test]
    fn positive_decreases_are_assigned_to_the_newer_local_day() {
        // 快照差值口径现在只用于 TRAE（无逐请求官方数据的平台）。
        let now = at_local_date(0, 12);
        let yesterday = at_local_date(1, 12);
        let stats = build_statistics(
            &[snap(yesterday, 100.0), snap(now, 70.0)],
            &[],
            &[trae_account()],
            now,
            &[],
            &no_official_windows(),
        );

        assert_eq!(stats["summary"]["usageToday"], 30.0);
        assert_eq!(stats["summary"]["usage7Days"], 30.0);
        assert_eq!(
            stats["daily"].as_array().unwrap().last().unwrap()["usage"],
            30.0
        );
        assert_eq!(stats["events"][0]["kind"], "usage");
        assert_eq!(stats["events"][0]["amount"], 30.0);
    }

    #[test]
    fn increases_reset_the_baseline_without_negative_usage() {
        let now = at_local_date(0, 12);
        let earlier = at_local_date(2, 12);
        let yesterday = at_local_date(1, 12);
        let stats = build_statistics(
            &[
                snap(earlier, 100.0),
                snap(yesterday, 125.0),
                snap(now, 115.0),
            ],
            &[],
            &[trae_account()],
            now,
            &[],
            &no_official_windows(),
        );

        assert_eq!(stats["summary"]["usageToday"], 10.0);
        assert_eq!(stats["summary"]["usage7Days"], 10.0);
        assert_eq!(stats["events"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn retention_and_month_windows_use_local_calendar_dates() {
        let now = at_local_date(0, 12);
        let old = at_local_date(CREDIT_SNAPSHOT_RETENTION_DAYS + 1, 12);
        let stats = build_statistics(&[snap(old, 100.0), snap(now, 80.0)], &[], &[], now, &[], &no_official_windows());

        assert_eq!(stats["summary"]["usage7Days"], 0.0);
        assert_eq!(stats["summary"]["usageThisMonth"], 0.0);
        assert_eq!(stats["coverageStartAt"], now);
    }

    #[test]
    fn checkin_events_are_separate_from_credit_usage() {
        let now = at_local_date(0, 12);
        let logs = vec![json!({
            "ts": now,
            "accountId": "account-1",
            "email": "one@example.com",
            "result": "success",
        })];
        let stats = build_statistics(
            &[snap(now - 60_000, 100.0), snap(now, 90.0)],
            &logs,
            &[trae_account()],
            now,
            &[],
            &no_official_windows(),
        );

        assert_eq!(stats["summary"]["usageToday"], 10.0);
        assert_eq!(stats["summary"]["todayCheckedInAccounts"], 1);
        assert_eq!(stats["events"].as_array().unwrap().len(), 2);
        let event_kinds: Vec<&str> = stats["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| event["kind"].as_str())
            .collect();
        assert!(event_kinds.contains(&"checkin"));
        assert!(event_kinds.contains(&"usage"));
    }

    #[test]
    fn checkins_without_identity_are_kept_as_events_but_not_counted_as_accounts() {
        let now = at_local_date(0, 12);
        let logs = vec![json!({
            "ts": now,
            "result": "success",
        })];
        let stats = build_statistics(&[], &logs, &[], now, &[], &no_official_windows());

        assert_eq!(stats["summary"]["todayCheckedInAccounts"], 0);
        assert_eq!(stats["events"][0]["kind"], "checkin");
    }

    #[test]
    fn historical_only_accounts_do_not_look_current() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(
            &[snap(now - 60_000, 100.0), snap(now, 80.0)],
            &[],
            &[json!({"id": "current-account"})],
            now,
            &[],
            &no_official_windows(),
        );

        assert_eq!(stats["summary"]["currentRemaining"], 0.0);
        let historical = stats["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["accountId"] == "account-1")
            .expect("historical account summary");
        assert_eq!(historical["isCurrent"], false);
        assert!(historical["currentRemaining"].is_null());
        assert_eq!(historical["lastSnapshotAt"], now);
    }

    #[test]
    fn snapshot_normalization_filters_old_values_and_caps_records() {
        let now = at_local_date(0, 12);
        let old = snap(
            now - (CREDIT_SNAPSHOT_RETENTION_DAYS + 1) * 24 * 3600 * 1000,
            100.0,
        );
        let mut values = vec![old];
        values.extend((0..(CREDIT_SNAPSHOT_MAX_RECORDS + 1)).map(|index| {
            snap(
                now - (CREDIT_SNAPSHOT_MAX_RECORDS as i64 - index as i64) * 1_000,
                100.0,
            )
        }));

        let normalized = normalize_snapshots(&values, now);

        assert_eq!(normalized.len(), CREDIT_SNAPSHOT_MAX_RECORDS);
        assert!(normalized.iter().all(|value| {
            snapshot_from_value(value)
                .map(|snapshot| {
                    snapshot.ts >= now - CREDIT_SNAPSHOT_RETENTION_DAYS * 24 * 3600 * 1000
                })
                .unwrap_or(false)
        }));
    }

    #[test]
    fn workbuddy_usage_comes_from_official_requests_not_snapshot_deltas() {
        // 用户实测场景：快照差值算出 5.06（被包变动污染），官方逐请求
        // 明细是 4.06——统计必须以官方明细为准。
        let now = at_local_date(0, 12);
        let snapshots = vec![snap(now - 3_600_000, 100.0), snap(now, 94.94)];
        let official_event = UsageEvent::new(now - 60_000, "account-1".to_string(), 4.06);
        let accounts = vec![json!({"id": "account-1", "email": "one@example.com"})];
        let stats = build_statistics(&snapshots, &[], &accounts, now, &[official_event], &no_official_windows());

        assert_eq!(stats["summary"]["usageToday"], 4.06);
        let account = &stats["accounts"].as_array().unwrap()[0];
        assert_eq!(account["usageToday"], 4.06);
        assert_eq!(account["currentRemaining"], 94.94);
        // 快照差值（5.06）不得混入事件流。
        let usage_events: Vec<&Value> = stats["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "usage")
            .collect();
        assert_eq!(usage_events.len(), 1);
        assert_eq!(usage_events[0]["amount"], 4.06);
    }

    #[test]
    fn workbuddy_usage_without_official_data_is_zero_not_snapshot_noise() {
        let now = at_local_date(0, 12);
        let snapshots = vec![snap(now - 3_600_000, 100.0), snap(now, 94.94)];
        let accounts = vec![json!({"id": "account-1", "email": "one@example.com"})];
        let stats = build_statistics(&snapshots, &[], &accounts, now, &[], &no_official_windows());

        assert_eq!(stats["summary"]["usageToday"], 0.0);
        let account = &stats["accounts"].as_array().unwrap()[0];
        assert_eq!(account["usageToday"], 0.0);
    }

    #[test]
    fn trae_usage_skips_windows_where_capacity_changed() {
        // 总额度变化（包到期/发放）窗口的余额下降不是消耗。
        let now = at_local_date(0, 12);
        let earlier = at_local_date(0, 8);
        let snapshots = vec![
            snapshot_value(earlier, "account-1", "one@example.com", 100.0, 100.0),
            // 总额度 100→60：包到期窗口，余额下降 40 不算消耗。
            snapshot_value(now - 1_800_000, "account-1", "one@example.com", 60.0, 60.0),
            // 总额度不变：真实消耗窗口。
            snapshot_value(now, "account-1", "one@example.com", 60.0, 55.0),
        ];
        let stats = build_statistics(&snapshots, &[], &[trae_account()], now, &[], &no_official_windows());

        assert_eq!(stats["summary"]["usageToday"], 5.0);
    }

    #[test]
    fn same_value_is_suppressed_only_inside_the_short_window() {
        let now = at_local_date(0, 12);
        let previous = snap(now - CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS - 1, 100.0);
        assert!(!should_suppress_duplicate(
            std::slice::from_ref(&previous),
            "account-1",
            100.0,
            100.0,
            now,
        ));
        assert!(should_suppress_duplicate(
            &[snap(now - CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS + 1, 100.0)],
            "account-1",
            100.0,
            100.0,
            now,
        ));
    }
}
