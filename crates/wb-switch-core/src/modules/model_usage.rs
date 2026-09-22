//! 模型用量统计：GLM 代理每次调用的 Token 与积分消耗记录。
//!
//! 独立于代理转发逻辑：glm_proxy 在每次调用成功后调用 [`record_usage`]
//! 完成落库，本模块不参与请求转发。
//!
//! 数值来源（均为 AutoClaw 官方数据，不做本地估算）：
//! - Token：上游响应 `usage`（流式取 SSE 末尾的 usage 块）；
//! - 积分：优先用官方流水账（`/agent-assetmgr/api/v1/ledgers_std`，描述含
//!   模型名的 debit 条目）在调用结束后即时精确归因；流水未命中且本次调用
//!   没有并发重叠时，退回钱包余额差（`/agent-assetmgr/api/v2/wallets`）；
//!   并发重叠且流水未命中时标记「待对账」，由 [`reconcile_with_ledger`]
//!   稍后补齐。计量不持有全局锁，代理调用可以真正并发。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, http_request, now_ms, store_dir};
use crate::modules::platform_checkin::{
    autoclaw_headers, read_autoclaw_token,
};
// 累加算术与派生指标统一到 stats::engine（原先本文件有两套累加器各算一遍）。
use crate::modules::stats::engine::UsageAgg;
// 保留上限与缓存命中率口径统一到 stats::units（原 MAX_RECORDS=4000 会在约
// 80~130 天时提前截断，使「保留 180 天」名不副实）。
use crate::modules::stats::units::{MAX_RECORDS, RETENTION_DAYS};
// 本地日 / 分桶口径统一到 stats::window（原先只有本模块有这套逻辑）。
use crate::modules::stats::window::{floor_to_bucket, local_day_start_ms, pick_bucket};
// 流水解析 / 匹配 / 认领统一到 stats::reconcile（原先即时认领与批量对账各写一遍）。
use crate::modules::stats::reconcile::{self, LedgerEntry, MatchWindow};
// 流水的网络读取统一到 stats::source::ledger。
use crate::modules::stats::source::ledger;

/// 兼容既有调用点：`api.rs` 直接引用 `model_usage::default_bucket_ms`。
pub use crate::modules::stats::window::default_bucket_ms;

const RECORDS_FILE: &str = "glm_usage_records.json";

static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn records_path() -> PathBuf {
    store_dir().join(RECORDS_FILE)
}

