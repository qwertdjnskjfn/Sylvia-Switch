//! WorkBuddy 网关错误分级（P0 · 契约 §4 / §13）。
//!
//! 本模块是**纯函数**：只吃 HTTP 状态码 + 上游响应体文本，吐出 [`ErrKind`]，
//! 不做任何 IO、不持有账号状态。账号池（P1 `workbuddy_gateway.rs`）据此决定
//! 「硬冷却 / 软冷却 / 模型冷却 / 换号 / 立即失败」；存量 `/workbuddy/v1` 与
//! `/workbuddy-native/v1` 的错误路径同样接入——传输层失败只换路不标失效，
//! ContextTooLong 直接 400 不重试。
//!
//! # 判定顺序即语义，不得调换（契约 §2-1 / §4 表，与 Go 侧 `Classify` 逐条对齐）
//!
//! 1. `402` → [`ErrKind::HardCredit`]（余额）
//! 2. body 命中嵌套业务码 `14018`（`error.data.code`，实测 HTTP 是 429）→ `HardCredit`
//! 3. body 命中 `HARD_MARKERS`（小写 + 中文原文双通道）→ `HardCredit`
//! 4. body 命中 `SESSION_DEAD_MARKERS` → `SessionDead`
//! 5. `429` → 先判模型级限流（业务码 `6004` / 文案）→ `ModelRate`，否则 `SoftRate`
//! 6. body 命中上下文超长（业务码 `11115` / `extError.code` / 中英文案）→ `ContextTooLong`
//! 7. `404` → `NotFound`
//! 8. `>=500` → `Server`
//! 9. `>=400` → `Client`
//! 10. 其余 → `None`
//!
//! 两处刻意排布：
//! - 第 2/3 步在状态码判定**之前**：HTTP 200 但业务 code 非 0 且含余额关键词
//!   的情况也要判成硬冷却；
//! - 第 6 步排在 429 之后、通用 4xx 之前：上下文超长是**请求侧**错误，换号无用，
//!   需要独立 kind 让调用方「立即失败」而不是轮转重传整个请求体；放在 429 之后
//!   是有意的——429 一律按限流归类，保持既有语义不变。
//!
//! # 与参考实现的对应关系
//!
//! 标记词表 / 业务码 / 判定顺序 / 时区口径 / 友好文案照抄
//! `_workbuddy-gateway-ref/momo0410-switch-gateway/crates/wb-switch-gateway/src/upstream/mod.rs`
//! （`148-209` 常量表、`289-306` 三路判定、`312-358` 分类、`379-417` 时区解析、`426+` 友好文案）。
//! 两处按本契约调整：`parse_reset_time` 的入参/返回用毫秒 epoch（与仓库现有 `now_ms`
//! 约定一致，参考实现是 `chrono::DateTime<Utc>` / Unix 秒）；枚举按 §13 新增
//! [`ErrKind::Transport`]（参考实现无此变体）。
//!
//! # 已知口径（刻意保留，勿「修」）
//!
//! - 业务码只认 **JSON 数字**：`{"code":"6004"}` 反序列化 `i64` 失败 → 不识别；
//! - `14018` 只认嵌套层 `error.data.code`，顶层 `{"code":14018}` 不认（实测上游
//!   额度耗尽是嵌套形态，且 HTTP 429）；
//! - 标记词是对整个 body 做子串匹配，不区分 JSON 键值——上游改文案时才可能漏判，
//!   由 A/B 固化测试（契约 §6-2）兜底。

use serde::Deserialize;

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

/// 上游错误分级（枚举与变体顺序按契约 §13 钉定，一字不改）。
///
/// 与参考实现的唯一差异是新增 [`ErrKind::Transport`]：契约 §4 要求「传输层失败
/// 只换路不标失效」，需要与 5xx 服务端错误区分开（后者喂熔断器，前者不喂）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// 成功 / 无错误。
    None,
    /// 429 软限流（账号级）→ 600s 起指数退避，封顶可配。
    SoftRate,
    /// 余额不足（402 / 25 个中英标记词 / 嵌套码 14018）→ 硬冷却至次日 04:00（等签到）。
    HardCredit,
    /// 模型级限流（429 + code=6004 或文案）→ 只冷 uid+model，不喂熔断器。
    ModelRate,
    /// 请求上下文超出模型窗口（11115 / extError / 中英文案）→ 立即 400，不换号不重传。
    ContextTooLong,
    /// session 失效（12153 / "Offline user session not found"）→ 标需重登并出池。
    SessionDead,
    /// 404 上游偶发 → 短冷却 60s。
    NotFound,
    /// 5xx 上游故障 → 指数退避重试 + 换号。
    Server,
    /// 其他 4xx / 业务错误 → 只换号不罚。
    Client,
    /// 传输层失败（连接 / 发送 / 读流中断）→ 只换路，不标失效、不喂熔断器。
    ///
    /// 由 HTTP 客户端层产生，[`classify`] 永远不会返回它。
    Transport,
}

impl ErrKind {
    /// 序列化名，与 Go 侧 `String()` 一致（`transport` 为本仓库新增）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrKind::None => "none",
            ErrKind::SoftRate => "soft_rate",
            ErrKind::HardCredit => "hard_credit",
            ErrKind::ModelRate => "model_rate",
            ErrKind::ContextTooLong => "context_too_long",
            ErrKind::SessionDead => "session_dead",
            ErrKind::NotFound => "not_found",
            ErrKind::Server => "server",
            ErrKind::Client => "client",
            ErrKind::Transport => "transport",
        }
    }
}

impl std::fmt::Display for ErrKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 余额不足关键词（小写比较 + 中文原文比较双通道）。
///
/// 收单复数两种写法：上游国际版（workbuddy.ai）实际返回的是
/// "Credits exhausted. Please visit the link below to purchase add-on packs
/// and get more credits: …"（**复数** Credits），只登记单数会导致
/// contains 恒不命中 → 被判成软冷却（60 秒）→ 无限重试。
const HARD_MARKERS: &[&str] = &[
    "insufficient credit",
    "insufficient credits",
    "no credit",
    "no credits",
    "credit exhausted",
    "credits exhausted",
    "credit exhaustion",
    "credits exhaustion",
    "out of credit",
    "out of credits",
    "quota exceeded",
    "quota exhaust",
    "payment required",
    "credit not enough",
    "credits not enough",
    "not enough credit",
    "not enough credits",
    "credit used up",
    "credits used up",
    "积分不足",
    "额度不足",
    "余额不足",
    "积分用完",
    "额度用尽",
    "没有积分",
];

