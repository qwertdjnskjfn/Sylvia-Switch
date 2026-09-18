//! WorkBuddy 官方请求用量投影。
//!
//! 这里负责请求最近 31 个自然日、处理分页、校验和归一化明细，并只向上层
//! 暴露统计字段与有限的请求摘要。投影会写入本地缓存，避免每次打开统计页
//! 都打官方用量接口；上游可能携带的 prompt/input 等字段永远不会被复制。

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration as StdDuration;

use crate::modules::account::{account_display_name, get_str};
use crate::modules::config::{atomic_write, official_usage_cache_file, store_dir};
use crate::modules::credits::authenticated_post;
use crate::modules::platform_checkin::{account_platform, PLATFORM_WORKBUDDY};

pub const OFFICIAL_USAGE_URL: &str =
    "https://www.workbuddy.cn/billing/meter/get-user-request-usage";
pub const OFFICIAL_USAGE_PAGE_SIZE: usize = 3_000;
pub const OFFICIAL_USAGE_DETAIL_LIMIT: usize = 100;
const OFFICIAL_USAGE_MAX_PAGES: usize = 100;
/// 缓存投影版本：账号范围或字段结构变化时递增，旧缓存直接失效重采。
/// v2：官方用量只覆盖 WorkBuddy 账号，平台账号（AutoClaw / TRAE）不再进入本投影。
const OFFICIAL_USAGE_PAYLOAD_VERSION: i64 = 2;
/// 投影有效期。官方口径的「今日消耗」随每次调用变化，跨天更会整体错位，
/// 因此缓存必须带时效：
/// - 超过该时长 → 视为过期，读缓存时自动重采；
/// - `rangeEnd` 不是今天 → 无论多新都必须重采（否则「今日消耗」会停留在
///   采集当天，表现为长期不随实际使用更新）。
/// 10 分钟是「不打扰官方接口」与「及时同步」之间的折中。
const OFFICIAL_USAGE_TTL_MS: i64 = 10 * 60 * 1000;
/// 后台同步周期：与 TTL 对齐，保证没有页面请求时投影也会滚动刷新。
pub const OFFICIAL_USAGE_SYNC_INTERVAL: StdDuration = StdDuration::from_secs(10 * 60);
static OFFICIAL_USAGE_MEMORY: Mutex<Option<Value>> = Mutex::new(None);

fn official_usage_fetch_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn take_object_fields(value: &Value, keys: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(object) = value.as_object() else {
        return out;
    };
    for key in keys {
        if let Some(field) = object.get(*key) {
            out.insert((*key).to_string(), field.clone());
        }
    }
    out
}

fn sanitize_models(value: Option<&Value>) -> Value {
    Value::Array(
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|item| {
                Value::Object(take_object_fields(
                    item,
                    &["model", "requestCount", "credit"],
                ))
            })
            .collect(),
    )
}

fn sanitize_daily(value: Option<&Value>) -> Value {
    Value::Array(
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|item| {
                let mut point = take_object_fields(item, &["date", "usage"]);
                point.insert("models".into(), sanitize_models(item.get("models")));
                Value::Object(point)
            })
            .collect(),
    )
}

fn sanitize_requests(value: Option<&Value>) -> Value {
    Value::Array(
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|item| {
                Value::Object(take_object_fields(
                    item,
                    &[
                        "accountId",
                        "accountName",
                        "requestId",
                        "credit",
                        "model",
                        "client",
                        "requestTime",
                    ],
                ))
            })
            .collect(),
    )
}

fn sanitize_accounts(value: Option<&Value>) -> Value {
    Value::Array(
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|item| {
                let mut account = take_object_fields(
                    item,
                    &[
                        "accountId",
                        "accountName",
                        "ok",
                        "requestCount",
                        "detailTruncated",
                        "usageToday",
                        "usage7Days",
                        "usageThisMonth",
                        "error",
                        "reportedTotal",
                        "fetchedCount",
                    ],
                );
                account.insert("models".into(), sanitize_models(item.get("models")));
                account.insert("daily".into(), sanitize_daily(item.get("daily")));
                Value::Object(account)
            })
            .collect(),
    )
}

