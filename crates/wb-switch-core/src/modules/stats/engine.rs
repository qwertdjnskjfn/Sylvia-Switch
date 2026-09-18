//! 聚合内核：调用级累加与派生指标的**唯一实现**（纯逻辑，可单测）。
//!
//! 此前同一套算术在三处各写一遍：`model_usage::MonitorAgg`、
//! `model_usage::accumulate`、`token_stats::Totals`（口径漂移 C7）。
//! 这里先集中「调用级用量累加 + 派生指标」，供各统计模块复用。
//!
//! 数值类型约定：本模块一律用 `f64`，与 `model_usage` 既有输出保持一致
//! （其 JSON 经 `json!` 渲染为浮点）。需要整数精确语义的模块
//! （`token_stats`）继续走 [`super::units`] 的 `*_u64` 系列，不在此强转。

use serde_json::{json, Value};

use super::units::{self, round4};

/// 调用级用量累加器。
///
/// 字段与 `model_usage` 的历史实现一一对应，行为等价。
#[derive(Clone, Debug, Default)]
pub struct UsageAgg {
    pub calls: u64,
    pub failed: u64,
    /// 「待对账」的调用笔数（积分尚未归因，**不计入 `credits`**）。
    pub credits_pending: u64,
    pub prompt: f64,
    pub completion: f64,
    /// 上游 `usage.total_tokens` 之和。
    ///
    /// ⚠️ 与 [`Self::total_tokens`]（`prompt + completion`）**可能不等**：上游的口径
    /// 还含 reasoning 等。历史实现里「汇总桶」用前者、「监控模型行」用后者，二者
    /// 都被接口对外暴露过，因此必须分别保留。
    pub reported_total_tokens: f64,
    /// 记录单独上报的「未缓存输入 Token」之和（用于展示）。
    pub uncached_prompt: f64,
    pub credits: f64,
    pub cached: f64,
    pub cache_hit_calls: u64,
    pub latency_sum: f64,
    pub latency_n: u64,
    pub max_latency: f64,
    pub ttfb_sum: f64,
    pub ttfb_n: u64,
    pub max_ttfb: f64,
    pub last_ts: i64,
}

