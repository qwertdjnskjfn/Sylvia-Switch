//! WorkBuddy 工具调用稳定性监控。
//!
//! ## 解决什么问题
//! lite 快速档模型存在**非确定性**：同一个"需要查代码"的请求，这次产出标准
//! `tool_calls`，下次却直接回答。这是模型固有波动，**不是配置损坏、不是协议
//! 失效、不是上游故障**——但表象上极像"工具调用坏了"，容易误判并去做错误的
//! 修复（改协议、换账号、重启服务）。
//!
//! 本模块把每次请求的结果记下来，计算「相邻请求间 called 状态翻转」的频率，
//! 并给出明确诊断：区分 **lite 档工具调用随机波动** / 数据不足 / 上游或协议
//! 真问题。同时记录请求实际携带的 `temperature`（用于判断网关是否透传了
//! temperature=0、以及客户端是否固定发了别的值），并给出三个应对方向的依据。
//!
//! 数据落盘 `~/.wb-switch/toolcall_monitor.json`（环形缓冲，保留最近 500 条）。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::modules::config::{atomic_write, now_ms, store_dir};

const MONITOR_FILE: &str = "toolcall_monitor.json";
/// 环形缓冲上限：500 条约覆盖数天的中频使用。
const MAX_EVENTS: usize = 500;
/// 相邻请求对的最小样本量：少于此值只报"数据不足"，不下波动结论。
const MIN_TRANSITIONS_FOR_VERDICT: usize = 5;

static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

fn monitor_path() -> PathBuf {
    store_dir().join(MONITOR_FILE)
}

/// 一次请求的工具调用观测记录。
#[derive(Clone, Debug)]
pub struct ToolCallEvent {
    /// 请求是否携带 tools 数组（IDE agent 请求才有监控意义）。
    pub had_tools: bool,
    /// 是否实际产出 tool_calls。
    pub called: bool,
    /// 调用的工具名列表。
    pub tool_names: Vec<String>,
    /// 请求耗时（毫秒）。
    pub latency_ms: u128,
    /// 请求是否成功（非 502）。
    pub ok: bool,
    /// 失败原因（成功时为空）。
    pub error: String,
    /// 请求携带的 temperature（None = 未携带，即由服务端/上游默认决定）。
    pub temperature: Option<f64>,
    /// 实际通道：web / qoder。
    pub channel: String,
    /// 实际档位：lite / pro / max。
    pub tier: String,
    /// 下游请求的模型名。
    pub requested_model: String,
}

/// 记录一次观测（内存 + 落盘）。调用方在请求结束后调用，不阻塞主流程。
pub fn record(ev: ToolCallEvent) {
    let entry = json!({
        "ts": now_ms(),
        "hadTools": ev.had_tools,
        "called": ev.called,
        "toolNames": ev.tool_names,
        "latencyMs": ev.latency_ms as u64,
        "ok": ev.ok,
        "error": if ev.error.is_empty() { Value::Null } else { json!(ev.error) },
        "temperature": ev.temperature,
        "channel": ev.channel,
        "tier": ev.tier,
        "requestedModel": ev.requested_model,
    });
    {
        let mut events = EVENTS.lock().unwrap();
        if events.is_empty() {
            // 首次使用：尝试从磁盘恢复，避免重启丢历史。
            if let Ok(text) = std::fs::read_to_string(monitor_path()) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if let Some(arr) = v.get("events").and_then(Value::as_array) {
                        *events = arr.clone();
                    }
                }
            }
        }
        events.push(entry);
        if events.len() > MAX_EVENTS {
            let drop = events.len() - MAX_EVENTS;
            events.drain(..drop);
        }
        let snapshot = events.clone();
        drop(events);
        let content = serde_json::to_string_pretty(&json!({
            "events": snapshot,
            "updatedAt": now_ms(),
        }))
        .unwrap_or_default();
        let _ = atomic_write(&monitor_path(), &content);
    }
}

/// 诊断分类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerdictKind {
    /// lite 档工具调用随机波动（正常现象，非故障）。
    LiteFluctuation,
    /// 样本不足，无法结论。
    InsufficientData,
    /// 上游/账号池问题（请求本身在失败，与工具协议无关）。
    UpstreamIssue,
    /// 协议未触发（从不调工具，属协议/提示词问题）。
    ProtocolNotTriggering,
}

impl VerdictKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            VerdictKind::LiteFluctuation => "lite-fluctuation",
            VerdictKind::InsufficientData => "insufficient-data",
            VerdictKind::UpstreamIssue => "upstream-issue",
            VerdictKind::ProtocolNotTriggering => "protocol-not-triggering",
        }
    }
}