fn sanitize_cached_payload(value: &Value) -> Option<Value> {
    if value.get("v").and_then(Value::as_i64) != Some(OFFICIAL_USAGE_PAYLOAD_VERSION) {
        return None;
    }
    let status = value.get("status")?.as_str()?;
    if !matches!(status, "complete" | "partial" | "unavailable") {
        return None;
    }
    let mut payload = take_object_fields(
        value,
        &[
            "v",
            "status",
            "rangeStart",
            "rangeEnd",
            "summary",
            "detailLimitPerAccount",
            "collectedAt",
            "errors",
        ],
    );
    payload.insert("daily".into(), sanitize_daily(value.get("daily")));
    payload.insert("models".into(), sanitize_models(value.get("models")));
    payload.insert("accounts".into(), sanitize_accounts(value.get("accounts")));
    payload.insert("requests".into(), sanitize_requests(value.get("requests")));
    Some(Value::Object(payload))
}

fn parse_official_usage_cache(text: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(text).ok()?;
    let payload = value.get("payload").unwrap_or(&value);
    sanitize_cached_payload(payload)
}

fn load_official_usage_cache_from(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_official_usage_cache(&text)
}

fn save_official_usage_cache_to(path: &Path, payload: &Value) {
    let body = json!({ "payload": payload });
    let content = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(store_dir);
    let _ = std::fs::create_dir_all(parent).and_then(|_| atomic_write(path, &content));
}

fn remembered_official_usage() -> Option<Value> {
    if let Ok(guard) = OFFICIAL_USAGE_MEMORY.lock() {
        if let Some(cached) = guard.as_ref() {
            return Some(cached.clone());
        }
    }
    let loaded = load_official_usage_cache_from(&official_usage_cache_file())?;
    if let Ok(mut guard) = OFFICIAL_USAGE_MEMORY.lock() {
        *guard = Some(loaded.clone());
    }
    Some(loaded)
}

fn remember_official_usage(payload: &Value) {
    if let Ok(mut guard) = OFFICIAL_USAGE_MEMORY.lock() {
        *guard = Some(payload.clone());
    }
    save_official_usage_cache_to(&official_usage_cache_file(), payload);
}

/// 投影是否仍然有效：必须落在 TTL 内，且采集区间覆盖到今天。
///
/// 只判断 TTL 是不够的——服务进程常驻时，昨晚采集的投影「看起来」只差几个小时，
/// 但它的 `rangeEnd` 还是昨天，直接返回会让「今日消耗」整整一天不更新。
fn payload_is_fresh(payload: &Value, at_ms: i64) -> bool {
    let Some(collected_at) = payload.get("collectedAt").and_then(Value::as_i64) else {
        return false;
    };
    if collected_at > at_ms || at_ms - collected_at > OFFICIAL_USAGE_TTL_MS {
        return false;
    }
    payload
        .get("rangeEnd")
        .and_then(Value::as_str)
        .map(|end| end == local_date_at(at_ms).format("%Y-%m-%d").to_string())
        .unwrap_or(false)
}

/// 统计页读取官方用量投影。
///
/// `refresh = true` 强制重采；否则缓存仅在 [`payload_is_fresh`] 通过时复用，
/// 过期或跨天会自动重采，保证「今日消耗」始终对齐官方实际使用情况。
pub async fn official_usage_for_statistics(accounts: &[Value], at_ms: i64, refresh: bool) -> Value {
    if !refresh {
        if let Some(cached) = remembered_official_usage() {
            if payload_is_fresh(&cached, at_ms) {
                return cached;
            }
        }
    }
    let _guard = official_usage_fetch_lock().lock().await;
    // 双检：等锁期间可能已被其它请求（或后台同步）刷新。
    if !refresh {
        if let Some(cached) = remembered_official_usage() {
            if payload_is_fresh(&cached, at_ms) {
                return cached;
            }
        }
    }
    let usage = collect_official_usage(accounts, at_ms).await;
    remember_official_usage(&usage);
    usage
}

/// 后台同步一轮官方用量投影（由 `serve` 的周期任务调用）。
///
/// 没有 WorkBuddy 账号时直接返回，避免对官方接口做无意义请求；失败不抛出，
/// 由下一次周期重试。
pub async fn background_sync_once() {
    let accounts = crate::modules::platform_checkin::all_checkin_accounts();
    if !accounts
        .iter()
        .any(|account| account_platform(account) == PLATFORM_WORKBUDDY)
    {
        return;
    }
    let at_ms = crate::modules::config::now_ms();
    let _ = official_usage_for_statistics(&accounts, at_ms, false).await;
}

