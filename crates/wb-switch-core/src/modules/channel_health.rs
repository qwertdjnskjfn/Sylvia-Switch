//! 渠道健康度监控看板：12 个模型渠道的统一健康评分、预警与趋势。
//!
//! ## 解决什么问题
//! 每个渠道的健康信息散落在各自模块里：请求日志在 `gateway_admin`、
//! 调用统计在 `model_usage`、账号池在 `grok_pool` 与各模块的
//! `list_for_display`、签到结果在 `auto_checkin_logs.json`。出问题时只能
//! 逐个模块翻，无法一眼看出「哪个渠道不健康、为什么不健康」。
//!
//! 本模块把这些数据源聚合成「渠道健康度」：每个渠道一个 0-100 的加权评分
//! （可用性 / 账号健康 / 异常 / 活跃度 / 签到），按契约阈值触发预警，并落盘
//! 历史快照供趋势图使用（懒采样：距上次采样 > 5 分钟才追加，保留 7 天）。
//!
//! 设计要点：
//! - 只做**聚合与投影**，不改动任何渠道模块的既有逻辑；
//! - 单个渠道取数失败不影响其他渠道（降级为空指标 + 中性分）；
//! - 字段命名、评分公式与预警阈值以 `docs/channel-health-contract.md` 为唯一准绳；
//! - 数据落盘 `~/.wb-switch/channel_health_history.json`（照抄 grok_pool 的
//!   7 天裁剪 + credit_usage 的 5 分钟去重懒采样）。

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, load_checkin_logs, now_ms, store_dir};
use crate::modules::gateway_overview::gateway_snapshot;
use crate::modules::stats::window::{floor_to_bucket, local_day_start_ms};
use crate::modules::{gateway_admin, grok, grok_pool, model_usage};

// ---------------------------------------------------------------------------
// 渠道清单
// ---------------------------------------------------------------------------

/// 渠道清单（12 个，顺序即展示顺序）。与契约第 1 节逐字一致。
struct ChannelEntry {
    id: &'static str,
    name: &'static str,
    /// 前端页面路由（「前往管理」跳转用）。
    page: &'static str,
    /// 对外调用基址（OpenAI 兼容端点前缀；miaoshou 沿用 GATEWAYS 占位「—」）。
    base: &'static str,
    /// 流量数据源：logs=gateway_admin 请求日志；usage=model_usage 调用统计；
    /// none=凭证模式（无请求级统计）。
    traffic_source: &'static str,
}

const CHANNELS: &[ChannelEntry] = &[
    ChannelEntry { id: "glm", name: "GLM 代理", page: "/glm-proxy", base: "/v1", traffic_source: "usage" },
    ChannelEntry { id: "glmweb", name: "GLM 网页版", page: "/glmweb", base: "/glmweb/v1", traffic_source: "logs" },
    ChannelEntry { id: "kimi", name: "Kimi", page: "/kimi", base: "/kimi/v1", traffic_source: "logs" },
    ChannelEntry { id: "qoder", name: "Qoder", page: "/qoder", base: "/qoder/v1", traffic_source: "logs" },
    ChannelEntry { id: "qwen", name: "Qwen 桌面版", page: "/qoder", base: "/qwen/v1", traffic_source: "logs" },
    ChannelEntry { id: "qwen-flash", name: "Qwen3.8-Flash", page: "/qoder", base: "/qwen-flash/v1", traffic_source: "logs" },
    ChannelEntry { id: "grok", name: "Grok", page: "/grok", base: "/grok/v1", traffic_source: "logs" },
    ChannelEntry { id: "loomy", name: "Loomy", page: "/loomy", base: "/loomy/v1", traffic_source: "usage" },
    // 妙手（美团智能体）：base 沿用 GATEWAYS 占位符（server 实际路由为 /miaoshou/v1）。
    ChannelEntry { id: "miaoshou", name: "妙手", page: "/miaoshou", base: "—", traffic_source: "logs" },
    // T5：workbuddy/trae 由 none 改为 logs——api.rs 的 workbuddy chat handler 与
    // trae models 目录请求现在会写 gateway_admin 请求日志 + model_usage 请求级
    // 记录，渠道健康度走请求日志口径（与 kimi/glmweb/miaoshou 一致）。
    ChannelEntry { id: "workbuddy", name: "WorkBuddy 反代", page: "/trae-workbuddy", base: "/workbuddy/v1", traffic_source: "logs" },
    ChannelEntry { id: "trae", name: "TRAE 反代", page: "/trae-workbuddy", base: "/trae/v1", traffic_source: "logs" },
    // DuMate（百度搭子）渠道（2026-10-01 接入）：api.rs 的 /dumate/v1 chat handler
    // 写 gateway_admin 请求日志，流量走 logs 口径。
    ChannelEntry { id: "dumate", name: "DuMate（百度搭子）", page: "/dumate", base: "/dumate/v1", traffic_source: "logs" },
    // MiniMax 渠道（2026-10-09 补登记）：api.rs 的 /minimax/v1 chat handler 写
    // gateway_admin 请求日志，流量走 logs 口径（与 kimi/glmweb/miaoshou/dumate 一致）。
    // ⚠️ 对话链路当前上游侧异常（2026-10-05 排查：恒定 in=21、答非所问），
    //    故其流量指标会偏弱——这是真实状态，不做粉饰。
    ChannelEntry { id: "minimax", name: "MiniMax", page: "/minimax", base: "/minimax/v1", traffic_source: "logs" },
];

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

const HISTORY_FILE: &str = "channel_health_history.json";
/// 历史采样去重窗口：距上次采样 > 5 分钟才追加（照抄 credit_usage 的懒采样）。
const SAMPLE_DEDUPE_MS: i64 = 5 * 60 * 1000;
/// 历史保留 7 天（照抄 grok_pool 的 HISTORY_KEEP_MS）。
const HISTORY_KEEP_MS: i64 = 7 * 24 * 3600 * 1000;
/// 趋势图点数上限（契约第 5.3 节：scoreTrend 最多 30 点）。
const MAX_TREND_POINTS: usize = 30;
/// 详情页最近请求日志条数上限（契约第 7.5 节：最近 30 条）。
const MAX_RECENT_LOGS: usize = 30;
/// 详情页签到结果条数上限。
const MAX_CHECKIN_RESULTS: usize = 10;

/// 历史快照（内存懒加载：首次加锁时从磁盘恢复，避免重启丢历史）。
static HISTORY: Mutex<Vec<Value>> = Mutex::new(Vec::new());

fn history_path() -> PathBuf {
    store_dir().join(HISTORY_FILE)
}

