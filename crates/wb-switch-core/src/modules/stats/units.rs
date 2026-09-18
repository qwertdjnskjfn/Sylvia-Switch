//! 统一统计口径：单位换算与边界规整的**唯一定义**。
//!
//! 背景：模型用量 / Token / 积分三个统计模块过去各写一套口径，同一概念
//! （输入 Token 语义、缓存命中率、保留上限）出现多处实现且边界处理不一致。
//! 本模块集中这些常量与纯函数，供各统计模块引用；后续阶段（P1+）再迁入
//! 事件模型与聚合引擎。
//!
//! 本模块**只放无 IO 的纯逻辑**，全部可单测。

/// 调用记录文件的保留条数上限。
///
/// 历史上 `model_usage` 用 4000、`credit_usage` 用 5000，两套并存。
/// 统一为 5000：按每日 30~50 次调用估算，180 天约需 5400~9000 条，
/// 4000 条会在约 80~130 天时提前截断，使「保留 180 天」名不副实。
pub const MAX_RECORDS: usize = 5_000;

/// 调用记录保留天数（与 [`MAX_RECORDS`] 共同约束，先到者生效）。
pub const RETENTION_DAYS: i64 = 180;

/// 输入 Token 归一化，返回 `(总输入, 未缓存输入)`。
///
/// 不同平台对 `input` 的语义不一致：多数（OpenAI / GLM 语义）**含**缓存命中
/// 部分；部分 AutoClaw 接口只记未缓存部分，此时会出现 `cached > input`。
/// 统一在此规整，保证后续 `cache_hit_rate` 落在 `[0, 1]`、`未缓存输入` 准确。
///
/// 浮点版本（供 JSON 数值使用）。
pub fn normalize_input_tokens(input: f64, cached: f64) -> (f64, f64) {
    if cached > input {
        (input + cached, input)
    } else {
        (input, (input - cached).max(0.0))
    }
}

/// 输入 Token 归一化（整数版本，语义与浮点版完全一致）。
///
/// 单独提供是为了保持 Token 统计原有的整数与饱和语义，避免响应里的
/// 整数被写成浮点（`265378116` → `265378116.0`）。
pub fn normalize_input_tokens_u64(input: u64, cached: u64) -> (u64, u64) {
    if cached > input {
        (input.saturating_add(cached), input)
    } else {
        (input, input.saturating_sub(cached))
    }
}

/// 缓存命中率 = 缓存命中输入 / 总输入，钳制在 `[0, 1]`；总输入为 0 时返回 0。
pub fn cache_hit_rate(input: f64, cached: f64) -> f64 {
    let (total, _) = normalize_input_tokens(input, cached);
    if total <= 0.0 {
        0.0
    } else {
        (cached / total).clamp(0.0, 1.0)
    }
}

/// 缓存命中率（整数入口）。
///
/// 返回 `0.0` 表示「无输入」，与浮点版一致。历史上 Token 统计在总输入为 0 时
/// 输出 `null` —— 需要保留该语义的调用方请自行用 `(total > 0).then(...)` 包裹。
pub fn cache_hit_rate_u64(input: u64, cached: u64) -> f64 {
    let (total, _) = normalize_input_tokens_u64(input, cached);
    if total == 0 {
        0.0
    } else {
        (cached as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// 总 Token 数。`input` 已含缓存命中部分，**不重复累加** `cached`。
pub fn total_tokens(input: f64, output: f64, cache_write: f64) -> f64 {
    (input + output + cache_write).max(0.0)
}

/// 总 Token 数（整数版本，语义与浮点版一致）。
pub fn total_tokens_u64(input: u64, output: u64, cache_write: u64) -> u64 {
    input.saturating_add(output).saturating_add(cache_write)
}

/// 按 4 位小数取整（沿用积分/比率类数值在接口中的既有精度）。
pub fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::token_stats;

    #[test]
    fn normalize_handles_cached_greater_than_input() {
        // 部分 AutoClaw 接口的 input 只记未缓存部分：此时总输入 = input + cached。
        assert_eq!(normalize_input_tokens_u64(100, 300), (400, 100));
        assert_eq!(normalize_input_tokens_u64(100, 100), (100, 0));
        assert_eq!(normalize_input_tokens_u64(300, 100), (300, 200));
        assert_eq!(normalize_input_tokens_u64(0, 0), (0, 0));
    }

    #[test]
    fn float_and_integer_variants_agree() {
        for (input, cached) in [
            (0u64, 0u64),
            (100, 300),
            (300, 100),
            (100, 100),
            (1, 0),
            (0, 5),
            (u64::from(u32::MAX), 7),
        ] {
            let (fi, fu) = normalize_input_tokens(input as f64, cached as f64);
            let (ii, iu) = normalize_input_tokens_u64(input, cached);
            assert_eq!((fi as u64, fu as u64), (ii, iu), "mismatch for {input}/{cached}");
            assert_eq!(
                round4(cache_hit_rate(input as f64, cached as f64)),
                round4(cache_hit_rate_u64(input, cached)),
                "rate mismatch for {input}/{cached}"
            );
        }
    }

    #[test]
    fn cache_hit_rate_is_clamped_to_unit_range() {
        // cached > input 时不溢出 1.0（历史缺陷：直接相除会得到 >1 的比率）。
        assert!(cache_hit_rate_u64(100, 300) <= 1.0);
        assert_eq!(cache_hit_rate_u64(100, 300), 0.75);
        assert_eq!(cache_hit_rate_u64(0, 0), 0.0);
        assert_eq!(cache_hit_rate_u64(200, 200), 1.0);
    }

    #[test]
    fn total_tokens_does_not_double_count_cached() {
        // input 已含 cached，故 total = input + output + cache_write。
        assert_eq!(total_tokens(100.0, 20.0, 5.0), 125.0);
        assert_eq!(total_tokens(0.0, 0.0, 0.0), 0.0);
        // 负数（异常输入）不应产生负总数。
        assert_eq!(total_tokens(-10.0, 0.0, 0.0), 0.0);
    }

    /// 冻结契约：`token_stats` 的既有口径必须与统一函数一致。
    ///
    /// 这条测试是重构安全网 —— 一旦有人改动任一侧的边界处理，会立即失败。
    #[test]
    fn token_stats_reported_totals_match_unified_spec() {
        let cases = [
            (100u64, 300u64, 7u64, 0u64),
            (300, 100, 0, 2),
            (0, 0, 0, 0),
            (1000, 1000, 50, 25),
        ];
        for (input, cached, output, write) in cases {
            let (expect_input, expect_uncached) =
                normalize_input_tokens_u64(input, cached);
            let patch = token_stats::totals_from_usage_for_test(input, cached, output, write);
            assert_eq!(patch["input"].as_u64().unwrap(), expect_input);
            assert_eq!(patch["uncachedInput"].as_u64().unwrap(), expect_uncached);
            assert_eq!(patch["cacheRead"].as_u64().unwrap(), cached);
            assert_eq!(patch["output"].as_u64().unwrap(), output);
            assert_eq!(patch["cacheWrite"].as_u64().unwrap(), write);
            assert_eq!(
                patch["total"].as_u64().unwrap(),
                expect_input.saturating_add(output).saturating_add(write)
            );
            // token_stats 在总输入为 0 时输出 null，统一函数输出 0.0 —— 两者等价。
            assert_eq!(
                round4(patch["cacheHitRate"].as_f64().unwrap_or(0.0)),
                round4(cache_hit_rate_u64(input, cached))
            );
        }
    }
}