pub fn load_records() -> Vec<Value> {
    std::fs::read_to_string(records_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn save_records(records: &[Value]) {
    let cutoff = now_ms() - RETENTION_DAYS * 24 * 3600 * 1000;
    let mut kept: Vec<Value> = records
        .iter()
        .filter(|r| r.get("ts").and_then(Value::as_i64).unwrap_or(0) >= cutoff)
        .cloned()
        .collect();
    if kept.len() > MAX_RECORDS {
        kept = kept.split_off(kept.len() - MAX_RECORDS);
    }
    let path = records_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(&kept) {
        let _ = atomic_write(&path, &content);
    }
}

/// 落库一条调用记录（由 glm_proxy 在调用完成后写入）。
pub fn record_usage(record: Value) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut records = load_records();
    records.push(record);
    save_records(&records);
}

/// T5：按渠道记录一次请求级调用（channel：`workbuddy`/`trae`/`glm`/`loomy`…）。
///
/// 与 [`record_usage`] 写同一份 `glm_usage_records.json`（channel 字段区分渠道，
/// 不清空历史数据），内部复用既有 `save_records`/裁剪逻辑；调用方传入的记录
/// 若缺 `channel` 字段，此处统一补齐，保证统计与筛选口径一致。
pub fn record_channel_call(channel: &str, mut record: Value) {
    if let Some(obj) = record.as_object_mut() {
        obj.insert("channel".to_string(), json!(channel));
    }
    record_usage(record);
}

// ---------------------------------------------------------------------------
// 钱包余额 / 积分计量
// ---------------------------------------------------------------------------

/// AutoClaw 钱包总余额（官方数据；本机登录账号）。
pub async fn wallet_balance() -> Result<f64, String> {
    let token = read_autoclaw_token().map_err(|e| format!("AutoClaw 登录态不可用: {e}"))?;
    wallet_balance_for(&token).await
}

/// AutoClaw 钱包总余额（按 token，供账号池计量归因）。
/// 网关按 token 所属区域路由（国际账号的钱包在国际网关）。
pub async fn wallet_balance_for(token: &str) -> Result<f64, String> {
    let region = crate::modules::autoclaw_accounts::region_for_token(token);
    let url = format!(
        "{}/agent-assetmgr/api/v2/wallets?biz_app_id=autoclaw",
        region.api_host()
    );
    let resp = http_request(&url, "GET", None, Some(&autoclaw_headers(Some(token)))).await;
    if resp.get("code").and_then(Value::as_i64) != Some(0) {
        let msg = resp
            .get("msg")
            .or_else(|| resp.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("查询钱包失败");
        return Err(msg.to_string());
    }
    resp.pointer("/data/total_balance")
        .and_then(Value::as_f64)
        .ok_or_else(|| "钱包响应缺少 total_balance".to_string())
}

/// 并发调用时间窗注册表：用于判定余额差是否可归因。
/// 窗口 = 一次调用的 [start, finish]；保留最近 10 分钟。
struct CallWindow {
    id: String,
    start_ms: i64,
    end_ms: Option<i64>,
}

static CALL_WINDOWS: std::sync::OnceLock<Mutex<Vec<CallWindow>>> = std::sync::OnceLock::new();

fn call_windows() -> &'static Mutex<Vec<CallWindow>> {
    CALL_WINDOWS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 认领一条流水（原子）；已在集合中则返回 false。
///
/// 注册表本体已统一到 `stats::reconcile`（跨模块共用同一份已认领集合），
/// 这里只负责首次调用时用磁盘记录的 `ledgerId` 播种。
fn claim_ledger_id(id: &str) -> bool {
    reconcile::ensure_seeded(|| {
        load_records()
            .iter()
            .filter_map(|r| r.get("ledgerId").and_then(Value::as_str).map(String::from))
            .collect()
    });
    reconcile::claim(id)
}

fn register_window(id: &str) {
    let now = now_ms();
    let mut windows = call_windows().lock().unwrap();
    windows.retain(|w| now - w.start_ms < 10 * 60 * 1000);
    windows.push(CallWindow {
        id: id.to_string(),
        start_ms: now,
        end_ms: None,
    });
}

fn close_window(id: &str) {
    let now = now_ms();
    if let Some(w) = call_windows()
        .lock()
        .unwrap()
        .iter_mut()
        .find(|w| w.id == id)
    {
        w.end_ms = Some(now);
    }
}

/// 判断本窗口（含 2 秒落账边距）是否与其他调用窗口重叠。
/// 重叠时余额差不可归因（无法区分谁扣的费），只能走流水或待对账。
fn window_overlaps_others(id: &str, start_ms: i64, end_ms: i64) -> bool {
    const MARGIN_MS: i64 = 2_000;
    let windows = call_windows().lock().unwrap();
    windows.iter().any(|w| {
        w.id != id
            && w.start_ms - MARGIN_MS <= end_ms
            && w.end_ms.unwrap_or_else(now_ms) + MARGIN_MS >= start_ms
    })
}

/// 积分计量：先流水精确归因，失败且无并发重叠时退回余额差，
/// 都不可行则记「待对账」。全程不持锁，代理调用可真正并发。
pub struct Measurement {
    id: String,
    started: i64,
    token: String,
    model: String,
    balance_before: Option<f64>,
    /// 余额快照的后台任务：与上游请求并行执行，避免钱包查询串在关键路径上
    /// （实测钱包接口 250-600ms，原先每个请求都要先等它）。
    balance_task: Option<tokio::task::JoinHandle<Result<f64, String>>>,
}

impl Measurement {
    pub async fn start(token: &str, model: &str) -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        register_window(&id);
        // 与上游请求并行采样「请求前余额」：扣费发生在请求结束附近，
        // 因此并发采样依然落在扣费之前，不影响余额差归因。
        let token_for_task = token.to_string();
        let balance_task =
            tokio::spawn(async move { wallet_balance_for(&token_for_task).await });
        Self {
            id,
            started: now_ms(),
            token: token.to_string(),
            model: model.to_string(),
            balance_before: None,
            balance_task: Some(balance_task),
        }
    }

    /// 放弃计量（上游尝试失败、未产生计费）。仅关闭时间窗。
    pub fn abort(self) {
        close_window(&self.id);
        if let Some(task) = self.balance_task {
            task.abort();
        }
    }

    /// 等待扣费落账后归因积分。
    /// 返回 (积分数, 流水 id)；流水命中时两者皆有，余额差归因时流水为空，
    /// 并发重叠且流水未命中时积分为空（记录进入「待对账」）。
    pub async fn finish(self) -> (Option<f64>, Option<String>) {
        close_window(&self.id);
        let balance_before = match self.balance_task {
            Some(task) => task.await.ok().and_then(|result| result.ok()),
            None => self.balance_before,
        };
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        // 1) 官方流水精确归因（描述含模型名，天然区分并发调用）。
        if let Some(hit) = try_claim_ledger_entry(&self.token, &self.model, self.started).await {
            return (Some(hit.0), Some(hit.1));
        }
        // 2) 无并发重叠时余额差可信。
        if !window_overlaps_others(&self.id, self.started, now_ms()) {
            if let Some(before) = balance_before {
                if let Ok(after) = wallet_balance_for(&self.token).await {
                    let delta = before - after;
                    if delta >= 0.0 {
                        return (Some(delta), None);
                    }
                }
            }
        }
        (None, None)
    }

    /// 归因积分并直接落库（glm_proxy 非流式路径用）。
    ///
    /// 归因逻辑与 `finish()` 一致，落库字段与 glm_proxy 的 record_value
    /// 对齐：requested/body 模型、池条目 id 与标签、延迟、上游 usage 的
    /// token 数。credits/ledgerId 由归因得出。
    pub async fn finish_and_record(
        self,
        requested_model: &str,
        actual_model: &str,
        usage: Option<&Value>,
        store_id: Option<&str>,
        account_label: Option<&str>,
        latency_ms: i64,
        ttfb_ms: Option<i64>,
    ) {
        let started = self.started;
        let (credits, ledger_id) = self.finish().await;
        let usage = usage.cloned().unwrap_or(Value::Null);
        record_usage(json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "ts": started,
            "model": actual_model,
            "routeId": Value::Null,
            "requestedModel": requested_model,
            "accountId": store_id.unwrap_or("local"),
            "accountLabel": account_label.unwrap_or("本机登录"),
            "promptTokens": usage.get("promptTokens").cloned().unwrap_or(json!(0)),
            "completionTokens": usage.get("completionTokens").cloned().unwrap_or(json!(0)),
            "reasoningTokens": usage.get("reasoningTokens").cloned().unwrap_or(Value::Null),
            "totalTokens": usage.get("totalTokens").cloned().unwrap_or(json!(0)),
            "credits": credits.map(|v| json!(v)).unwrap_or(Value::Null),
            "creditsPending": credits.is_none(),
            "ledgerId": ledger_id.map(|v| json!(v)).unwrap_or(Value::Null),
            "latencyMs": latency_ms,
            "ttfbMs": ttfb_ms.map(|v| json!(v)).unwrap_or(Value::Null),
            "ok": true,
        }));
    }
}

/// 在官方流水里找本次调用的 debit 条目并原子认领。
///
/// 匹配规则与窗口统一由 `stats::reconcile` 提供（与批量对账同一套逻辑），
/// 即时认领用 `[started − 容差, now]`：超前的流水此时还不存在。
async fn try_claim_ledger_entry(token: &str, model: &str, started: i64) -> Option<(f64, String)> {
    let now = now_ms();
    let mut candidates = fetch_ledger_debit_candidates(token, started - 10_000)
        .await
        .ok()?;
    let window = MatchWindow::for_immediate(started, now);
    let index = reconcile::pick_best_match(&candidates, model, started, window)?;
    let best = candidates.remove(index);
    claim_ledger_id(&best.id).then_some((best.amount, best.id))
}

