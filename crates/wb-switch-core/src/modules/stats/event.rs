//! 统一事件模型：`StatEvent` / `EventKey`（纯逻辑，可单测）。
//!
//! 三个统计模块此前各有一套「一次调用」的表示（代理记录 / 会话日志行 / 官方请求 /
//! 流水条目），彼此无法对齐，于是同一笔消费被投影两次、调用次数一句话三义。这里把
//! 「一次调用」收敛成一个类型，并为每个来源提供归一化适配器。
//!
//! 三条硬规则（见 `reports/stats-unification-plan.md` §3.2）：
//! - **R1** 禁止 token ↔ 积分换算：`tokens` 是物理量，`credits` 是计费量，
//!   二者只按同一个 [`StatEvent`] 并存，不做任何折算。
//! - **R2** 积分权威源优先级：`Ledger > Official > BalanceDelta > Pending`，
//!   同一笔调用只有一个来源生效（由 [`CreditCharge::source`] 表达）。
//! - **R3** Token 权威源优先级：`Proxy > SessionScan`，同一调用只取一份
//!   （见 [`merge_token_sources`]）。

use serde_json::Value;

use super::units;
use super::window;

/// 事件来源。派生顺序即「Token 权威性」顺序（越靠前越优先）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    /// 代理实时记账（`glm_usage_records.json`）：含 cacheWrite、延迟、改投溯源。
    Proxy = 0,
    /// 本地会话日志扫描（`token_stats`）：事后补齐代理未覆盖的调用。
    SessionScan = 1,
    /// 官方用量投影的逐请求明细。
    Official = 2,
    /// 官方流水（`ledgers_std` 的 debit 条目）——积分的最终事实源。
    Ledger = 3,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Proxy => "proxy",
            Origin::SessionScan => "session",
            Origin::Official => "official",
            Origin::Ledger => "ledger",
        }
    }
}

/// 去重锚点：同一来源内定位「一次调用」。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventKey {
    pub origin: Origin,
    /// 作用域：路由 id / 会话 id / 账号 id …
    pub scope: String,
    /// 来源内的稳定标识：requestId / messageIndex / ledgerId …
    pub external_id: String,
}

impl EventKey {
    pub fn proxy(route: &str, request_id: &str) -> Self {
        Self::new(Origin::Proxy, route, request_id)
    }
    pub fn session(session_id: &str, message_index: usize) -> Self {
        Self::new(Origin::SessionScan, session_id, &message_index.to_string())
    }
    pub fn official(account_id: &str, request_id: &str) -> Self {
        Self::new(Origin::Official, account_id, request_id)
    }
    pub fn ledger(entry_id: &str) -> Self {
        Self::new(Origin::Ledger, "ledger", entry_id)
    }
    fn new(origin: Origin, scope: &str, external_id: &str) -> Self {
        Self {
            origin,
            scope: scope.to_string(),
            external_id: external_id.to_string(),
        }
    }
}

/// Token 计量：**物理量**，不参与任何计费换算（R1）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenCounts {
    /// 总输入，已含 `cache_read`。
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl TokenCounts {
    /// 总 Token：`input + output + cache_write`（`input` 已含缓存命中，勿重复加）。
    pub fn total(&self) -> u64 {
        units::total_tokens_u64(self.input, self.output, self.cache_write)
    }

    /// 未缓存输入。
    pub fn uncached_input(&self) -> u64 {
        units::normalize_input_tokens_u64(self.input, self.cache_read).1
    }

    /// 缓存命中率（0~1），走统一守卫。
    pub fn cache_hit_rate(&self) -> f64 {
        units::cache_hit_rate_u64(self.input, self.cache_read)
    }

    /// 把「input 只记未缓存部分」的形态规整为标准形态并返回自身。
    pub fn normalized(mut self) -> Self {
        self.input = units::normalize_input_tokens_u64(self.input, self.cache_read).0;
        self
    }
}