#[derive(Clone, Debug)]
struct RequestRow {
    request_id: String,
    credit: f64,
    model: String,
    client: String,
    request_time: String,
    request_ts: i64,
    date: NaiveDate,
}

#[derive(Clone, Debug)]
struct OfficialPage {
    total: usize,
    raw_len: usize,
    rows: Vec<RequestRow>,
}

#[derive(Clone, Debug)]
struct AccountFetch {
    rows: Vec<RequestRow>,
    reported_total: usize,
    fetched_raw: usize,
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

fn parse_number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(value)) => value.as_f64(),
        Some(Value::String(value)) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn response_code(response: &Value) -> Option<i64> {
    response.get("code").and_then(|value| {
        value.as_i64().or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
    })
}

fn error_message(response: &Value) -> String {
    let upstream_message = response
        .get("message")
        .or_else(|| response.get("msg"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(|message| message.chars().take(160).collect::<String>());
    match response_code(response) {
        Some(code) if code == -1 => "官方请求失败（网络或服务不可达）".to_string(),
        Some(code) => upstream_message
            .map(|message| format!("官方请求失败（code={code}）：{message}"))
            .unwrap_or_else(|| format!("官方请求失败（code={code}）")),
        None => "官方响应格式无效".to_string(),
    }
}

fn local_datetime_to_parts(value: NaiveDateTime) -> Option<(NaiveDate, i64)> {
    Local
        .from_local_datetime(&value)
        .single()
        .map(|date| (date.date_naive(), date.timestamp_millis()))
}

fn timestamp_to_parts(ts: i64) -> Option<(NaiveDate, i64)> {
    Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|date| (date.date_naive(), ts))
}

/// 解析官方请求时间（epoch 秒/毫秒、RFC3339 或 "YYYY-MM-DD HH:MM:SS"）
/// → (本地日期, 毫秒时间戳, 原文)。供统计投影把请求明细转成消耗事件。
pub(crate) fn parse_request_time(value: Option<&Value>) -> Option<(NaiveDate, i64, String)> {
    let value = value?;
    if let Some(number) = parse_number(Some(value)) {
        if !number.is_finite() {
            return None;
        }
        let ts = if number.abs() < 10_000_000_000.0 {
            (number * 1000.0).round() as i64
        } else {
            number.round() as i64
        };
        let (date, ts) = timestamp_to_parts(ts)?;
        return Some((date, ts, ts.to_string()));
    }

    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        let ts = parsed.timestamp_millis();
        let date = Local.timestamp_millis_opt(ts).single()?.date_naive();
        return Some((date, ts, text.to_string()));
    }
    for pattern in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(text, pattern) {
            let (date, ts) = local_datetime_to_parts(parsed)?;
            return Some((date, ts, text.to_string()));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        let parsed = date.and_hms_opt(12, 0, 0)?;
        let (date, ts) = local_datetime_to_parts(parsed)?;
        return Some((date, ts, text.to_string()));
    }
    None
}

fn compact_string(value: Option<&Value>, fallback: &str) -> String {
    let text = non_empty(value).unwrap_or_else(|| fallback.to_string());
    text.chars().take(160).collect()
}

fn normalize_row(value: &Value) -> Option<RequestRow> {
    let credit = parse_number(value.get("credit"))?;
    if !credit.is_finite() || credit < 0.0 {
        return None;
    }
    let (date, request_ts, request_time) = parse_request_time(
        value
            .get("requestTime")
            .or_else(|| value.get("request_time")),
    )?;
    Some(RequestRow {
        request_id: compact_string(
            value.get("requestId").or_else(|| value.get("request_id")),
            "unknown",
        ),
        credit,
        model: compact_string(value.get("model"), "—"),
        client: compact_string(value.get("client"), "—"),
        request_time,
        request_ts,
        date,
    })
}