// ---------------------------------------------------------------------------
// 入口：总览 / 详情
// ---------------------------------------------------------------------------

/// 渠道健康度总览（契约第 5.1 节）。
///
/// `hours` 取值约束（1/6/24/168，默认 24）在 [`parse_hours`] 归一。
/// 请求时若距上次采样 > 5 分钟，先落一次历史采样再返回。
pub async fn overview(hours: i64) -> Value {
    let hours = parse_hours(hours);
    let now = now_ms();

    let mut channels: Vec<Value> = Vec::new();
    let mut alerts: Vec<Value> = Vec::new();
    for entry in CHANNELS {
        let report = build_report(entry, hours, now).await;
        if let Some(list) = report.channel.get("alerts").and_then(Value::as_array) {
            alerts.extend(list.iter().cloned());
        }
        channels.push(report.channel);
    }

    // 懒采样：距上次采样 > 5 分钟才把当前各渠道评分落进历史。
    maybe_sample(&channels, now);

    // 趋势：从历史文件降采样后挂到各渠道（含刚采样的当前点）。
    for channel in channels.iter_mut() {
        let id = channel
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        channel["scoreTrend"] = json!(trend_for(&id, hours, now, false));
    }

    let count_status = |want: &str| {
        channels
            .iter()
            .filter(|c| c.get("status").and_then(Value::as_str) == Some(want))
            .count()
    };
    let total_requests = sum_metric(&channels, "requests");
    let ok_requests = sum_metric(&channels, "okRequests");
    let err_requests = sum_metric(&channels, "errRequests");
    let total_accounts = sum_metric(&channels, "totalAccounts");
    let healthy_accounts = sum_metric(&channels, "healthyAccounts");
    let avg_success_rate = (total_requests > 0)
        .then(|| round4(ok_requests as f64 / total_requests as f64));

    json!({
        "generatedAt": now,
        "windowHours": hours,
        "summary": {
            "channels": channels.len(),
            "ok": count_status("ok"),
            "warning": count_status("warning"),
            "critical": count_status("critical"),
            "totalRequests": total_requests,
            "okRequests": ok_requests,
            "errRequests": err_requests,
            "avgSuccessRate": avg_success_rate,
            "totalAccounts": total_accounts,
            "healthyAccounts": healthy_accounts,
            "activeAlerts": alerts.len(),
        },
        "channels": channels,
        "alerts": alerts,
    })
}

/// 单渠道详情（契约第 5.2 节）。
///
/// 未知渠道返回 `channel: null` + 各数组为空的空态 JSON（不 500），
/// 由前端渲染「渠道不存在」。
pub async fn detail(gateway: &str, hours: i64) -> Value {
    let hours = parse_hours(hours);
    let now = now_ms();

    let Some(entry) = CHANNELS.iter().find(|c| c.id == gateway) else {
        return json!({
            "channel": Value::Null,
            "trend": [],
            "buckets": [],
            "models": [],
            "accounts": [],
            "recentLogs": [],
            "checkin": { "accounts": 0, "todayCheckedIn": 0, "lastResults": [] },
        });
    };

    let report = build_report(entry, hours, now).await;
    let mut channel = report.channel;
    channel["scoreTrend"] = json!(trend_for(entry.id, hours, now, false));

    json!({
        "channel": channel,
        "trend": trend_for(entry.id, hours, now, true),
        "buckets": report.traffic.buckets,
        "models": report.traffic.models,
        "accounts": report.accounts,
        "recentLogs": report.traffic.recent_logs,
        "checkin": {
            "accounts": report.checkin.accounts,
            "todayCheckedIn": report.checkin.today_checked_in,
            "lastResults": report.checkin.last_results,
        },
    })
}

// ---------------------------------------------------------------------------
// 参数解析
// ---------------------------------------------------------------------------

/// hours 归一：默认 24；只接受 1/6/24/168；≤0 回落 24；>168 取 168；
/// 其余数值（非数字由 API 层回落 24）同样回落 24。
fn parse_hours(hours: i64) -> i64 {
    match hours {
        h if h <= 0 => 24,
        1 | 6 | 24 | 168 => hours,
        h if h > 168 => 168,
        _ => 24,
    }
}

/// 趋势/分桶桶宽：1h→5min、6h→30min、24h→1h、168h→6h（契约第 5.2 节）。
fn bucket_ms_for(hours: i64) -> i64 {
    match hours {
        1 => 5 * 60_000,
        6 => 30 * 60_000,
        168 => 6 * 3_600_000,
        _ => 60 * 60_000,
    }
}

// ---------------------------------------------------------------------------
// 流量采集
// ---------------------------------------------------------------------------

/// 单渠道的流量指标（按 trafficSource 分流采集）。
#[derive(Default)]
struct TrafficStats {
    requests: u64,
    ok_requests: u64,
    err_requests: u64,
    success_rate: Option<f64>,
    avg_latency_ms: Option<f64>,
    tokens: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_hit_rate: Option<f64>,
    models: Vec<Value>,
    buckets: Vec<Value>,
    recent_logs: Vec<Value>,
}

fn collect_traffic(entry: &ChannelEntry, hours: i64, now: i64) -> TrafficStats {
    match entry.traffic_source {
        // GLM/Loomy：model_usage 调用统计（provider=autoclaw / loomy）。
        "usage" => usage_traffic(if entry.id == "loomy" { "loomy" } else { "autoclaw" }, hours),
        // 其余：gateway_admin 请求日志（kimi/qoder/qwen/qwen-flash/grok/glmweb/miaoshou/
        // workbuddy/trae——后两者自 T5 起由 api.rs 的 chat/models handler 记录）。
        "logs" => logs_traffic(entry.id, hours, now),
        // none：凭证模式，无请求级统计，全部记 0/null。
        _ => TrafficStats::default(),
    }
}