/// 积分的归因来源（R2：权威性 `Ledger > Official > BalanceDelta > Pending`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreditSource {
    /// 官方流水精确命中。
    Ledger,
    /// 官方用量投影的逐请求 `credit`。
    Official,
    /// 钱包余额差（无并发重叠时才可信）。
    BalanceDelta,
    /// 尚未归因，等待流水对账。
    Pending,
}

impl CreditSource {
    /// 权威性数值：越小越优先。
    pub fn precedence(self) -> u8 {
        match self {
            CreditSource::Ledger => 0,
            CreditSource::Official => 1,
            CreditSource::BalanceDelta => 2,
            CreditSource::Pending => 3,
        }
    }
}

/// 积分计费量。`used` 为 `None` 表示尚未归因（等价于 `Pending`）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CreditCharge {
    pub used: Option<f64>,
    pub source: Option<CreditSource>,
    pub ledger_id: Option<String>,
}

impl CreditCharge {
    pub fn pending() -> Self {
        Self {
            used: None,
            source: Some(CreditSource::Pending),
            ledger_id: None,
        }
    }

    pub fn from_ledger(amount: f64, ledger_id: &str) -> Self {
        Self {
            used: Some(amount),
            source: Some(CreditSource::Ledger),
            ledger_id: Some(ledger_id.to_string()),
        }
    }

    pub fn is_pending(&self) -> bool {
        self.source == Some(CreditSource::Pending) || self.used.is_none()
    }

    /// 计入合计的积分数：**待对账一律按 0 计**（口径 C3 统一后的规则）。
    ///
    /// 待对账金额本身不可知（记录里 `credits` 为 null），因此只能计 0 并单独计数，
    /// 避免「一边按 0 计、一边全额计」的两套口径并存。
    pub fn counted(&self) -> f64 {
        if self.is_pending() {
            0.0
        } else {
            self.used.unwrap_or(0.0)
        }
    }
}

/// 调用性能。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Perf {
    pub latency_ms: Option<i64>,
    pub ttfb_ms: Option<i64>,
}

/// 统一事件：**一次调用的唯一表示**。
#[derive(Clone, Debug)]
pub struct StatEvent {
    pub key: EventKey,
    /// 调用开始时刻（毫秒）。
    pub ts: i64,
    pub account_id: String,
    pub account_label: String,
    /// 实际上游模型。
    pub model: String,
    pub requested_model: Option<String>,
    pub provider: Option<String>,
    pub tokens: TokenCounts,
    pub credits: CreditCharge,
    pub perf: Perf,
    pub ok: bool,
    pub error: Option<String>,
}

impl StatEvent {
    /// 归属的本地日（`YYYY-MM-DD`）——**统一按 `ts` 判定**（口径 C2）。
    pub fn day(&self) -> Option<String> {
        window::local_day_key(self.ts)
    }

    /// 是否命中缓存（用于 `cacheHitCalls` 这类计数）。
    pub fn cache_hit(&self) -> bool {
        self.tokens.cache_read > 0
    }

    /// 弱同一性键：`(账号, 模型, 秒级时间戳)`。
    ///
    /// 用于识别「同一调用被两个来源各记一次」。各来源的 id 空间不同
    /// （`requestId` vs `sessionId+序号`），无法用 [`EventKey`] 跨来源对齐，
    /// 因此退化为这个弱键；**只用于合并 token 来源，不用于积分去重**
    /// （积分靠 `ledgerId` 精确认领）。
    pub fn weak_identity(&self) -> (String, String, i64) {
        (
            self.account_id.clone(),
            self.model.clone(),
            self.ts.div_euclid(1000),
        )
    }
}