fn parse_page(response: &Value) -> Result<OfficialPage, String> {
    if response_code(response) != Some(0) && response_code(response) != Some(200) {
        return Err(error_message(response));
    }
    let Some(data) = response.get("data").and_then(Value::as_object) else {
        return Err("官方响应格式无效".to_string());
    };
    let Some(raw_rows) = data.get("data").and_then(Value::as_array) else {
        return Err("官方响应格式无效".to_string());
    };
    let total = data
        .get("total")
        .and_then(|value| {
            value.as_u64().or_else(|| {
                value
                    .as_str()
                    .and_then(|text| text.trim().parse::<u64>().ok())
            })
        })
        .unwrap_or(raw_rows.len() as u64)
        .min(usize::MAX as u64) as usize;
    Ok(OfficialPage {
        total,
        raw_len: raw_rows.len(),
        rows: raw_rows.iter().filter_map(normalize_row).collect(),
    })
}

fn should_fetch_next_page(
    page_number: usize,
    total: usize,
    fetched_raw: usize,
    page_len: usize,
) -> bool {
    page_len > 0 && fetched_raw < total && page_number < OFFICIAL_USAGE_MAX_PAGES
}

fn local_date_at(ts: i64) -> NaiveDate {
    Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|date| date.date_naive())
        .unwrap_or_else(|| Local::now().date_naive())
}

async fn fetch_account_usage(
    account: &Value,
    range_start: NaiveDate,
    range_end: NaiveDate,
) -> Result<AccountFetch, String> {
    let start_time = format!("{range_start} 00:00:00");
    let end_time = format!("{range_end} 23:59:59");
    let mut page_number = 1;
    let mut fetched_raw = 0;
    let mut reported_total = 0;
    let mut rows = Vec::new();
    let mut seen_request_ids = HashSet::new();

    loop {
        let response = authenticated_post(
            account,
            OFFICIAL_USAGE_URL,
            json!({
                "startTime": start_time,
                "endTime": end_time,
                "pageNum": page_number,
                "pageSize": OFFICIAL_USAGE_PAGE_SIZE,
            }),
        )
        .await;
        let page = parse_page(&response)?;
        reported_total = reported_total.max(page.total);
        fetched_raw += page.raw_len;
        for row in page
            .rows
            .into_iter()
            .filter(|row| row.date >= range_start && row.date <= range_end)
        {
            if seen_request_ids.insert((row.request_id.clone(), row.request_ts)) {
                rows.push(row);
            }
        }

        if !should_fetch_next_page(page_number, reported_total, fetched_raw, page.raw_len) {
            if page.raw_len == 0 && fetched_raw < reported_total {
                return Err("官方用量分页数据不完整".to_string());
            }
            if page_number >= OFFICIAL_USAGE_MAX_PAGES && fetched_raw < reported_total {
                return Err("官方用量分页超过安全上限".to_string());
            }
            break;
        }
        page_number += 1;
    }

    Ok(AccountFetch {
        rows,
        reported_total,
        fetched_raw,
    })
}

fn aggregate_rows(
    rows: &[RequestRow],
    today: NaiveDate,
) -> (f64, f64, f64, HashMap<NaiveDate, f64>) {
    let mut today_usage = 0.0;
    let mut week_usage = 0.0;
    let mut month_usage = 0.0;
    let mut daily = HashMap::new();
    for row in rows {
        let distance = (today - row.date).num_days();
        if distance == 0 {
            today_usage += row.credit;
        }
        if (0..7).contains(&distance) {
            week_usage += row.credit;
        }
        if row.date.year() == today.year() && row.date.month() == today.month() {
            month_usage += row.credit;
        }
        *daily.entry(row.date).or_insert(0.0) += row.credit;
    }
    (today_usage, week_usage, month_usage, daily)
}

fn model_name(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() || model == "—" {
        "未知模型".to_string()
    } else {
        model.to_string()
    }
}

fn add_model_usage(usage: &mut HashMap<String, (usize, f64)>, row: &RequestRow) {
    let entry = usage.entry(model_name(&row.model)).or_insert((0, 0.0));
    entry.0 += 1;
    entry.1 += row.credit;
}

fn model_usage_values(usage: HashMap<String, (usize, f64)>) -> Vec<Value> {
    let mut models: Vec<(String, (usize, f64))> = usage.into_iter().collect();
    models.sort_by(
        |(left_name, (left_count, left_credit)), (right_name, (right_count, right_credit))| {
            right_credit
                .partial_cmp(left_credit)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right_count.cmp(left_count))
                .then_with(|| left_name.cmp(right_name))
        },
    );
    models
        .into_iter()
        .map(|(model, (request_count, credit))| {
            json!({
                "model": model,
                "requestCount": request_count,
                "credit": credit,
            })
        })
        .collect()
}