/// logs 渠道流量：`gateway_admin::list_logs(Some(id), 500)` 按 ts 窗口过滤后聚合。
fn logs_traffic(id: &str, hours: i64, now: i64) -> TrafficStats {
    let window_start = now - hours * 3600 * 1000;
    let entries: Vec<Value> = gateway_admin::list_logs(Some(id), 500)
        .into_iter()
        .filter(|e| {
            let ts = e.get("ts").and_then(Value::as_i64).unwrap_or(0);
            // 窗口内（容忍 1 分钟时钟漂移，与 model_usage 的口径一致）。
            ts >= window_start && ts <= now + 60_000
        })
        .collect();

    let mut stats = TrafficStats::default();
    stats.requests = entries.len() as u64;
    stats.ok_requests = entries
        .iter()
        .filter(|e| e.get("ok").and_then(Value::as_bool) == Some(true))
        .count() as u64;
    stats.err_requests = stats.requests.saturating_sub(stats.ok_requests);
    stats.success_rate = (stats.requests > 0)
        .then(|| round4(stats.ok_requests as f64 / stats.requests as f64));

    // 平均延迟：仅统计带了 latencyMs 的条目。
    let latency_sum: f64 = entries
        .iter()
        .filter_map(|e| e.get("latencyMs").and_then(Value::as_f64))
        .sum();
    let latency_n = entries
        .iter()
        .filter(|e| e.get("latencyMs").and_then(Value::as_f64).is_some())
        .count();
    stats.avg_latency_ms = (latency_n > 0).then(|| round1(latency_sum / latency_n as f64));

    // 按模型聚合（calls / ok / failed / successRate / share / avgLatencyMs）。
    let mut per_model: BTreeMap<String, (u64, u64, f64, u64)> = BTreeMap::new();
    for e in &entries {
        let model = e
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let ok = e.get("ok").and_then(Value::as_bool) == Some(true);
        let slot = per_model.entry(model).or_insert((0, 0, 0.0, 0));
        slot.0 += 1;
        if !ok {
            slot.1 += 1;
        }
        if let Some(latency) = e.get("latencyMs").and_then(Value::as_f64) {
            slot.2 += latency;
            slot.3 += 1;
        }
    }
    let mut rows: Vec<(String, (u64, u64, f64, u64))> = per_model.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(&b.0)));
    let total_calls = stats.requests.max(1) as f64;
    stats.models = rows
        .iter()
        .map(|(name, (calls, failed, latency_sum, latency_n))| {
            let ok = calls.saturating_sub(*failed);
            json!({
                "model": name,
                "calls": calls,
                "ok": ok,
                "failed": failed,
                "successRate": round4(ok as f64 / *calls as f64),
                "share": round4(*calls as f64 / total_calls),
                "avgLatencyMs": (*latency_n > 0).then(|| round1(latency_sum / *latency_n as f64)),
            })
        })
        .collect();

    // 分桶序列（补齐空桶，保证时间轴连续，与 model_usage 的 series 一致）。
    let bucket_ms = bucket_ms_for(hours);
    let mut by_bucket: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    for e in &entries {
        let ts = e.get("ts").and_then(Value::as_i64).unwrap_or(0);
        if ts <= 0 {
            continue;
        }
        let slot = by_bucket.entry(floor_to_bucket(ts, bucket_ms)).or_insert((0, 0));
        slot.0 += 1;
        if e.get("ok").and_then(Value::as_bool) != Some(true) {
            slot.1 += 1;
        }
    }
    let mut cursor = floor_to_bucket(window_start, bucket_ms);
    let last = floor_to_bucket(now, bucket_ms);
    while cursor <= last {
        let (requests, failed) = by_bucket.get(&cursor).copied().unwrap_or((0, 0));
        stats.buckets.push(json!({
            "ts": cursor,
            "label": bucket_label(cursor),
            "requests": requests,
            "ok": requests.saturating_sub(failed),
            "failed": failed,
        }));
        cursor += bucket_ms;
    }

    // 最近请求（list_logs 为倒序，直接取前 N 条）。
    stats.recent_logs = entries
        .iter()
        .take(MAX_RECENT_LOGS)
        .map(|e| json!({
            "ts": e.get("ts").and_then(Value::as_i64).unwrap_or(0),
            "model": e.get("model").and_then(Value::as_str).unwrap_or("unknown"),
            "ok": e.get("ok").and_then(Value::as_bool).unwrap_or(false),
            "latencyMs": e.get("latencyMs").and_then(Value::as_i64)
                .or_else(|| e.get("latencyMs").and_then(Value::as_f64).map(|f| f as i64)),
            "error": e.get("error").cloned().unwrap_or(Value::Null),
        }))
        .collect();

    stats
}

/// usage 渠道流量：`model_usage::get_monitor_filtered(hours, bucket_ms, provider)`。
fn usage_traffic(provider: &str, hours: i64) -> TrafficStats {
    let monitor = model_usage::get_monitor_filtered(hours, bucket_ms_for(hours), Some(provider));
    let summary = monitor.get("summary").cloned().unwrap_or(json!({}));

    let calls = summary.get("calls").and_then(Value::as_u64).unwrap_or(0);
    let ok_calls = summary.get("okCalls").and_then(Value::as_u64).unwrap_or(0);
    let failed = summary.get("failedCalls").and_then(Value::as_u64).unwrap_or(0);
    let num = |key: &str| summary.get(key).and_then(Value::as_f64).unwrap_or(0.0);

    let mut stats = TrafficStats {
        requests: calls,
        ok_requests: ok_calls,
        err_requests: failed,
        success_rate: (calls > 0).then(|| round4(ok_calls as f64 / calls as f64)),
        avg_latency_ms: (calls > 0).then(|| num("avgLatencyMs")),
        tokens: num("totalTokens") as u64,
        prompt_tokens: num("promptTokens") as u64,
        completion_tokens: num("completionTokens") as u64,
        cache_hit_rate: (calls > 0).then(|| num("cacheHitRate")),
        ..Default::default()
    };

    // models：透传 get_monitor_filtered 的 models 数组，并补齐契约字段 ok/failed。
    stats.models = monitor
        .get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|m| {
                    let mut m = m.clone();
                    let ok = m.get("okCalls").and_then(Value::as_u64).unwrap_or(0);
                    let failed = m.get("failedCalls").and_then(Value::as_u64).unwrap_or(0);
                    if let Some(obj) = m.as_object_mut() {
                        obj.entry("ok").or_insert(json!(ok));
                        obj.entry("failed").or_insert(json!(failed));
                    }
                    m
                })
                .collect()
        })
        .unwrap_or_default();

    // buckets：透传 series（{ts,total,failed} → 契约的 {ts,label,requests,ok,failed}）。
    stats.buckets = monitor
        .get("series")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|b| {
                    let ts = b.get("ts").and_then(Value::as_i64).unwrap_or(0);
                    let requests = b.get("total").and_then(Value::as_u64).unwrap_or(0);
                    let failed = b.get("failed").and_then(Value::as_u64).unwrap_or(0);
                    json!({
                        "ts": ts,
                        "label": bucket_label(ts),
                        "requests": requests,
                        "ok": requests.saturating_sub(failed),
                        "failed": failed,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    stats
}

/// 桶起点时间 → 本地「HH:MM」标签（契约 5.2 的 label 形如 "14:00"）。
fn bucket_label(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 账号统计
// ---------------------------------------------------------------------------

/// 账号统计（归一化后的计数 + 详情页账号池）。
#[derive(Default)]
struct AccountStats {
    total: u64,
    healthy: u64,
    cooling: u64,
    error: u64,
    /// grok 专用：`pool_overview` 账号 healthScore 均值（无账号为 None）。
    avg_health: Option<f64>,
    /// 详情页账号池（契约 5.2 的 accounts 字段）。
    accounts: Vec<Value>,
}

/// 账号看板视图（归一化字段 + 内部分类标记）。
struct AccountView {
    name: String,
    status: String,
    enabled: bool,
    health_score: Option<i64>,
    cooling: bool,
    error: bool,
    healthy: bool,
    last_error: Option<String>,
    detail: Option<String>,
}

impl AccountView {
    /// 投影为契约第 5.2 节的账号字段。
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "status": self.status,
            "enabled": self.enabled,
            "healthScore": self.health_score,
            "cooldown": self.cooling,
            "lastError": self.last_error,
            "detail": self.detail,
        })
    }
}