// ---------------------------------------------------------------------------
// 流水账对账
// ---------------------------------------------------------------------------

/// 按账号 token 查询官方流水 debit 条目（必须是该账号自己的 token，见
/// `reconcile_with_ledger`）。
///
/// 实现已统一到 `stats::source::ledger`（与积分统计页共用同一次读取），
/// 这里保留本地薄封装只为少改调用点。
async fn fetch_ledger_debit_candidates(
    token: &str,
    oldest_needed_ms: i64,
) -> Result<Vec<LedgerEntry>, String> {
    ledger::fetch_entries(token, oldest_needed_ms).await
}

/// 用官方流水账为「待对账」记录补齐精确积分。
///
/// 匹配规则：流水描述中的模型名与记录模型完全一致（不区分大小写，避免
/// GLM-5.3 误配 GLM-5.3-Flash），且流水时间落在调用时间窗口内；每条流水
/// 只消费一次（按流水 id 去重）。
///
/// 🔴 流水必须**按记录所属账号分别查询**：扣费记在各账号自己的账本里。
/// 只用本机登录账号的 token 查一次，池内其它账号的记录会永远匹配不到
/// （长时间停在「待对账」，今日积分因此长期偏小），还可能把本机账号的
/// 扣费错配到别的账号的记录上。
pub async fn reconcile_with_ledger() -> Result<usize, String> {
    let accounts = crate::modules::autoclaw_accounts::pool_accounts();
    if accounts.is_empty() {
        return Err("AutoClaw 登录态不可用".to_string());
    }

    // 已被历史记录消费掉的流水 id，避免重复匹配；再加上本进程在途认领。
    let existing_records = load_records();
    let mut used_ledger_ids: std::collections::HashSet<String> = existing_records
        .iter()
        .filter_map(|r| r.get("ledgerId").and_then(Value::as_str).map(String::from))
        .collect();
    for id in reconcile::claimed_snapshot() {
        used_ledger_ids.insert(id);
    }

    // 待对账记录里最早的一条决定要翻多深的流水（窗口下限再留 10 秒余量）。
    let pending: Vec<i64> = existing_records
        .iter()
        .filter(|r| r.get("creditsPending").and_then(Value::as_bool) == Some(true))
        .filter_map(|r| r.get("ts").and_then(Value::as_i64))
        .collect();
    // 没有待对账记录就不必请求流水。
    if pending.is_empty() {
        return Ok(0);
    }
    let oldest_pending = pending.iter().min().copied().unwrap_or(now_ms()) - 10_000;

    let mut by_account: std::collections::HashMap<String, Vec<LedgerEntry>> =
        std::collections::HashMap::new();
    let mut failures: Vec<String> = Vec::new();
    for account in &accounts {
        match fetch_ledger_debit_candidates(&account.bearer(), oldest_pending).await {
            Ok(entries) => {
                // 已被历史记录消费掉的流水 id 不再作为候选；本进程在途认领的也一样。
                let used: std::collections::HashSet<&str> = used_ledger_ids
                    .iter()
                    .map(String::as_str)
                    .collect();
                let candidates: Vec<LedgerEntry> = entries
                    .into_iter()
                    .filter(|entry| !used.contains(entry.id.as_str()))
                    .collect();
                by_account.insert(account.id.clone(), candidates);
            }
            Err(error) => failures.push(format!("{}：{error}", account.label)),
        }
    }
    if by_account.is_empty() {
        return Err(format!("查询 AutoClaw 流水失败（{}）", failures.join("；")));
    }

    let _guard = WRITE_LOCK.lock().unwrap();
    let mut records = load_records();
    let mut matched = 0usize;
    for record in records.iter_mut() {
        if record.get("creditsPending").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(model) = record.get("model").and_then(Value::as_str) else {
            continue;
        };
        let Some(ts) = record.get("ts").and_then(Value::as_i64) else {
            continue;
        };
        // 只在同一账号自己的流水里找候选：跨账号匹配会把别人的扣费写成这次消耗。
        let Some(account_id) = record.get("accountId").and_then(Value::as_str) else {
            continue;
        };
        let Some(candidates) = by_account.get_mut(account_id) else {
            continue;
        };
        // 匹配规则与窗口统一走 stats::reconcile：同模型（忽略大小写）+ 落在
        // [ts − 容差, ts + 记账号漂移上限] + 时间最接近优先。
        let window = MatchWindow::for_reconcile(ts);
        let Some(index) = reconcile::pick_best_match(candidates, model, ts, window) else {
            continue;
        };
        // 命中即从候选池移除，天然满足「每条流水只消费一次」。
        let candidate = candidates.remove(index);
        record["credits"] = json!(candidate.amount);
        record["creditsPending"] = json!(false);
        record["ledgerId"] = json!(candidate.id);
        matched += 1;
    }
    if matched > 0 {
        save_records(&records);
    }
    Ok(matched)
}

// ---------------------------------------------------------------------------
// usage 提取（供 glm_proxy 记录调用）
// ---------------------------------------------------------------------------

/// 从非流式响应 JSON 提取 token 用量与缓存命中情况。
pub fn extract_usage(response: &Value) -> Value {
    let usage = response.get("usage").cloned().unwrap_or(Value::Null);
    let num = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let reasoning = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_f64);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.pointer("/prompt_tokens_details/cache_read"))
        .or_else(|| usage.get("cached_tokens"))
        .or_else(|| usage.get("cache_read_input_tokens"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let prompt = num(&usage, "prompt_tokens");
    let completion = num(&usage, "completion_tokens");
    let total = num(&usage, "total_tokens");
    let uncached_prompt = (prompt - cached).max(0.0);
    let cache_hit = cached > 0.0;
    json!({
        "promptTokens": prompt,
        "completionTokens": completion,
        "totalTokens": total,
        "reasoningTokens": reasoning.map(|v| json!(v)).unwrap_or(Value::Null),
        "cachedTokens": cached,
        "uncachedPromptTokens": uncached_prompt,
        "cacheHit": cache_hit,
    })
}

/// 从 SSE 全文提取末尾的 usage 块（OpenAI 流式在最后一个 chunk 携带）。
pub fn extract_usage_from_sse(sse_text: &str) -> Value {
    for line in sse_text.lines().rev() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if v.get("usage").map(Value::is_object).unwrap_or(false) {
            return extract_usage(&v);
        }
    }
    json!({
        "promptTokens": 0.0,
        "completionTokens": 0.0,
        "totalTokens": 0.0,
        "reasoningTokens": Value::Null,
        "cachedTokens": 0.0,
        "uncachedPromptTokens": 0.0,
        "cacheHit": false,
    })
}