/// 统计 + 诊断结果（读全局事件）。
pub fn stats() -> Value {
    let events = {
        let mut g = EVENTS.lock().unwrap();
        if g.is_empty() {
            if let Ok(text) = std::fs::read_to_string(monitor_path()) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if let Some(arr) = v.get("events").and_then(Value::as_array) {
                        *g = arr.clone();
                    }
                }
            }
        }
        g.clone()
    };
    compute_stats(&events)
}

/// 纯统计 + 诊断：不碰全局状态，便于单测（Rust 测试默认并行，共享 static 会互相污染）。
fn compute_stats(events: &[Value]) -> Value {
    let with_tools: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("hadTools").and_then(Value::as_bool) == Some(true))
        .collect();
    let total = events.len();
    let with_tools_n = with_tools.len();
    let called = with_tools
        .iter()
        .filter(|e| e.get("called").and_then(Value::as_bool) == Some(true))
        .count();
    let direct = with_tools_n - called;
    let ok_n = with_tools
        .iter()
        .filter(|e| e.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    let ok_rate = if with_tools_n > 0 {
        ok_n as f64 / with_tools_n as f64
    } else {
        0.0
    };

    // 相邻「带 tools 请求」之间的 called 状态翻转次数 = 不一致现象的直接度量。
    let mut transitions = 0usize;
    let mut flips = 0usize;
    let mut last_called: Option<bool> = None;
    let mut flip_ts: Vec<i64> = Vec::new();
    for e in &with_tools {
        let c = e.get("called").and_then(Value::as_bool).unwrap_or(false);
        if let Some(prev) = last_called {
            transitions += 1;
            if prev != c {
                flips += 1;
                if let Some(ts) = e.get("ts").and_then(Value::as_i64) {
                    flip_ts.push(ts);
                }
            }
        }
        last_called = Some(c);
    }
    let fluctuation_rate = if transitions > 0 {
        flips as f64 / transitions as f64
    } else {
        0.0
    };

    // 诊断：先排除上游问题，再判协议未触发，最后才归为 lite 波动。
    let (kind, verdict, detail) = if transitions < MIN_TRANSITIONS_FOR_VERDICT {
        (
            VerdictKind::InsufficientData,
            "样本不足，暂不下结论".to_string(),
            format!(
                "当前仅有 {transitions} 个相邻「带 tools 请求」样本（需 ≥{MIN_TRANSITIONS_FOR_VERDICT}）。请继续在 WorkBuddy 中使用该模型发起需要查代码的请求。"
            ),
        )
    } else if ok_rate < 0.8 {
        (
            VerdictKind::UpstreamIssue,
            "上游/账号池异常（与工具协议无关）".to_string(),
            format!(
                "带 tools 请求成功率仅 {:.0}%——请求本身在失败（空回复/额度/风控），此时统计「是否调工具」没有意义。请先解决可用性问题（见 Qwen 通道页健康状态）。",
                ok_rate * 100.0
            ),
        )
    } else if called == 0 {
        (
            VerdictKind::ProtocolNotTriggering,
            "工具调用从未触发（协议/提示词问题）".to_string(),
            "所有带 tools 请求都直接回答，一次都没调用。这不是波动，是协议或提示词未生效——请检查 TOOL_USE_PROTOCOL 是否注入、工具描述是否清晰。".to_string(),
        )
    } else if flips > 0 {
        let rate_pct = fluctuation_rate * 100.0;
        (
            VerdictKind::LiteFluctuation,
            format!("lite 档工具调用随机波动（{rate_pct:.0}% 相邻请求不一致）"),
            format!(
                "请求全部成功（{:.0}%）、工具确实能被调用（{called}/{with_tools_n}），但相邻请求的「调/不调」状态发生 {flips} 次翻转。\
                 这是 lite 快速档的固有非确定性，**不是配置损坏、不是协议失效、不是上游故障**——无需改协议或换账号。",
                ok_rate * 100.0
            ),
        )
    } else {
        (
            VerdictKind::LiteFluctuation,
            "工具调用行为一致（当前未观测到波动）".to_string(),
            format!("{transitions} 个相邻样本的「调/不调」状态完全一致，暂未波动。"),
        )
    };

    // temperature 透传可见性：统计请求实际携带的值分布。
    let mut temp_counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut temp_missing = 0usize;
    for e in &with_tools {
        match e.get("temperature") {
            Some(Value::Number(n)) => {
                *temp_counts.entry(n.to_string()).or_insert(0) += 1;
            }
            _ => temp_missing += 1,
        }
    }
    let temperature_view: Vec<Value> = temp_counts
        .iter()
        .map(|(v, n)| json!({"value": v, "count": n}))
        .collect();

    // 趋势：按时间分 8 桶，展示每桶的调用率。
    let trend = build_trend(&with_tools);

    json!({
        "total": total,
        "withTools": with_tools_n,
        "called": called,
        "direct": direct,
        "callRate": if with_tools_n > 0 { called as f64 / with_tools_n as f64 } else { 0.0 },
        "okRate": ok_rate,
        "transitions": transitions,
        "flips": flips,
        "fluctuationRate": fluctuation_rate,
        "lastFlipTs": flip_ts.last().copied(),
        "verdict": verdict,
        "verdictKind": kind.as_str(),
        "verdictDetail": detail,
        "isConfigProblem": false,
        "temperature": {
            "missing": temp_missing,
            "distribution": temperature_view,
            "note": "missing = 请求未携带 temperature（由上游默认决定）。若分布里出现固定非 0 值，说明客户端在覆盖 temperature=0。",
        },
        "trend": trend,
        "updatedAt": now_ms(),
    })
}