fn collect_accounts(id: &str, now: i64) -> AccountStats {
    // grok：账号池健康以 grok_pool::pool_overview 为准
    // （ok/errored/cooling 计数 + healthScore 均值），账号明细取 list_for_display。
    if id == "grok" {
        let pool = grok_pool::pool_overview();
        let accounts = grok::list_for_display();
        let avg_health = pool
            .get("accounts")
            .and_then(Value::as_array)
            .filter(|arr| !arr.is_empty())
            .map(|arr| {
                let sum: i64 = arr
                    .iter()
                    .map(|a| a.get("healthScore").and_then(Value::as_i64).unwrap_or(100))
                    .sum();
                round1(sum as f64 / arr.len() as f64)
            });
        return AccountStats {
            total: pool.get("total").and_then(Value::as_u64).unwrap_or(0),
            healthy: pool.get("ok").and_then(Value::as_u64).unwrap_or(0),
            cooling: pool.get("cooling").and_then(Value::as_u64).unwrap_or(0),
            error: pool.get("errored").and_then(Value::as_u64).unwrap_or(0),
            avg_health,
            accounts: accounts.iter().map(|a| normalize_account(a, now).to_json()).collect(),
        };
    }

    // 其余渠道：统一走各模块的 list_for_display（字段名各不相同，归一后使用）。
    let raw: Vec<Value> = match id {
        // GLM 代理与 GLM 网页版共用 autoclaw 账号池（契约第 1.1 节）。
        "glm" | "glmweb" => crate::modules::autoclaw_accounts::list_for_display(),
        "kimi" => crate::modules::kimi::list_for_display(),
        "qoder" => crate::modules::qoder::list_for_display(),
        // qwen-flash 走 Qoder CN 专用池：只保留 region=="cn" 的账号。
        "qwen-flash" => crate::modules::qoder::list_for_display()
            .into_iter()
            .filter(|a| a.get("region").and_then(Value::as_str) == Some("cn"))
            .collect(),
        // Qwen 桌面版无账号池：有桌面 token 即合成 1 个账号。
        "qwen" => {
            if crate::modules::qwen::extract_desktop_token().is_some() {
                vec![json!({ "name": "Qwen 桌面版", "status": "ok", "enabled": true })]
            } else {
                Vec::new()
            }
        }
        "loomy" => crate::modules::loomy::list_for_display(),
        // 妙手：账号 = 本机客户端登录态 + 已导入池账号（仅统计数量）。
        "miaoshou" => crate::modules::platform_checkin::discover_miaoshou_accounts()
            .into_iter()
            .map(|a| {
                json!({
                    "name": a.get("nickname").or_else(|| a.get("email")).cloned().unwrap_or(Value::Null),
                    "status": "ok",
                    "enabled": true,
                })
            })
            .collect(),
        "trae" => crate::modules::trae_accounts::list_for_display(),
        // MiniMax（2026-10-09）：桌面端账号池；status 口径同 gateway_overview。
        "minimax" => crate::modules::minimax::load_config()
            .accounts
            .iter()
            .map(|a| {
                let cooling = a.cooldown_until_ms > crate::modules::minimax::now_ms();
                json!({
                    "name": a.label,
                    "status": if !a.enabled {
                        "disabled"
                    } else if cooling || a.last_error.is_some() {
                        "cooling"
                    } else {
                        "ok"
                    },
                    "enabled": a.enabled,
                })
            })
            .collect(),
        // workbuddy：凭证模式，无账号池。
        _ => Vec::new(),
    };

    let mut stats = AccountStats::default();
    for item in &raw {
        let view = normalize_account(item, now);
        stats.total += 1;
        if view.error {
            stats.error += 1;
        } else if view.cooling {
            stats.cooling += 1;
        } else if view.healthy {
            stats.healthy += 1;
        }
        stats.accounts.push(view.to_json());
    }
    stats
}