/// session 失效标记。
const SESSION_DEAD_MARKERS: &[&str] = &["Offline user session not found", "12153"];

/// 模型级限流的判定依据（429 + 其中之一）。
///
/// 主力信号是上游业务码 6004；文案关键词作为兜底——上游改码不改文案时仍能识别，
/// 但**必须**同时是 429（由 [`classify`] 的门控保证），避免把其他场景的「频率限制」
/// 字样误判成模型限流。
const MODEL_RATE_MARKERS: &[&str] = &["超出频率限制", "切换其他模型"];

/// 上游「模型级限流」的业务码。
const MODEL_RATE_CODE: i64 = 6004;

/// 上游「上下文超长」的业务码。
///
/// 实测响应（2026-09-15 现场，prompt 1121509 > 上限 1048576）：
/// `400 {"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum",
///      "extError":{"code":"context_length_exceeded",...},
///      "displayMsg":{"en":"The request exceeds the model context limit...",
///                    "zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}`
const CONTEXT_TOO_LONG_CODE: i64 = 11115;

/// 上下文超长的判定文案（中英双通道兜底）。
///
/// 措辞取自上游真实响应，刻意含中英两版 displayMsg——上游按 Accept-Language
/// 切换语言，只认一种会漏判。msg 有多种写法：实测同一业务码下遇到过
/// "prompt is too long"（国际版）与 "input length too long"（国服 glm-5.3）。
const CONTEXT_TOO_LONG_MARKERS: &[&str] = &[
    "context_length_exceeded",
    "prompt is too long",
    "input length too long",
    "exceeds the model context limit",
    "对话内容超出模型长度上限",
    "超出模型长度上限",
];

/// 上游「额度耗尽」的业务码。
///
/// 与 [`MODEL_RATE_CODE`]（6004）的关键区别在于**嵌套层级**：6004 在顶层 `code`，
/// 而 14018 藏在 `error.data.code`。实测该响应的 HTTP 状态是 **429**，
/// 而 429 分支若不识别它就会落进软冷却（仅 60 秒）→ 无限重试。
const CREDIT_EXHAUSTED_CODE: i64 = 14018;

/// 上游统一信封（只探顶层 `code`；其余字段由 `context_too_long_message` 的
/// 局部 `Env` 负责，这里不重复登记，避免登记用不到的字段）。
#[derive(Debug, Default, Deserialize)]
struct ApiEnvelope {
    #[serde(default)]
    code: i64,
}

/// 嵌套 14018 探针：`{"error":{"data":{"code":14018}}}`。
#[derive(Debug, Default, Deserialize)]
struct CreditExhaustedProbe {
    #[serde(default)]
    error: CreditExhaustedError,
}

#[derive(Debug, Default, Deserialize)]
struct CreditExhaustedError {
    #[serde(default)]
    data: CreditExhaustedData,
}

#[derive(Debug, Default, Deserialize)]
struct CreditExhaustedData {
    #[serde(default)]
    code: i64,
}

/// extError.code 探针：`{"extError":{"code":"..."}}`。
#[derive(Debug, Default, Deserialize)]
struct ExtErrorProbe {
    #[serde(default)]
    #[serde(rename = "extError")]
    ext_error: ExtErrorInner,
}

#[derive(Debug, Default, Deserialize)]
struct ExtErrorInner {
    #[serde(default)]
    code: String,
}

/// 报告响应体是否为「额度耗尽」业务码（含嵌套层级）。
fn is_credit_exhausted_code(body: &str) -> bool {
    serde_json::from_str::<CreditExhaustedProbe>(body)
        .map(|p| p.error.data.code == CREDIT_EXHAUSTED_CODE)
        .unwrap_or(false)
}

/// 报告 429 响应体是否为「模型级限流」（该账号该模型额度用尽）。
///
/// 与软限流的区别：软限流是整个账号被限速，而模型级限流只影响当前请求的那个模型，
/// 该账号换模型仍可用——上游文案「您也可以切换其他模型继续使用」明确指出了这一点。
///
/// 注意本函数**自身不门控状态码**（与参考实现一致）：429 的门控在 [`classify`]
/// 第 5 步；单独调用它时需自行保证只对 429 使用。
pub fn is_model_rate_limited(body: &str) -> bool {
    if serde_json::from_str::<ApiEnvelope>(body)
        .map(|env| env.code == MODEL_RATE_CODE)
        .unwrap_or(false)
    {
        return true;
    }
    // 文案兜底：上游改码不改文案时仍能识别。
    MODEL_RATE_MARKERS.iter().any(|m| body.contains(m))
}

