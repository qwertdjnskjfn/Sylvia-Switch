//! 官方流水（`ledgers_std`）的解析、匹配与认领：**唯一实现**（纯逻辑可测）。
//!
//! 此前「即时认领」（`try_claim_ledger_entry`）与「批量对账」（`reconcile_with_ledger`）
//! 各写一遍相同的筛选 + 取最近逻辑，差异只在该用多宽的窗口；两处一旦改动不同步，
//! 就会出现「同一次调用有时认领得上、有时认领不上」。这里把规则收敛到一处，
//! 窗口由调用方显式给出。

use chrono::{DateTime, Local};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// 扣费落账允许的最大偏差：流水条目时间与调用时间之差超过该值不参与匹配。
///
/// 实测（2026-09-14）：不同账号的 `occurred_at` 漂移差别很大 —— 有的账号稳定在
/// 0~5 分钟，有的账号记账时间会比调用时间**超前约 25 分钟**（批量结算落账）。
/// 固定 5 分钟窗口会让后者的记录永远停在「待对账」。放宽到 35 分钟，
/// 配合「时间最接近优先 + 每条流水只消费一次」，既能吸收漂移，
/// 又不会把明显无关的条目拉进来。
pub const RECONCILE_WINDOW_MS: i64 = 35 * 60 * 1000;

/// 允许流水略早于调用时刻的容差（记账抖动）。
pub const LEDGER_EARLY_TOLERANCE_MS: i64 = 10_000;

/// 一条已解析的流水 debit 条目。
#[derive(Clone, Debug, PartialEq)]
pub struct LedgerEntry {
    pub id: String,
    pub ts: i64,
    pub amount: f64,
    pub model: String,
}

/// 匹配窗口（闭区间，毫秒）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchWindow {
    pub not_before_ms: i64,
    pub not_after_ms: i64,
}

impl MatchWindow {
    /// 批量对账窗口：`[调用时刻 − 容差, 调用时刻 + 记账号漂移上限]`。
    pub fn for_reconcile(call_ts: i64) -> Self {
        Self {
            not_before_ms: call_ts - LEDGER_EARLY_TOLERANCE_MS,
            not_after_ms: call_ts + RECONCILE_WINDOW_MS,
        }
    }

    /// 即时认领窗口：`[调用开始 − 容差, 当前时刻]`。
    ///
    /// 即时认领发生在调用刚结束时，超前的流水此时**还不存在**，因此上界取 now。
    pub fn for_immediate(call_started_ms: i64, now_ms: i64) -> Self {
        Self {
            not_before_ms: call_started_ms - LEDGER_EARLY_TOLERANCE_MS,
            not_after_ms: now_ms,
        }
    }
}

/// 从流水描述提取模型名（`"AutoClaw GLM-5.3-Flash模型消耗积分"` → `"GLM-5.3-Flash"`）。
pub fn model_from_description(description: &str) -> Option<String> {
    let name = description
        .strip_prefix("AutoClaw ")?
        .strip_suffix("模型消耗积分")?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 解析 `occurred_at`（RFC3339）为本地毫秒时间戳。
pub fn parse_occurred_at(value: &Value) -> Option<i64> {
    value
        .get("occurred_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Local).timestamp_millis())
}

/// 把官方流水响应里的一条 `entry` 解析为 [`LedgerEntry`]。
///
/// 仅接受 `mutation_type == "debit"` 且描述能解析出模型名的条目；
/// 金额取绝对值（官方以负数表示扣减）。
pub fn parse_entry(entry: &Value) -> Option<LedgerEntry> {
    if entry.get("mutation_type").and_then(Value::as_str) != Some("debit") {
        return None;
    }
    let description = entry.get("description").and_then(Value::as_str).unwrap_or("");
    let model = model_from_description(description)?;
    let id = entry.get("id").and_then(Value::as_str)?;
    let ts = parse_occurred_at(entry)?;
    Some(LedgerEntry {
        id: id.to_string(),
        ts,
        amount: entry
            .get("amount")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .abs(),
        model,
    })
}

/// 在候选里挑「模型同名（忽略大小写）且在窗口内、时间最接近调用时刻」的一条，返回下标。
///
/// 调用方命中后应把该下标对应条目**从候选池移除**，从而天然满足
/// 「每条流水只消费一次」。
pub fn pick_best_match(
    entries: &[LedgerEntry],
    model: &str,
    call_ts: i64,
    window: MatchWindow,
) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.ts >= window.not_before_ms
                && entry.ts <= window.not_after_ms
                && entry.model.eq_ignore_ascii_case(model)
        })
        .min_by_key(|(_, entry)| (entry.ts - call_ts).abs())
        .map(|(index, _)| index)
}