fn aggregate_models(rows: &[RequestRow]) -> Vec<Value> {
    let mut usage = HashMap::new();
    for row in rows {
        add_model_usage(&mut usage, row);
    }
    model_usage_values(usage)
}

/// 生成 range_start..=range_end 的逐日序列（无数据的天补 0，模型聚合缺省为空）。
fn daily_series(
    totals: &HashMap<NaiveDate, f64>,
    models: &HashMap<NaiveDate, HashMap<String, (usize, f64)>>,
    range_start: NaiveDate,
    range_end: NaiveDate,
) -> Vec<Value> {
    let mut daily = Vec::new();
    let mut date = range_start;
    while date <= range_end {
        daily.push(json!({
            "date": date.format("%Y-%m-%d").to_string(),
            "usage": totals.get(&date).copied().unwrap_or(0.0),
            "models": models
                .get(&date)
                .map(|models| model_usage_values(models.clone()))
                .unwrap_or_default(),
        }));
        date += Duration::days(1);
    }
    daily
}

fn request_value(account_id: &str, account_name: &str, row: &RequestRow) -> Value {
    json!({
        "accountId": account_id,
        "accountName": account_name,
        "requestId": row.request_id,
        "credit": row.credit,
        "model": row.model,
        "client": row.client,
        "requestTime": row.request_time,
    })
}