impl UsageAgg {
    /// 累加一条 `glm_usage_records.json` 形态的调用记录。
    ///
    /// 缺失或非数值字段按 0 处理（保持历史容错行为）。
    pub fn add_record(&mut self, record: &Value) {
        self.calls += 1;
        if record.get("ok").and_then(Value::as_bool) == Some(false) {
            self.failed += 1;
        }
        if record.get("creditsPending").and_then(Value::as_bool) == Some(true) {
            self.credits_pending += 1;
        }
        for (field, target) in [
            ("promptTokens", &mut self.prompt),
            ("completionTokens", &mut self.completion),
            ("totalTokens", &mut self.reported_total_tokens),
            ("credits", &mut self.credits),
            ("cachedTokens", &mut self.cached),
            ("uncachedPromptTokens", &mut self.uncached_prompt),
        ] {
            *target += record.get(field).and_then(Value::as_f64).unwrap_or(0.0);
        }
        // 命中判定优先用记录里的显式标志；缺失时退回「本次是否带缓存 Token」。
        let is_hit = record
            .get("cacheHit")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                record
                    .get("cachedTokens")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    > 0.0
            });
        if is_hit {
            self.cache_hit_calls += 1;
        }
        let latency = record
            .get("latencyMs")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if latency > 0.0 {
            self.latency_sum += latency;
            self.latency_n += 1;
            self.max_latency = self.max_latency.max(latency);
        }
        let ttfb = record.get("ttfbMs").and_then(Value::as_f64).unwrap_or(0.0);
        if ttfb > 0.0 {
            self.ttfb_sum += ttfb;
            self.ttfb_n += 1;
            self.max_ttfb = self.max_ttfb.max(ttfb);
        }
        self.last_ts = self
            .last_ts
            .max(record.get("ts").and_then(Value::as_i64).unwrap_or(0));
    }

    /// 平均总延迟（保留 1 位小数）；无样本时为 0。
    pub fn avg_latency(&self) -> f64 {
        mean(self.latency_sum, self.latency_n)
    }

    /// 平均首字延迟：预填充（含排队）耗时，越小代表「越早开始出字」。
    pub fn avg_ttfb(&self) -> f64 {
        mean(self.ttfb_sum, self.ttfb_n)
    }

    /// 出字阶段耗时（总延迟 − 首字延迟），近似「思考 + 生成」时间。
    pub fn avg_stream_ms(&self) -> f64 {
        if self.latency_n == 0 || self.ttfb_n == 0 {
            0.0
        } else {
            round1((self.latency_sum - self.ttfb_sum) / self.latency_n as f64)
        }
    }

    /// 成功率（0~1，4 位小数）：分母含失败。
    pub fn success_rate(&self) -> f64 {
        ratio(
            self.calls.saturating_sub(self.failed) as f64,
            self.calls as f64,
        )
    }

    /// 缓存命中率（0~1，**4 位小数**），走统一守卫。
    ///
    /// 取整是为了与同层的 [`Self::success_rate`] / [`Self::share`] 保持一致的接口精度
    /// （`model_usage` 的既有契约：桶级与模型级命中率都是 4 位小数）。
    pub fn cache_hit_rate(&self) -> f64 {
        units::round4(units::cache_hit_rate(self.prompt, self.cached))
    }

    /// 总 Token：本累加器只覆盖 prompt/completion（GLM 记录无 cacheWrite）。
    pub fn total_tokens(&self) -> f64 {
        units::total_tokens(self.prompt, self.completion, 0.0)
    }

    /// 本模型占全部调用的比例（0~1，4 位小数）。
    pub fn share(&self, total_calls: u64) -> f64 {
        ratio(self.calls as f64, total_calls as f64)
    }

    /// 平均单次积分（0 次调用时为 0，2 位小数）。
    pub fn avg_credits(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            ((self.credits / self.calls as f64) * 100.0).round() / 100.0
        }
    }

    /// 缓存节省的积分的**估算值**（`cached × 单价 × 0.8`），仅供展示参考。
    ///
    /// ⚠️ 这是**估算**：上游按模型定价 + 缓存折扣计费，没有公开的换算关系，
    /// 因此该值**不得计入任何积分合计**（口径规则 R1：禁止 token ↔ 积分换算）。
    /// 单价用「本次聚合的 credits / 上游上报总 Token」近似。
    pub fn saved_credits(&self) -> f64 {
        if self.reported_total_tokens > 0.0 && self.credits > 0.0 && self.cached > 0.0 {
            let unit_cost = self.credits / self.reported_total_tokens;
            (self.cached * unit_cost * 0.8 * 100.0).round() / 100.0
        } else {
            0.0
        }
    }

    /// 汇总桶的 JSON 投影（`/api/glm-usage/summary` 的 `windows.*` 与 `models.*`）。
    ///
    /// 🔴 字段名与取值必须与该接口的历史输出**逐字段一致** —— 它是 `model_usage`
    /// 的对外契约，`to_summary_bucket` 是唯一投影点（原先由 `accumulate` 直接
    /// 增量写 JSON）。
    pub fn to_summary_bucket(&self) -> Value {
        json!({
            "calls": self.calls,
            "failedCalls": self.failed,
            "promptTokens": self.prompt,
            "completionTokens": self.completion,
            "totalTokens": self.reported_total_tokens,
            "credits": self.credits,
            "creditsPendingCount": self.credits_pending,
            "cachedTokens": self.cached,
            "uncachedPromptTokens": self.uncached_prompt,
            "cacheHitCalls": self.cache_hit_calls,
            "cacheHitRate": self.cache_hit_rate(),
            "savedTokens": self.cached,
            "savedCredits": self.saved_credits(),
        })
    }
}

/// 合法比率：分母为 0 时返回 0，否则 `part / total` 取 4 位小数。
///
/// ⚠️ 不做上下限钳制 —— 需要钳制的场景（缓存命中率）请用
/// [`units::cache_hit_rate`]；本函数用于成功率、占比这类天然落在 [0,1] 的比值。
pub fn ratio(part: f64, total: f64) -> f64 {
    if total <= 0.0 {
        0.0
    } else {
        round4(part / total)
    }
}

/// 占位比例（整数调用数版本）。
pub fn share(calls: u64, total_calls: u64) -> f64 {
    ratio(calls as f64, total_calls as f64)
}