/// 报告上游响应是否为「请求上下文超出模型窗口」。
///
/// 三路判定，任一命中即成立：业务码 11115、extError.code=context_length_exceeded、
/// 或真实文案关键词。不按 status 门控：上游以 400 为主，但判定依据是业务语义
/// 而非状态码，上游若改用 413 也能识别。
pub fn is_context_too_long(body: &str) -> bool {
    if serde_json::from_str::<ApiEnvelope>(body)
        .map(|env| env.code == CONTEXT_TOO_LONG_CODE)
        .unwrap_or(false)
    {
        return true;
    }
    if serde_json::from_str::<ExtErrorProbe>(body)
        .map(|ext| ext.ext_error.code.eq_ignore_ascii_case("context_length_exceeded"))
        .unwrap_or(false)
    {
        return true;
    }
    let lower = body.to_lowercase();
    CONTEXT_TOO_LONG_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

/// 按 HTTP 状态码 + body 判定错误类别。
///
/// 与 Go 侧 `Classify` 逐条对齐，包括「body 关键词判定先于状态码」「模型级限流
/// 优先于账号级软限流」「上下文超长早于通用 4xx」的顺序。顺序即语义，不得调换。
pub fn classify(status: u16, body: &str) -> ErrKind {
    if status == 402 {
        return ErrKind::HardCredit;
    }
    // 额度耗尽的业务码优先判定：它的 HTTP 状态是 429，若不先拦，
    // 会落进下面的 429 分支被判成软冷却（仅 60 秒冷却）→ 无限重试。
    if is_credit_exhausted_code(body) {
        return ErrKind::HardCredit;
    }
    let lower = body.to_lowercase();
    for m in HARD_MARKERS {
        // 双通道：小写比较（英文）或原文比较（中文，大小写不敏感但中文无大小写）
        if lower.contains(&m.to_lowercase()) || body.contains(m) {
            return ErrKind::HardCredit;
        }
    }
    for m in SESSION_DEAD_MARKERS {
        if body.contains(m) {
            return ErrKind::SessionDead;
        }
    }
    if status == 429 {
        // 模型级限流优先于账号级软限流：两者的冷却粒度与时长都不同
        //（模型级按 uid+model 冷却到上游给的重置时间）。
        if is_model_rate_limited(body) {
            return ErrKind::ModelRate;
        }
        return ErrKind::SoftRate;
    }
    // 上下文超长必须早于通用 4xx 判定：它是请求侧错误，换号无用，
    // 需要独立 kind 让调用方「立即失败」而不是轮转重传整个请求体。
    // 放在 429 之后是有意的：429 一律按限流归类，保持既有语义不变。
    if is_context_too_long(body) {
        return ErrKind::ContextTooLong;
    }
    if status == 404 {
        return ErrKind::NotFound;
    }
    if status >= 500 {
        return ErrKind::Server;
    }
    if status >= 400 {
        return ErrKind::Client;
    }
    // HTTP 200 但业务 code 非 0 且含余额关键词的情况已被上面 HARD_MARKERS 捕获。
    ErrKind::None
}

// ---------------------------------------------------------------------------
// 重置时间解析
// ---------------------------------------------------------------------------

/// 上游（CodeBuddy 国服）的业务时区偏移（秒）：CST（UTC+8）。
///
/// 用于解释**无时区后缀**的重置时间字面量。为什么不用本地时区：上游是国服服务，
/// 其自然日/重置时刻都按 CST 计；用容器本地时区解释会让同一份响应在开发机（+08:00）
/// 与 UTC 容器上得出相差 8 小时的结果。中国无夏令时，固定 +8，不依赖 tzdata。
const UPSTREAM_ZONE_OFFSET_SECS: i32 = 8 * 60 * 60;

/// 从上游报错文案里提取「重置时刻」（毫秒 epoch）；解析不出返回 None。
///
/// 实测文案（2026-09-15 现场）：
/// `您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。`
///
/// 时区处理：
/// - "UTC+8" / "UTC+08:00" → 固定偏移（**实测文案用的就是这种**）
/// - "Z"                   → UTC
/// - 无时区后缀 / 后缀非法  → 按上游业务时区（CST）解释，而非容器本地时区
///
/// 只返回**未来**的时刻：解析出过去的时间说明文案里的重置点已过（如重放旧日志），
/// 此时返回 None 交给调用方回退固定冷却，避免写入一个立即失效的冷却。
///
/// 入参 `now_ms` 与返回均为毫秒 epoch（参考实现是 `chrono::DateTime<Utc>` / Unix 秒）。
pub fn parse_reset_time(body: &str, now_ms: i64) -> Option<i64> {
    let (literal, tz) = find_reset_literal(body)?;
    // 'T' 分隔符归一成空格（日期段不可能含 'T'，第一个 'T' 必是分隔符）。
    let literal = literal.replacen('T', " ", 1);
    let offset_secs = match tz.map(str::trim) {
        None | Some("") => UPSTREAM_ZONE_OFFSET_SECS,
        Some("Z") => 0,
        Some(tz) => parse_utc_offset_secs(tz).unwrap_or(UPSTREAM_ZONE_OFFSET_SECS),
    };
    // 显式范围校验：非法日期时间直接 None，不依赖 chrono 的报错口径。
    if !valid_datetime(&literal) {
        return None;
    }
    let naive = chrono::NaiveDateTime::parse_from_str(&literal, "%Y-%m-%d %H:%M:%S").ok()?;
    // 本地时刻按固定偏移换算成 epoch：UTC 时刻 = 字面量当作 UTC - 偏移。
    let ts_ms = naive.and_utc().timestamp_millis() - (offset_secs as i64) * 1000;
    if ts_ms <= now_ms {
        return None;
    }
    Some(ts_ms)
}

/// 在 body 中查找第一个「YYYY-MM-DD HH:MM:SS」字面量及其时区后缀。
///
/// 语义等价于参考实现的正则
/// `(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2})(?:\s*(UTC[+-]\d{1,2}(?::\d{2})?|Z))?`：
/// 从左到右取第一个匹配；时区后缀可选（先吃掉任意空白再匹配），匹配不上即无后缀。
/// 本 crate 未引入 `regex` 依赖（不为一个模式动共享 Cargo.toml），故手写扫描器。
fn find_reset_literal(body: &str) -> Option<(&str, Option<&str>)> {
    for (idx, _) in body.char_indices() {
        let rest = &body[idx..];
        let Some(len) = match_datetime_literal(rest) else {
            continue;
        };
        let (literal, after) = rest.split_at(len);
        return Some((literal, match_tz_suffix(after)));
    }
    None
}

/// 尝试匹配定长的 `YYYY-MM-DD[ T]HH:MM:SS`（19 字节），返回字面量字节长度。
fn match_datetime_literal(s: &str) -> Option<usize> {
    const LEN: usize = 19; // "YYYY-MM-DD HH:MM:SS"
    let b = s.as_bytes();
    if b.len() < LEN {
        return None;
    }
    let dig = |i: usize| b[i].is_ascii_digit();
    if !(dig(0) && dig(1) && dig(2) && dig(3)) {
        return None;
    }
    if b[4] != b'-' || !dig(5) || !dig(6) {
        return None;
    }
    if b[7] != b'-' || !dig(8) || !dig(9) {
        return None;
    }
    // 日期与时间之间只允许单个空格或 'T'。
    if !(b[10] == b' ' || b[10] == b'T') {
        return None;
    }
    if !dig(11) || !dig(12) || b[13] != b':' {
        return None;
    }
    if !dig(14) || !dig(15) || b[16] != b':' {
        return None;
    }
    if !dig(17) || !dig(18) {
        return None;
    }
    Some(LEN)
}

/// 匹配时区后缀（`UTC+8` / `UTC+08:00` / `UTC-05:30` / `Z`），返回原始片段。
///
/// 与正则 `(?:\s*(UTC[+-]\d{1,2}(?::\d{2})?|Z))?` 一致：先吃掉任意空白再尝试匹配，
/// 匹配不上则整个可选组为空（返回 None，由调用方按 CST 解释）。
fn match_tz_suffix(s: &str) -> Option<&str> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix("UTC") {
        let b = rest.as_bytes();
        // 符号位（必有）。
        let mut i = match b.first() {
            Some(b'+') | Some(b'-') => 1,
            _ => return None,
        };
        // 1~2 位小时（至少 1 位）。
        let hstart = i;
        while i < b.len() && i - hstart < 2 && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == hstart {
            return None;
        }
        // 可选的 ":MM"（必须紧跟两位数字，否则整段不纳入后缀）。
        if i + 2 < b.len() && b[i] == b':' && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit() {
            i += 3;
        }
        Some(&s[..3 + i])
    } else if s.starts_with('Z') {
        Some(&s[..1])
    } else {
        None
    }
}