/// 把各模块的账号条目归一成看板字段（源字段名见契约第 2 节，逐一适配）。
fn normalize_account(raw: &Value, now: i64) -> AccountView {
    let name = ["name", "label", "email", "nickname", "uid"]
        .iter()
        .find_map(|f| {
            raw.get(*f)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("账号")
        .to_string();
    let status = raw
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("ok")
        .to_string();
    let enabled = raw.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    // 冷却判定：grok 的 isCooling/cooldownUntil（时间戳）与 kimi 的 cooldownSeconds（剩余秒数）。
    let cooldown_until = raw.get("cooldownUntil").and_then(Value::as_i64).unwrap_or(0);
    let cooldown_seconds = raw.get("cooldownSeconds").and_then(Value::as_i64).unwrap_or(0);
    let is_cooling = raw.get("isCooling").and_then(Value::as_bool) == Some(true);
    let cooling = is_cooling || cooldown_until > now || cooldown_seconds > 0;
    let error = matches!(status.as_str(), "error" | "invalid");
    // 健康判定与 gateway_overview::account_healthy 同口径：enabled=false 不健康；
    // status 缺省（此处归一为 ok）/ok/active/healthy 健康，其余不健康。
    let healthy =
        !error && !cooling && enabled && matches!(status.as_str(), "ok" | "active" | "healthy");
    let health_score = raw.get("healthScore").and_then(Value::as_i64);
    let last_error = ["lastError", "error", "lastErrorKind"]
        .iter()
        .find_map(|f| {
            raw.get(*f)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(String::from);

    AccountView {
        name,
        status,
        enabled,
        health_score,
        cooling,
        error,
        healthy,
        last_error,
        detail: account_detail(raw),
    }
}

/// 账号附加说明（尽力而为）：额度 / 冷却剩余 / 会话天数 / 风险等级。
fn account_detail(raw: &Value) -> Option<String> {
    // Qoder：额度（对象含 used/total 时算剩余百分比，数字原样展示）。
    if let Some(quota) = raw.get("quota").filter(|q| !q.is_null()) {
        if let Some(obj) = quota.as_object() {
            let used = obj.get("used").and_then(Value::as_f64);
            let total = obj.get("total").and_then(Value::as_f64);
            if let (Some(used), Some(total)) = (used, total) {
                if total > 0.0 {
                    return Some(format!("额度 {:.0}%", (1.0 - used / total) * 100.0));
                }
            }
        } else if let Some(n) = quota.as_f64() {
            return Some(format!("额度 {n:.0}"));
        }
    }
    // Kimi：冷却剩余秒数。
    if let Some(secs) = raw.get("cooldownSeconds").and_then(Value::as_i64) {
        if secs > 0 {
            return Some(format!("冷却中（剩余 {secs}s）"));
        }
    }
    // Loomy：会话剩余天数。
    if let Some(days) = raw.get("expiresDays").and_then(Value::as_i64) {
        return Some(format!("会话剩余 {days} 天"));
    }
    // Grok：风险等级 + 连续失败次数。
    if let Some(risk) = raw.get("riskLevel").and_then(Value::as_str) {
        let failures = raw
            .get("consecutiveFailures")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        return Some(format!("风险 {risk} · 连续失败 {failures}"));
    }
    None
}

// ---------------------------------------------------------------------------
// 签到统计
// ---------------------------------------------------------------------------

/// 签到统计：当日（本地 0 点起）result ∈ {success, already} 的去重 accountId 数。
#[derive(Default)]
struct CheckinStats {
    /// 签到日志中出现过（匹配到本渠道）的去重账号数。
    accounts: u64,
    /// 当日签到成功/已签到的去重账号数。
    today_checked_in: u64,
    /// 最近签到结果（倒序）。
    last_results: Vec<Value>,
}

fn collect_checkin(id: &str, now: i64) -> CheckinStats {
    let keys = checkin_keys(id);
    if keys.is_empty() {
        // 无签到平台（kimi/qoder/qwen/grok/miaoshou 等）：checkinAccounts=0，
        // 评分走「无签到能力中性 70 分」分支。
        return CheckinStats::default();
    }

    let day_start = local_day_start_ms(now);
    let mut seen_accounts: HashSet<String> = HashSet::new();
    let mut seen_today: HashSet<String> = HashSet::new();
    let mut last_results: Vec<Value> = Vec::new();

    // load_checkin_logs 为插入序（旧 → 新），倒序遍历即最新在前。
    for entry in load_checkin_logs().iter().rev() {
        if !entry_matches(entry, &keys) {
            continue;
        }
        if let Some(account_id) = entry.get("accountId").and_then(Value::as_str) {
            seen_accounts.insert(account_id.to_string());
            let result = entry.get("result").and_then(Value::as_str).unwrap_or_default();
            let ts = entry.get("ts").and_then(Value::as_i64).unwrap_or(0);
            if ts >= day_start && (result == "success" || result == "already") {
                seen_today.insert(account_id.to_string());
            }
        }
        if last_results.len() < MAX_CHECKIN_RESULTS {
            last_results.push(json!({
                "ts": entry.get("ts").and_then(Value::as_i64).unwrap_or(0),
                "accountId": entry.get("accountId").cloned().unwrap_or(Value::Null),
                "result": entry.get("result").cloned().unwrap_or(Value::Null),
                "error": entry.get("error").cloned().unwrap_or(Value::Null),
            }));
        }
    }

    CheckinStats {
        accounts: seen_accounts.len() as u64,
        today_checked_in: seen_today.len() as u64,
        last_results,
    }
}

/// 渠道的签到匹配键：账号 id / 名称 / 邮箱等身份串。
///
/// 只有存在签到平台的渠道才建键：WorkBuddy 账号库 → workbuddy；autoclaw 池 →
/// glm（GLM 网页版共用同池，另有 ChatGLM 网页签到）；loomy 池 → loomy；
/// trae 池 → trae。其余渠道无每日签到，匹配键为空。
fn checkin_keys(channel_id: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let push_field = |raw: &Value, keys: &mut Vec<String>, field: &str| {
        if let Some(s) = raw.get(field).and_then(Value::as_str).map(str::trim) {
            // 键长 ≥ 3，避免过短串造成跨渠道误匹配。
            if s.len() >= 3 && !keys.iter().any(|k| k == s) {
                keys.push(s.to_string());
            }
        }
    };

    match channel_id {
        // GLM 代理：autoclaw 账号池（签到 email 形如 "AutoClaw · {label}"）。
        "glm" => {
            for a in crate::modules::autoclaw_accounts::list_for_display() {
                push_field(&a, &mut keys, "id");
                push_field(&a, &mut keys, "label");
            }
        }
        // GLM 网页版：与 GLM 代理共用 autoclaw 池，另纳入 ChatGLM 网页签到账号。
        "glmweb" => {
            for a in crate::modules::autoclaw_accounts::list_for_display() {
                push_field(&a, &mut keys, "id");
                push_field(&a, &mut keys, "label");
            }
            for a in crate::modules::platform_checkin::discover_chatglm_accounts() {
                push_field(&a, &mut keys, "id");
                push_field(&a, &mut keys, "email");
                push_field(&a, &mut keys, "nickname");
            }
        }
        "loomy" => {
            for a in crate::modules::loomy::list_for_display() {
                push_field(&a, &mut keys, "id");
                push_field(&a, &mut keys, "label");
            }
        }
        "trae" => {
            for a in crate::modules::trae_accounts::list_for_display() {
                push_field(&a, &mut keys, "id");
                push_field(&a, &mut keys, "label");
            }
        }
        // WorkBuddy 反代：账号池视图为空，但签到账号来自账号库（accounts.json）。
        "workbuddy" => {
            for a in crate::modules::account::load_accounts() {
                push_field(&a, &mut keys, "id");
                push_field(&a, &mut keys, "email");
                push_field(&a, &mut keys, "nickname");
            }
        }
        // DuMate（百度搭子）：本机单账号（id=dumate:local，email/nickname 带
        // DuMate 字样），键即渠道前缀，靠 accountId/email 包含匹配命中。
        "dumate" => keys.push("dumate".to_string()),
        // MiniMax（2026-10-09）：键含渠道前缀，签到条目按前缀匹配命中。
        "minimax" => keys.push("minimax".to_string()),
        _ => {}
    }
    keys
}

/// 签到条目 → 渠道匹配（尽力而为）：条目的 accountId/email 包含渠道身份串即算命中。
fn entry_matches(entry: &Value, keys: &[String]) -> bool {
    let account_id = entry.get("accountId").and_then(Value::as_str).unwrap_or_default();
    let email = entry.get("email").and_then(Value::as_str).unwrap_or_default();
    keys.iter().any(|key| {
        key == account_id
            || key == email
            || (!account_id.is_empty() && account_id.contains(key.as_str()))
            || (!email.is_empty() && email.contains(key.as_str()))
    })
}

// ---------------------------------------------------------------------------
// 渠道基础信息
// ---------------------------------------------------------------------------

/// 单渠道的基础信息（enabled/modelCount/note）与账号统计。
#[derive(Default)]
struct ChannelMeta {
    enabled: bool,
    model_count: u64,
    note: Option<String>,
    /// `gateway_snapshot.health == "error"` 时的错误信息（触发 overviewError 预警）。
    overview_error: Option<String>,
    accounts: AccountStats,
    /// F17：探测超时（不是真的未连接）。总览展示「探测超时（上次正常于 xx）」。
    stale: bool,
    /// 最近一次探测成功的时刻（ms）。
    last_ok_at: Option<i64>,
}

/// 毫秒时间戳格式化为本地时间（探测超时提示用）。
fn format_ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "未知时间".to_string())
}

async fn collect_meta(entry: &ChannelEntry, now: i64) -> ChannelMeta {
    match entry.id {
        // GATEWAYS 表内渠道：enabled/health/modelCount 走 gateway_snapshot。
        "glm" | "kimi" | "qoder" | "qwen" | "qwen-flash" | "grok" | "loomy" | "miaoshou"
        | "minimax" => {
            let snapshot = gateway_snapshot(entry.id).await;
            let overview_error = (snapshot.get("health").and_then(Value::as_str) == Some("error"))
                .then(|| {
                    snapshot
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("网关总览异常")
                        .to_string()
                });
            ChannelMeta {
                enabled: snapshot.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                model_count: snapshot.get("modelCount").and_then(Value::as_u64).unwrap_or(0),
                note: None,
                overview_error,
                accounts: collect_accounts(entry.id, now),
                stale: false,
                last_ok_at: None,
            }
        }
        // glmweb：GATEWAYS 未收录；账号 = autoclaw 池，模型数取 glm_web 目录。
        "glmweb" => {
            let model_count = crate::modules::glm_web::models_payload()
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0) as u64;
            let accounts = collect_accounts("glmweb", now);
            let enabled = accounts.total > 0 || model_count > 0;
            ChannelMeta {
                enabled,
                model_count,
                note: None,
                overview_error: None,
                accounts,
                stale: false,
                last_ok_at: None,
            }
        }
        // workbuddy：凭证模式；enabled 依据 proxy_status（enabled || server.ok）。
        // F17 轻探测：有缓存端口时只打该端口 /global/health，没有缓存才跑完整
        // proxy_status（内含端口扫描）。内层 10s + 外层 3s：外层先触发 = 端口挂起
        // （连接被接受、无响应）→ stale；内层快速 Err = 连接被拒（端口真死）。
        "workbuddy" => {
            let light = crate::modules::workbuddy_proxy::cached_server_port()
                .map(|port| crate::modules::workbuddy_proxy::probe_port_health(
                    port,
                    std::time::Duration::from_secs(10),
                ));
            let status = match light {
                Some(fut) => {
                    match tokio::time::timeout(std::time::Duration::from_secs(3), fut).await {
                        Ok(true) => crate::modules::workbuddy_proxy::proxy_status().await,
                        Ok(false) => json!({}),
                        Err(_) => json!({ "__stale": true }),
                    }
                }
                None => {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        crate::modules::workbuddy_proxy::proxy_status(),
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(_) => json!({ "__stale": true }),
                    }
                }
            };
            let stale = status.get("__stale").and_then(Value::as_bool) == Some(true);
            let server_ok = status
                .pointer("/server/ok")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let last_ok_at = {
                let at = crate::modules::workbuddy_proxy::last_discovery_ok_ms();
                if at > 0 {
                    Some(at)
                } else {
                    None
                }
            };
            let overview_error = if stale {
                Some(match last_ok_at {
                    Some(at) => format!("探测超时（上次正常于 {}）", format_ts(at)),
                    None => "探测超时".to_string(),
                })
            } else {
                None
            };
            ChannelMeta {
                enabled: status.get("enabled").and_then(Value::as_bool).unwrap_or(false) || server_ok,
                model_count: status.get("modelCount").and_then(Value::as_u64).unwrap_or(0),
                // T5：traffic_source 已改 logs，不再标注「暂无请求级统计」。
                note: None,
                overview_error,
                accounts: AccountStats::default(),
                stale,
                last_ok_at,
            }
        }
        // trae：账号 = trae 账号池，enabled 依据 credential.ok。
        // proxy_status 为本地缓存读取（无网络），5s 超时仅兜底；超时标记 stale。
        "trae" => {
            let accounts = collect_accounts("trae", now);
            let status = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::modules::trae_proxy::proxy_status(),
            )
            .await;
            let (status, stale) = match status {
                Ok(v) => (v, false),
                Err(_) => (json!({ "__stale": true }), true),
            };
            let cred_ok = status
                .pointer("/credential/ok")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            ChannelMeta {
                enabled: cred_ok,
                model_count: status
                    .pointer("/models/count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                // T5：traffic_source 已改 logs，不再标注「暂无请求级统计」。
                note: None,
                overview_error: if stale {
                    Some("探测超时".to_string())
                } else {
                    None
                },
                accounts,
                stale,
                last_ok_at: None,
            }
        }
        _ => ChannelMeta::default(),
    }
}