// ---------------------------------------------------------------------------
// 汇总
// ---------------------------------------------------------------------------

/// 按时间下界汇总，返回 `(总计, 各模型)`。
///
/// 累加算术统一在 `stats::engine::UsageAgg` —— 本文件原先有两套累加器
/// （汇总用 `accumulate` 增量写 JSON、监控用 `MonitorAgg`），吃同样的记录各算一遍，
/// 现合并为一套。
fn build_summary(
    records: &[Value],
    since: Option<i64>,
) -> (UsageAgg, std::collections::BTreeMap<String, UsageAgg>) {
    let mut total = UsageAgg::default();
    let mut models: std::collections::BTreeMap<String, UsageAgg> = std::collections::BTreeMap::new();
    for record in records {
        if since.is_some_and(|start| {
            record.get("ts").and_then(Value::as_i64).unwrap_or(0) < start
        }) {
            continue;
        }
        let model = record
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        models.entry(model).or_default().add_record(record);
        total.add_record(record);
    }
    (total, models)
}

/// 把汇总结果投影为接口 JSON（字段与历史 `empty_bucket` + `accumulate` 完全一致）。
fn summary_window_json(
    agg: &UsageAgg,
    models: &std::collections::BTreeMap<String, UsageAgg>,
) -> Value {
    let models_json: serde_json::Map<String, Value> = models
        .iter()
        .map(|(name, agg)| (name.clone(), agg.to_summary_bucket()))
        .collect();
    json!({ "total": agg.to_summary_bucket(), "models": Value::Object(models_json) })
}

/// 汇总统计：今日 / 近 7 天 / 全部 三个窗口，按模型分列 + 总计。
/// 读取前先尝试流水对账，尽可能把「待对账」补成精确值。
pub async fn get_statistics() -> Value {
    summarize_statistics(None).await
}

/// T5：按 channel 过滤的汇总统计（`channel` 合法值：`workbuddy`/`trae`/`glm`/
/// `loomy`/`autoclaw`…，见 [`record_matches_provider`]）。
/// `channel=None` 时与 [`get_statistics`] 完全等价（不过滤）。
///
/// 签名说明：契约 §三 写作 `pub fn get_statistics_filtered(channel: Option<&str>) -> Value`，
/// 但「None 时等价现 get_statistics」要求走同一条流水对账 + 钱包余额路径（均为
/// async），故实现为 `async fn`；入参/返回值与契约一致。
pub async fn get_statistics_filtered(channel: Option<&str>) -> Value {
    summarize_statistics(channel).await
}

async fn summarize_statistics(channel: Option<&str>) -> Value {
    let reconcile_matched = reconcile_with_ledger().await.unwrap_or(0);
    let records: Vec<Value> = load_records()
        .into_iter()
        .filter(|record| record_matches_provider(record, channel))
        .collect();
    let now = now_ms();
    let today_start = local_day_start_ms(now);
    let week_start = now - 7 * 24 * 3600 * 1000;

    let (seven_total, seven_models) = build_summary(&records, Some(week_start));
    // 余额与续航预测：日均消耗取近 7 天代理调用积分均值（签到加赠不计入）。
    let balance = wallet_balance().await.ok();
    let daily_avg = seven_total.credits / 7.0;
    let days_remaining = match (balance, daily_avg > 0.0) {
        (Some(balance), true) => Some((balance / daily_avg * 10.0).round() / 10.0),
        _ => None,
    };

    let (today_total, today_models) = build_summary(&records, Some(today_start));
    let (all_total, all_models) = build_summary(&records, None);

    json!({
        "generatedAt": now,
        "reconcileMatched": reconcile_matched,
        "balance": {
            "current": balance.map(|b| json!(b)).unwrap_or(Value::Null),
            "dailyAvg7d": (daily_avg * 10.0).round() / 10.0,
            "daysRemaining": days_remaining.map(|d| json!(d)).unwrap_or(Value::Null),
        },
        "windows": {
            "today": summary_window_json(&today_total, &today_models),
            "sevenDays": summary_window_json(&seven_total, &seven_models),
            "all": summary_window_json(&all_total, &all_models),
        },
    })
}

/// 监控聚合累加器。
///
/// 算术实现已统一到 [`crate::modules::stats::engine::UsageAgg`]（原先与
/// `token_stats::Totals`、本文件的 `accumulate` 三处各写一遍），这里只保留历史
/// 类型名，通过 `Deref` 让既有字段访问（`agg.calls` / `agg.max_latency` …）继续成立。
#[derive(Default)]
struct MonitorAgg(UsageAgg);

