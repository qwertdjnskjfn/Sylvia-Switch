//! 时间窗口与分桶：统计模块的**唯一实现**。
//!
//! 这些逻辑原先只存在于 `model_usage`，导致其余统计模块各自切日 / 各自补 0，
//! 粒度规则不统一（口径漂移 C8）。集中到这里后：
//! - `model_usage` 直接复用（行为保持不变）；
//! - `token_stats` / `credit_usage` 后续改用同一套本地日与桶宽定义。
//!
//! 全部为纯函数，可单测。

use chrono::{Local, TimeZone};

/// 本地零点（毫秒时间戳）。用于「今日」窗口的起点。
pub fn local_day_start_ms(now_ms: i64) -> i64 {
    Local
        .timestamp_opt(now_ms.div_euclid(1000), 0)
        .single()
        .map(|dt| {
            dt.date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|d| d.and_local_timezone(Local).single().unwrap_or(dt))
                .unwrap_or(dt)
                .timestamp_millis()
        })
        .unwrap_or(now_ms)
}

/// 时间戳 → 本地日 key（`YYYY-MM-DD`）。
///
/// 返回 `None` 表示时间戳超出可表示范围（与 `token_stats` 既有语义一致）。
/// **统计模块的日归属一律走这里**，避免「按调用时刻」与「按流水记账时刻」
/// 两套基准各自切日造成跨午夜错位（口径漂移 C2）。
pub fn local_day_key(ts_ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ts_ms)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

/// 人类可读的桶宽档位（时间轴标签才会整齐）。
pub const NICE_BUCKETS: [i64; 12] = [
    5 * 60_000,
    15 * 60_000,
    30 * 60_000,
    60 * 60_000,
    3 * 3_600_000,
    6 * 3_600_000,
    12 * 3_600_000,
    24 * 3_600_000,
    3 * 24 * 3_600_000,
    7 * 24 * 3_600_000,
    30 * 24 * 3_600_000,
    90 * 24 * 3_600_000,
];

/// 趋势图的点数上限：挑桶时保证 `span / bucket <= 该值`。
pub const MAX_BUCKET_POINTS: i64 = 240;

/// 桶宽下限（1 分钟）：再细的时间轴没有可读性。
pub const MIN_BUCKET_MS: i64 = 60_000;

/// 监控窗口的默认桶宽：窗口越长桶越粗，保证点数可控。
pub fn default_bucket_ms(window_hours: i64) -> i64 {
    match window_hours {
        h if h <= 0 => 60 * 60 * 1000,
        h if h <= 2 => 5 * 60 * 1000,
        h if h <= 24 => 60 * 60 * 1000,
        h if h <= 168 => 6 * 3600 * 1000,
        _ => 24 * 3600 * 1000,
    }
}

/// 挑桶宽：在「不超过 [`MAX_BUCKET_POINTS`] 个点」的前提下取最细的档位，
/// 且不细于请求粒度。
pub fn pick_bucket(span_ms: i64, requested_ms: i64) -> i64 {
    let requested = requested_ms.max(MIN_BUCKET_MS);
    let candidates: Vec<i64> = NICE_BUCKETS
        .iter()
        .copied()
        .filter(|unit| span_ms / unit <= MAX_BUCKET_POINTS)
        .collect();
    match candidates
        .iter()
        .copied()
        .filter(|unit| *unit >= requested)
        .min()
    {
        Some(unit) => unit,
        None => candidates
            .last()
            .copied()
            .unwrap_or_else(|| (span_ms / MAX_BUCKET_POINTS).max(requested)),
    }
}

/// 把时间戳对齐到所在桶的起点。
pub fn floor_to_bucket(ts_ms: i64, bucket_ms: i64) -> i64 {
    if bucket_ms <= 0 {
        return ts_ms;
    }
    ts_ms - ts_ms.rem_euclid(bucket_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_start_is_local_midnight_and_not_after_now() {
        let now = 1_789_000_000_000i64; // 任意时刻
        let start = local_day_start_ms(now);
        assert!(start <= now, "day start must not be in the future");
        assert!(now - start < 24 * 3600 * 1000, "must be within the same day");
        assert_eq!(local_day_key(start).unwrap().len(), 10);
        // 零点自身应幂等
        assert_eq!(local_day_start_ms(start), start);
    }

    #[test]
    fn day_key_matches_local_format() {
        let key = local_day_key(1_789_000_000_000i64).unwrap();
        assert_eq!(key.len(), 10);
        assert_eq!(&key[4..5], "-");
        assert_eq!(&key[7..8], "-");
        assert!(local_day_key(i64::MAX).is_none());
    }

    #[test]
    fn pick_bucket_respects_point_budget_and_requested_granularity() {
        // 跨 30 小时、未指定粒度 → 选到不超过 240 点的最细档
        let span = 30 * 3600_000i64;
        let bucket = pick_bucket(span, 0);
        assert!(span / bucket <= MAX_BUCKET_POINTS, "too many points");
        assert!(bucket >= MIN_BUCKET_MS);

        // 指定 1 小时粒度 → 不得细于 1 小时
        let bucket = pick_bucket(span, 3600_000);
        assert!(bucket >= 3600_000, "must not be finer than requested");
        assert!(span / bucket <= MAX_BUCKET_POINTS);

        // 极长跨度（3 年）仍不超过点数上限
        let bucket = pick_bucket(3 * 365 * 24 * 3600_000, 0);
        assert!(3 * 365 * 24 * 3600_000 / bucket <= MAX_BUCKET_POINTS);
    }

    #[test]
    fn default_bucket_grows_with_window() {
        // ⚠️ `0` 表示「全部历史」，故意用粗桶（1h）；而 1~2h 的窄窗口用细桶（5min）。
        // 因此桶宽对 window_hours **不是全局单调**的，比较时必须从 1 开始。
        let seq = [1i64, 2, 24, 25, 168, 169, 8760];
        let buckets: Vec<i64> = seq.iter().copied().map(default_bucket_ms).collect();
        for pair in buckets.windows(2) {
            assert!(pair[1] >= pair[0], "bucket must not shrink as window grows");
        }
        assert_eq!(default_bucket_ms(0), 3600_000, "全部历史 → 1 小时桶");
        assert_eq!(default_bucket_ms(1), 300_000, "1~2 小时窗口 → 5 分钟桶");
    }

    #[test]
    fn floor_to_bucket_aligns_and_is_idempotent() {
        let bucket = 3600_000i64;
        let ts = 1_789_000_123_456i64;
        let floored = floor_to_bucket(ts, bucket);
        assert_eq!(floored % bucket, 0);
        assert!(floored <= ts && ts - floored < bucket);
        assert_eq!(floor_to_bucket(floored, bucket), floored);
        // 非法桶宽不应 panic，也不应改变时间戳
        assert_eq!(floor_to_bucket(ts, 0), ts);
    }
}