// ---------------------------------------------------------------------------
// 评分与预警
// ---------------------------------------------------------------------------

/// 评分与预警结果。
struct ScoreResult {
    score: i64,
    status: &'static str,
    /// 异常率（none 渠道对外输出 null，但评分与预警仍用该值）。
    anomaly_rate: f64,
    alerts: Vec<Value>,
}

/// 健康评分（契约第 3 节，权重之和 = 1）与预警（契约第 4 节）。
fn score_channel(
    entry: &ChannelEntry,
    hours: i64,
    traffic: &TrafficStats,
    accounts: &AccountStats,
    checkin: &CheckinStats,
    overview_error: Option<&str>,
    enabled: bool,
) -> ScoreResult {
    let requests = traffic.requests;
    let req_err_rate = if requests > 0 {
        traffic.err_requests as f64 / requests as f64
    } else {
        0.0
    };
    let acct_anomaly_ratio = if accounts.total > 0 {
        (accounts.error + accounts.cooling) as f64 / accounts.total as f64
    } else {
        0.0
    };
    let anomaly_rate = 0.7 * req_err_rate + 0.3 * acct_anomaly_ratio;

    // 五个分量（各 0-100）加权求和，四舍五入取整。
    let availability = if requests > 0 {
        traffic.success_rate.unwrap_or(0.0) * 100.0
    } else {
        50.0
    };
    let account_health = if accounts.total > 0 {
        accounts.healthy as f64 / accounts.total as f64 * 100.0
    } else {
        50.0
    };
    let anomaly_score = (1.0 - anomaly_rate) * 100.0;
    let activity = if requests > 0 { 100.0 } else { 40.0 };
    let checkin_score = if checkin.accounts > 0 {
        checkin.today_checked_in as f64 / checkin.accounts as f64 * 100.0
    } else {
        70.0
    };
    let score = (0.35 * availability
        + 0.25 * account_health
        + 0.20 * anomaly_score
        + 0.10 * activity
        + 0.10 * checkin_score)
        .round() as i64;
    let mut status = if score >= 85 {
        "ok"
    } else if score >= 60 {
        "warning"
    } else {
        "critical"
    };

    let mut alerts: Vec<Value> = Vec::new();

    // successRate：requests >= 10 时 < 0.90 警告、< 0.70 严重。
    if requests >= 10 {
        if let Some(rate) = traffic.success_rate {
            if rate < 0.70 {
                alerts.push(alert(
                    entry,
                    "critical",
                    "successRate",
                    Some(rate),
                    Some(0.70),
                    format!("成功率 {} 低于 70%", pct(rate)),
                ));
            } else if rate < 0.90 {
                alerts.push(alert(
                    entry,
                    "warning",
                    "successRate",
                    Some(rate),
                    Some(0.90),
                    format!("成功率 {} 低于 90%", pct(rate)),
                ));
            }
        }
    }

    // anomalyRate：requests > 0 或 totalAccounts > 0 时 > 0.10 警告、> 0.25 严重。
    if requests > 0 || accounts.total > 0 {
        if anomaly_rate > 0.25 {
            alerts.push(alert(
                entry,
                "critical",
                "anomalyRate",
                Some(anomaly_rate),
                Some(0.25),
                format!("异常率 {} 高于 25%", pct(anomaly_rate)),
            ));
        } else if anomaly_rate > 0.10 {
            alerts.push(alert(
                entry,
                "warning",
                "anomalyRate",
                Some(anomaly_rate),
                Some(0.10),
                format!("异常率 {} 高于 10%", pct(anomaly_rate)),
            ));
        }
    }

    // healthyAccounts：totalAccounts > 0 且健康账号为 0 → 严重。
    if accounts.total > 0 && accounts.healthy == 0 {
        alerts.push(alert(
            entry,
            "critical",
            "healthyAccounts",
            Some(0.0),
            None,
            "无健康可用账号".to_string(),
        ));
    }

    // cooldownRatio：totalAccounts > 0 时 > 0.30 警告、> 0.60 严重。
    if accounts.total > 0 {
        let ratio = accounts.cooling as f64 / accounts.total as f64;
        if ratio > 0.60 {
            alerts.push(alert(
                entry,
                "critical",
                "cooldownRatio",
                Some(ratio),
                Some(0.60),
                format!("冷却账号占比 {} 高于 60%", pct(ratio)),
            ));
        } else if ratio > 0.30 {
            alerts.push(alert(
                entry,
                "warning",
                "cooldownRatio",
                Some(ratio),
                Some(0.30),
                format!("冷却账号占比 {} 高于 30%", pct(ratio)),
            ));
        }
    }

    // noTraffic：requests == 0 且渠道 enabled、窗口 >= 24h → 提示。
    if requests == 0 && enabled && hours >= 24 {
        alerts.push(alert(
            entry,
            "info",
            "noTraffic",
            Some(0.0),
            None,
            format!("近 {hours} 小时无请求记录"),
        ));
    }

    // checkinMiss：checkinAccounts > 0 且今日无人签到 → 警告。
    if checkin.accounts > 0 && checkin.today_checked_in == 0 {
        alerts.push(alert(
            entry,
            "warning",
            "checkinMiss",
            Some(0.0),
            None,
            "今日暂无账号签到成功".to_string(),
        ));
    }

    // overviewError：gateway_snapshot.health == "error" → 严重（恒检查）。
    if let Some(err) = overview_error {
        alerts.push(alert(
            entry,
            "critical",
            "overviewError",
            None,
            None,
            format!("网关总览返回异常：{err}"),
        ));
    }

    // grokAvgHealth：仅 grok，pool_overview 账号 healthScore 均值 < 50 警告、< 30 严重。
    if entry.id == "grok" && accounts.total > 0 {
        if let Some(avg) = accounts.avg_health {
            if avg < 30.0 {
                alerts.push(alert(
                    entry,
                    "critical",
                    "grokAvgHealth",
                    Some(avg),
                    Some(30.0),
                    format!("账号平均健康分 {avg:.0} 低于 30"),
                ));
            } else if avg < 50.0 {
                alerts.push(alert(
                    entry,
                    "warning",
                    "grokAvgHealth",
                    Some(avg),
                    Some(50.0),
                    format!("账号平均健康分 {avg:.0} 低于 50"),
                ));
            }
        }
    }

    // 任一 warning/critical 预警触发时，最终状态不得好于 warning。
    if alerts
        .iter()
        .any(|a| a.get("severity").and_then(Value::as_str) == Some("critical"))
    {
        status = "critical";
    } else if alerts
        .iter()
        .any(|a| a.get("severity").and_then(Value::as_str) == Some("warning"))
        && status == "ok"
    {
        status = "warning";
    }

    ScoreResult {
        score,
        status,
        anomaly_rate,
        alerts,
    }
}