impl std::ops::Deref for MonitorAgg {
    type Target = UsageAgg;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for MonitorAgg {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl MonitorAgg {
    /// 累加一条调用记录；算术实现见 `UsageAgg::add_record`（`stats::engine`）。
    fn add(&mut self, record: &Value) {
        self.0.add_record(record);
    }
}

fn monitor_model_json(model: &str, agg: &MonitorAgg, total_calls: u64) -> Value {
    let share = agg.share(total_calls);
    json!({
        "model": model,
        "calls": agg.calls,
        "okCalls": agg.calls.saturating_sub(agg.failed),
        "failedCalls": agg.failed,
        "successRate": agg.success_rate(),
        "share": share,
        "avgLatencyMs": agg.avg_latency(),
        "maxLatencyMs": agg.max_latency,
        // 首字延迟与出字阶段耗时：区分「预填充慢」和「思考慢」。
        "avgTtfbMs": agg.avg_ttfb(),
        "maxTtfbMs": agg.max_ttfb,
        "avgStreamMs": agg.avg_stream_ms(),
        "promptTokens": agg.prompt,
        "completionTokens": agg.completion,
        "totalTokens": agg.total_tokens(),
        "cachedTokens": agg.cached,
        "cacheHitRate": agg.cache_hit_rate(),
        "savedTokens": agg.cached,
        "credits": (agg.credits * 100.0).round() / 100.0,
        "lastTs": if agg.last_ts > 0 { json!(agg.last_ts) } else { Value::Null },
    })
}

/// 判断调用记录是否归属于指定的 provider（autoclaw, loomy, trae_work_cn）。
///
/// T5：记录自带 `channel` 字段（workbuddy/trae/glm/loomy…）时**优先按 channel
/// 归类**；缺省（历史记录）回退 `accountId` 前缀判定，行为与从前一致。
pub fn record_matches_provider(record: &Value, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let f = filter.trim().to_lowercase();
    if f.is_empty() || f == "all" {
        return true;
    }
    let provider = record
        .get("channel")
        .and_then(Value::as_str)
        .map(|c| c.to_lowercase())
        .or_else(|| {
            record
                .get("provider")
                .and_then(Value::as_str)
                .map(|p| p.to_lowercase())
        })
        .unwrap_or_else(|| {
            let account_id = record.get("accountId").and_then(Value::as_str).unwrap_or("");
            if account_id.starts_with("loomy") {
                "loomy".to_string()
            } else if account_id.starts_with("trae") {
                "trae_work_cn".to_string()
            } else {
                "autoclaw".to_string()
            }
        });
    if f == "autoclaw" || f == "glm" {
        // GLM 代理历史记录无 channel 字段（accountId=local/池条目），归 autoclaw；
        // 新记录若显式带 channel="glm" 同样归入该类。
        provider == "autoclaw" || provider == "glm"
    } else if f == "loomy" {
        provider == "loomy"
    } else if f == "trae" || f == "trae_work_cn" {
        provider == "trae_work_cn" || provider == "trae"
    } else if f == "workbuddy" {
        provider == "workbuddy"
    } else {
        provider.eq_ignore_ascii_case(&f)
    }
}

/// 模型监控：把调用记录整理成「窗口内按模型汇总」+「按时间分桶序列」，
/// 前端表格与趋势图可直接消费。`window_hours <= 0` 表示全部历史。
pub fn build_monitor(records: &[Value], now: i64, window_hours: i64, bucket_ms: i64) -> Value {
    build_monitor_filtered(records, now, window_hours, bucket_ms, None)
}

/// 带账号/平台过滤的模型监控聚合。
pub fn build_monitor_filtered(
    records: &[Value],
    now: i64,
    window_hours: i64,
    bucket_ms: i64,
    provider_filter: Option<&str>,
) -> Value {
    // 窗口起点：固定窗口按小时回推；「全部」取最早一条记录（无记录时回退 24 小时），
    // 否则桶会被历史时间跨度撑大，趋势图只剩一根柱子。
    let from = if window_hours > 0 {
        now - window_hours * 3600 * 1000
    } else {
        records
            .iter()
            .filter(|record| record_matches_provider(record, provider_filter))
            .filter_map(|record| record.get("ts").and_then(Value::as_i64))
            .filter(|ts| *ts > 0)
            .min()
            .unwrap_or(now - 24 * 3600 * 1000)
    };
    let span = now.saturating_sub(from).max(60_000);
    let bucket_ms = pick_bucket(span, bucket_ms);

    let mut per_model: std::collections::BTreeMap<String, MonitorAgg> =
        std::collections::BTreeMap::new();
    let mut total = MonitorAgg::default();
    // 桶 -> (总调用, 失败数, 各模型调用数)
    let mut buckets: std::collections::BTreeMap<i64, (u64, u64, std::collections::BTreeMap<String, u64>)> =
        std::collections::BTreeMap::new();

    for record in records {
        if !record_matches_provider(record, provider_filter) {
            continue;
        }
        let ts = record.get("ts").and_then(Value::as_i64).unwrap_or(0);
        // 过滤窗口外与未来时间（时钟漂移容忍 1 分钟）。
        if ts < from || ts > now + 60_000 {
            continue;
        }
        let model = record
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        per_model.entry(model.clone()).or_default().add(record);
        total.add(record);

        let bucket_ts = floor_to_bucket(ts, bucket_ms);
        let entry = buckets.entry(bucket_ts).or_insert_with(|| {
            (0, 0, std::collections::BTreeMap::new())
        });
        entry.0 += 1;
        if record.get("ok").and_then(Value::as_bool) == Some(false) {
            entry.1 += 1;
        }
        *entry.2.entry(model).or_insert(0) += 1;
    }

    let mut models: Vec<(String, MonitorAgg)> = per_model.into_iter().collect();
    models.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then_with(|| a.0.cmp(&b.0)));
    let total_calls = total.calls;
    let series_models: Vec<String> = models
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| {
            // 仅保留在时间序列里出现过的模型（前 6 个用于堆叠图，其余前端归入「其他」）
            buckets.values().any(|(_, _, by_model)| by_model.contains_key(name))
        })
        .collect();

    // 补齐空桶，保证趋势图时间轴连续。
    let first_bucket = if from > 0 {
        floor_to_bucket(from, bucket_ms)
    } else {
        buckets
            .keys()
            .next()
            .copied()
            .unwrap_or_else(|| floor_to_bucket(now, bucket_ms))
    };
    let last_bucket = floor_to_bucket(now, bucket_ms);
    let mut series = Vec::new();
    let mut cursor = first_bucket;
    while cursor <= last_bucket {
        let (calls, failed, by_model) = buckets.remove(&cursor).unwrap_or_default();
        series.push(json!({
            "ts": cursor,
            "total": calls,
            "failed": failed,
            "byModel": by_model,
        }));
        cursor += bucket_ms;
    }