// ---------------------------------------------------------------------------
// 认领注册表
// ---------------------------------------------------------------------------

/// 本进程已认领的流水 id（含已落库与认领后尚未落库的），防止并发重复归因。
static CLAIMED: OnceLock<Mutex<Option<HashSet<String>>>> = OnceLock::new();

fn claimed() -> &'static Mutex<Option<HashSet<String>>> {
    CLAIMED.get_or_init(|| Mutex::new(None))
}

/// 用历史记录里的 `ledgerId` 播种注册表（**仅首次生效**）。
///
/// 认领前必须先播种，否则历史已消费的流水会被重复认领。
pub fn ensure_seeded<F: FnOnce() -> HashSet<String>>(seed: F) {
    let mut guard = claimed().lock().unwrap();
    if guard.is_none() {
        *guard = Some(seed());
    }
}

/// 认领一条流水；返回 `true` 表示本次成功认领（此前未被认领过）。
pub fn claim(id: &str) -> bool {
    let mut guard = claimed().lock().unwrap();
    guard
        .get_or_insert_with(HashSet::new)
        .insert(id.to_string())
}

/// 已认领集合的快照：对账时用来排除「本进程在途认领」的流水。
pub fn claimed_snapshot() -> HashSet<String> {
    claimed().lock().unwrap().clone().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, ts: i64, model: &str, amount: f64) -> LedgerEntry {
        LedgerEntry {
            id: id.to_string(),
            ts,
            amount,
            model: model.to_string(),
        }
    }

    fn raw(id: &str, occurred: &str, desc: &str, amount: f64, mutation: &str) -> Value {
        json!({
            "id": id,
            "occurred_at": occurred,
            "description": desc,
            "amount": amount,
            "mutation_type": mutation,
        })
    }

    #[test]
    fn description_extraction_requires_both_affixes() {
        assert_eq!(
            model_from_description("AutoClaw GLM-5.3-Flash模型消耗积分").as_deref(),
            Some("GLM-5.3-Flash")
        );
        // 签到加赠、积分过期等描述不匹配
        assert_eq!(model_from_description("每日签到赠送积分"), None);
        assert_eq!(model_from_description("AutoClaw 模型消耗积分"), None);
        assert_eq!(model_from_description("GLM-5.3模型消耗积分"), None);
        // 首尾空白会被裁掉
        assert_eq!(
            model_from_description("AutoClaw  GLM-5.3 模型消耗积分").as_deref(),
            Some("GLM-5.3")
        );
    }

    #[test]
    fn parse_entry_filters_non_debit_and_bad_rows() {
        let good = raw("L1", "2026-09-18T04:00:00+08:00", "AutoClaw GLM-5.3模型消耗积分", -12.5, "debit");
        let parsed = parse_entry(&good).expect("debit 应被解析");
        assert_eq!(parsed.id, "L1");
        assert_eq!(parsed.model, "GLM-5.3");
        assert_eq!(parsed.amount, 12.5, "金额取绝对值");
        assert!(parsed.ts > 0);

        // 非 debit
        assert_eq!(parse_entry(&raw("L2", "2026-09-18T04:00:00+08:00", "AutoClaw GLM-5.3模型消耗积分", -1.0, "credit")), None);
        // 描述不匹配
        assert_eq!(parse_entry(&raw("L3", "2026-09-18T04:00:00+08:00", "每日签到赠送", -1.0, "debit")), None);
        // 缺字段
        assert_eq!(parse_entry(&json!({ "mutation_type": "debit" })), None);
        // 时间非法
        assert_eq!(parse_entry(&raw("L4", "not-a-date", "AutoClaw GLM-5.3模型消耗积分", -1.0, "debit")), None);
    }

    #[test]
    fn pick_best_match_prefers_closest_in_window() {
        let entries = vec![
            entry("far", 1_000_000, "GLM-5.3", 1.0),
            entry("near", 1_000_100, "GLM-5.3", 2.0),
            entry("late", 1_000_200, "GLM-5.3", 3.0),
        ];
        let window = MatchWindow {
            not_before_ms: 999_000,
            not_after_ms: 1_100_000,
        };
        let index = pick_best_match(&entries, "GLM-5.3", 1_000_090, window).unwrap();
        assert_eq!(entries[index].id, "near", "应取时间最接近的一条");
    }

    #[test]
    fn pick_best_match_is_case_insensitive_but_exact() {
        let entries = vec![entry("a", 1_000_000, "GLM-5.3-Flash", 1.0)];
        let window = MatchWindow {
            not_before_ms: 0,
            not_after_ms: 2_000_000,
        };
        // 大小写不同视为同一模型
        assert!(pick_best_match(&entries, "glm-5.3-flash", 1_000_000, window).is_some());
        // 🔴 前缀相同但模型不同必须**不**匹配（GLM-5.3 不得误配 GLM-5.3-Flash）
        assert_eq!(pick_best_match(&entries, "GLM-5.3", 1_000_000, window), None);
    }

    #[test]
    fn pick_best_match_respects_window_bounds() {
        let entries = vec![entry("a", 1_000_000, "GLM-5.3", 1.0)];
        let too_early = MatchWindow {
            not_before_ms: 1_000_001,
            not_after_ms: 2_000_000,
        };
        let too_late = MatchWindow {
            not_before_ms: 0,
            not_after_ms: 999_999,
        };
        assert_eq!(pick_best_match(&entries, "GLM-5.3", 1_000_000, too_early), None);
        assert_eq!(pick_best_match(&entries, "GLM-5.3", 1_000_000, too_late), None);
        // 边界是闭区间
        let inclusive = MatchWindow {
            not_before_ms: 1_000_000,
            not_after_ms: 1_000_000,
        };
        assert!(pick_best_match(&entries, "GLM-5.3", 1_000_000, inclusive).is_some());
    }

    #[test]
    fn window_constructors_match_historical_semantics() {
        let reconcile = MatchWindow::for_reconcile(1_000_000);
        assert_eq!(reconcile.not_before_ms, 1_000_000 - LEDGER_EARLY_TOLERANCE_MS);
        assert_eq!(reconcile.not_after_ms, 1_000_000 + RECONCILE_WINDOW_MS);
        // 批量记账可**超前**调用时刻，所以上界必须大于调用时刻
        assert!(reconcile.not_after_ms > 1_000_000);

        let immediate = MatchWindow::for_immediate(1_000_000, 1_002_000);
        assert_eq!(immediate.not_before_ms, 1_000_000 - LEDGER_EARLY_TOLERANCE_MS);
        assert_eq!(immediate.not_after_ms, 1_002_000);
    }

    #[test]
    fn removing_matched_entry_guarantees_single_consumption() {
        // 两条同模型调用时间接近，一条流水 → 只能被其中一条消费
        let mut entries = vec![entry("only", 1_000_050, "GLM-5.3", 9.0)];
        let window = MatchWindow {
            not_before_ms: 999_000,
            not_after_ms: 2_000_000,
        };
        let first = pick_best_match(&entries, "GLM-5.3", 1_000_000, window);
        assert!(first.is_some());
        entries.remove(first.unwrap());
        let second = pick_best_match(&entries, "GLM-5.3", 1_000_100, window);
        assert_eq!(second, None, "同一条流水不得被消费两次");
    }

    #[test]
    fn claim_registry_is_idempotent() {
        let id = format!("test-claim-{}", std::process::id());
        ensure_seeded(HashSet::new);
        assert!(claim(&id), "首次认领应成功");
        assert!(!claim(&id), "重复认领应失败");
        assert!(claimed_snapshot().contains(&id));
    }
}