/// 组装预警对象（契约第 4 节）。
fn alert(
    entry: &ChannelEntry,
    severity: &str,
    metric: &str,
    value: Option<f64>,
    threshold: Option<f64>,
    message: String,
) -> Value {
    json!({
        "channelId": entry.id,
        "channelName": entry.name,
        "severity": severity,
        "metric": metric,
        "value": value.map(round4),
        "threshold": threshold.map(round4),
        "message": message,
    })
}

/// 百分比文案（1 位小数）。
fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

// ---------------------------------------------------------------------------
// 单渠道报告
// ---------------------------------------------------------------------------

/// 单渠道完整报告（overview/detail 共用）。
struct ChannelReport {
    /// ChannelHealth（scoreTrend 暂为空数组，由调用方挂上历史趋势）。
    channel: Value,
    traffic: TrafficStats,
    accounts: Vec<Value>,
    checkin: CheckinStats,
}

async fn build_report(entry: &ChannelEntry, hours: i64, now: i64) -> ChannelReport {
    let meta = collect_meta(entry, now).await;
    let traffic = collect_traffic(entry, hours, now);
    let checkin = collect_checkin(entry.id, now);
    let scored = score_channel(
        entry,
        hours,
        &traffic,
        &meta.accounts,
        &checkin,
        meta.overview_error.as_deref(),
        meta.enabled,
    );

    // none 渠道无请求级统计：成功率/异常率等流量指标输出 null。
    let anomaly_rate = if entry.traffic_source == "none" {
        Value::Null
    } else {
        json!(round4(scored.anomaly_rate))
    };

    let channel = json!({
        "id": entry.id,
        "name": entry.name,
        "page": entry.page,
        "base": entry.base,
        "enabled": meta.enabled,
        "status": scored.status,
        "score": scored.score,
        "trafficSource": entry.traffic_source,
        "note": meta.note,
        "stale": meta.stale,
        "lastOkAt": meta.last_ok_at,
        "metrics": {
            "requests": traffic.requests,
            "okRequests": traffic.ok_requests,
            "errRequests": traffic.err_requests,
            "successRate": traffic.success_rate,
            "anomalyRate": anomaly_rate,
            "avgLatencyMs": traffic.avg_latency_ms,
            "tokens": traffic.tokens,
            "promptTokens": traffic.prompt_tokens,
            "completionTokens": traffic.completion_tokens,
            "cacheHitRate": traffic.cache_hit_rate,
            "totalAccounts": meta.accounts.total,
            "healthyAccounts": meta.accounts.healthy,
            "coolingAccounts": meta.accounts.cooling,
            "errorAccounts": meta.accounts.error,
            "todayCheckedIn": checkin.today_checked_in,
            "checkinAccounts": checkin.accounts,
            "modelCount": meta.model_count,
        },
        "scoreTrend": [],
        "alerts": scored.alerts,
    });

    ChannelReport {
        channel,
        traffic,
        accounts: meta.accounts.accounts,
        checkin,
    }
}