/// 均值（保留 1 位小数）；无样本返回 0。
pub fn mean(sum: f64, n: u64) -> f64 {
    if n == 0 {
        0.0
    } else {
        round1(sum / n as f64)
    }
}

/// 保留 1 位小数（耗时类指标的既有精度）。
pub fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// 桶起点序列（含首尾，步长 `step`）。用于补齐空桶、保证趋势图时间轴连续。
///
/// `step <= 0` 或首尾颠倒时返回空序列（调用方应保证 `last >= first`）。
pub fn bucket_range(first: i64, last: i64, step: i64) -> Vec<i64> {
    if step <= 0 || last < first {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cursor = first;
    while cursor <= last {
        out.push(cursor);
        cursor += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(ok: bool, prompt: f64, completion: f64, cached: f64, credits: Option<f64>) -> Value {
        json!({
            "ts": 1_700_000_000_000i64,
            "ok": ok,
            "promptTokens": prompt,
            "completionTokens": completion,
            "cachedTokens": cached,
            "credits": credits,
            "creditsPending": credits.is_none(),
            "latencyMs": 1000.0,
            "ttfbMs": 200.0,
        })
    }

    #[test]
    fn aggregates_calls_tokens_and_credits() {
        let mut agg = UsageAgg::default();
        agg.add_record(&record(true, 100.0, 20.0, 30.0, Some(5.0)));
        agg.add_record(&record(false, 50.0, 10.0, 0.0, None));
        assert_eq!(agg.calls, 2);
        assert_eq!(agg.failed, 1);
        assert_eq!(agg.credits_pending, 1);
        assert_eq!(agg.prompt, 150.0);
        assert_eq!(agg.completion, 30.0);
        assert_eq!(agg.total_tokens(), 180.0);
        // 待对账记录 credits=null → 按 0 计入（口径：pending 不进合计）
        assert_eq!(agg.credits, 5.0);
        assert_eq!(agg.success_rate(), 0.5);
        assert_eq!(agg.avg_credits(), 2.5);
    }

    #[test]
    fn cache_hit_rate_uses_guard_and_never_exceeds_one() {
        let mut agg = UsageAgg::default();
        // prompt 只记未缓存部分 → cached > prompt 的异常形态
        agg.add_record(&record(true, 100.0, 0.0, 300.0, Some(1.0)));
        assert!(agg.cache_hit_rate() <= 1.0, "must be clamped");
        assert_eq!(agg.cache_hit_rate(), 0.75);

        let empty = UsageAgg::default();
        assert_eq!(empty.cache_hit_rate(), 0.0);
    }

    /// 冻结精度契约：模型级/桶级命中率在接口里是 **4 位小数**
    /// （0.1.53 曾在改走统一函数时丢掉取整，被前后快照 diff 抓到）。
    #[test]
    fn cache_hit_rate_is_rounded_to_four_decimals() {
        let mut agg = UsageAgg::default();
        agg.add_record(&json!({ "promptTokens": 3, "cachedTokens": 2, "ok": true }));
        assert_eq!(agg.cache_hit_rate(), 0.6667);
        assert_eq!(agg.success_rate(), 1.0);
        assert_eq!(agg.share(3), 0.3333);
    }

    #[test]
    fn latency_metrics_only_count_positive_samples() {
        let mut agg = UsageAgg::default();
        agg.add_record(&record(true, 1.0, 1.0, 0.0, Some(1.0)));
        let mut no_timing = record(true, 1.0, 1.0, 0.0, Some(1.0));
        no_timing["latencyMs"] = json!(0);
        no_timing["ttfbMs"] = json!(0);
        agg.add_record(&no_timing);
        assert_eq!(agg.latency_n, 1);
        assert_eq!(agg.ttfb_n, 1);
        assert_eq!(agg.avg_latency(), 1000.0);
        assert_eq!(agg.avg_ttfb(), 200.0);
        assert_eq!(agg.avg_stream_ms(), 800.0);
    }

    #[test]
    fn empty_aggregate_is_all_zero_and_panic_free() {
        let agg = UsageAgg::default();
        assert_eq!(agg.avg_latency(), 0.0);
        assert_eq!(agg.avg_ttfb(), 0.0);
        assert_eq!(agg.avg_stream_ms(), 0.0);
        assert_eq!(agg.success_rate(), 0.0);
        assert_eq!(agg.share(0), 0.0);
        assert_eq!(agg.avg_credits(), 0.0);
    }

    #[test]
    fn ratio_and_mean_guard_against_zero_denominator() {
        assert_eq!(ratio(1.0, 0.0), 0.0);
        assert_eq!(ratio(1.0, 3.0), 0.3333);
        assert_eq!(share(2, 4), 0.5);
        assert_eq!(share(2, 0), 0.0);
        assert_eq!(mean(10.0, 0), 0.0);
        assert_eq!(mean(1.0, 3), 0.3);
    }

    #[test]
    fn bucket_range_is_inclusive_and_safe() {
        assert_eq!(bucket_range(0, 30, 10), vec![0, 10, 20, 30]);
        assert!(bucket_range(0, -1, 10).is_empty());
        assert!(bucket_range(0, 10, 0).is_empty());
        assert_eq!(bucket_range(5, 5, 10), vec![5]);
    }

    /// 冻结契约：汇总桶的字段与取值必须与历史 `accumulate` 实现一致。
    ///
    /// ⚠️ 注意 `totalTokens` 用的是**上游上报值之和**，与 `total_tokens()`
    /// （`prompt + completion`）不是同一个数 —— 两条路径都被接口暴露过。
    #[test]
    fn summary_bucket_matches_legacy_shape() {
        let hit = json!({
            "ok": true, "promptTokens": 100.0, "completionTokens": 20.0,
            "totalTokens": 125.0, "cachedTokens": 30.0, "uncachedPromptTokens": 70.0,
            "credits": 5.0, "latencyMs": 1000.0,
        });
        let failed = json!({
            "ok": false, "promptTokens": 50.0, "completionTokens": 10.0,
            "totalTokens": 60.0, "cachedTokens": 0.0, "credits": Value::Null,
            "creditsPending": true,
        });
        let mut agg = UsageAgg::default();
        agg.add_record(&hit);
        agg.add_record(&failed);
        let bucket = agg.to_summary_bucket();

        assert_eq!(bucket["calls"], json!(2));
        assert_eq!(bucket["failedCalls"], json!(1));
        assert_eq!(bucket["promptTokens"], json!(150.0));
        assert_eq!(bucket["completionTokens"], json!(30.0));
        assert_eq!(
            bucket["totalTokens"],
            json!(185.0),
            "用上游上报值求和，而非 prompt+completion"
        );
        assert_eq!(bucket["credits"], json!(5.0), "待对账记录按 0 计入");
        assert_eq!(bucket["creditsPendingCount"], json!(1));
        assert_eq!(bucket["cachedTokens"], json!(30.0));
        assert_eq!(bucket["uncachedPromptTokens"], json!(70.0));
        assert_eq!(bucket["cacheHitCalls"], json!(1));
        assert_eq!(bucket["cacheHitRate"], json!(0.2));
        assert_eq!(bucket["savedTokens"], json!(30.0));
        // 30 × (5 / 185) × 0.8 = 0.6486… → 0.65
        assert_eq!(bucket["savedCredits"], json!(0.65));
    }

    #[test]
    fn saved_credits_is_zero_without_credits_or_cache() {
        let mut agg = UsageAgg::default();
        agg.add_record(&json!({ "promptTokens": 100.0, "cachedTokens": 50.0, "ok": true }));
        assert_eq!(agg.saved_credits(), 0.0, "无积分则无估算");
        let mut agg2 = UsageAgg::default();
        agg2.add_record(&json!({ "promptTokens": 100.0, "cachedTokens": 0.0, "credits": 5.0, "ok": true }));
        assert_eq!(agg2.saved_credits(), 0.0, "无缓存则无节省");
    }

    /// 冻结契约：累加器必须容忍残缺记录（历史实现靠 `unwrap_or(0)`）。
    #[test]
    fn tolerates_missing_and_non_numeric_fields() {
        let mut agg = UsageAgg::default();
        agg.add_record(&json!({}));
        agg.add_record(&json!({ "promptTokens": "abc", "credits": null, "ok": "yes" }));
        assert_eq!(agg.calls, 2);
        assert_eq!(agg.failed, 0, "非法 ok 值不计失败");
        assert_eq!(agg.prompt, 0.0);
        assert_eq!(agg.credits, 0.0);
        assert_eq!(agg.last_ts, 0);
    }
}