/// 解析 "UTC+8" / "UTC-05:30" 形式的固定偏移（秒）；非法返回 None。
fn parse_utc_offset_secs(s: &str) -> Option<i32> {
    let rest = s.strip_prefix("UTC")?;
    let (sign, rest) = match rest.chars().next()? {
        '+' => (1, &rest[1..]),
        '-' => (-1, &rest[1..]),
        _ => return None,
    };
    let (hours, minutes) = match rest.split_once(':') {
        Some((h, mm)) => (h.parse::<i32>().ok()?, mm.parse::<i32>().ok()?),
        None => (rest.parse::<i32>().ok()?, 0),
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

/// 显式校验 "YYYY-MM-DD HH:MM:SS" 字面量的取值域（含闰年与月内天数）。
///
/// 让「非法日期时间 → None」不依赖 chrono 的报错口径，矩阵测试可稳定断言。
fn valid_datetime(literal: &str) -> bool {
    let b = literal.as_bytes();
    if b.len() != 19 {
        return false;
    }
    let num = |from: usize, to: usize| -> Option<u32> {
        literal
            .get(from..to)?
            .parse::<u32>()
            .ok()
            .filter(|_| b[from..to].iter().all(u8::is_ascii_digit))
    };
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (
        num(0, 4),
        num(5, 7),
        num(8, 10),
        num(11, 13),
        num(14, 16),
        num(17, 19),
    ) else {
        return false;
    };
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return false;
    }
    h < 24 && mi < 60 && s < 60
}

/// 该年该月的天数（公历，含闰年规则）。
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// 友好文案
// ---------------------------------------------------------------------------

/// `friendly_message` 输出上限（字节）：把上游原始 JSON 提炼成一句人话，
/// 不允许把整段响应体透给客户端。
const FRIENDLY_MAX_BYTES: usize = 200;

/// 把上游的原始错误体提炼成一句可读的原因，供客户端展示。
///
/// 此前直接把整段上游 JSON 拼进 OpenAI 错误体的 message，客户端看到的是
/// 「all accounts unavailable (cooling/disabled): upstream soft_rate (http 429):
/// {"error":{"data":{"code":14018,...}}}」—— 又长又难懂。
///
/// 返回空串表示没有更优的表述，调用方应回退到原始文案。
///
/// [`ErrKind::ContextTooLong`] 例外：见 [`context_too_long_message`]——必须保留
/// 上游原文，否则下游客户端认不出上下文溢出、不会触发自动压缩。
pub fn friendly_message(kind: ErrKind, status: u16, body: &str) -> String {
    // 业务码优先，但**只有响应体里真的带 14018 时才把该码写进文案**：
    // 硬冷却也可能来自 HTTP 402 或关键词命中，此时硬写「上游 14018」
    // 会让用户拿着一个与响应不符的码去排查。
    if is_credit_exhausted_code(body) {
        return "账号额度已耗尽（上游 14018）：请为该账号充值，或等待签到 / 免费额度恢复后重试"
            .to_string();
    }
    match kind {
        ErrKind::HardCredit => {
            "账号额度已耗尽：请为该账号充值，或等待签到 / 免费额度恢复后重试".to_string()
        }
        ErrKind::ModelRate => "该账号在此模型上已达频率上限，已按上游给出的重置时间冷却；同一账号的其他模型仍可用".to_string(),
        ErrKind::SoftRate => "账号被上游限流（HTTP 429），已短暂冷却，稍后会自动重试".to_string(),
        ErrKind::SessionDead => "账号登录态已失效，需在「账号管理」页重新登录".to_string(),
        ErrKind::NotFound => "上游返回 404（接口或模型不存在），已短暂冷却并切换账号".to_string(),
        ErrKind::Server if status > 0 => {
            format!("上游服务异常（HTTP {status}），已切换到其他账号")
        }
        ErrKind::ContextTooLong => context_too_long_message(body),
        ErrKind::Transport => "网络传输失败（连接 / 读取中断），已切换到其他账号重试".to_string(),
        _ => String::new(),
    }
}

/// 把上下文超长的上游响应体提炼成一条**保留原文**的客户端消息。
///
/// 为什么必须保留上游原文：下游客户端（如 DeepSeek Harness）靠文案模式识别上下文溢出
///（`prompt is too long` / `context_length_exceeded` / `exceeds the model context limit`），
/// 据此触发自动压缩并重试。若只回自己的措辞，客户端就认不出这是溢出，
/// 只会把它当成普通失败。
///
/// 输出形如：`<上游 msg>（<中文 displayMsg>）`，两种语言的特征串都在，
/// 中文提示同时给人类看。上游字段缺失时逐级回退，最终回退到原始 body。
/// 全文截断到 `FRIENDLY_MAX_BYTES` 字节（参考实现此处用 400，本仓库统一 200）。
pub fn context_too_long_message(body: &str) -> String {
    #[derive(Default, Deserialize)]
    struct Env {
        #[serde(default)]
        msg: String,
        #[serde(default, rename = "extError")]
        ext_error: ExtErrorMessage,
        #[serde(default, rename = "displayMsg")]
        display_msg: DisplayMessage,
    }
    #[derive(Default, Deserialize)]
    struct ExtErrorMessage {
        #[serde(default)]
        message: String,
    }
    #[derive(Default, Deserialize)]
    struct DisplayMessage {
        #[serde(default)]
        zh: String,
        #[serde(default)]
        en: String,
    }
    let env = serde_json::from_str::<Env>(body).unwrap_or_default();

    let mut primary = env.msg.trim().to_string();
    if primary.is_empty() {
        primary = env.ext_error.message.trim().to_string();
    }
    let mut hint = env.display_msg.zh.trim().to_string();
    if hint.is_empty() {
        hint = env.display_msg.en.trim().to_string();
    }

    let out = if primary.is_empty() && hint.is_empty() {
        truncate(body.trim(), FRIENDLY_MAX_BYTES)
    } else if primary.is_empty() {
        hint
    } else if hint.is_empty() || primary.contains(&hint) {
        primary
    } else {
        format!("{primary}（{hint}）")
    };
    truncate(&out, FRIENDLY_MAX_BYTES)
}

/// 与 Go 侧 `truncate` 一致：先 TrimSpace，再按字节截断。
///
/// Go 的 `s[:n]` 是纯字节切片（可能切在多字节字符中间）；Rust 字符串不允许
/// 非字符边界切片，这里回退到**不越过 n 的最近字符边界**——仅在极端
/// 多字节场景下与 Go 输出相差几个字节，展示用途无实质差异。
fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.len() > n {
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// 矩阵测试（无网络、无 IO；固化为 A/B 回归基准，防上游改码时静默劣化）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：把 UTC 日历时刻换算成毫秒 epoch（与实现内的偏移运算互相印证）。
    fn utc_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .and_then(|d| d.and_hms_opt(h, mi, s))
            .expect("测试用日历时刻应合法")
            .and_utc()
            .timestamp_millis()
    }

    // ---- ErrKind 序列化名 ----

    #[test]
    fn err_kind_as_str_matches_reference() {
        assert_eq!(ErrKind::None.as_str(), "none");
        assert_eq!(ErrKind::SoftRate.as_str(), "soft_rate");
        assert_eq!(ErrKind::HardCredit.as_str(), "hard_credit");
        assert_eq!(ErrKind::ModelRate.as_str(), "model_rate");
        assert_eq!(ErrKind::ContextTooLong.as_str(), "context_too_long");
        assert_eq!(ErrKind::SessionDead.as_str(), "session_dead");
        assert_eq!(ErrKind::NotFound.as_str(), "not_found");
        assert_eq!(ErrKind::Server.as_str(), "server");
        assert_eq!(ErrKind::Client.as_str(), "client");
        // Transport 为本仓库新增（契约 §13），参考实现无此变体。
        assert_eq!(ErrKind::Transport.as_str(), "transport");
        assert_eq!(ErrKind::HardCredit.to_string(), "hard_credit");
    }

    // ---- 状态码基线分支 ----

    #[test]
    fn status_based_classification() {
        // None：成功与 3xx 都不算错误。
        assert_eq!(classify(200, ""), ErrKind::None);
        assert_eq!(classify(204, "ok"), ErrKind::None);
        assert_eq!(classify(302, ""), ErrKind::None);
        // SoftRate / NotFound / Server / Client。
        assert_eq!(classify(429, ""), ErrKind::SoftRate);
        assert_eq!(classify(404, ""), ErrKind::NotFound);
        assert_eq!(classify(500, ""), ErrKind::Server);
        assert_eq!(classify(503, ""), ErrKind::Server);
        assert_eq!(classify(400, ""), ErrKind::Client);
        assert_eq!(classify(418, ""), ErrKind::Client);
        // classify 永不返回 Transport（传输层由客户端产生）。
        assert_ne!(classify(400, ""), ErrKind::Transport);
    }

    #[test]
    fn transport_kind_is_client_layer_only() {
        // classify 对任意输入都不产生 Transport。
        for status in [200u16, 400, 402, 404, 429, 500, 503] {
            for body in ["", "12153", "余额不足", r#"{"code":6004}"#] {
                assert_ne!(classify(status, body), ErrKind::Transport);
            }
        }
        // 该分支仍需可用的序列化名与友好文案。
        assert_eq!(ErrKind::Transport.as_str(), "transport");
        assert!(!friendly_message(ErrKind::Transport, 0, "").is_empty());
    }

    // ---- 402 / 硬标记 ----

    #[test]
    fn hard_credit_on_402() {
        assert_eq!(classify(402, ""), ErrKind::HardCredit);
        // 402 压过一切后续判定（含 6004 与 12153）。
        assert_eq!(classify(402, r#"{"code":6004,"msg":"x"}"#), ErrKind::HardCredit);
        assert_eq!(classify(402, "12153"), ErrKind::HardCredit);
    }

    #[test]
    fn hard_markers_all_25_entries() {
        // 25 个中英标记词全量命中：200 与 429 两个状态下都必须判硬冷却
        //（429 时证明硬标记优先于软限流，不会落进 60s 无限重试）。
        assert_eq!(HARD_MARKERS.len(), 25, "标记词表应为 25 个（契约 §2-2）");
        for (i, m) in HARD_MARKERS.iter().enumerate() {
            assert_eq!(
                classify(200, m),
                ErrKind::HardCredit,
                "HARD_MARKERS[{i}] = {m:?} 在 200 下应判硬冷却"
            );
            assert_eq!(
                classify(429, m),
                ErrKind::HardCredit,
                "HARD_MARKERS[{i}] = {m:?} 在 429 下应压过软限流"
            );
        }
    }

    #[test]
    fn hard_markers_case_insensitive_and_plural() {
        // 大小写不敏感（小写通道）。
        assert_eq!(classify(200, "Insufficient Credit"), ErrKind::HardCredit);
        assert_eq!(classify(200, "QUOTA EXCEEDED"), ErrKind::HardCredit);
        // 复数形态（国际版实测文案）。
        assert_eq!(classify(200, "insufficient credits"), ErrKind::HardCredit);
        assert_eq!(
            classify(
                200,
                "Credits exhausted. Please visit the link below to purchase add-on packs and get more credits: …"
            ),
            ErrKind::HardCredit
        );
        // 中文原文通道。
        assert_eq!(classify(200, "您的积分不足"), ErrKind::HardCredit);
        assert_eq!(classify(200, "额度用尽"), ErrKind::HardCredit);
        // 5xx 也被硬标记压过（余额问题先于服务端故障）。
        assert_eq!(classify(500, "quota exceeded"), ErrKind::HardCredit);
    }

    // ---- 嵌套 14018 / envelope 探针 ----

    #[test]
    fn nested_credit_exhausted_code() {
        let body = r#"{"error":{"data":{"code":14018,"msg":"Credits exhausted. Please visit the link below to purchase add-on packs and get more credits: …"}}}"#;
        assert_eq!(classify(429, body), ErrKind::HardCredit);
        assert_eq!(classify(200, r#"{"error":{"data":{"code":14018,"msg":"x"}}}"#), ErrKind::HardCredit);
        assert_eq!(classify(500, r#"{"error":{"data":{"code":14018}}}"#), ErrKind::HardCredit);
        // 硬冷却优先于模型级限流。
        assert_eq!(classify(429, r#"{"code":6004,"msg":"余额不足"}"#), ErrKind::HardCredit);
    }

    #[test]
    fn envelope_probe_top_level_vs_nested_code() {
        // 顶层 code=14018 不识别（实测额度耗尽是嵌套形态）。
        assert_eq!(classify(200, r#"{"code":14018,"msg":"x"}"#), ErrKind::None);
        // 嵌套 error.data.code 才识别；层级差一层即不命中。
        assert!(is_credit_exhausted_code(r#"{"error":{"data":{"code":14018}}}"#));
        assert!(!is_credit_exhausted_code(r#"{"error":{"code":14018}}"#));
        assert!(!is_credit_exhausted_code(r#"{"data":{"code":14018}}"#));
        // 非 JSON / 字符串码均不识别（i64 反序列化失败）。
        assert!(!is_credit_exhausted_code("14018"));
        assert!(!is_credit_exhausted_code(r#"{"error":{"data":{"code":"14018"}}}"#));
        // 顶层 code 探针（6004 / 11115 走 ApiEnvelope）。
        assert!(is_model_rate_limited(r#"{"code":6004,"msg":"x"}"#));
        assert!(!is_model_rate_limited(r#"{"code":6005,"msg":"x"}"#));
        assert!(!is_model_rate_limited(r#"{"code":"6004","msg":"x"}"#));
        assert!(is_context_too_long(r#"{"code":11115,"msg":"x"}"#));
    }

    #[test]
    fn envelope_probe_ext_error_code() {
        // extError.code 三路判定之一，大小写不敏感。
        assert!(is_context_too_long(r#"{"extError":{"code":"context_length_exceeded"}}"#));
        assert!(is_context_too_long(r#"{"extError":{"code":"Context_Length_Exceeded"}}"#));
        // 其他 extError.code 不算上下文超长（body 也不含任何标记词子串）。
        assert!(!is_context_too_long(r#"{"extError":{"code":"invalid_request_error"}}"#));
        assert!(!is_context_too_long(r#"{"extError":{"code":"rate_limit_exceeded"}}"#));
        // 注意：标记词通道是**整段 body 的子串匹配**（参考实现口径，勿当精确匹配）——
        // 即使 extError.code 不是精确值，只要 body 里出现 "context_length_exceeded"
        // 子串，第三条路照样命中。
        assert!(is_context_too_long(r#"{"extError":{"code":"context_length_exceeded_typo"}}"#));
    }

    // ---- session 失效 ----

    #[test]
    fn session_dead_markers() {
        assert_eq!(
            classify(401, r#"{"code":12153,"msg":"Offline user session not found"}"#),
            ErrKind::SessionDead
        );
        assert_eq!(classify(200, "12153"), ErrKind::SessionDead);
        assert_eq!(classify(500, "Offline user session not found"), ErrKind::SessionDead);
    }

    #[test]
    fn hard_markers_take_priority_over_session_dead() {
        // hardMarkers 判定在 sessionDeadMarkers 之前。
        assert_eq!(classify(401, "12153 余额不足"), ErrKind::HardCredit);
    }

    #[test]
    fn session_dead_takes_priority_over_429() {
        // session 失效在 429 分支之前：失效号不该按限流冷却后重试。
        assert_eq!(
            classify(429, r#"{"code":12153,"msg":"Offline user session not found"}"#),
            ErrKind::SessionDead
        );
    }

    // ---- 模型级限流 ----

    #[test]
    fn model_rate_at_429() {
        let body = r#"{"code":6004,"msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。"}"#;
        assert_eq!(classify(429, body), ErrKind::ModelRate);
        assert_eq!(classify(429, "超出频率限制"), ErrKind::ModelRate);
        assert_eq!(classify(429, "请切换其他模型"), ErrKind::ModelRate);
    }

    #[test]
    fn model_rate_only_at_429() {
        // 非 429 不按模型限流归类（业务码只在 429 分支检查；文案标记需 429 门控）。
        assert_eq!(classify(200, "超出频率限制"), ErrKind::None);
        assert_eq!(classify(400, r#"{"code":6004,"msg":"x"}"#), ErrKind::Client);
        assert_eq!(classify(500, r#"{"code":6004,"msg":"x"}"#), ErrKind::Server);
    }

    #[test]
    fn model_rate_takes_priority_over_soft_rate() {
        // 同为 429：6004 判模型冷却，其余判软冷却。
        assert_eq!(classify(429, r#"{"code":6004}"#), ErrKind::ModelRate);
        assert_eq!(classify(429, r#"{"code":6005}"#), ErrKind::SoftRate);
        assert_eq!(classify(429, r#"{"msg":"slow down"}"#), ErrKind::SoftRate);
    }

    // ---- 上下文超长 ----

    #[test]
    fn context_too_long_three_paths() {
        let body = r#"{"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum","extError":{"code":"context_length_exceeded","type":"invalid_request_error"},"displayMsg":{"en":"The request exceeds the model context limit...","zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}"#;
        assert_eq!(classify(400, body), ErrKind::ContextTooLong);
        assert_eq!(classify(400, r#"{"extError":{"code":"context_length_exceeded"}}"#), ErrKind::ContextTooLong);
        assert_eq!(classify(400, "prompt is too long"), ErrKind::ContextTooLong);
        assert_eq!(classify(400, "input length too long"), ErrKind::ContextTooLong);
        assert_eq!(classify(400, "The request exceeds the model context limit"), ErrKind::ContextTooLong);
        assert_eq!(classify(400, "对话内容超出模型长度上限，请精简对话或减少附件后重试。"), ErrKind::ContextTooLong);
    }

    #[test]
    fn context_too_long_not_status_gated() {
        // 不按 status 门控：200 / 413 也要能识别（上游若改状态码照样命中）。
        assert_eq!(classify(200, r#"{"code":11115}"#), ErrKind::ContextTooLong);
        assert_eq!(classify(413, "prompt is too long"), ErrKind::ContextTooLong);
        assert_eq!(classify(403, r#"{"extError":{"code":"context_length_exceeded"}}"#), ErrKind::ContextTooLong);
    }

    #[test]
    fn context_too_long_before_generic_4xx() {
        // 早于通用 4xx：否则会被判成 Client 而进入换号重传路径。
        assert_eq!(classify(400, r#"{"code":11115,"msg":"prompt is too long"}"#), ErrKind::ContextTooLong);
        assert_ne!(classify(400, r#"{"code":11115}"#), ErrKind::Client);
    }

    #[test]
    fn context_too_long_after_429() {
        // 429 一律按限流归类（上下文超长判定刻意排在 429 之后）。
        assert_eq!(classify(429, "prompt is too long"), ErrKind::SoftRate);
        assert_eq!(classify(429, r#"{"code":11115}"#), ErrKind::SoftRate);
    }

    // ---- 404 / 5xx / 4xx 兜底 ----

    #[test]
    fn not_found_branch() {
        assert_eq!(classify(404, ""), ErrKind::NotFound);
        assert_eq!(classify(404, r#"{"msg":"model not found"}"#), ErrKind::NotFound);
    }

    #[test]
    fn server_branch() {
        assert_eq!(classify(500, ""), ErrKind::Server);
        assert_eq!(classify(502, "bad gateway"), ErrKind::Server);
        assert_eq!(classify(599, ""), ErrKind::Server);
    }

    #[test]
    fn client_branch() {
        assert_eq!(classify(400, ""), ErrKind::Client);
        assert_eq!(classify(401, r#"{"msg":"unauthorized"}"#), ErrKind::Client);
        assert_eq!(classify(403, r#"{"code":1001,"msg":"forbidden"}"#), ErrKind::Client);
    }

    // ---- parse_reset_time：时区矩阵 ----

    #[test]
    fn parse_reset_time_utc8_suffix() {
        let now = utc_ms(2026, 9, 15, 0, 0, 0);
        let ts = parse_reset_time(
            "您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。",
            now,
        )
        .expect("应解析出重置时刻");
        assert_eq!(ts, utc_ms(2026, 9, 15, 5, 25, 47));
        // 返回毫秒 epoch：契约 §13 要求与 now_ms 约定一致，
        // 参考实现此处返回的是 Unix 秒（ts = 秒 × 1000）。
        let expect_secs = chrono::NaiveDate::from_ymd_opt(2026, 9, 15)
            .and_then(|d| d.and_hms_opt(5, 25, 47))
            .expect("测试用日历时刻应合法")
            .and_utc()
            .timestamp();
        assert_eq!(ts, expect_secs * 1000);
    }

    #[test]
    fn parse_reset_time_utc8_colon_form() {
        let now = utc_ms(2026, 9, 15, 0, 0, 0);
        // "UTC+08:00" 与 "UTC+8" 等价。
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 UTC+08:00 重置", now),
            Some(utc_ms(2026, 9, 15, 5, 25, 47))
        );
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 UTC+8 重置", now),
            Some(utc_ms(2026, 9, 15, 5, 25, 47))
        );
        // 负偏移。
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 UTC-05:30 重置", now),
            Some(utc_ms(2026, 9, 15, 18, 55, 47))
        );
    }

    #[test]
    fn parse_reset_time_z_suffix() {
        let now = utc_ms(2026, 9, 15, 0, 0, 0);
        assert_eq!(
            parse_reset_time("reset at 2026-09-15 13:25:47 Z please", now),
            Some(utc_ms(2026, 9, 15, 13, 25, 47))
        );
        // 'Z' 紧跟秒数（无空白）也认。
        assert_eq!(
            parse_reset_time("reset at 2026-09-15T13:25:47Z please", now),
            Some(utc_ms(2026, 9, 15, 13, 25, 47))
        );
    }

    #[test]
    fn parse_reset_time_without_suffix_uses_cst() {
        let now = utc_ms(2026, 9, 15, 0, 0, 0);
        // 无后缀按上游业务时区 CST（+8）解释，而非容器本地时区。
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 重置", now),
            Some(utc_ms(2026, 9, 15, 5, 25, 47))
        );
        assert_eq!(
            parse_reset_time("2026-09-15T13:25:47 重置", now),
            Some(utc_ms(2026, 9, 15, 5, 25, 47))
        );
    }

    #[test]
    fn parse_reset_time_illegal_suffix_falls_back_to_cst() {
        let now = utc_ms(2026, 9, 15, 0, 0, 0);
        // 分钟越界 / 小时越界 → 偏移解析失败 → 回退 CST。
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 UTC+8:61 重置", now),
            Some(utc_ms(2026, 9, 15, 5, 25, 47))
        );
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 UTC+99 重置", now),
            Some(utc_ms(2026, 9, 15, 5, 25, 47))
        );
        // 无符号 / 非 UTC 前缀 → 后缀整体不匹配 → 同样按 CST。
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 CST 重置", now),
            Some(utc_ms(2026, 9, 15, 5, 25, 47))
        );
    }

    #[test]
    fn parse_reset_time_rejects_past_and_now() {
        // 过去时刻 → None（文案里的重置点已过，交给调用方回退固定冷却）。
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 UTC+8 重置", utc_ms(2026, 9, 16, 0, 0, 0)),
            None
        );
        // 恰好等于 now → 不算未来 → None。
        let exact = utc_ms(2026, 9, 15, 5, 25, 47);
        assert_eq!(parse_reset_time("将在 2026-09-15 13:25:47 UTC+8 重置", exact), None);
        // 晚 1ms → 接受。
        assert_eq!(
            parse_reset_time("将在 2026-09-15 13:25:47 UTC+8 重置", exact - 1),
            Some(exact)
        );
    }

    #[test]
    fn parse_reset_time_invalid_literals() {
        let now = utc_ms(2026, 9, 15, 0, 0, 0);
        // 无任何时间信息。
        assert_eq!(parse_reset_time("没有任何时间信息", now), None);
        assert_eq!(parse_reset_time("", now), None);
        // 日期/时间越界（含不存在的 2 月 30 日）。
        assert_eq!(parse_reset_time("将在 2026-13-01 00:00:00 UTC+8 重置", now), None);
        assert_eq!(parse_reset_time("将在 2026-02-30 00:00:00 UTC+8 重置", now), None);
        assert_eq!(parse_reset_time("将在 2026-09-15 25:00:00 UTC+8 重置", now), None);
        assert_eq!(parse_reset_time("将在 2026-09-15 13:60:00 UTC+8 重置", now), None);
        // 位数不齐（月/日必须两位）→ 不匹配。
        assert_eq!(parse_reset_time("将在 2026-9-15 13:25:47 重置", now), None);
        // 闰年 2 月 29 日合法。
        assert_eq!(
            parse_reset_time("将在 2028-02-29 13:25:47 UTC+8 重置", now),
            Some(utc_ms(2028, 2, 29, 5, 25, 47))
        );
    }

    #[test]
    fn parse_reset_time_first_match_wins() {
        let now = utc_ms(2026, 9, 15, 0, 0, 0);
        // 多个时间字面量取第一个（与参考正则的 captures 语义一致）。
        let ts = parse_reset_time(
            "将在 2026-09-15 13:25:47 UTC+8 重置，之后 2026-09-16 01:00:00 Z 再说",
            now,
        )
        .expect("应解析出第一个重置时刻");
        assert_eq!(ts, utc_ms(2026, 9, 15, 5, 25, 47));
    }

    #[test]
    fn parse_utc_offset_validation() {
        assert_eq!(parse_utc_offset_secs("UTC+8"), Some(8 * 3600));
        assert_eq!(parse_utc_offset_secs("UTC-05:30"), Some(-5 * 3600 - 30 * 60));
        assert_eq!(parse_utc_offset_secs("UTC+08"), Some(8 * 3600));
        assert_eq!(parse_utc_offset_secs("UTC+24"), None);
        assert_eq!(parse_utc_offset_secs("UTC+8:61"), None);
        assert_eq!(parse_utc_offset_secs("UTC"), None);
        assert_eq!(parse_utc_offset_secs("CST+8"), None);
    }

    // ---- friendly_message ----

    #[test]
    fn friendly_message_by_kind() {
        assert_eq!(
            friendly_message(ErrKind::HardCredit, 402, ""),
            "账号额度已耗尽：请为该账号充值，或等待签到 / 免费额度恢复后重试"
        );
        // 只有 body 真带 14018 才写码。
        assert!(friendly_message(ErrKind::HardCredit, 402, r#"{"error":{"data":{"code":14018}}}"#)
            .contains("14018"));
        assert!(friendly_message(ErrKind::SoftRate, 429, "").contains("429"));
        assert!(friendly_message(ErrKind::Server, 502, "").contains("502"));
        assert!(!friendly_message(ErrKind::SessionDead, 401, "").is_empty());
        assert!(!friendly_message(ErrKind::NotFound, 404, "").is_empty());
        assert!(!friendly_message(ErrKind::ModelRate, 429, "").is_empty());
        // 空串 = 没有更优表述，调用方回退原始文案。
        assert_eq!(friendly_message(ErrKind::Client, 400, ""), "");
        assert_eq!(friendly_message(ErrKind::None, 200, ""), "");
    }

    #[test]
    fn friendly_message_context_too_long_keeps_original_text() {
        let body = r#"{"msg":"prompt is too long: 1119655 tokens > 1048576 maximum","displayMsg":{"en":"The request exceeds the model context limit...","zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}"#;
        let msg = friendly_message(ErrKind::ContextTooLong, 400, body);
        // 下游客户端靠原文特征串识别溢出，必须保留。
        assert!(msg.contains("prompt is too long"));
        assert!(msg.contains("对话内容超出模型长度上限"));
        assert!(msg.starts_with("prompt is too long"));
        // 字段缺失回退原文。
        assert_eq!(context_too_long_message("plain error"), "plain error");
        assert_eq!(friendly_message(ErrKind::ContextTooLong, 400, "plain error"), "plain error");
    }

    #[test]
    fn friendly_message_truncates_long_body() {
        // 超长 body 提炼后仍截断到 200 字节内。
        let long = format!("{{\"msg\":\"{}\"}}", "x".repeat(1000));
        let msg = friendly_message(ErrKind::ContextTooLong, 400, &long);
        assert!(msg.len() <= FRIENDLY_MAX_BYTES, "应截断到 200 字节内");
        // 纯文本回退同样截断。
        let msg2 = friendly_message(ErrKind::ContextTooLong, 400, &"y".repeat(1000));
        assert_eq!(msg2.len(), FRIENDLY_MAX_BYTES);
    }

    #[test]
    fn friendly_message_context_too_long_fallback_chain() {
        // msg 缺失 → 用 extError.message。
        assert_eq!(
            context_too_long_message(r#"{"extError":{"message":"input length too long"}}"#),
            "input length too long"
        );
        // msg 与 displayMsg 都有 → 组合输出。
        assert_eq!(
            context_too_long_message(r#"{"msg":"prompt is too long","displayMsg":{"zh":"对话内容超出模型长度上限"}}"#),
            "prompt is too long（对话内容超出模型长度上限）"
        );
        // msg 缺失、displayMsg.zh 缺失 → 回退 en。
        assert_eq!(
            context_too_long_message(r#"{"displayMsg":{"en":"The request exceeds the model context limit"}}"#),
            "The request exceeds the model context limit"
        );
    }

    #[test]
    fn truncate_on_char_boundary() {
        assert_eq!(truncate("  hello  ", 10), "hello");
        assert_eq!(truncate("abcdef", 3), "abc");
        // 中文按字符边界回退，不产生非法 UTF-8。
        let t = truncate("积分不足余额", 7);
        assert!(t.chars().count() <= 4);
        assert!(t.len() <= 7);
    }

    // ---- 判定顺序总矩阵（顺序即语义） ----

    #[test]
    fn classification_order_matrix() {
        // 402 > 14018 > 硬标记 > session 失效 > 429(6004 > 软) > 上下文超长 > 404 > 5xx > 4xx。
        let cases: &[(u16, &str, ErrKind)] = &[
            // 402 最优先
            (402, "", ErrKind::HardCredit),
            (402, r#"{"error":{"data":{"code":14018}}}"#, ErrKind::HardCredit),
            // 14018 压过 429 软限流（不先拦会无限重试）
            (429, r#"{"error":{"data":{"code":14018}}}"#, ErrKind::HardCredit),
            // 硬标记压过 session 失效与 429
            (401, "12153 余额不足", ErrKind::HardCredit),
            (429, "quota exceeded", ErrKind::HardCredit),
            // session 失效压过 429
            (429, "Offline user session not found", ErrKind::SessionDead),
            // 429 内：6004 压过软限流
            (429, r#"{"code":6004}"#, ErrKind::ModelRate),
            (429, "超出频率限制", ErrKind::ModelRate),
            (429, r#"{"msg":"too many requests"}"#, ErrKind::SoftRate),
            // 上下文超长压过通用 4xx，但不压过 429
            (400, r#"{"code":11115}"#, ErrKind::ContextTooLong),
            (400, "input length too long", ErrKind::ContextTooLong),
            (429, r#"{"code":11115}"#, ErrKind::SoftRate),
            // 404 / 5xx / 4xx 兜底
            (404, "", ErrKind::NotFound),
            (500, "", ErrKind::Server),
            (400, "", ErrKind::Client),
            (200, "", ErrKind::None),
        ];
        for (status, body, expect) in cases {
            assert_eq!(
                classify(*status, body),
                *expect,
                "classify({status}, {body:?}) 应为 {expect}"
            );
        }
    }
}