/// 按时间分桶的调用率趋势（最多 8 桶，从旧到新）。
fn build_trend(with_tools: &[&Value]) -> Vec<Value> {
    const BUCKETS: usize = 8;
    if with_tools.is_empty() {
        return Vec::new();
    }
    let first = with_tools.first().and_then(|e| e.get("ts").and_then(Value::as_i64)).unwrap_or(0);
    let last = with_tools.last().and_then(|e| e.get("ts").and_then(Value::as_i64)).unwrap_or(0);
    let span = (last - first).max(1);
    let mut buckets: Vec<(usize, usize)> = vec![(0, 0); BUCKETS];
    for e in with_tools {
        let ts = e.get("ts").and_then(Value::as_i64).unwrap_or(first);
        let idx = (((ts - first) as f64 / span as f64) * (BUCKETS as f64 - 1.0)).round() as usize;
        let idx = idx.min(BUCKETS - 1);
        buckets[idx].1 += 1;
        if e.get("called").and_then(Value::as_bool) == Some(true) {
            buckets[idx].0 += 1;
        }
    }
    buckets
        .into_iter()
        .enumerate()
        .map(|(i, (c, n))| {
            json!({
                "bucket": i,
                "total": n,
                "called": c,
                "callRate": if n > 0 { json!(c as f64 / n as f64) } else { Value::Null },
            })
        })
        .collect()
}

/// 最近 n 条事件（旧→新），供实时事件流展示。
pub fn recent_events(n: usize) -> Vec<Value> {
    let events = {
        let mut g = EVENTS.lock().unwrap();
        if g.is_empty() {
            if let Ok(text) = std::fs::read_to_string(monitor_path()) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if let Some(arr) = v.get("events").and_then(Value::as_array) {
                        *g = arr.clone();
                    }
                }
            }
        }
        g.clone()
    };
    let skip = events.len().saturating_sub(n);
    events.into_iter().skip(skip).collect()
}

/// 清空监控数据（换测试批次时用）。
pub fn clear() {
    {
        let mut g = EVENTS.lock().unwrap();
        g.clear();
    }
    let _ = atomic_write(
        &monitor_path(),
        &serde_json::to_string_pretty(&json!({"events": [], "updatedAt": now_ms()})).unwrap_or_default(),
    );
}