/// 从 `glm_usage_records.json` 的一条记录归一化为 [`StatEvent`]。
///
/// 缺失字段按 0 / `None` 处理，与历史容错行为一致。
pub fn from_proxy_record(record: &Value) -> StatEvent {
    let ts = record.get("ts").and_then(Value::as_i64).unwrap_or(0);
    let model = record
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let route = record
        .get("routeId")
        .and_then(Value::as_str)
        .unwrap_or("local");
    let request_id = record
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let prompt = record
        .get("promptTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = record
        .get("cachedTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens = TokenCounts {
        input: prompt,
        output: record
            .get("completionTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read: cached,
        // GLM 记录没有 cacheWrite 字段（Anthropic 语义才区分）
        cache_write: 0,
        reasoning: record
            .get("reasoningTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
    .normalized();

    let credits_raw = record.get("credits").and_then(Value::as_f64);
    let ledger_id = record
        .get("ledgerId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let credits = match (credits_raw, ledger_id.as_deref()) {
        (Some(amount), Some(id)) => CreditCharge::from_ledger(amount, id),
        (Some(amount), None) => CreditCharge {
            used: Some(amount),
            source: Some(CreditSource::BalanceDelta),
            ledger_id: None,
        },
        (None, _) => CreditCharge::pending(),
    };

    StatEvent {
        key: EventKey::proxy(route, request_id),
        ts,
        account_id: record
            .get("accountId")
            .and_then(Value::as_str)
            .unwrap_or("local")
            .to_string(),
        account_label: record
            .get("accountLabel")
            .and_then(Value::as_str)
            .unwrap_or("本机登录")
            .to_string(),
        model,
        requested_model: record
            .get("requestedModel")
            .and_then(Value::as_str)
            .map(str::to_string),
        provider: record.get("provider").and_then(Value::as_str).map(str::to_string),
        tokens,
        credits,
        perf: Perf {
            latency_ms: record.get("latencyMs").and_then(Value::as_i64),
            ttfb_ms: record.get("ttfbMs").and_then(Value::as_i64),
        },
        ok: record.get("ok").and_then(Value::as_bool).unwrap_or(true),
        error: record.get("error").and_then(Value::as_str).map(str::to_string),
    }
}

/// 按 [`EventKey`] 去重：同 key 保留先出现者（调用方应先按来源权威性排序）。
pub fn dedupe_by_key(events: Vec<StatEvent>) -> Vec<StatEvent> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(events.len());
    for event in events {
        if seen.insert(event.key.clone()) {
            out.push(event);
        }
    }
    out
}

/// 合并 Token 双来源（R3）：同一弱同一性键上**保留代理记账**，
/// 会话扫描只补代理未覆盖的调用。
///
/// 返回按 `ts` 升序排列的事件；被丢弃的扫描事件数量由调用方从长度差得出。
pub fn merge_token_sources(events: Vec<StatEvent>) -> Vec<StatEvent> {
    let mut best: std::collections::HashMap<(String, String, i64), StatEvent> =
        std::collections::HashMap::new();
    for event in events {
        let key = event.weak_identity();
        match best.get(&key) {
            // `Origin` 的派生序即权威性序：Proxy(0) < SessionScan(1)
            Some(existing) if existing.key.origin <= event.key.origin => {}
            _ => {
                best.insert(key, event);
            }
        }
    }
    let mut out: Vec<StatEvent> = best.into_values().collect();
    out.sort_by_key(|e| e.ts);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proxy_record(id: &str, ts: i64, credits: Option<f64>, ledger: Option<&str>) -> Value {
        json!({
            "id": id,
            "ts": ts,
            "model": "glm-5.3",
            "routeId": "r1",
            "accountId": "acc-1",
            "accountLabel": "本机登录",
            "promptTokens": 100,
            "completionTokens": 20,
            "cachedTokens": 300,
            "credits": credits,
            "creditsPending": credits.is_none(),
            "ledgerId": ledger,
            "latencyMs": 1500,
            "ttfbMs": 300,
            "ok": true,
        })
    }

    #[test]
    fn adapter_maps_pending_and_attributed_credits() {
        let pending = from_proxy_record(&proxy_record("a", 1_700_000_000_000, None, None));
        assert!(pending.credits.is_pending());
        assert_eq!(pending.credits.counted(), 0.0, "待对账不得计入合计");

        let via_ledger = from_proxy_record(&proxy_record("b", 1_700_000_000_000, Some(7.5), Some("L1")));
        assert!(!via_ledger.credits.is_pending());
        assert_eq!(via_ledger.credits.counted(), 7.5);
        assert_eq!(via_ledger.credits.source, Some(CreditSource::Ledger));

        let via_balance = from_proxy_record(&proxy_record("c", 1_700_000_000_000, Some(3.0), None));
        assert_eq!(via_balance.credits.source, Some(CreditSource::BalanceDelta));
        assert_eq!(via_balance.credits.counted(), 3.0);
    }

    #[test]
    fn adapter_normalizes_input_semantics() {
        // prompt=100 只记未缓存部分、cached=300 → 总输入 400、未缓存 100
        let e = from_proxy_record(&proxy_record("a", 1_700_000_000_000, None, None));
        assert_eq!(e.tokens.input, 400);
        assert_eq!(e.tokens.uncached_input(), 100);
        assert_eq!(e.tokens.total(), 420);
        assert!(e.tokens.cache_hit_rate() <= 1.0);
        assert!(e.cache_hit());
    }

    #[test]
    fn credit_source_precedence_is_ledger_first() {
        let mut sources = [
            CreditSource::Pending,
            CreditSource::BalanceDelta,
            CreditSource::Official,
            CreditSource::Ledger,
        ];
        sources.sort_by_key(|s| s.precedence());
        assert_eq!(sources[0], CreditSource::Ledger);
        assert_eq!(sources[3], CreditSource::Pending);
    }

    #[test]
    fn dedupe_keeps_one_event_per_key() {
        let a = from_proxy_record(&proxy_record("dup", 1_700_000_000_000, Some(1.0), None));
        let b = from_proxy_record(&proxy_record("dup", 1_700_000_000_000, Some(1.0), None));
        let c = from_proxy_record(&proxy_record("other", 1_700_000_000_000, Some(1.0), None));
        assert_eq!(dedupe_by_key(vec![a, b, c]).len(), 2);
    }

    /// R3：同一调用的代理记账与会话扫描各一条 → 只保留代理那条。
    #[test]
    fn merge_prefers_proxy_over_session_scan() {
        let mut proxy = from_proxy_record(&proxy_record("p1", 1_700_000_000_400, Some(1.0), None));
        let mut scan = proxy.clone();
        scan.key = EventKey::session("sess-1", 3);
        scan.tokens.cache_write = 999; // 扫描侧独有的字段，用于识别谁被保留
        proxy.tokens.cache_write = 0;

        let merged = merge_token_sources(vec![scan, proxy]);
        assert_eq!(merged.len(), 1, "同一调用只应保留一份");
        assert_eq!(merged[0].key.origin, Origin::Proxy);
        assert_eq!(merged[0].tokens.cache_write, 0);
    }

    #[test]
    fn merge_keeps_distinct_calls_and_sorts_by_ts() {
        let a = from_proxy_record(&proxy_record("a", 1_700_000_002_000, Some(1.0), None));
        let b = from_proxy_record(&proxy_record("b", 1_700_000_001_000, Some(1.0), None));
        let merged = merge_token_sources(vec![a, b]);
        assert_eq!(merged.len(), 2);
        assert!(merged[0].ts <= merged[1].ts);
    }

    #[test]
    fn day_uses_call_timestamp() {
        let e = from_proxy_record(&proxy_record("a", 1_700_000_000_000, None, None));
        let day = e.day().expect("day key");
        assert_eq!(day.len(), 10);
        assert_eq!(&day[4..5], "-");
    }
}