/// 汇总各渠道的 metrics 计数字段。
fn sum_metric(channels: &[Value], key: &str) -> u64 {
    channels
        .iter()
        .filter_map(|c| {
            c.pointer(&format!("/metrics/{key}"))
                .and_then(Value::as_u64)
        })
        .sum()
}

// ---------------------------------------------------------------------------
// 历史采样与趋势
// ---------------------------------------------------------------------------

/// 内存懒加载：首次加锁时从磁盘恢复历史（照抄 toolcall_monitor 的范式）。
fn load_history_into(guard: &mut Vec<Value>) {
    if guard.is_empty() {
        if let Ok(text) = std::fs::read_to_string(history_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(arr) = v.get("snapshots").and_then(Value::as_array) {
                    *guard = arr.clone();
                }
            }
        }
    }
}

/// 懒采样：距上次采样 > 5 分钟才把当前各渠道评分追加进历史（保留 7 天）。
fn maybe_sample(channels: &[Value], now: i64) {
    let mut guard = HISTORY.lock().unwrap();
    load_history_into(&mut guard);

    let last_at = guard
        .iter()
        .filter_map(|s| s.get("at").and_then(Value::as_i64))
        .max()
        .unwrap_or(0);
    if now.saturating_sub(last_at) <= SAMPLE_DEDUPE_MS {
        return;
    }

    for channel in channels {
        guard.push(json!({
            "at": now,
            "id": channel.get("id").cloned().unwrap_or(Value::Null),
            "score": channel.get("score").cloned().unwrap_or(Value::Null),
            "requests": channel.pointer("/metrics/requests").cloned().unwrap_or(json!(0)),
            "successRate": channel.pointer("/metrics/successRate").cloned().unwrap_or(Value::Null),
            "healthy": channel.pointer("/metrics/healthyAccounts").cloned().unwrap_or(json!(0)),
            "totalAccounts": channel.pointer("/metrics/totalAccounts").cloned().unwrap_or(json!(0)),
        }));
    }

    // 裁剪 7 天前的记录。
    guard.retain(|s| {
        s.get("at")
            .and_then(Value::as_i64)
            .map(|t| now.saturating_sub(t) <= HISTORY_KEEP_MS)
            .unwrap_or(false)
    });

    // 落盘前确保 store_dir 存在（atomic_write 不建目录；首次运行时该目录可能尚未创建）。
    let _ = std::fs::create_dir_all(store_dir());
    let content = serde_json::to_string_pretty(&json!({ "snapshots": &*guard })).unwrap_or_default();
    let _ = atomic_write(&history_path(), &content);
}

/// 从历史文件读某渠道的趋势（窗口内 ≤30 点等间隔降采样，保留末点）。
///
/// `full = true` 时附带 requests/successRate（详情页 trend），否则只留 at/score。
fn trend_for(id: &str, hours: i64, now: i64, full: bool) -> Vec<Value> {
    let mut guard = HISTORY.lock().unwrap();
    load_history_into(&mut guard);

    let window_start = now - hours * 3600 * 1000;
    let points: Vec<&Value> = guard
        .iter()
        .filter(|s| s.get("id").and_then(Value::as_str) == Some(id))
        .filter(|s| {
            s.get("at")
                .and_then(Value::as_i64)
                .is_some_and(|t| t >= window_start && t <= now)
        })
        .collect();

    downsample(&points, MAX_TREND_POINTS)
        .into_iter()
        .map(|s| {
            if full {
                json!({
                    "at": s.get("at").cloned().unwrap_or(Value::Null),
                    "score": s.get("score").cloned().unwrap_or(Value::Null),
                    "requests": s.get("requests").cloned().unwrap_or(json!(0)),
                    "successRate": s.get("successRate").cloned().unwrap_or(Value::Null),
                })
            } else {
                json!({
                    "at": s.get("at").cloned().unwrap_or(Value::Null),
                    "score": s.get("score").cloned().unwrap_or(Value::Null),
                })
            }
        })
        .collect()
}

/// 等间隔降采样（保留首末点）。
fn downsample<T>(items: &[T], max: usize) -> Vec<&T> {
    if max == 0 {
        return Vec::new();
    }
    if items.len() <= max {
        return items.iter().collect();
    }
    let last = items.len() - 1;
    (0..max).map(|i| &items[i * last / (max - 1)]).collect()
}

// ---------------------------------------------------------------------------
// 数值口径
// ---------------------------------------------------------------------------

/// 比率保留 4 位小数（与 model_usage 的口径一致）。
fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

/// 毫秒延迟保留 1 位小数。
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