    json!({
        "generatedAt": now,
        "from": from,
        "to": now,
        "windowHours": window_hours,
        "bucketMs": bucket_ms,
        "provider": provider_filter.unwrap_or("all"),
        "summary": {
            "calls": total.calls,
            "okCalls": total.calls.saturating_sub(total.failed),
            "failedCalls": total.failed,
            "successRate": total.success_rate(),
            "avgLatencyMs": total.avg_latency(),
            "avgTtfbMs": total.avg_ttfb(),
            "maxTtfbMs": total.max_ttfb,
            "avgStreamMs": total.avg_stream_ms(),
            "promptTokens": total.prompt,
            "completionTokens": total.completion,
            "totalTokens": total.prompt + total.completion,
            "cachedTokens": total.cached,
            "cacheHitRate": if total.prompt > 0.0 { ((total.cached / total.prompt) * 10000.0).round() / 10000.0 } else { 0.0 },
            "savedTokens": total.cached,
            "credits": (total.credits * 100.0).round() / 100.0,
            "modelCount": series_models.len(),
        },
        "models": models
            .iter()
            .map(|(name, agg)| monitor_model_json(name, agg, total_calls))
            .collect::<Vec<_>>(),
        "seriesModels": series_models,
        "series": series,
    })
}

/// 读取磁盘记录并生成监控数据。
pub fn get_monitor(window_hours: i64, bucket_ms: i64) -> Value {
    get_monitor_filtered(window_hours, bucket_ms, None)
}

/// 支持平台过滤的监控数据读取。
pub fn get_monitor_filtered(window_hours: i64, bucket_ms: i64, provider: Option<&str>) -> Value {
    build_monitor_filtered(&load_records(), now_ms(), window_hours, bucket_ms, provider)
}

/// 汇总多账号（AutoClaw, Loomy, TRAE Work CN）的模型使用与关键指标。
pub async fn get_accounts_overview() -> Value {
    let now = now_ms();
    let records = load_records();

    // 1. 并发获取各平台当前钱包/额度余额
    let (autoclaw_bal, loomy_bal, trae_bal) = tokio::join!(
        wallet_balance(),
        crate::modules::loomy::credits_local(),
        crate::modules::platform_checkin::get_trae_work_cn_credits(),
    );

    let autoclaw_balance_val = autoclaw_bal.ok();
    let loomy_balance_val = loomy_bal.get("balance").and_then(Value::as_f64);
    let trae_balance_val = trae_bal
        .get("totalRemaining")
        .or_else(|| trae_bal.get("remainingTotal"))
        .and_then(Value::as_f64);

    let platforms = [
        ("autoclaw", "AutoClaw", "AutoClaw 账号", autoclaw_balance_val, "积分"),
        ("loomy", "Loomy", "Loomy 账号", loomy_balance_val, "积分"),
        ("trae_work_cn", "TRAE Work CN", "TRAE Work CN 账号", trae_balance_val, "额度"),
    ];

    let mut account_items = Vec::new();

    for (provider_id, name, label, balance, unit) in platforms {
        let mut agg = MonitorAgg::default();
        let mut models = std::collections::HashSet::new();

        for r in &records {
            if record_matches_provider(r, Some(provider_id)) {
                agg.add(r);
                if let Some(m) = r.get("model").and_then(Value::as_str) {
                    models.insert(m.to_string());
                }
            }
        }

        let total_tokens = agg.prompt + agg.completion;
        let uncached_prompt = (agg.prompt - agg.cached).max(0.0);
        let cache_hit_rate = if agg.prompt > 0.0 {
            ((agg.cached / agg.prompt) * 10000.0).round() / 10000.0
        } else {
            0.0
        };

        account_items.push(json!({
            "provider": provider_id,
            "name": name,
            "label": label,
            "balance": balance.map(|b| (b * 100.0).round() / 100.0),
            "balanceUnit": unit,
            "calls": agg.calls,
            "okCalls": agg.calls.saturating_sub(agg.failed),
            "failedCalls": agg.failed,
            "successRate": agg.success_rate(),
            "avgLatencyMs": agg.avg_latency(),
            "avgTtfbMs": agg.avg_ttfb(),
            "promptTokens": agg.prompt,
            "completionTokens": agg.completion,
            "cachedTokens": agg.cached,
            "uncachedPromptTokens": uncached_prompt,
            "cacheHitCalls": agg.cache_hit_calls,
            "cacheHitRate": cache_hit_rate,
            "totalTokens": total_tokens,
            "credits": (agg.credits * 100.0).round() / 100.0,
            "lastTs": if agg.last_ts > 0 { json!(agg.last_ts) } else { Value::Null },
            "modelsCount": models.len(),
            "status": if balance.is_some() || agg.calls > 0 { "active" } else { "idle" },
        }));
    }

    json!({
        "ok": true,
        "generatedAt": now,
        "accounts": account_items,
    })
}