/// 查询 WorkBuddy 账号的官方请求用量投影，每个账号单独成行、单独聚合。
///
/// AutoClaw / TRAE Work CN 等平台账号的凭证不属于 workbuddy.cn，请求官方
/// 用量接口只会得到网关 401，因此从一开始就不进入本投影；它们的消耗由
/// 平台积分快照单独推导（见 credit_usage）。
pub async fn collect_official_usage(accounts: &[Value], at_ms: i64) -> Value {
    let accounts: Vec<Value> = accounts
        .iter()
        .filter(|account| account_platform(account) == PLATFORM_WORKBUDDY)
        .cloned()
        .collect();
    let today = local_date_at(at_ms);
    let range_start = today - Duration::days(30);
    let range_end = today;
    let range_start_text = range_start.format("%Y-%m-%d").to_string();
    let range_end_text = range_end.format("%Y-%m-%d").to_string();

    let mut successful_accounts = 0usize;
    let mut account_rows = Vec::new();
    let mut errors = Vec::new();
    let mut daily_totals: HashMap<NaiveDate, f64> = HashMap::new();
    let mut daily_models: HashMap<NaiveDate, HashMap<String, (usize, f64)>> = HashMap::new();
    let mut account_daily_totals: HashMap<String, HashMap<NaiveDate, f64>> = HashMap::new();
    let mut account_daily_models: HashMap<
        String,
        HashMap<NaiveDate, HashMap<String, (usize, f64)>>,
    > = HashMap::new();
    let mut total_today = 0.0;
    let mut total_week = 0.0;
    let mut total_month = 0.0;
    let mut recent_requests: Vec<(i64, Value)> = Vec::new();
    let mut model_totals: HashMap<String, (usize, f64)> = HashMap::new();

    for (index, account) in accounts.iter().enumerate() {
        let account_id = get_str(account, "id").unwrap_or_else(|| format!("unknown-{index}"));
        let account_name = account_display_name(account);
        match fetch_account_usage(account, range_start, range_end).await {
            Ok(result) => {
                successful_accounts += 1;
                let (usage_today, usage_week, usage_month, daily) =
                    aggregate_rows(&result.rows, today);
                total_today += usage_today;
                total_week += usage_week;
                total_month += usage_month;
                for row in &result.rows {
                    add_model_usage(&mut model_totals, row);
                    // 每日按模型聚合（全量，不受 100 条明细限制）
                    add_model_usage(daily_models.entry(row.date).or_default(), row);
                    // 单账号每日按模型聚合（同样不受明细条数限制）
                    add_model_usage(
                        account_daily_models
                            .entry(account_id.clone())
                            .or_default()
                            .entry(row.date)
                            .or_default(),
                        row,
                    );
                }
                for (date, amount) in daily {
                    *daily_totals.entry(date).or_insert(0.0) += amount;
                    *account_daily_totals
                        .entry(account_id.clone())
                        .or_default()
                        .entry(date)
                        .or_insert(0.0) += amount;
                }

                let mut sorted_rows = result.rows.clone();
                sorted_rows.sort_by_key(|row| Reverse(row.request_ts));
                for row in sorted_rows.iter().take(OFFICIAL_USAGE_DETAIL_LIMIT) {
                    recent_requests.push((
                        row.request_ts,
                        request_value(&account_id, &account_name, row),
                    ));
                }
                account_rows.push(json!({
                    "accountId": account_id,
                    "accountName": account_name,
                    "ok": true,
                    "requestCount": result.reported_total,
                    "detailTruncated": result.reported_total > OFFICIAL_USAGE_DETAIL_LIMIT,
                    "usageToday": usage_today,
                    "usage7Days": usage_week,
                    "usageThisMonth": usage_month,
                    "error": Value::Null,
                    "reportedTotal": result.reported_total,
                    "fetchedCount": result.fetched_raw,
                    "models": aggregate_models(&result.rows),
                    "daily": daily_series(
                        account_daily_totals
                            .get(&account_id)
                            .unwrap_or(&HashMap::new()),
                        account_daily_models
                            .get(&account_id)
                            .unwrap_or(&HashMap::new()),
                        range_start,
                        range_end,
                    ),
                }));
            }
            Err(error) => {
                errors.push(json!({
                    "accountId": account_id,
                    "accountName": account_name,
                    "error": error,
                }));
                account_rows.push(json!({
                    "accountId": account_id,
                    "accountName": account_name,
                    "ok": false,
                    "requestCount": 0,
                    "detailTruncated": false,
                    "usageToday": Value::Null,
                    "usage7Days": Value::Null,
                    "usageThisMonth": Value::Null,
                    "error": errors.last().and_then(|item| item.get("error")).cloned().unwrap_or(Value::Null),
                    "reportedTotal": Value::Null,
                    "fetchedCount": 0,
                    "models": [],
                    "daily": [],
                }));
            }
        }
    }

    recent_requests.sort_by_key(|(ts, _)| Reverse(*ts));
    let requests: Vec<Value> = recent_requests
        .into_iter()
        .map(|(_, value)| value)
        .collect();

    let daily = daily_series(&daily_totals, &daily_models, range_start, range_end);

    let status = if accounts.is_empty() {
        "unavailable"
    } else if successful_accounts == accounts.len() {
        "complete"
    } else if successful_accounts > 0 {
        "partial"
    } else {
        "unavailable"
    };

    json!({
        "v": OFFICIAL_USAGE_PAYLOAD_VERSION,
        "status": status,
        "rangeStart": range_start_text,
        "rangeEnd": range_end_text,
        "collectedAt": at_ms,
        "summary": {
            "usageToday": total_today,
            "usage7Days": total_week,
            "usageThisMonth": total_month,
        },
        "daily": daily,
        "accounts": account_rows,
        "requests": requests,
        "models": model_usage_values(model_totals),
        "detailLimitPerAccount": OFFICIAL_USAGE_DETAIL_LIMIT,
        "errors": errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_date(days_ago: i64, hour: u32) -> (NaiveDate, i64) {
        let date = Local::now().date_naive() - Duration::days(days_ago);
        let timestamp = Local
            .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, 0, 0)
            .single()
            .expect("valid local test date")
            .timestamp_millis();
        (date, timestamp)
    }

    fn row(date: NaiveDate, ts: i64, credit: f64) -> RequestRow {
        RequestRow {
            request_id: "request-1".to_string(),
            credit,
            model: "model-a".to_string(),
            client: "client-a".to_string(),
            request_time: format!("{date} 12:00:00"),
            request_ts: ts,
            date,
        }
    }

    #[test]
    fn parses_success_page_and_does_not_copy_prompt_fields() {
        let page = parse_page(&json!({
            "code": 0,
            "data": {
                "total": "1",
                "data": [{
                    "requestId": "req-1",
                    "credit": "1.25",
                    "model": "model-a",
                    "client": "cli",
                    "requestTime": "2026-08-24 12:34:56",
                    "input": "do not expose this prompt",
                    "inputTrunc": "also secret"
                }]
            }
        }))
        .expect("valid official page");

        assert_eq!(page.total, 1);
        assert_eq!(page.raw_len, 1);
        assert_eq!(page.rows[0].credit, 1.25);
        let output = request_value("account-1", "one@example.com", &page.rows[0]);
        let text = output.to_string();
        assert!(!text.contains("do not expose"));
        assert!(!text.contains("inputTrunc"));
        assert_eq!(output["requestId"], "req-1");
    }

    #[test]
    fn malformed_rows_are_ignored_without_turning_into_zero_usage() {
        let page = parse_page(&json!({
            "code": 200,
            "data": {
                "total": 3,
                "data": [
                    {"requestId": "valid", "credit": 2, "requestTime": "2026-08-24 12:00:00"},
                    {"requestId": "negative", "credit": -1, "requestTime": "2026-08-24 12:00:00"},
                    {"requestId": "bad-time", "credit": 4, "requestTime": "not-a-date"}
                ]
            }
        }))
        .expect("page shape is valid");

        assert_eq!(page.raw_len, 3);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].credit, 2.0);
    }

    #[test]
    fn pagination_stops_on_empty_short_total_and_page_limit() {
        assert!(!should_fetch_next_page(1, 10, 0, 0));
        assert!(should_fetch_next_page(1, 10, 2, 2));
        assert!(!should_fetch_next_page(
            1,
            2_000,
            2_000,
            OFFICIAL_USAGE_PAGE_SIZE
        ));
        assert!(should_fetch_next_page(
            1,
            6_000,
            OFFICIAL_USAGE_PAGE_SIZE,
            OFFICIAL_USAGE_PAGE_SIZE
        ));
        assert!(!should_fetch_next_page(
            OFFICIAL_USAGE_MAX_PAGES,
            usize::MAX,
            OFFICIAL_USAGE_PAGE_SIZE * OFFICIAL_USAGE_MAX_PAGES,
            OFFICIAL_USAGE_PAGE_SIZE,
        ));
    }

    #[test]
    fn aggregation_uses_positive_credits_and_local_today_windows() {
        let (today, today_ts) = local_date(0, 12);
        let (yesterday, yesterday_ts) = local_date(1, 12);
        let (last_month, last_month_ts) = local_date(40, 12);
        let rows = vec![
            row(today, today_ts, 1.5),
            row(yesterday, yesterday_ts, 2.0),
            row(last_month, last_month_ts, 5.0),
        ];

        let (today_usage, week_usage, month_usage, daily) = aggregate_rows(&rows, today);
        assert_eq!(today_usage, 1.5);
        assert_eq!(week_usage, 3.5);
        // 月初时“昨天”可能属于上月（甚至跨年），此时本月仅包含今天这条。
        let expected_month_usage = if yesterday.year() == today.year()
            && yesterday.month() == today.month()
        {
            3.5
        } else {
            1.5
        };
        assert_eq!(month_usage, expected_month_usage);
        assert_eq!(daily[&today], 1.5);
        assert_eq!(daily[&yesterday], 2.0);
    }

    #[test]
    fn model_aggregation_sorts_by_credit_and_groups_requests() {
        let (today, today_ts) = local_date(0, 12);
        let mut first = row(today, today_ts, 1.0);
        first.model = "model-a".to_string();
        let mut second = row(today, today_ts + 1, 4.0);
        second.model = "model-b".to_string();
        let mut third = row(today, today_ts + 2, 2.0);
        third.model = "model-a".to_string();

        let models = aggregate_models(&[first, second, third]);
        assert_eq!(models[0]["model"], "model-b");
        assert_eq!(models[0]["requestCount"], 1);
        assert_eq!(models[0]["credit"], 4.0);
        assert_eq!(models[1]["model"], "model-a");
        assert_eq!(models[1]["requestCount"], 2);
        assert_eq!(models[1]["credit"], 3.0);
    }

    #[test]
    fn rejects_error_and_missing_data_shapes() {
        assert!(matches!(
            parse_page(&json!({"code": 500, "msg": "bad"})),
            Err(error) if error == "官方请求失败（code=500）：bad"
        ));
        assert!(matches!(
            parse_page(&json!({"code": 0, "data": {"total": 0}})),
            Err(error) if error == "官方响应格式无效"
        ));
        assert_eq!(
            error_message(&json!({"code": -1, "message": "secret token"})),
            "官方请求失败（网络或服务不可达）"
        );
    }

    #[test]
    fn cache_round_trip_keeps_projection_and_strips_prompt_fields() {
        let payload = json!({
            "v": 2,
            "status": "complete",
            "rangeStart": "2026-07-26",
            "rangeEnd": "2026-08-25",
            "collectedAt": 1,
            "summary": { "usageToday": 1.5, "usage7Days": 3.0, "usageThisMonth": 3.0 },
            "daily": [{ "date": "2026-08-25", "usage": 1.5, "models": [{ "model": "model-a", "requestCount": 1, "credit": 1.5 }] }],
            "models": [{ "model": "model-a", "requestCount": 1, "credit": 1.5 }],
            "accounts": [{
                "accountId": "account-1",
                "accountName": "one@example.com",
                "ok": true,
                "requestCount": 1,
                "detailTruncated": false,
                "usageToday": 1.5,
                "usage7Days": 1.5,
                "usageThisMonth": 1.5,
                "error": null,
                "reportedTotal": 1,
                "fetchedCount": 1,
                "models": [{ "model": "model-a", "requestCount": 1, "credit": 1.5 }],
                "daily": []
            }],
            "requests": [{
                "accountId": "account-1",
                "accountName": "one@example.com",
                "requestId": "req-1",
                "credit": 1.5,
                "model": "model-a",
                "client": "cli",
                "requestTime": "2026-08-25 12:00:00",
                "input": "do not persist this prompt"
            }],
            "detailLimitPerAccount": 100,
            "errors": [],
            "secret": "drop-me"
        });
        let path = std::env::temp_dir().join(format!(
            "wb-switch-official-usage-cache-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        save_official_usage_cache_to(&path, &payload);
        let loaded = load_official_usage_cache_from(&path).expect("valid cache");
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded["status"], "complete");
        assert_eq!(loaded["summary"]["usageToday"], 1.5);
        assert_eq!(loaded["requests"][0]["requestId"], "req-1");
        assert!(loaded.get("secret").is_none());
        let text = loaded.to_string();
        assert!(!text.contains("do not persist"));
        assert!(!text.contains("drop-me"));
    }

    #[test]
    fn cache_parser_rejects_corrupt_and_unknown_status() {
        assert!(parse_official_usage_cache("not-json").is_none());
        assert!(parse_official_usage_cache("{}").is_none());
        assert!(parse_official_usage_cache(r#"{"payload":{"status":"nope"}}"#).is_none());
        // v1 及更早缓存包含平台账号的 401 记录，结构过期必须整份作废
        assert!(parse_official_usage_cache(r#"{"payload":{"v":1,"status":"complete"}}"#).is_none());
        assert!(parse_official_usage_cache(r#"{"payload":{"status":"complete"}}"#).is_none());
    }

    fn cached_payload(collected_at: i64, range_end: &str) -> Value {
        json!({
            "v": OFFICIAL_USAGE_PAYLOAD_VERSION,
            "status": "complete",
            "collectedAt": collected_at,
            "rangeEnd": range_end,
        })
    }

    #[test]
    fn projection_expires_when_stale_or_from_an_earlier_day() {
        let now = crate::modules::config::now_ms();
        let today = local_date_at(now).format("%Y-%m-%d").to_string();
        let yesterday = (local_date_at(now) - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();

        // 刚采集且覆盖到今天 → 有效（读缓存不打扰官方接口）
        assert!(payload_is_fresh(&cached_payload(now, &today), now));
        // 同一天但超过 TTL → 过期重采
        assert!(!payload_is_fresh(
            &cached_payload(now - OFFICIAL_USAGE_TTL_MS - 1, &today),
            now
        ));
        // 采集区间只到昨天 → 无论多新都失效（跨天后「今日消耗」不能停留在旧投影）
        assert!(!payload_is_fresh(&cached_payload(now, &yesterday), now));
        // 缺字段 / 未来时间戳 → 失效
        assert!(!payload_is_fresh(&json!({"status": "complete"}), now));
        assert!(!payload_is_fresh(&cached_payload(now + 60_000, &today), now));
    }
}