/// 三方向建议（与文档结论一致，供前端直接渲染）。
pub fn advisories() -> Value {
    json!({
        "accept": {
            "title": "方向一：接受",
            "body": "多数请求已符合预期；偶发的「未调用工具、直接回答」标记为 lite 档可接受的噪声波动。无需改协议或换账号——那是在修一个没坏的东西。",
        },
        "reduceRandomness": {
            "title": "方向二：降低随机性",
            "body": "设置 temperature=0 可降低模型随机性。请结合上方 temperature 分布判断：若客户端（WorkBuddy）固定发送了非 0 值，需要在客户端侧改；若网关收到的是空（missing），说明由上游默认决定，可在网关侧补默认 temperature=0。",
        },
        "switchTier": {
            "title": "方向三：换档位",
            "body": "lite 快速档本身波动更大；若未来可用 pro/max 额度，工具调用稳定性通常会明显改善。但当前 Qoder 账号为 Free 档、pro/max 零额度，暂时无法通过换档完全消除波动。",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造合成事件序列（纯数据，不碰全局 static——测试并行安全）。
    fn synth(pattern: &[(bool, bool, Option<f64>)]) -> Vec<Value> {
        pattern
            .iter()
            .enumerate()
            .map(|(i, (had, called, temp))| {
                json!({
                    "ts": 1_000_000 + i as i64 * 60_000,
                    "hadTools": had,
                    "called": called,
                    "toolNames": if *called { json!(["read_file"]) } else { json!([]) },
                    "latencyMs": 1500,
                    "ok": true,
                    "error": Value::Null,
                    "temperature": temp,
                    "channel": "qoder",
                    "tier": "lite",
                    "requestedModel": "qwen3.8-flash",
                })
            })
            .collect()
    }

    #[test]
    fn detects_fluctuation_and_labels_it_lite_not_config() {
        // 调/不调交替 6 次 → 5 次翻转，全部成功
        let events = synth(&[
            (true, true, None),
            (true, false, None),
            (true, true, None),
            (true, false, None),
            (true, true, None),
            (true, false, None),
        ]);
        let s = compute_stats(&events);
        assert_eq!(s["withTools"], 6);
        assert_eq!(s["called"], 3);
        assert_eq!(s["direct"], 3);
        assert_eq!(s["transitions"], 5);
        assert_eq!(s["flips"], 5);
        assert_eq!(s["verdictKind"], "lite-fluctuation");
        // 关键：必须明确否认是配置问题
        assert_eq!(s["isConfigProblem"], false);
        assert!(s["verdictDetail"].as_str().unwrap().contains("不是配置损坏"));
        assert!(s["verdict"].as_str().unwrap().contains("随机波动"));
    }

    #[test]
    fn stable_behavior_is_not_flagged_as_fluctuation() {
        let events = synth(&[(true, true, None); 6]);
        let s = compute_stats(&events);
        assert_eq!(s["flips"], 0);
        assert_eq!(s["fluctuationRate"], 0.0);
        assert_eq!(s["verdictKind"], "lite-fluctuation");
        assert!(s["verdict"].as_str().unwrap().contains("未观测到波动"));
    }

    #[test]
    fn upstream_failures_are_not_misreported_as_fluctuation() {
        // 全部失败：必须判为上游问题，而不是 lite 波动
        let mut events = synth(&[(true, false, None); 6]);
        for e in events.iter_mut() {
            e["ok"] = json!(false);
        }
        let s = compute_stats(&events);
        assert_eq!(s["verdictKind"], "upstream-issue");
        assert!(!s["verdict"].as_str().unwrap().contains("随机波动"));
        assert_eq!(s["isConfigProblem"], false);
    }

    #[test]
    fn never_calling_tools_is_a_protocol_problem_not_noise() {
        let events = synth(&[(true, false, None); 6]);
        let s = compute_stats(&events);
        assert_eq!(s["verdictKind"], "protocol-not-triggering");
    }

    #[test]
    fn insufficient_samples_yields_no_verdict() {
        let events = synth(&[(true, true, None), (true, false, None)]);
        let s = compute_stats(&events);
        assert_eq!(s["verdictKind"], "insufficient-data");
        assert_eq!(s["flips"], 1);
    }

    #[test]
    fn temperature_distribution_is_visible() {
        let events = synth(&[
            (true, true, Some(0.0)),
            (true, false, Some(0.0)),
            (true, true, None),
        ]);
        let s = compute_stats(&events);
        let dist = s["temperature"]["distribution"].as_array().unwrap();
        assert_eq!(dist.len(), 1, "应只有一个显式值 0.0");
        assert_eq!(dist[0]["value"], "0.0");
        assert_eq!(dist[0]["count"], 2);
        assert_eq!(s["temperature"]["missing"], 1);
    }

    #[test]
    fn requests_without_tools_are_excluded_from_fluctuation() {
        // 不带 tools 的请求不计入波动统计（无监控意义）
        let events = synth(&[
            (false, false, None),
            (true, true, None),
            (false, false, None),
            (true, false, None),
            (true, true, None),
            (true, false, None),
        ]);
        let s = compute_stats(&events);
        assert_eq!(s["total"], 6);
        assert_eq!(s["withTools"], 4);
        assert_eq!(s["transitions"], 3);
        assert_eq!(s["flips"], 3);
    }

    #[test]
    fn trend_buckets_are_bounded() {
        let events = synth(&[(true, true, None); 10]);
        let s = compute_stats(&events);
        let trend = s["trend"].as_array().unwrap();
        assert_eq!(trend.len(), 8, "最多 8 桶");
    }

    #[test]
    fn advisories_cover_three_directions() {
        let a = advisories();
        assert!(a.get("accept").is_some());
        assert!(a.get("reduceRandomness").is_some());
        assert!(a.get("switchTier").is_some());
        assert!(a["switchTier"]["body"].as_str().unwrap().contains("零额度"));
    }
}