/// 明细记录（按时间倒序，最多 limit 条）。
pub fn recent_records(limit: usize) -> Vec<Value> {
    let mut records = load_records();
    records.reverse();
    records.truncate(limit);
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ts: i64, model: &str, credits_pending: bool) -> Value {
        json!({
            "id": format!("r-{ts}-{model}"),
            "ts": ts,
            "model": model,
            "promptTokens": 10.0,
            "completionTokens": 20.0,
            "totalTokens": 30.0,
            "credits": if credits_pending { Value::Null } else { json!(1.0) },
            "creditsPending": credits_pending,
        })
    }

    #[test]
    fn accumulate_sums_tokens_and_counts_pending() {
        let mut agg = UsageAgg::default();
        agg.add_record(&record(1, "glm-5.3", false));
        let mut pending = record(2, "glm-5.3", true);
        pending["credits"] = json!(Value::Null);
        agg.add_record(&pending);
        let bucket = agg.to_summary_bucket();
        assert_eq!(bucket["calls"], json!(2));
        assert_eq!(bucket["totalTokens"], json!(60.0));
        assert_eq!(bucket["credits"], json!(1.0), "待对账按 0 计入");
        assert_eq!(bucket["creditsPendingCount"], json!(1));
    }

    #[test]
    fn extract_usage_and_accumulate_with_cache_hit() {
        let resp = json!({
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 200,
                "total_tokens": 1200,
                "prompt_tokens_details": {
                    "cached_tokens": 800
                }
            }
        });
        let usage = extract_usage(&resp);
        assert_eq!(usage["promptTokens"], json!(1000.0));
        assert_eq!(usage["cachedTokens"], json!(800.0));
        assert_eq!(usage["uncachedPromptTokens"], json!(200.0));
        assert_eq!(usage["cacheHit"], json!(true));

        let mut agg = UsageAgg::default();
        let mut rec = record(1, "glm-5.3", false);
        rec["promptTokens"] = usage["promptTokens"].clone();
        rec["completionTokens"] = usage["completionTokens"].clone();
        rec["totalTokens"] = usage["totalTokens"].clone();
        rec["cachedTokens"] = usage["cachedTokens"].clone();
        rec["uncachedPromptTokens"] = usage["uncachedPromptTokens"].clone();
        rec["cacheHit"] = usage["cacheHit"].clone();
        rec["credits"] = json!(10.0);
        agg.add_record(&rec);
        let bucket = agg.to_summary_bucket();

        assert_eq!(bucket["cachedTokens"], json!(800.0));
        assert_eq!(bucket["uncachedPromptTokens"], json!(200.0));
        assert_eq!(bucket["cacheHitCalls"], json!(1));
        assert_eq!(bucket["cacheHitRate"], json!(0.8));
        assert_eq!(bucket["savedTokens"], json!(800.0));
        // 10.0 / 1200 * 800 * 0.8 = 5.3333... -> 5.33
        assert_eq!(bucket["savedCredits"], json!(5.33));
    }

    #[test]
    fn summarize_windows_filter_by_ts() {
        let now = now_ms();
        let records = vec![
            record(now, "glm-5.3", false),
            record(now - 30 * 24 * 3600 * 1000, "glm-5.3-flash", false),
        ];
        let today_start = local_day_start_ms(now);
        let today_calls: u64 = records
            .iter()
            .filter(|r| r["ts"].as_i64().unwrap_or(0) >= today_start)
            .count() as u64;
        assert_eq!(today_calls, 1);
    }

    #[test]
    fn monitor_aggregates_models_and_buckets_time() {
        let now = 1_800_000_000_000_i64;
        let hour = 3600 * 1000_i64;
        let records = vec![
            json!({"id":"a","ts": now - 30*60*1000, "model":"glm-5.3-flash", "promptTokens": 10.0,
                   "completionTokens": 5.0, "credits": 1.0, "latencyMs": 1200.0, "ok": true}),
            json!({"id":"b","ts": now - 20*60*1000, "model":"glm-5.3-flash", "promptTokens": 10.0,
                   "completionTokens": 5.0, "credits": 2.0, "latencyMs": 800.0, "ok": false}),
            json!({"id":"c","ts": now - 3*hour, "model":"glm-5.3", "promptTokens": 100.0,
                   "completionTokens": 50.0, "credits": 5.0, "latencyMs": 3000.0, "ok": true}),
            // 窗口外（48 小时前）不计入
            json!({"id":"old","ts": now - 48*hour, "model":"glm-5.3", "ok": true}),
        ];
        let monitor = build_monitor(&records, now, 24, hour);
        let summary = &monitor["summary"];
        assert_eq!(summary["calls"], json!(3));
        assert_eq!(summary["failedCalls"], json!(1));
        assert_eq!(summary["okCalls"], json!(2));
        assert_eq!(summary["successRate"], json!(0.6667));
        assert_eq!(summary["credits"], json!(8.0));
        assert_eq!(summary["totalTokens"], json!(180.0));
        assert_eq!(summary["modelCount"], json!(2));

        let models = monitor["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        // 按调用次数降序
        assert_eq!(models[0]["model"], json!("glm-5.3-flash"));
        assert_eq!(models[0]["calls"], json!(2));
        assert_eq!(models[0]["share"], json!(0.6667));
        assert_eq!(models[0]["okCalls"], json!(1));
        assert_eq!(models[0]["failedCalls"], json!(1));
        assert_eq!(models[0]["avgLatencyMs"], json!(1000.0));
        assert_eq!(models[0]["maxLatencyMs"], json!(1200.0));
        assert!(models[0]["lastTs"].as_i64().is_some());
        assert_eq!(models[1]["model"], json!("glm-5.3"));
        assert_eq!(models[1]["share"], json!(0.3333));

        // 24 小时 + 1 小时桶：序列连续（约 25 个点），空桶被补齐
        let series = monitor["series"].as_array().unwrap();
        assert!(series.len() >= 24 && series.len() <= 27, "实际 {} 点", series.len());
        let busy_total: u64 = series.iter().map(|p| p["total"].as_u64().unwrap_or(0)).sum();
        assert_eq!(busy_total, 3);
        let full_buckets = series.iter().filter(|p| p["total"].as_u64().unwrap_or(0) == 0).count();
        assert!(full_buckets > 0, "应有空桶被补齐");
        assert_eq!(monitor["seriesModels"][0], json!("glm-5.3-flash"));
    }

    #[test]
    fn monitor_coarsens_bucket_to_keep_points_bounded() {
        let now = 1_800_000_000_000_i64;
        let records = vec![json!({
            "id": "a", "ts": now - 100 * 24 * 3600 * 1000, "model": "glm-5.3", "ok": true
        })];
        // 全部窗口 + 1 分钟桶：必须自动加粗，点数收敛在 240 以内
        let monitor = build_monitor(&records, now, 0, 60_000);
        let points = monitor["series"].as_array().unwrap().len();
        assert!(points <= 241, "点数应被收敛，实际 {points}");
        assert!(monitor["bucketMs"].as_i64().unwrap() > 60_000);
        assert_eq!(monitor["summary"]["calls"], json!(1));
    }

    #[test]
    fn monitor_all_window_derives_bucket_from_earliest_record() {
        let now = 1_800_000_000_000_i64;
        let records = vec![
            json!({"id":"a","ts": now - 25*3600*1000, "model":"glm-5.3-flash", "ok": true}),
            json!({"id":"b","ts": now - 3600*1000, "model":"glm-5.3-flash", "ok": true}),
        ];
        let monitor = build_monitor(&records, now, 0, default_bucket_ms(0));
        assert_eq!(monitor["summary"]["calls"], json!(2));
        // 全部窗口从最早记录起算，25 小时跨度 → 1 小时桶（而不是被 1970 年撑成一根柱子）
        assert_eq!(monitor["bucketMs"], json!(3600 * 1000));
        let points = monitor["series"].as_array().unwrap().len();
        assert!(points >= 24 && points <= 27, "应约 25 个点，实际 {points}");
        assert_eq!(monitor["from"], json!(now - 25 * 3600 * 1000));
    }

    #[test]
    fn monitor_splits_first_token_and_stream_time() {
        let now = 1_800_000_000_000_i64;
        let hour = 3600 * 1000_i64;
        let records = vec![
            json!({"id":"a","ts": now - 600_000, "model":"m", "ok": true,
                   "latencyMs": 6000.0, "ttfbMs": 4000.0, "promptTokens": 100.0}),
            json!({"id":"b","ts": now - 300_000, "model":"m", "ok": true,
                   "latencyMs": 4000.0, "ttfbMs": 2000.0, "promptTokens": 100.0}),
        ];
        let monitor = build_monitor(&records, now, 24, hour);
        assert_eq!(monitor["summary"]["avgTtfbMs"], json!(3000.0));
        assert_eq!(monitor["summary"]["maxTtfbMs"], json!(4000.0));
        assert_eq!(monitor["summary"]["avgLatencyMs"], json!(5000.0));
        // 出字阶段 ≈ 总延迟 - 首字延迟
        assert_eq!(monitor["summary"]["avgStreamMs"], json!(2000.0));
        assert_eq!(monitor["models"][0]["avgTtfbMs"], json!(3000.0));
        assert_eq!(monitor["models"][0]["maxTtfbMs"], json!(4000.0));
    }

    #[test]
    fn monitor_marks_empty_window() {
        let now = 1_800_000_000_000_i64;
        let monitor = build_monitor(&[], now, 24, 3600 * 1000);
        assert_eq!(monitor["summary"]["calls"], json!(0));
        assert_eq!(monitor["summary"]["successRate"], json!(0.0));
        assert!(monitor["models"].as_array().unwrap().is_empty());
        assert!(monitor["series"].as_array().unwrap().iter().all(|p| p["total"] == json!(0)));
    }

    #[test]
    fn monitor_filters_by_provider() {
        let now = 1_800_000_000_000_i64;
        let hour = 3600 * 1000_i64;
        let records = vec![
            json!({
                "id": "1", "ts": now - 1000, "model": "glm-5.3", "provider": "autoclaw",
                "promptTokens": 100.0, "completionTokens": 50.0, "ok": true
            }),
            json!({
                "id": "2", "ts": now - 2000, "model": "spark-pro", "provider": "loomy",
                "promptTokens": 200.0, "completionTokens": 80.0, "ok": true
            }),
            json!({
                "id": "3", "ts": now - 3000, "model": "claude-3.5", "accountId": "trae_work_cn:123",
                "promptTokens": 300.0, "completionTokens": 120.0, "ok": true
            }),
        ];

        let all_monitor = build_monitor_filtered(&records, now, 24, hour, None);
        assert_eq!(all_monitor["summary"]["calls"], json!(3));

        let autoclaw_monitor = build_monitor_filtered(&records, now, 24, hour, Some("autoclaw"));
        assert_eq!(autoclaw_monitor["summary"]["calls"], json!(1));
        assert_eq!(autoclaw_monitor["models"][0]["model"], json!("glm-5.3"));

        let loomy_monitor = build_monitor_filtered(&records, now, 24, hour, Some("loomy"));
        assert_eq!(loomy_monitor["summary"]["calls"], json!(1));
        assert_eq!(loomy_monitor["models"][0]["model"], json!("spark-pro"));

        let trae_monitor = build_monitor_filtered(&records, now, 24, hour, Some("trae_work_cn"));
        assert_eq!(trae_monitor["summary"]["calls"], json!(1));
        assert_eq!(trae_monitor["models"][0]["model"], json!("claude-3.5"));
    }

    #[test]
    fn channel_field_drives_provider_classification() {
        // T5：channel 字段优先于 accountId 前缀（workbuddy/trae 记录按 channel 归类）。
        let wb = json!({"id": "w", "ts": 1, "model": "opencode/mimo", "channel": "workbuddy",
                        "accountId": "workbuddy", "ok": true});
        let trae = json!({"id": "t", "ts": 2, "model": "models", "channel": "trae",
                          "accountId": "trae_work_cn", "ok": true});
        let glm = json!({"id": "g", "ts": 3, "model": "glm-5.3", "channel": "glm",
                         "accountId": "local", "ok": true});
        let legacy = json!({"id": "l", "ts": 4, "model": "glm-5.3", "accountId": "local", "ok": true});

        assert!(record_matches_provider(&wb, Some("workbuddy")));
        assert!(!record_matches_provider(&wb, Some("autoclaw")));
        assert!(!record_matches_provider(&wb, Some("loomy")));

        assert!(record_matches_provider(&trae, Some("trae")));
        assert!(record_matches_provider(&trae, Some("trae_work_cn")));

        // channel="glm" 归入 autoclaw 类（GLM 代理历史记录无 channel，同样归此类）。
        assert!(record_matches_provider(&glm, Some("autoclaw")));
        assert!(record_matches_provider(&glm, Some("glm")));
        assert!(record_matches_provider(&legacy, Some("autoclaw")));
        assert!(!record_matches_provider(&legacy, Some("workbuddy")));

        // None / all / 空串不过滤。
        assert!(record_matches_provider(&wb, None));
        assert!(record_matches_provider(&wb, Some("all")));
        assert!(record_matches_provider(&wb, Some("")));
    }
}

