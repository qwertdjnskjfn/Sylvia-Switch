//! MiniMax 通道（1.6.0 新网关）：MiniMax Agent 网页端反代——账号池 + 每日签到 +
//! OpenAI 兼容对话（实验性）。协议结论来自 2026-10-03/04 对四个开源仓库的交叉
//! 拆解（Zhengyuuuui/minimax2api、juangchuank-ops/minimax2api、snake-aabb-wtf/
//! minimaxM3-web2api、wangmingdong/workbuddy-signin，全部 verified——见记忆
//! minimax-code-research 与工作流报告），所有端点/签名配方均有代码级来源。
//!
//! 双区（token 互不通用）：
//! - international：agent.minimax.io（对话 /minimax-cloud/api/v1/agent/...，签到同前缀）
//! - domestic：agent.minimaxi.com（对话 /archon/api/v1/agent/...，签到 /minimax-cloud/...）
//!
//! 双签名（盐/后缀三仓库逐字一致）：
//!   x-signature = md5(unix秒 + "I*7Cf%WZ#S&%1RlZJ&C2" + body)
//!   yy          = md5(JSencodeURIComponent(url) + "_" + body + md5(unix毫秒) + "ooui")
//! 签到族 yy 签「相对路径?签名query」（含 op_ticket=undefined），线上 query 省略
//! op_ticket；agent 对话族 yy 的 host 绑定三仓库口径矛盾——由 `yy_absolute_chat`
//! 配置切换（默认签绝对 URL，待实测修正）。
//!
//! 已知边界（诚实标注）：消息 payload 的真实形状未被完整逆向（juangchuank 自述
//! 臆造得 400），对话走「payload 模板注入」（用户从 HAR 抓包填模板），默认模板
//! 为最常见形态；签到/余额端点是三仓库交叉验证的可靠部分。

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

const SIGNATURE_SALT: &str = "I*7Cf%WZ#S&%1RlZJ&C2";
const SIGNATURE_SUFFIX: &str = "ooui";
pub const PROVIDER_BIGMODEL: &str = "domestic"; // agent.minimaxi.com（国内版）
pub const PROVIDER_ZAI: &str = "international"; // agent.minimax.io（国际版）

pub fn host_for(region: &str) -> &'static str {
    match region {
        PROVIDER_ZAI => "https://agent.minimax.io",
        _ => "https://agent.minimaxi.com",
    }
}

fn stream_host_for(rest_host: &str) -> String {
    // agent.minimax.io → agent-stream.minimax.io
    rest_host.replacen("agent.", "agent-stream.", 1)
}

pub fn provider_label(region: &str) -> &'static str {
    match region {
        PROVIDER_ZAI => "国际版",
        _ => "国内版",
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn unix_seconds() -> u64 {
    now_ms() / 1000
}

// ---------------------------------------------------------------------------
// 签名引擎（纯函数，md-5 依赖）
// ---------------------------------------------------------------------------

fn md5_hex(s: &str) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// x-signature = md5(unix秒 + 盐 + body)
pub fn x_signature(unix_sec: u64, body: &str) -> String {
    md5_hex(&format!("{unix_sec}{SIGNATURE_SALT}{body}"))
}

/// JS encodeURIComponent：保留 A-Za-z0-9 - _ . ! ~ * ' ( )
fn js_encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// yy = md5(JSencodeURIComponent(url) + "_" + body + md5(unix毫秒) + "ooui")
pub fn yy_signature(url: &str, body: &str, unix_ms: u64) -> String {
    md5_hex(&format!(
        "{}_{}{}{SIGNATURE_SUFFIX}",
        js_encode_uri_component(url),
        body,
        md5_hex(&unix_ms.to_string())
    ))
}

/// URLSearchParams 语义编码（值转字符串、空格→+、保插入序）。
fn form_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '*' | '-' | '.' | '_') {
            out.push(c);
        } else if c == ' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 配置与账号池
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct MmAccount {
    pub id: String,
    pub label: String,
    /// domestic | international
    pub region: String,
    /// 网页端 JWT（localStorage._token，约 40 天）
    pub token: String,
    /// 浏览器指纹三件套 + 屏幕尺寸（yy 签名覆盖，必须取自真实浏览器）
    pub uuid: String,
    pub device_id: String,
    pub user_id: String,
    pub agent_id: String,
    pub screen_w: String,
    pub screen_h: String,
    /// 站点域名覆写（域名口径存在跨仓库矛盾时用；空 = 按 region 默认）
    pub base_url: String,
    pub enabled: bool,
    pub added_at: u64,
    pub last_error: Option<String>,
    pub cooldown_until_ms: u64,
    /// 最近一次签到日期（YYYY-MM-DD），幂等用
    pub signin_last_date: String,
    pub signin_points_total: u64,
    /// 凭据来源：""=手动粘贴；"desktop"=桌面端 auth.json 只读共享
    pub source: String,
    /// source=desktop 时：auth.json 绝对路径（每次调用现读 accessToken，
    /// 桌面端自行续期后自动跟上；绝不代刷新——refresh 一次性轮换 +
    /// generation 跨进程锁，代刷新会把桌面端挤下线）
    pub auth_path: String,
    /// auth.json records 里含 oauth 的完整键（含 \x00 后缀）
    pub auth_key: String,
    /// 最近一次探测的积分余额（credit/details 未过期求和；探测时写回）
    pub balance: Option<f64>,
    /// 同一轮探测的已消耗积分
    pub credit_consumed: Option<f64>,
    pub balance_at: u64,
    /// 熔断：连续失败次数（成功即清零）。2026-10-05
    pub fail_streak: u32,
}

#[derive(Clone, Debug)]
pub struct MmConfig {
    pub enabled: bool,
    pub api_key: String,
    pub cache_enabled: bool,
    /// 对话 payload 模板：{{message}} 占位符替换为用户消息（真实形状待抓包）
    pub chat_payload_template: String,
    /// agent 对话端点 yy 是否签绝对 URL（三仓库口径矛盾，默认 true 按 juangchuank）
    pub yy_absolute_chat: bool,
    /// 账号选择模式（2026-10-05 新增，对齐 glm_proxy 的 ACCOUNT_MODE_* 范式）
    pub account_mode: String,
    /// 单次请求换号上限：**0 = 不限制**（默认；按候选池顺序试完为止）。2026-10-05
    pub max_failover: u32,
    /// 熔断冷却基数（秒），默认 60；风控(429/403) 自动 ×2。2026-10-05 可配置
    pub cooldown_base_secs: u64,
    /// 熔断冷却上限（秒），默认 1800（30 分钟）
    pub cooldown_max_secs: u64,
    /// account_mode=pinned 时生效：**固定集合**（有序、可同时多个）。
    /// 单账号固定是它的特例；`pinned_account_id` 保留为兼容字段（= 集合首元素）。
    pub pinned_account_ids: Vec<String>,
    /// 兼容字段（旧配置/旧 exe 可读）：等于 `pinned_account_ids` 的首元素
    pub pinned_account_id: String,
    pub accounts: Vec<MmAccount>,
}

// ---------------------------------------------------------------------------
// 账号选择模式（对齐 glm_proxy::ACCOUNT_MODE_*；本轮为 MiniMax 补齐「多账号 + 切换」）
// ---------------------------------------------------------------------------
/// 轮询：所有可用账号轮流用（默认，等同历史行为）。
pub const MM_MODE_ROUND_ROBIN: &str = "round_robin";
/// 本机优先：desktop 来源账号（桌面端登录态）排在前面，其余其次。
pub const MM_MODE_LOCAL_FIRST: &str = "local_first";
/// 只用本机：仅 desktop 来源账号；无可用则明确报错（提示去桌面端登录）。
pub const MM_MODE_LOCAL_ONLY: &str = "local_only";
/// 固定账号：只用**固定集合** `pinned_account_ids`（有序、可同时多个；集合内换号，绝不换出集合）。
pub const MM_MODE_PINNED: &str = "pinned";

/// 换号次数默认值：**0 = 不限制**（把候选池里所有可用账号依次试完）。
/// 2026-10-05 调整：此前硬编码为 3，导致固定集合超过 3 个时**排在后面的账号当次请求轮不到**。
/// 需要限制延迟时可把 `max_failover` 配成正整数。
pub const DEFAULT_MAX_FAILOVER: u32 = 0;

/// 安全硬上限：即使配置成很大的值，也不会让单次请求无节制重试（防延迟爆炸）。
pub const MAX_FAILOVER_HARD_CAP: usize = 64;

/// 计算本次请求最多尝试几个候选（**纯函数**，便于单测）。
/// `configured = 0` 表示不限制 → 试完整个候选池（仍受硬上限约束）。
pub fn failover_attempts(pool_len: usize, configured: u32) -> usize {
    let want = if configured == 0 {
        pool_len
    } else {
        (configured as usize).min(pool_len)
    };
    want.min(MAX_FAILOVER_HARD_CAP)
}

/// 归一化账号模式：未知值一律回落到 round_robin（防配置手改写坏）。
pub fn normalize_account_mode(value: &str) -> String {
    match value.trim() {
        MM_MODE_LOCAL_FIRST => MM_MODE_LOCAL_FIRST.to_string(),
        MM_MODE_LOCAL_ONLY => MM_MODE_LOCAL_ONLY.to_string(),
        MM_MODE_PINNED => MM_MODE_PINNED.to_string(),
        _ => MM_MODE_ROUND_ROBIN.to_string(),
    }
}

pub fn config_path() -> std::path::PathBuf {
    crate::modules::config::store_dir().join("minimax_config.json")
}

pub fn gen_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

pub fn gen_api_key() -> String {
    format!("sk-minimax-{}", uuid::Uuid::new_v4())
}

pub fn load_config() -> MmConfig {
    let default = || MmConfig {
        enabled: false,
        api_key: gen_api_key(),
        cache_enabled: false,
        chat_payload_template: r#"{"message":{"content":"{{message}}"}}"#.to_string(),
        yy_absolute_chat: true,
        max_failover: DEFAULT_MAX_FAILOVER,
        cooldown_base_secs: 60,
        cooldown_max_secs: 1800,
        account_mode: MM_MODE_ROUND_ROBIN.to_string(),
        pinned_account_ids: Vec::new(),
        pinned_account_id: String::new(),
        accounts: Vec::new(),
    };
    let text = match std::fs::read_to_string(config_path()) {
        Ok(t) => t,
        Err(_) => {
            let cfg = default();
            save_config(&cfg);
            return cfg;
        }
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return default();
    };
    let mut cfg = default();
    cfg.enabled = v.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    if let Some(k) = v.get("api_key").and_then(Value::as_str) {
        if !k.is_empty() {
            cfg.api_key = k.to_string();
        }
    }
    if let Some(tpl) = v.get("chat_payload_template").and_then(Value::as_str) {
        if !tpl.is_empty() {
            cfg.chat_payload_template = tpl.to_string();
        }
    }
    cfg.yy_absolute_chat = v.get("yy_absolute_chat").and_then(Value::as_bool).unwrap_or(true);
    // 账号模式（2026-10-05 新增）：归一化读取，未知值回落轮询
    cfg.account_mode = normalize_account_mode(
        v.get("account_mode").and_then(Value::as_str).unwrap_or(""),
    );
    cfg.pinned_account_id = v
        .get("pinned_account_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    cfg.max_failover = v
        .get("max_failover")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_FAILOVER as u64)
        .min(MAX_FAILOVER_HARD_CAP as u64) as u32;
    cfg.cooldown_base_secs = v
        .get("cooldown_base_secs")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(5, 3600);
    cfg.cooldown_max_secs = v
        .get("cooldown_max_secs")
        .and_then(Value::as_u64)
        .unwrap_or(1800)
        .clamp(60, 86_400);
    // 固定集合（2026-10-05）：新字段优先，缺失时用旧单值迁移（逻辑见 parse_pin_set）
    cfg.pinned_account_ids = parse_pin_set(v.get("pinned_account_ids"), &cfg.pinned_account_id);
    if let Some(list) = v.get("accounts").and_then(Value::as_array) {
        cfg.accounts = list
            .iter()
            .filter_map(|a| {
                let token = a.get("token").and_then(Value::as_str)?.to_string();
                Some(MmAccount {
                    id: a.get("id").and_then(Value::as_str).unwrap_or(&gen_id()).to_string(),
                    label: a.get("label").and_then(Value::as_str).unwrap_or("未命名").to_string(),
                    region: a.get("region").and_then(Value::as_str).unwrap_or(PROVIDER_BIGMODEL).to_string(),
                    token,
                    uuid: a.get("uuid").and_then(Value::as_str).unwrap_or_default().to_string(),
                    device_id: a.get("device_id").and_then(Value::as_str).unwrap_or_default().to_string(),
                    user_id: a.get("user_id").and_then(Value::as_str).unwrap_or_default().to_string(),
                    agent_id: a.get("agent_id").and_then(Value::as_str).unwrap_or_default().to_string(),
                    screen_w: a.get("screen_w").and_then(Value::as_str).unwrap_or("1707").to_string(),
                    screen_h: a.get("screen_h").and_then(Value::as_str).unwrap_or("1067").to_string(),
                    base_url: a.get("base_url").and_then(Value::as_str).unwrap_or_default().to_string(),
                    enabled: a.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                    added_at: a.get("added_at").and_then(Value::as_u64).unwrap_or(0),
                    last_error: a.get("last_error").and_then(Value::as_str).map(String::from),
                    cooldown_until_ms: a.get("cooldown_until_ms").and_then(Value::as_u64).unwrap_or(0),
                    signin_last_date: a.get("signin_last_date").and_then(Value::as_str).unwrap_or("").to_string(),
                    signin_points_total: a.get("signin_points_total").and_then(Value::as_u64).unwrap_or(0),
                    source: a.get("source").and_then(Value::as_str).unwrap_or("").to_string(),
                    auth_path: a.get("auth_path").and_then(Value::as_str).unwrap_or("").to_string(),
                    auth_key: a.get("auth_key").and_then(Value::as_str).unwrap_or("").to_string(),
                    balance: a.get("balance").and_then(Value::as_f64),
                    credit_consumed: a.get("credit_consumed").and_then(Value::as_f64),
                    balance_at: a.get("balance_at").and_then(Value::as_u64).unwrap_or(0),
                    fail_streak: a.get("fail_streak").and_then(Value::as_u64).unwrap_or(0) as u32,
                })
            })
            .collect();
    }
    // 固定集合规范化：去重 + 剔除悬空 id（见 normalize_pin_set）
    {
        let accounts = cfg.accounts.clone();
        normalize_pin_set(&mut cfg.pinned_account_ids, &accounts);
    }
    cfg.pinned_account_id = cfg.pinned_account_ids.first().cloned().unwrap_or_default();
    cfg
}

pub fn save_config(cfg: &MmConfig) {
    let accounts: Vec<Value> = cfg
        .accounts
        .iter()
        .map(|a| {
            json!({
                "id": a.id, "label": a.label, "region": a.region, "token": a.token,
                "uuid": a.uuid, "device_id": a.device_id, "user_id": a.user_id,
                "agent_id": a.agent_id, "screen_w": a.screen_w, "screen_h": a.screen_h,
                "base_url": a.base_url, "enabled": a.enabled, "added_at": a.added_at,
                "last_error": a.last_error, "cooldown_until_ms": a.cooldown_until_ms,
                "signin_last_date": a.signin_last_date, "signin_points_total": a.signin_points_total,
                "source": a.source, "auth_path": a.auth_path, "auth_key": a.auth_key,
                "balance": a.balance, "credit_consumed": a.credit_consumed, "balance_at": a.balance_at,
                "fail_streak": a.fail_streak,
            })
        })
        .collect();
    // 双写：兼容字段恒等于集合首元素（旧版 exe 回滚仍可读）
    let legacy_pin = cfg.pinned_account_ids.first().cloned().unwrap_or_default();
    let v = json!({
        "enabled": cfg.enabled, "api_key": cfg.api_key,
        "chat_payload_template": cfg.chat_payload_template,
        "yy_absolute_chat": cfg.yy_absolute_chat,
        "max_failover": cfg.max_failover,
        "cooldown_base_secs": cfg.cooldown_base_secs,
        "cooldown_max_secs": cfg.cooldown_max_secs,
        "account_mode": cfg.account_mode,
        "pinned_account_ids": cfg.pinned_account_ids,
        "pinned_account_id": legacy_pin,
        "accounts": accounts, "updated_at": now_ms(),
    });
    if let Some(dir) = config_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(config_path(), serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// 账号当前生效 token：desktop 来源每次现读 auth.json（桌面端续期后自动跟上）；
/// 手动来源用存储值。auth.json 的 records 是对象，键含 \x00 后缀——auth_key 为空
/// 时取第一个含 accessToken 的条目；读失败回落存储值。
pub fn effective_token(acc: &MmAccount) -> String {
    if acc.source == "desktop" && !acc.auth_path.is_empty() {
        if let Ok(text) = std::fs::read_to_string(&acc.auth_path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(obj) = v.get("records").and_then(Value::as_object) {
                    let key = if acc.auth_key.is_empty() {
                        obj.keys().next().cloned().unwrap_or_default()
                    } else {
                        acc.auth_key.clone()
                    };
                    if let Some(tok) = obj
                        .get(&key)
                        .and_then(|r| r.get("accessToken"))
                        .and_then(Value::as_str)
                    {
                        return tok.to_string();
                    }
                    // 键不匹配（桌面端轮换后键名变化）：取首个含 accessToken 的条目
                    if let Some((_, r)) = obj.iter().find(|(_, r)| r.get("accessToken").and_then(Value::as_str).is_some()) {
                        if let Some(tok) = r.get("accessToken").and_then(Value::as_str) {
                            return tok.to_string();
                        }
                    }
                }
            }
        }
    }
    acc.token.clone()
}

/// 云端域名：desktop 来源走 agent.minimax.{cn|io}（实测桌面 OAuth token 用
/// 纯 Bearer 即可通过云端鉴权）；web 来源按账号 host（签名路径）。
fn cloud_host(acc: &MmAccount) -> String {
    if acc.source == "desktop" {
        match acc.region.as_str() {
            PROVIDER_ZAI => "https://agent.minimax.io".to_string(),
            _ => "https://agent.minimax.cn".to_string(),
        }
    } else {
        account_host(acc)
    }
}

/// desktop 来源是否走纯 Bearer（免签名）路径。
fn is_bearer_only(acc: &MmAccount) -> bool {
    acc.source == "desktop"
}

/// desktop 积分明细：credit/details（响应顶层 details[]，金额为字符串）。
/// 返回 (未过期剩余合计, 已消耗合计, 最早到期时间戳)。
pub async fn credit_summary(acc: &MmAccount) -> Option<(f64, f64, u64)> {
    let v = cloud_get(acc, "/minimax-cloud/api/v1/credit/details", None).await.ok()?;
    // 注意：details 在响应顶层（不在 data 下）
    let arr = v.get("details").and_then(Value::as_array)?;
    let now = now_ms();
    let mut remaining = 0.0f64;
    let mut consumed = 0.0f64;
    let mut min_expire = u64::MAX;
    let mut any = false;
    for g in arr {
        let expire = g.get("expire_at_ms").and_then(Value::as_i64).unwrap_or(0) as u64;
        let rem = g
            .get("remaining_amount")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let used = g
            .get("consumed_amount")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        consumed += used;
        if expire > now {
            remaining += rem;
            any = true;
        }
        if expire > 0 {
            min_expire = min_expire.min(expire);
        }
    }
    any.then_some((remaining, consumed, if min_expire == u64::MAX { 0 } else { min_expire }))
}

/// desktop 余额口径：credit/details 的 remaining_amount 求和（积分 30 天到期）。
pub async fn credits_remaining(acc: &MmAccount) -> Option<f64> {
    credit_summary(acc).await.map(|(r, _, _)| r)
}

/// 模型直连端点域名（desktop 实测：国内 agent.minimaxi.com 200；国际
/// agent.minimax.io——区域绑定，跨区 401 invalid access token）。
fn cloud_model_host(acc: &MmAccount) -> String {
    if acc.source == "desktop" {
        match acc.region.as_str() {
            PROVIDER_ZAI => "https://agent.minimax.io".to_string(),
            _ => "https://agent.minimaxi.com".to_string(),
        }
    } else {
        account_host(acc)
    }
}

fn account_host(acc: &MmAccount) -> String {
    if !acc.base_url.is_empty() {
        return acc.base_url.trim_end_matches('/').to_string();
    }
    host_for(&acc.region).to_string()
}

// ---------------------------------------------------------------------------
// 22 参数 query（固定序，来源 juangchuank signin.go:157-182 / snake client_metadata）
// ---------------------------------------------------------------------------

/// 时区偏移：-60 × 本机 getTimezoneOffset()（分钟），UTC+8 → 28800。
fn timezone_offset_value() -> String {
    let offset_min = 480i64; // 本机 UTC+8；如需跨时区部署改为读系统时区
    (offset_min * 60).to_string()
}

/// 构造 query 键值对。`for_signature=true` 时包含 op_ticket=undefined（签名串用），
/// `false` 为线上 wire query（axios 丢弃 undefined 参数）。
fn client_metadata(acc: &MmAccount, for_signature: bool) -> Vec<(String, String)> {
    let ms = now_ms();
    let mut v: Vec<(String, String)> = vec![
        ("device_platform".into(), "web".into()),
        ("biz_id".into(), "3".into()),
        ("app_id".into(), "3001".into()),
        ("version_code".into(), "22201".into()),
        ("unix".into(), ms.to_string()),
        ("timezone_offset".into(), timezone_offset_value()),
        ("sys_language".into(), "zh".into()),
        ("lang".into(), "zh".into()),
        ("uuid".into(), acc.uuid.clone()),
        ("device_id".into(), acc.device_id.clone()),
        ("os_name".into(), "Windows".into()),
        ("browser_name".into(), "Chrome".into()),
        ("device_memory".into(), "16".into()),
        ("cpu_core_num".into(), "8".into()),
        ("browser_language".into(), "zh-CN".into()),
        ("browser_platform".into(), "Win32".into()),
        ("user_id".into(), acc.user_id.clone()),
    ];
    if for_signature {
        v.push(("op_ticket".into(), "undefined".into()));
    }
    v.push(("screen_width".into(), acc.screen_w.clone()));
    v.push(("screen_height".into(), acc.screen_h.clone()));
    v.push(("token".into(), acc.token.clone()));
    v.push(("client".into(), "web".into()));
    if acc.region == PROVIDER_BIGMODEL {
        v.push(("region".into(), "cn".into()));
    }
    v
}

fn query_string(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 组装签名四头 + wire query。返回 (wire_query, x_timestamp, x_signature, yy)。
fn sign_request(acc: &MmAccount, path: &str, body: &str, yy_absolute: bool) -> (String, String, String, String) {
    let signed_query = query_string(&client_metadata(acc, true));
    let wire_query = query_string(&client_metadata(acc, false));
    let sec = unix_seconds();
    let xs = x_signature(sec, body);
    // 签到族（/minimax-cloud、/matrix）yy 签相对路径；agent 对话族默认签绝对 URL
    let yy_input = if yy_absolute && !path.starts_with("/minimax-cloud") && !path.starts_with("/matrix") {
        format!("{}{path}?{signed_query}", account_host(acc))
    } else {
        format!("{path}?{signed_query}")
    };
    let yy = yy_signature(&yy_input, body, now_ms());
    (wire_query, sec.to_string(), xs, yy)
}

// ---------------------------------------------------------------------------
// 上游调用
// ---------------------------------------------------------------------------

const MINIMAX_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36";

fn upstream_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .user_agent(MINIMAX_UA)
            .timeout(Duration::from_secs(120))
            .build()
            .expect("failed to build minimax client")
    })
}

/// **对话专用**HTTP 路径：改用系统 `curl.exe` 子进程。
///
/// 🔴 2026-10-05 实测（本渠道的根因，实验驱动）：
/// 同一 IP / 同一 token / 同一 payload / 同一 header，仅换 HTTP 客户端：
/// | 客户端 | `usage.input_tokens` | 回复 |
/// | --- | --- | --- |
/// | **curl.exe 8.16（Schannel）** | **40~41** | **正确** |
/// | Python urllib（OpenSSL） | 41 | 正确 |
/// | reqwest（rustls / native-tls / h1.1 / h2 均试过）| **21（恒定）** | 答非所问或 503/529 |
///
/// `input_tokens` **恒定 21**（与 prompt 长度无关，2 字符与 2000 字符都是 21）
/// ⇒ 上游对 reqwest 发出的请求**未按内容处理**。
/// 已逐一排除：端点 / payload 形状 / max_tokens / anthropic-version / User-Agent /
/// content 字符串 vs 数组 / HTTP 版本（h1.1 vs h2）/ TLS 后端（rustls vs native-tls）/
/// Accept-Encoding / Expect: 100-continue / accept 头 / 连接池复用。
/// 剩余差异只能在 TLS ClientHello 字节层，**抓包才能确认**；
/// 而 curl 已验证 100% 可用，故直接改用 curl 绕开 reqwest（不再猜）。
///
/// 代价：每次调用 fork 一个进程（~2s 启动开销，chat 场景可接受）。
///
/// 🔧 v1 → v2：payload 由**临时文件**改为 **stdin**（`--data-binary @-`）。
/// v1 的缺陷：在 `Command::output()` 返回**之前**就 `remove_file` 了，
/// 竞态下 curl 仍在读 → `CURLE_READ_ERROR(26)`。stdin 方案无此竞态，
/// 也不需要清理临时文件（更少 IO）。
fn chat_via_curl(url: &str, payload: &Value, token: &str, timeout_secs: u64) -> Result<(u16, String), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // 中文经命令行传参会产出非法 UTF-8（AGENTS.md 坑 #6）→ 走 stdin
    let body_bytes = serde_json::to_vec(payload).map_err(|e| format!("payload 序列化失败: {e}"))?;

    let mut child = Command::new("curl")
        .arg("-s")                    // 静默（不要进度条污染 stdout）
        .arg("--max-time").arg(timeout_secs.to_string())
        .arg("-w").arg("\n%{http_code}")   // 末尾附状态码，便于区分「HTTP 错误」与「网络错误」
        .arg("-X").arg("POST")
        .arg(url)
        .arg("-H").arg("Content-Type: application/json")
        .arg("-H").arg("anthropic-version: 2023-06-01")
        .arg("-H").arg(format!("Authorization: Bearer {token}"))
        .arg("-H").arg(format!("User-Agent: {MINIMAX_UA}"))
        .arg("--data-binary").arg("@-")   // 从 stdin 读 body
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("调起 curl 失败（系统需自带 curl.exe）: {e}"))?;

    // 写 stdin 后关闭（curl 读到 EOF 才会发请求）
    {
        let mut sin = child
            .stdin
            .take()
            .ok_or_else(|| "取 curl stdin 失败".to_string())?;
        sin.write_all(&body_bytes)
            .map_err(|e| format!("写 curl stdin 失败: {e}"))?;
        // sin 在此 drop → 关闭管道
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("等待 curl 失败: {e}"))?;

    let mut body = String::from_utf8_lossy(&out.stdout).to_string();
    // 拆出末尾的 http_code（curl 非 0 退出时 stdout 可能只有状态码行）
    let status = body
        .rfind('\n')
        .and_then(|i| body.get(i + 1..))
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(if out.status.success() { 200 } else { 502 });
    if let Some(i) = body.rfind('\n') {
        body.truncate(i);
    }
    if !out.status.success() && body.trim().is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "curl 退出码 {:?}（传输层失败）：{}",
            out.status.code(),
            err.chars().take(160).collect::<String>()
        ));
    }
    Ok((status, body))
}

/// 签名 GET（/minimax-cloud 族；GET 的签名 body 按约定为 "{}"）。
async fn cloud_get(acc: &MmAccount, path: &str, extra_query: Option<&str>) -> Result<Value, String> {
    let host = cloud_host(acc);
    // desktop 来源：纯 Bearer（实测桌面 OAuth token 在签名+token 头形态下 401，
    // 纯 Bearer 200——且域名应为 agent.minimax.cn 而非 agent.minimaxi.com）
    if is_bearer_only(acc) {
        let mut url = format!("{host}{path}");
        if let Some(eq) = extra_query {
            let sep = if url.contains('?') { "&" } else { "?" };
            url = format!("{url}{sep}{eq}");
        }
        let resp = upstream_client()
            .get(&url)
            .bearer_auth(effective_token(acc))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("MiniMax 上游请求失败: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return parse_response(status, &text);
    }
    let (wire, ts, xs, yy) = sign_request(acc, path, "{}", cfg_yy_absolute());
    let mut url = format!("{host}{path}?{wire}");
    if let Some(eq) = extra_query {
        url = format!("{url}&{eq}");
    }
    let resp = upstream_client()
        .get(&url)
        .header("token", effective_token(&acc))
        .header("x-timestamp", &ts)
        .header("x-signature", &xs)
        .header("yy", &yy)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("MiniMax 上游请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    parse_response(status, &text)
}

fn cfg_yy_absolute() -> bool {
    load_config().yy_absolute_chat
}

fn parse_response(status: u16, text: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(text)
        .map_err(|_| format!("上游返回非 JSON（HTTP {status}）: {}", text.chars().take(120).collect::<String>()))?;
    let code = v.pointer("/base_resp/status_code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let msg = v
            .pointer("/base_resp/status_msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(format!("上游业务错误（base_resp.status_code={code}）: {msg}"));
    }
    Ok(v)
}

async fn cloud_post(acc: &MmAccount, path: &str, body: &str) -> Result<Value, String> {
    cloud_post_q(acc, path, body, None).await
}

/// 带 query 的 POST（签到族需要 timezone_id；云域名按 source 分流）。
async fn cloud_post_q(
    acc: &MmAccount,
    path: &str,
    body: &str,
    extra_query: Option<&str>,
) -> Result<Value, String> {
    let host = cloud_host(acc);
    if is_bearer_only(acc) {
        let mut url = format!("{host}{path}");
        if let Some(eq) = extra_query {
            url = format!("{url}?{eq}");
        }
        let resp = upstream_client()
            .post(&url)
            .bearer_auth(effective_token(acc))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("MiniMax 上游请求失败: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return parse_response(status, &text);
    }
    let (wire, ts, xs, yy) = sign_request(acc, path, body, cfg_yy_absolute());
    let mut url = format!("{host}{path}?{wire}");
    if let Some(eq) = extra_query {
        url = format!("{url}&{eq}");
    }
    let resp = upstream_client()
        .post(&url)
        .header("token", effective_token(&acc))
        .header("x-timestamp", &ts)
        .header("x-signature", &xs)
        .header("yy", &yy)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("MiniMax 上游请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    parse_response(status, &text)
}

// ---------------------------------------------------------------------------
// 签到（Prepare → status → claim → 对账）与余额
// ---------------------------------------------------------------------------

/// 账号余额（op_credit_summary.total_remaining_amount，字符串 float）。
pub async fn membership_balance(acc: &MmAccount) -> Result<f64, String> {
    let v = cloud_post(acc, "/matrix/api/v1/commerce/get_membership_info", "{}").await?;
    let raw = v
        .pointer("/data/op_credit_summary/total_remaining_amount")
        .cloned()
        .unwrap_or(Value::Null);
    let s = match &raw {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    s.trim().parse::<f64>().map_err(|_| format!("余额字段解析失败: {s}"))
}

/// 本地日期（YYYY-MM-DD，UTC+8）——签到幂等去重键与展示共用。
pub fn today_str() -> String {
    use chrono::FixedOffset;
    let cst = FixedOffset::east_opt(8 * 3600).unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
    let now_local = chrono::Utc::now().with_timezone(&cst);
    now_local.format("%Y-%m-%d").to_string()
}

/// 单账号签到全流程：Prepare → status（已领跳过）→ claim → credit/details 对账。
pub async fn signin_account(acc: &MmAccount) -> Value {
    let mut steps: Vec<Value> = Vec::new();
    let account_id = acc.id.clone();
    // ① Prepare 序列（顺序契约：/config → /agent → /channel/connections，
    //    乱序会导致「签到被记录但积分静默丢失且不可救」——juangchuank 实测注释）
    for p in ["/minimax-cloud/api/v1/config", "/minimax-cloud/api/v1/agent", "/minimax-cloud/api/v1/channel/connections"] {
        match cloud_get(acc, p, None).await {
            Ok(_) => steps.push(json!({"step": p, "ok": true})),
            Err(e) => {
                steps.push(json!({"step": p, "ok": false, "error": e}));
                return json!({"ok": false, "id": account_id, "account": acc.label, "error": format!("Prepare 失败: {e}"), "steps": steps});
            }
        }
    }
    // ② status 面板（时区参数：domestic 走 timezone_offset（wangmingdong 实测口径）、
    //    international 亦用 timezone_offset（juangchuank）；timezone_id 口径为国内站
    //    备选，出现拒绝时在账号配置里改 base_url/参数——待实测）
    // 签到端点必须带时区（实测缺失 → 1406010011 invalid timezone_id）
    let tz = "timezone_id=Asia/Shanghai";
    let panel = match cloud_get(acc, "/minimax-cloud/api/v1/signin/status", Some(tz)).await {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "id": account_id, "account": acc.label, "error": format!("签到面板查询失败: {e}"), "steps": steps}),
    };
    let days = panel.pointer("/data/panel/days").cloned().unwrap_or(json!([]));
    let today = panel
        .pointer("/data/panel/days")
        .and_then(Value::as_array)
        .and_then(|arr| arr.iter().find(|d| d.get("is_today").and_then(Value::as_bool) == Some(true)))
        .cloned()
        .unwrap_or(json!({}));
    let today_status = today.get("status").and_then(Value::as_i64).unwrap_or(0);
    if today_status == 3 {
        return json!({"ok": true, "id": account_id, "account": acc.label, "action": "already-claimed",
                      "points": today.get("points"), "steps": steps});
    }
    // ③ claim（body 固定 {}）
    let claim = match cloud_post_q(acc, "/minimax-cloud/api/v1/signin/claim", "{}", Some(tz)).await {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "id": account_id, "account": acc.label, "error": format!("签到领取失败: {e}"), "steps": steps}),
    };
    let claim_result = claim.pointer("/data/claim_result").and_then(Value::as_i64).unwrap_or(0);
    let points = claim.pointer("/data/points").cloned().unwrap_or(json!(0));
    // ④ 对账：credit/details 找当日发放（claim_result=1 不代表发放，juangchuank 实测）
    let mut paid = false;
    if let Ok(details) = cloud_get(acc, "/minimax-cloud/api/v1/credit/details", Some(tz)).await {
        if let Some(arr) = details.get("details").and_then(Value::as_array) {
            paid = arr.iter().any(|g| {
                let amount = g
                    .get("granted_amount")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<f64>().ok())
                    .or_else(|| g.get("granted_amount").and_then(Value::as_f64))
                    .unwrap_or(0.0);
                amount > 0.0
            });
        }
    }
    steps.push(json!({"step": "claim", "claim_result": claim_result, "points": points, "paid": paid}));
    json!({
        "ok": true, "id": account_id, "account": acc.label,
        "action": if claim_result == 2 { "already-claimed" } else { "claimed" },
        "points": points, "paid": paid, "steps": steps,
        "panelDays": days,
    })
}

// ---------------------------------------------------------------------------
// 对话（实验性）：网页端会话流
// ---------------------------------------------------------------------------

static CHAT_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// 解析固定集合（**纯函数**，便于单测）：新字段 `pinned_account_ids` 优先；
/// 缺失/为空时用旧单值 `pinned_account_id` 迁移（兼容 1.0.39 之前的配置）。
pub fn parse_pin_set(value: Option<&Value>, legacy_single: &str) -> Vec<String> {
    let mut ids: Vec<String> = value
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let legacy = legacy_single.trim();
    if ids.is_empty() && !legacy.is_empty() {
        ids.push(legacy.to_string());
    }
    ids
}

/// 固定集合规范化（**纯函数**）：去重 + 剔除已不存在的账号（悬空 id 自动清理），保持原顺序。
pub fn normalize_pin_set(ids: &mut Vec<String>, accounts: &[MmAccount]) {
    let known: std::collections::HashSet<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    ids.retain(|id| known.contains(id.as_str()) && seen.insert(id.clone()));
}

/// 账号可用性判定（唯一口径，供 ordered_pool / 状态页 / 测试复用）：
/// 启用 + 未冷却 + 有凭据（desktop 来源 token 恒为空——现读 auth.json，不能按它过滤）。
pub fn account_usable(a: &MmAccount, now: u64) -> bool {
    a.enabled
        && a.cooldown_until_ms < now
        && (!a.token.is_empty() || a.source == "desktop")
}

/// 账号识别 + 选择（2026-10-05 新增，对齐 glm_proxy::ordered_pool_with_region 范式）。
///
/// 返回**有序候选列表**：调用方按序取用（失败可换下一个号）——
/// 这就是「多账号同时可用 + 可切换」的核心。
///
/// 模式：
/// - `round_robin`：全部可用账号（保持既有轮询行为）
/// - `local_first`：desktop 来源（本机桌面端登录态）排前，其余其次
/// - `local_only`：仅 desktop 来源，无则明确报错
/// - `pinned`：仅 `pinned_account_ids`（**固定集合**，保序；集合内跳过不可用者、集合内换号，绝不越界）
///
/// 区域隔离：给定 region 时**只**在该区域内选号，禁止跨区借用。
pub fn ordered_pool(cfg: &MmConfig, region: Option<&str>) -> Result<Vec<MmAccount>, String> {
    let now = now_ms();
    let in_region: Vec<MmAccount> = cfg
        .accounts
        .iter()
        .filter(|a| match region {
            Some(r) => a.region == r,
            None => true,
        })
        .cloned()
        .collect();
    let region_hint = match region {
        Some(r) => format!("（区域 {}）", provider_label(r)),
        None => String::new(),
    };
    let usable: Vec<MmAccount> = in_region
        .iter()
        .filter(|a| account_usable(a, now))
        .cloned()
        .collect();
    let cooling = in_region
        .iter()
        .filter(|a| a.enabled && a.cooldown_until_ms >= now)
        .count();

    match cfg.account_mode.as_str() {
        MM_MODE_LOCAL_ONLY => {
            let local: Vec<MmAccount> = usable
                .iter()
                .filter(|a| a.source == "desktop")
                .cloned()
                .collect();
            if local.is_empty() {
                return Err(format!(
                    "已选择「只用本机账号」{region_hint}，但池中没有桌面端登录态账号：\
                     请在 MiniMax 桌面端登录后点「从桌面端导入」，或把选号策略改回「轮询」"
                ));
            }
            Ok(local)
        }
        MM_MODE_LOCAL_FIRST => {
            let mut list: Vec<MmAccount> = usable
                .iter()
                .filter(|a| a.source == "desktop")
                .cloned()
                .collect();
            list.extend(usable.iter().filter(|a| a.source != "desktop").cloned());
            if list.is_empty() {
                return Err(pool_empty_msg(cooling, &region_hint));
            }
            Ok(list)
        }
        MM_MODE_PINNED => {
            // 固定集合（2026-10-05）：有序、可同时多个；集合内跳过不可用者（集合内换号），
            // 但**绝不换出集合**（候选池恒等于 集合 ∩ 可用）。
            let ids = &cfg.pinned_account_ids;
            if ids.is_empty() {
                return Err(
                    "已选择「固定账号」，但固定集合为空：请在 MiniMax 页把账号设为「固定」（可同时固定多个）"
                        .to_string(),
                );
            }
            let mut picked: Vec<MmAccount> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            for id in ids {
                match in_region.iter().find(|a| &a.id == id) {
                    None => missing.push(id.clone()),
                    Some(a) if !a.enabled => skipped.push(format!("「{}」已禁用", a.label)),
                    Some(a) if a.cooldown_until_ms >= now => skipped.push(format!(
                        "「{}」冷却中（约 {}s 后恢复）",
                        a.label,
                        a.cooldown_until_ms.saturating_sub(now) / 1000
                    )),
                    Some(a) if !account_usable(a, now) => {
                        skipped.push(format!("「{}」凭据为空", a.label))
                    }
                    Some(a) => picked.push(a.clone()),
                }
            }
            if picked.is_empty() {
                // 集合全不可用 → 报可操作错（不静默回落轮询）
                let mut parts = vec![format!(
                    "固定集合{region_hint}内没有可用账号（共 {} 个）",
                    ids.len()
                )];
                if !missing.is_empty() {
                    parts.push(format!("已删除：{}", missing.join("、")));
                }
                if !skipped.is_empty() {
                    parts.push(format!("不可用：{}", skipped.join("、")));
                }
                parts.push("请在 MiniMax 页调整固定集合，或改用其他选号策略".to_string());
                return Err(parts.join("；"));
            }
            Ok(picked)
        }
        _ => {
            if usable.is_empty() {
                return Err(pool_empty_msg(cooling, &region_hint));
            }
            Ok(usable)
        }
    }
}

/// 账号池为空的**可操作**文案（区分「全在冷却」与「压根没号」）。
fn pool_empty_msg(cooling: usize, region_hint: &str) -> String {
    if cooling > 0 {
        format!(
            "{cooling} 个候选账号{region_hint}均处于冷却保护期（近期触发限流）：\
             待冷却结束自动恢复，或在账号卡片上手动解除"
        )
    } else {
        format!(
            "MiniMax 账号池无可用账号{region_hint}：请先添加账号（网页端 JWT + 指纹三件套）\
             或从桌面端导入登录态"
        )
    }
}

/// 账号池错误是否属于**瞬时**（稍后重试有意义）：
/// 目前唯一判定依据是「全部候选都在冷却保护期」——冷却会自动过期，故可重试。
/// 其余（固定集合为空 / 账号被删除·禁用 / 凭据为空 / 池里没号）属**配置类**，重试无用。
pub fn is_transient_pool_error(msg: &str) -> bool {
    msg.contains("冷却")
}

/// 供 chat_once 使用：把账号池错误映射为语义正确的状态码（2026-10-05）。
/// - 瞬时（全在冷却）→ **503** Service Unavailable（客户端稍后可重试）
/// - 配置类 → **409** Conflict（需用户去页面改配置，重试无用）
pub fn ordered_pool_for_chat(
    cfg: &MmConfig,
    region: Option<&str>,
) -> Result<Vec<MmAccount>, (u16, String)> {
    match ordered_pool(cfg, region) {
        Ok(v) => Ok(v),
        Err(e) => {
            let code = if is_transient_pool_error(&e) { 503 } else { 409 };
            Err((code, e))
        }
    }
}

/// 取一个账号：候选集合与顺序由 `ordered_pool`（即选号模式）决定，轮询用全局游标。
fn pick_account(cfg: &MmConfig, region: Option<&str>) -> Option<MmAccount> {
    let pool = ordered_pool(cfg, region).ok()?;
    if pool.is_empty() {
        return None;
    }
    let start = CHAT_CURSOR.fetch_add(1, Ordering::Relaxed) % pool.len();
    Some(pool[start].clone())
}

/// 从模板渲染消息体（{{message}} 占位符）。模板形状待抓包校准——上游拒绝时
/// 会原样透出 base_resp 错误。
fn render_chat_payload(template: &str, message: &str, acc: &MmAccount) -> String {
    template
        .replace("{{message}}", &serde_json::to_string(message).unwrap_or_default().trim_matches('"').to_string())
        .replace("{{user_id}}", &acc.user_id)
        .replace("{{agent_id}}", &acc.agent_id)
}

/// 从 SSE 文本流提取回复：type==6 取 agent_message_chunk.msg_content 累积，
/// type==2 的 agent_message 作全量兜底（juangchuank/snake 一致口径）。
fn extract_reply_from_sse(sse: &str) -> String {
    let mut out = String::new();
    let mut fallback = String::new();
    for line in sse.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Ok(v) = serde_json::from_str::<Value>(data.trim()) else { continue };
        match v.get("type").and_then(Value::as_i64) {
            Some(6) => {
                if let Some(t) = v.pointer("/agent_message_chunk/msg_content").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
            Some(2) => {
                if let Some(t) = v.get("agent_message").and_then(Value::as_str) {
                    fallback = t.to_string();
                }
            }
            _ => {}
        }
    }
    if out.is_empty() { fallback } else { out }
}

/// 对话（实验性）：会话创建 → 空闲轮询 → 发消息（SSE 一次性收集）→ 提取回复。
/// payload 形状待抓包校准——上游 400 时请抓 HAR 更新 chat_payload_template。
/// desktop 直连路径的默认模型。
///
/// 2026-10-05 修复：此前该值在 payload 里**硬编码为 `MiniMax-M3.1-Flash-Preview`**
/// （4 处），导致调用方指定的模型名被丢弃 —— 实测请求 `MiniMax-M3` 却回显
/// `M3.1-Flash-Preview`。现已改为「用请求参数，缺省用本常量」。
pub const DEFAULT_CHAT_MODEL: &str = "MiniMax-M3";

/// 单轮对话。
///
/// `model`：调用方指定的模型名（如 `MiniMax-M3` / `MiniMax-M2.7`）；
/// 传 `None` 或空白则用 [`DEFAULT_CHAT_MODEL`]。
/// 单账号执行（2026-10-05 从 chat_once 抽出）：不再自行选号，账号由调用方给定。
/// 这样 chat_once 才能在外层实现「按候选池顺序换号 + 熔断」。
async fn chat_with_account(
    acc: &MmAccount,
    prompt: &str,
    model: &str,
    yy_absolute_chat: bool,
    template: &str,
) -> Result<Value, (u16, String)> {
    if is_bearer_only(acc) {
        let mavis = format!("{}/mavis/api/v1/llm/v1/messages", cloud_model_host(acc));
        let payload = json!({
            "model": model,
            "max_tokens": 4096,
            "messages": [{ "role": "user", "content": prompt }],
        });
        // 走 curl 子进程：reqwest 对该上游恒定返回 in=21（未按内容处理），
        // 根因与全部排除过程见 chat_via_curl 的文档注释。
        // ⚠️ 必须在闭包外取好 token：`acc` 是借用，`move` 闭包不能带走引用。
        let chat_token = effective_token(acc);
        let (st, text) = tokio::task::spawn_blocking(move || {
            chat_via_curl(&mavis, &payload, &chat_token, 120)
        })
        .await
        .map_err(|e| (502u16, format!("对话任务异常: {e}")))?
        .map_err(|e| (502u16, e))?;
        if !(200..300).contains(&st) {
            return Err((st, format!("模型直连 HTTP {st}: {}", text.chars().take(150).collect::<String>())));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| (502u16, format!("模型响应解析失败: {e}")))?;
        // 只取 type=="text" 块——thinking 块（推理文本）不能当回复透出
        let content = v
            .get("content")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        return Ok(json!({
            "id": v.get("id").cloned().unwrap_or(json!("minimax-desktop")),
            "object": "chat.completion",
            "created": now_ms() / 1000,
            // 上游未回 model 时回显**请求的**模型名（此前硬编码 M3.1-Flash-Preview，掩盖了实际未生效）
            "model": v.get("model").cloned().unwrap_or(json!(model.clone())),
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": v.get("usage").cloned().unwrap_or(json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})),
            "minimax": {"account": acc.label, "region": acc.region, "route": "model-direct"},
        }));
    }

    let host = account_host(acc);
    let agent_id = if acc.agent_id.is_empty() {
        // 未提供 agent_id → 走 /agent 发现（Prepare 序列的第一个 GET 会建立）
        let v = cloud_get(acc, "/minimax-cloud/api/v1/agent", None)
            .await
            .map_err(|e| (502u16, e))?;
        v.pointer("/data/agent_id")
            .or_else(|| v.pointer("/data/agent/id"))
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| {
                v.pointer("/data/agent_id")
                    .and_then(Value::as_i64)
                    .map(|i| i.to_string())
            })
            .unwrap_or_default()
    } else {
        acc.agent_id.clone()
    };
    if agent_id.is_empty() {
        return Err((502, "未能获取 agent_id（请检查账号是否已初始化，或手动填写 agent_id）".to_string()));
    }

    // desktop 来源 → 官方模型直连端点（2026-10-04 实测 200：Bearer 免签名、
    // Anthropic 协议；比网页会话流简单可靠，且国际/国内由域名区分）
    if is_bearer_only(acc) {
        let mavis = format!("{}/mavis/api/v1/llm/v1/messages", cloud_model_host(acc));
        let payload = json!({
            "model": model,
            "max_tokens": 4096,
            "messages": [{ "role": "user", "content": prompt }],
        });
        // 走 curl 子进程：reqwest 对该上游恒定返回 in=21（未按内容处理），
        // 根因与全部排除过程见 chat_via_curl 的文档注释。
        // ⚠️ 必须在闭包外取好 token：`acc` 是借用，`move` 闭包不能带走引用。
        let chat_token = effective_token(acc);
        let (st, text) = tokio::task::spawn_blocking(move || {
            chat_via_curl(&mavis, &payload, &chat_token, 120)
        })
        .await
        .map_err(|e| (502u16, format!("对话任务异常: {e}")))?
        .map_err(|e| (502u16, e))?;
        if !(200..300).contains(&st) {
            return Err((st, format!("模型直连 HTTP {st}: {}", text.chars().take(150).collect::<String>())));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| (502u16, format!("模型响应解析失败: {e}")))?;
        let content = v
            .get("content")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        return Ok(json!({
            "id": v.get("id").cloned().unwrap_or(json!("minimax-desktop")),
            "object": "chat.completion",
            "created": now_ms() / 1000,
            "model": v.get("model").cloned().unwrap_or(json!(model.clone())),
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": v.get("usage").cloned().unwrap_or(json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})),
            "minimax": {"account": acc.label, "region": acc.region, "route": "model-direct"},
        }));
    }

    // ① 创建会话（路径按区域：国内 archon / 国际 minimax-cloud——snake vs juangchuank 实测口径）
    let (session_path_prefix, stream_host) = if acc.region == PROVIDER_BIGMODEL {
        (format!("{host}/archon/api/v1"), host.to_string().replace("agent.", "agent-stream."))
    } else {
        (format!("{host}/minimax-cloud/api/v1"), stream_host_for(&host))
    };
    let create_url = format!("{session_path_prefix}/agent/{agent_id}/session");
    let (wire, ts, xs, yy) = sign_request(acc, &format!("/agent/{agent_id}/session"), "{}", yy_absolute_chat);
    // 会话创建签名串不含 query 尾（相对路径 + 参数序）——与 GET 签名一致
    let create_resp = upstream_client()
        .post(&format!("{create_url}?{wire}"))
        .header("token", effective_token(acc))
        .header("x-timestamp", &ts)
        .header("x-signature", &xs)
        .header("yy", &yy)
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|e| (502u16, format!("会话创建失败: {e}")))?;
    let st = create_resp.status().as_u16();
    let ctext = create_resp.text().await.unwrap_or_default();
    if !(200..300).contains(&st) {
        return Err((st, format!("会话创建 HTTP {st}: {}", ctext.chars().take(150).collect::<String>())));
    }
    let cv: Value = serde_json::from_str(&ctext).map_err(|e| (502u16, format!("会话响应解析失败: {e}")))?;
    let session_id = cv
        .pointer("/data/session_id")
        .or_else(|| cv.pointer("/data/id"))
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            cv.pointer("/data/session_id")
                .and_then(Value::as_i64)
                .map(|i| i.to_string())
        })
        .unwrap_or_default();
    if session_id.is_empty() {
        return Err((502, format!("会话创建未返回 session_id: {}", ctext.chars().take(150).collect::<String>())));
    }

    // ② 空闲轮询（status.type==0，最多 5×1s，snake 口径）
    for _ in 0..5 {
        let idle_url = format!("{session_path_prefix}/session/{session_id}");
        let (w2, t2, x2, y2) = sign_request(acc, &format!("/session/{session_id}"), "{}", yy_absolute_chat);
        let r = upstream_client()
            .get(&format!("{idle_url}?{w2}"))
            .header("token", effective_token(acc))
            .header("x-timestamp", &t2)
            .header("x-signature", &x2)
            .header("yy", &y2)
            .send()
            .await;
        if let Ok(resp) = r {
            if let Ok(v) = resp.json::<Value>().await {
                if v.pointer("/data/status/type").and_then(Value::as_i64) == Some(0) {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // ③ 发消息（SSE 域 + 绝对/相对 yy 按配置；payload 模板注入）
    let msg_path = format!("/session/{session_id}/message");
    let payload_str = render_chat_payload(template, prompt, acc);
    let stream_host = if acc.region == PROVIDER_BIGMODEL {
        stream_host.replace("agent.minimaxi.com", "agent-stream.minimaxi.com")
    } else {
        stream_host
    };
    let msg_url_base = format!("{stream_host}{session_path_prefix}{msg_path}");
    let (wire3, ts3, xs3, yy3) = sign_request(acc, &msg_path, &payload_str, yy_absolute_chat);
    let msg_url = if yy_absolute_chat {
        format!("{msg_url_base}?{wire3}")
    } else {
        format!("{msg_url_base}?{wire3}")
    };
    let resp = upstream_client()
        .post(&msg_url)
        .header("token", effective_token(acc))
        .header("x-timestamp", &ts3)
        .header("x-signature", &xs3)
        .header("yy", &yy3)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .body(payload_str)
        .send()
        .await
        .map_err(|e| (502u16, format!("消息发送失败: {e}")))?;
    let st = resp.status().as_u16();
    let sse = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&st) {
        return Err((st, format!("消息发送 HTTP {st}: {}", sse.chars().take(150).collect::<String>())));
    }
    let reply = extract_reply_from_sse(&sse);
    Ok(json!({
        "id": format!("chatcmpl-minimax-{}", &session_id[..session_id.len().min(12)]),
        "object": "chat.completion",
        "created": now_ms() / 1000,
        "model": format!("minimax-web-{}", acc.region),
        "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
        "minimax": {"account": acc.label, "region": acc.region, "sessionId": session_id},
    }))
}

/// 对话入口（2026-10-05 增强）：**按 ordered_pool 候选顺序换号重试 + 失败熔断**。
///
/// - 候选顺序由选号模式决定（轮询 / 本机优先 / 仅本机 / 固定账号）；
/// - 单个账号失败且属于「可换号错误」时，自动取下一个账号（默认**试完整个候选池**，
///   失败账号写入 `last_error` + 指数退避冷却（`cooldown_after_failure`），连续失败计数 `fail_streak`；
/// - 成功则清零计数；`pinned` 模式：候选 = 固定集合 ∩ 可用，**集合内换号、绝不换出集合**（仅剩 1 个候选时严格不换）；
///   可用 `max_failover` 配置上限，0 = 不限制；硬上限 `MAX_FAILOVER_HARD_CAP`）。
/// - 返回体带 `failover`（第几个候选成功）与 `minimax.account`（实际账号），便于排障。
pub async fn chat_once(
    prompt: &str,
    region_hint: Option<&str>,
    model: Option<&str>,
) -> Result<Value, (u16, String)> {
    let cfg = load_config();
    if !cfg.enabled {
        return Err((503, "MiniMax 网关未启用".to_string()));
    }
    // 模型名：优先请求参数，其次内置默认；两端去空白，空串视为未指定
    let model: String = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_CHAT_MODEL)
        .to_string();
    let region = normalize_region(region_hint);
    let pool = ordered_pool_for_chat(&cfg, region)?;
    if pool.is_empty() {
        return Err((503, "MiniMax 账号池无可用账号：请先添加账号（网页端 JWT + 指纹三件套）".to_string()));
    }
    let start = CHAT_CURSOR.fetch_add(1, Ordering::Relaxed) % pool.len();
    let attempts = failover_attempts(pool.len(), cfg.max_failover);
    let mut last: Option<(u16, String)> = None;
    for k in 0..attempts {
        let acc = &pool[(start + k) % pool.len()];
        match chat_with_account(acc, prompt, &model, cfg.yy_absolute_chat, &cfg.chat_payload_template).await {
            Ok(mut v) => {
                mark_account_success(&acc.id);
                if let Some(o) = v.as_object_mut() {
                    o.insert("failover".to_string(), json!(k));
                }
                return Ok(v);
            }
            Err((st, msg)) => {
                let retryable = is_retryable_status(st);
                if retryable || st == 0 {
                    mark_account_failure(&acc.id, st, &msg);
                }
                // pinned 时**集合内**允许换号；只有候选只剩 1 个才严格不换。
                // 越界保护：pool 在 pinned 模式下恒等于「固定集合 ∩ 可用」，天然不会换出集合。
                let strict = cfg.account_mode == MM_MODE_PINNED && pool.len() <= 1;
                if strict || !retryable {
                    return Err((st, msg));
                }
                last = Some((st, msg));
            }
        }
    }
    Err(last.unwrap_or((503, "候选账号均已尝试，无可用结果".to_string())))
}

/// 该失败是否值得**换号重试**：网络错/限流/风控/凭据失效/服务端错 → 换号有意义；
/// 请求级参数错（400/404/405/409/422 等）→ 换号无用，直接透出，避免把同一个错扩散到整个池。
///
/// 注意：本通道的 **401/403 来自上游 MiniMax**，语义是「**这个账号**的 JWT/登录态被拒」，
/// 而非网关鉴权失败（网关鉴权用的是本渠道自己的 api_key）→ 因此 401 也应换号
/// （账号 JWT 过期/失效是最常见失败之一，见 1.0.41）。
pub fn is_retryable_status(status: u16) -> bool {
    status == 0
        || status == 401
        || status == 403
        || status == 408
        || status == 425
        || status == 429
        || status >= 500
}

/// 失败后的**指数退避**冷却（毫秒，**纯函数**）：限流/风控（429/403）起步 ×2；
/// 连续失败按 2^n 放大（n 截到 5），整体受 `max_ms` 封顶。
pub fn cooldown_after_failure_with(streak: u32, status: u16, base_ms: u64, max_ms: u64) -> u64 {
    let base = if status == 429 || status == 403 {
        base_ms.saturating_mul(2)
    } else {
        base_ms
    };
    let shift = streak.saturating_sub(1).min(5);
    (base << shift).min(max_ms)
}

/// 读配置的冷却计算（基数/上限可配，见 config 的 cooldown_base_secs / cooldown_max_secs）。
pub fn cooldown_after_failure(streak: u32, status: u16) -> u64 {
    let cfg = load_config();
    cooldown_after_failure_with(
        streak,
        status,
        cfg.cooldown_base_secs.saturating_mul(1000),
        cfg.cooldown_max_secs.saturating_mul(1000),
    )
}

/// 记一次失败：累加 `fail_streak`、写 `last_error`、置冷却（熔断）。
///
/// **单账号豁免**（2026-10-05 实测教训）：若池里**没有其他可用账号**，则**不置冷却** ——
/// 冷却的意义是"优先用健康的备选"，没有备选时它只会把整个渠道锁死（实测：
/// 唯一账号连续失败 4 次 → 长冷却 → 渠道 503「均在冷却保护期」，比直接报上游错更难排查）。
/// 此时仍记录 `fail_streak` 与 `last_error`，便于页面看原因。
pub fn mark_account_failure(id: &str, status: u16, msg: &str) {
    let mut cfg = load_config();
    let now = now_ms();
    let usable_others = cfg
        .accounts
        .iter()
        .filter(|a| a.id != id && account_usable(a, now))
        .count();
    if let Some(a) = cfg.accounts.iter_mut().find(|x| x.id == id) {
        a.fail_streak = a.fail_streak.saturating_add(1);
        let brief: String = msg.chars().take(160).collect();
        a.last_error = Some(format!("HTTP {status}: {brief}"));
        a.cooldown_until_ms = if usable_others > 0 {
            now + cooldown_after_failure(a.fail_streak, status)
        } else {
            0
        };
    }
    save_config(&cfg);
}

/// 手动解除冷却（2026-10-05）：清 `cooldown_until_ms`、`fail_streak`、`last_error`。
/// `id` 为空则解除全部账号。返回解除的账号 id 列表。
pub fn clear_cooldown(id: &str) -> Vec<String> {
    let mut cfg = load_config();
    let mut hit = Vec::new();
    for a in cfg.accounts.iter_mut() {
        if !id.is_empty() && a.id != id {
            continue;
        }
        if a.cooldown_until_ms > 0 || a.fail_streak > 0 || a.last_error.is_some() {
            a.cooldown_until_ms = 0;
            a.fail_streak = 0;
            a.last_error = None;
            hit.push(a.id.clone());
        }
    }
    if !hit.is_empty() {
        save_config(&cfg);
    }
    hit
}

/// 记一次成功：清零熔断计数与错误（仅在需要变化时落盘，避免每次请求都写配置）。
fn mark_account_success(id: &str) {
    let mut cfg = load_config();
    if let Some(a) = cfg.accounts.iter_mut().find(|x| x.id == id) {
        if a.fail_streak > 0 || a.last_error.is_some() {
            a.fail_streak = 0;
            a.last_error = None;
            save_config(&cfg);
        }
    }
}



/// 扫描桌面端登录态（~/.minimax/auth/prod/{cn,en}/mcode-public/auth.json，只读）。
pub fn discover_desktop() -> Value {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    for (dir, region) in [("cn", "domestic"), ("en", "international")] {
        let path = std::path::PathBuf::from(&home)
            .join(".minimax")
            .join("auth")
            .join("prod")
            .join(dir)
            .join("mcode-public")
            .join("auth.json");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(recs) = v.get("records").and_then(Value::as_object) else { continue };
        let Some((key, rec)) = recs.iter().find(|(_, r)| r.get("accessToken").and_then(Value::as_str).is_some()) else {
            continue;
        };
        let shared = desktop_shared(dir);
        let label = shared
            .get("userName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push(json!({
            "region": region,
            "regionLabel": provider_label(region),
            "authPath": path.display().to_string(),
            "authKey": key,
            "label": label,
            "userId": shared.get("userID").cloned().unwrap_or(Value::Null),
            "realUserID": shared.get("realUserID").cloned().unwrap_or(Value::Null),
            "deviceID": shared.get("deviceID").cloned().unwrap_or(Value::Null),
            "expiresAtMs": rec.get("expiresAtMs").cloned().unwrap_or(Value::Null),
            "tokenExpired": rec
                .get("expiresAtMs")
                .and_then(Value::as_u64)
                .map(|e| e < now_ms())
                .unwrap_or(false),
        }));
    }
    json!({ "ok": !out.is_empty(), "accounts": out })
}

/// 桌面端 sharedUser（%APPDATA%/MiniMax/minimax-agent[-cn]-config.json）。
fn desktop_shared(region_dir: &str) -> Value {
    let base = std::env::var("APPDATA").unwrap_or_default();
    let cfg = if region_dir == "cn" {
        std::path::PathBuf::from(&base).join("MiniMax").join("minimax-agent-cn-config.json")
    } else {
        std::path::PathBuf::from(&base).join("MiniMax").join("minimax-agent-config.json")
    };
    std::fs::read_to_string(&cfg)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("sharedUser").cloned())
        .unwrap_or(json!({}))
}

/// 桌面端账号可用模型（~/.minimax/config.yaml 的 provider.minimax.models，
/// 粗解析：models: 块内 6 空格缩进键名；defaultModel 提到最前）。
pub fn desktop_models() -> Value {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let text = std::fs::read_to_string(
        std::path::PathBuf::from(home).join(".minimax").join("config.yaml"),
    )
    .unwrap_or_default();
    let mut models: Vec<String> = Vec::new();
    let mut in_models = false;
    for line in text.lines() {
        if line.trim_end() == "    models:" {
            in_models = true;
            continue;
        }
        if in_models {
            let is_exact_level = line.starts_with("      ") && !line.starts_with("       ");
            if is_exact_level && line.contains(':') && !line.trim_start().starts_with('-') {
                let name = line.trim().trim_end_matches(':').to_string();
                if !name.is_empty() && !name.contains(' ') && !models.contains(&name) {
                    models.push(name);
                }
            } else if !line.starts_with("      ") && !line.trim().is_empty() {
                in_models = false;
            }
        }
        if line.starts_with("defaultModel:") {
            let dm = line.split(':').nth(1).unwrap_or("").trim().to_string();
            if let Some(short) = dm.rsplit('/').next() {
                if !short.is_empty() && !models.contains(&short.to_string()) {
                    models.insert(0, short.to_string());
                }
            }
        }
    }
    json!(models)
}

fn desktop_shared_label(region_dir: &str) -> String {
    // %APPDATA%/MiniMax/minimax-agent[-cn]-config.json 的 sharedUser.userID
    let base = std::env::var("APPDATA").unwrap_or_default();
    let cfg = if region_dir == "cn" {
        std::path::PathBuf::from(&base).join("MiniMax").join("minimax-agent-cn-config.json")
    } else {
        std::path::PathBuf::from(&base).join("MiniMax").join("minimax-agent-config.json")
    };
    std::fs::read_to_string(&cfg)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v.pointer("/sharedUser/userID")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| {
                    v.pointer("/sharedUser/realUserID")
                        .map(|x| x.to_string())
                })
        })
        .unwrap_or_else(|| "桌面端".to_string())
}

/// 从桌面端登录态导入账号（只读共享；不落 refresh token）。uuid/device_id
/// 本机生成且随账号持久化（格式合规：uuid v4 / 纯数字 device_id）。
pub fn capture_from_desktop() -> Value {
    let disc = discover_desktop();
    let Some(list) = disc.get("accounts").and_then(Value::as_array).cloned() else {
        return json!({ "ok": false, "error": "未找到桌面端登录态（~/.minimax/auth/prod/*/mcode-public/auth.json）" });
    };
    let mut cfg = load_config();
    let mut added = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    for d in &list {
        let path = d.get("authPath").and_then(Value::as_str).unwrap_or("").to_string();
        let key = d.get("authKey").and_then(Value::as_str).unwrap_or("").to_string();
        if cfg.accounts.iter().any(|a| a.auth_path == path && a.auth_key == key) {
            skipped.push(format!("{}（已在池）", d.get("label").and_then(Value::as_str).unwrap_or("?")));
            continue;
        }
        let region = d.get("region").and_then(Value::as_str).unwrap_or(PROVIDER_BIGMODEL).to_string();
        let id = gen_id();
        cfg.accounts.push(MmAccount {
            id: id.clone(),
            label: format!("桌面端-{}", d.get("label").and_then(Value::as_str).unwrap_or(region.as_str())),
            region,
            token: String::new(), // desktop 来源每次现读
            uuid: uuid::Uuid::new_v4().to_string(),
            // 真实桌面端 deviceID（sharedUser，纯数字）——比本机生成更可信
            device_id: d
                .get("deviceID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            user_id: d.get("userId").and_then(Value::as_str).unwrap_or_default().to_string(),
            agent_id: String::new(),
            screen_w: "1707".into(),
            screen_h: "1067".into(),
            base_url: String::new(),
            enabled: true,
            added_at: now_ms(),
            last_error: None,
            cooldown_until_ms: 0,
            signin_last_date: String::new(),
            signin_points_total: 0,
            source: "desktop".into(),
            auth_path: path,
            auth_key: key,
            balance: None,
            credit_consumed: None,
            balance_at: 0,
            fail_streak: 0,
        });
        added += 1;
    }
    // 同步已有 desktop 账号身份（userName/deviceID 随桌面端更新而变化）
    for a in cfg.accounts.iter_mut().filter(|a| a.source == "desktop") {
        let dir = if a.region == PROVIDER_ZAI { "en" } else { "cn" };
        let shared = desktop_shared(dir);
        if let Some(n) = shared.get("userName").and_then(Value::as_str) {
            if !n.is_empty() {
                a.label = format!("桌面端-{n}");
            }
        }
        if let Some(did) = shared.get("deviceID").and_then(Value::as_str) {
            if !did.is_empty() {
                a.device_id = did.to_string();
            }
        }
        if let Some(uid) = shared.get("userID").and_then(Value::as_str) {
            if !uid.is_empty() {
                a.user_id = uid.to_string();
            }
        }
    }
    if added > 0 && !cfg.enabled {
        cfg.enabled = true;
    }
    save_config(&cfg);
    json!({ "ok": true, "added": added, "skipped": skipped })
}

/// 桌面端账号信息离线聚合（不依赖 token）：身份 + 模型表 + 内置技能。
pub fn desktop_details() -> Value {
    let disc = discover_desktop();
    let skills: Vec<String> = std::fs::read_dir(
        std::path::PathBuf::from(
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_default(),
        )
        .join(".minimax")
        .join(".builtin-skills"),
    )
    .map(|rd| {
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .collect()
    })
    .unwrap_or_default();
    json!({
        "accounts": disc.get("accounts").cloned().unwrap_or(json!([])),
        "models": desktop_models(),
        "builtinSkills": skills,
        "note": "积分/签到面板需要有效 token（桌面端惰性续期：在桌面端做一次操作后自动可用）",
    })
}

fn normalize_region(region: Option<&str>) -> Option<&'static str> {
    match region {
        Some("cn") | Some("domestic") | Some("china") => Some(PROVIDER_BIGMODEL),
        Some("intl") | Some("international") | Some("overseas") => Some(PROVIDER_ZAI),
        _ => None,
    }
}

/// 模型清单（区域分组，glmplan 同款契约）。
pub fn models_payload_filtered(cfg: &MmConfig, region: Option<&str>) -> Value {
    let want = normalize_region(region);
    let regions_present: Vec<String> = cfg
        .accounts
        .iter()
        .filter(|a| a.enabled)
        .map(|a| a.region.clone())
        .collect();
    let mut data = Vec::new();
    // 模型表优先取桌面端 config.yaml 的真实目录（defaultModel 置顶），回退硬编码
    let dm = desktop_models();
    let model_names: Vec<String> = dm
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            ["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    for m in &model_names {
        let present: Vec<&str> = regions_present.iter().map(|s| s.as_str()).collect();
        for r in ["international", "domestic"] {
            if present.contains(&r) && want.map(|w| w == r).unwrap_or(true) {
                data.push(json!({
                    "id": m, "object": "model", "created": 0,
                    "owned_by": format!("minimax:{r}"), "region": r,
                }));
            }
        }
    }
    let domestic_n = data.iter().filter(|e| e["region"] == "domestic").count();
    let intl_n = data.iter().filter(|e| e["region"] == "international").count();
    let mut regions = serde_json::Map::new();
    if want.map(|w| w == "domestic").unwrap_or(true) {
        regions.insert("domestic".to_string(), json!({"models": domestic_n}));
    }
    if want.map(|w| w == "international").unwrap_or(true) {
        regions.insert("international".to_string(), json!({"models": intl_n}));
    }
    json!({"object": "list", "data": data, "regions": Value::Object(regions)})
}

/// 每日签到循环：每 30 分钟检查，本日未签且本地时间在 07:00-10:00 窗口 → 全量签到（账号间 2s gap）。
/// 刷新单个账号的积分快照（2026-10-05 抽出为公共函数，供**手动签到 / 自动签到 / 探测**共用）。
///
/// 口径：`desktop` 来源读 `credit/details`（未过期剩余 + 已消耗）；网页来源读 `membership_balance`。
/// 返回 `(balance, consumed)`；是否写回由调用方决定。
pub async fn refresh_credit_snapshot(acc: &MmAccount) -> (Option<f64>, Option<f64>) {
    if acc.source == "desktop" {
        match credit_summary(acc).await {
            Some((remaining, used, _expire)) => (Some(remaining), Some(used)),
            None => (None, None),
        }
    } else {
        (membership_balance(acc).await.ok(), None)
    }
}

/// 刷新快照并**写回配置**（按 id 定位账号，重读避免覆盖并发写入）。拿到余额才写，返回该余额。
pub async fn refresh_and_save_credit(acc: &MmAccount) -> Option<f64> {
    let (bal, consumed) = refresh_credit_snapshot(acc).await;
    let bal = bal?;
    let mut cfg = load_config();
    if let Some(a) = cfg.accounts.iter_mut().find(|x| x.id == acc.id) {
        a.balance = Some(bal);
        a.credit_consumed = consumed;
        a.balance_at = now_ms();
    }
    save_config(&cfg);
    Some(bal)
}

pub fn spawn_signin_loop() {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(150)).await;
        loop {
            let hour = (now_ms() / 1000 / 3600 + 8) % 24; // UTC+8 粗略本地小时
            if (7..10).contains(&hour) {
                let cfg = load_config();
                if cfg.enabled {
                    let today = today_str();
                    let due: Vec<MmAccount> = cfg
                        .accounts
                        .iter()
                        .filter(|a| a.enabled && a.signin_last_date != today)
                        .cloned()
                        .collect();
                    for acc in due {
                        let res = signin_account(&acc).await;
                        let signed = res.get("ok").and_then(Value::as_bool) == Some(true);
                        let mut cfg2 = load_config();
                        if let Some(a) = cfg2.accounts.iter_mut().find(|x| x.id == acc.id) {
                            if signed {
                                a.signin_last_date = today.clone();
                                if let Some(p) = res.get("points").and_then(Value::as_u64) {
                                    a.signin_points_total += p;
                                }
                            } else {
                                a.last_error = res.get("error").and_then(Value::as_str).map(String::from);
                            }
                        }
                        save_config(&cfg2);
                        // 2026-10-05 修复：自动签到后**必须刷新积分快照**。
                        // 原缺陷：此处只回写签到日期/累计分，而 `/api/minimax/status` 只读
                        // balance 快照字段 → “已签到但积分不变”。与手动签到路由同一口径。
                        if signed {
                            let _ = refresh_and_save_credit(&acc).await;
                        }
                        crate::modules::notify::notify(
                            "minimax-signin",
                            "MiniMax 每日签到",
                            &format!("账号 {}：{}", acc.label, serde_json::to_string(&res).unwrap_or_default().chars().take(120).collect::<String>()),
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await; // 防风控节律
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        }
    });
}

/// 健康探测：余额查询（区分封禁/额度/登录态由 HTTP 码与业务码判断）。
pub async fn probe_account(acc: &MmAccount) -> (u16, String) {
    match cloud_post(acc, "/matrix/api/v1/commerce/get_membership_info", "{}").await {
        Ok(_) => (200, String::new()),
        Err(e) => (0, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用账号构造器（只关心与选号相关的字段）。
    fn acc(id: &str, source: &str, region: &str, enabled: bool, cooldown_ms: u64, token: &str) -> MmAccount {
        MmAccount {
            id: id.to_string(),
            label: id.to_string(),
            region: region.to_string(),
            token: token.to_string(),
            uuid: String::new(),
            device_id: String::new(),
            user_id: String::new(),
            agent_id: String::new(),
            screen_w: "1707".to_string(),
            screen_h: "1067".to_string(),
            base_url: String::new(),
            enabled,
            added_at: 0,
            last_error: None,
            cooldown_until_ms: cooldown_ms,
            signin_last_date: String::new(),
            signin_points_total: 0,
            source: source.to_string(),
            auth_path: String::new(),
            auth_key: String::new(),
            balance: None,
            credit_consumed: None,
            balance_at: 0,
            fail_streak: 0,
        }
    }

    fn cfg_with(mode: &str, pin: &str, accounts: Vec<MmAccount>) -> MmConfig {
        MmConfig {
            enabled: true,
            api_key: "sk-test".to_string(),
            cache_enabled: false,
            chat_payload_template: String::new(),
            yy_absolute_chat: true,
            max_failover: 0,
            cooldown_base_secs: 60,
            cooldown_max_secs: 1800,
            account_mode: mode.to_string(),
            pinned_account_ids: if pin.is_empty() { Vec::new() } else { vec![pin.to_string()] },
            pinned_account_id: pin.to_string(),
            accounts,
        }
    }

    #[test]
    fn normalize_mode_falls_back_to_round_robin() {
        assert_eq!(normalize_account_mode("pinned"), MM_MODE_PINNED);
        assert_eq!(normalize_account_mode(" local_first "), MM_MODE_LOCAL_FIRST);
        assert_eq!(normalize_account_mode("nonsense"), MM_MODE_ROUND_ROBIN);
        assert_eq!(normalize_account_mode(""), MM_MODE_ROUND_ROBIN);
    }

    #[test]
    fn usable_requires_enabled_credential_and_no_cooldown() {
        let now = now_ms();
        assert!(account_usable(&acc("a", "", "domestic", true, 0, "tok"), now));
        assert!(!account_usable(&acc("a", "", "domestic", false, 0, "tok"), now));
        assert!(!account_usable(&acc("a", "", "domestic", true, now + 60_000, "tok"), now));
        assert!(!account_usable(&acc("a", "", "domestic", true, 0, ""), now));
        // desktop 来源 token 恒空，仍视为可用（调用时现读 auth.json）
        assert!(account_usable(&acc("a", "desktop", "domestic", true, 0, ""), now));
    }

    #[test]
    fn round_robin_filters_region_disabled_and_cooling() {
        let cfg = cfg_with(
            MM_MODE_ROUND_ROBIN,
            "",
            vec![
                acc("w1", "", "domestic", true, 0, "t1"),
                acc("w2", "", "domestic", true, 0, "t2"),
                acc("intl", "", "international", true, 0, "t3"),
                acc("off", "", "domestic", false, 0, "t4"),
                acc("cool", "", "domestic", true, now_ms() + 60_000, "t5"),
            ],
        );
        let ids: Vec<String> = ordered_pool(&cfg, Some("domestic"))
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec!["w1", "w2"]);
        assert_eq!(ordered_pool(&cfg, None).unwrap().len(), 3);
    }

    #[test]
    fn local_first_puts_desktop_accounts_first() {
        let cfg = cfg_with(
            MM_MODE_LOCAL_FIRST,
            "",
            vec![
                acc("w1", "", "domestic", true, 0, "t1"),
                acc("d1", "desktop", "domestic", true, 0, ""),
            ],
        );
        let ids: Vec<String> = ordered_pool(&cfg, Some("domestic"))
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec!["d1", "w1"]);
    }

    #[test]
    fn local_only_errors_without_desktop_account() {
        let cfg = cfg_with(MM_MODE_LOCAL_ONLY, "", vec![acc("w1", "", "domestic", true, 0, "t1")]);
        let err = ordered_pool(&cfg, Some("domestic")).unwrap_err();
        assert!(err.contains("只用本机账号"), "err={err}");
    }

    #[test]
    fn pinned_single_pin_is_strict_and_errors_are_actionable() {
        let base = vec![
            acc("w1", "", "domestic", true, 0, "t1"),
            acc("w2", "", "domestic", true, 0, "t2"),
        ];
        // 单账号固定 = 集合只有一个元素：只返回它（严格语义）
        let list = ordered_pool(&cfg_with(MM_MODE_PINNED, "w2", base.clone()), Some("domestic")).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "w2");

        // 集合为空
        let e1 = ordered_pool(&cfg_with(MM_MODE_PINNED, "", base.clone()), Some("domestic")).unwrap_err();
        assert!(e1.contains("固定集合为空"), "e1={e1}");

        // 集合里的 id 已被删除（悬空）→ 报“已删除”
        let e2 = ordered_pool(&cfg_with(MM_MODE_PINNED, "nope", base.clone()), Some("domestic")).unwrap_err();
        assert!(e2.contains("已删除"), "e2={e2}");

        // 集合内的账号被禁用
        let mut off = base.clone();
        off[1].enabled = false;
        let e3 = ordered_pool(&cfg_with(MM_MODE_PINNED, "w2", off), Some("domestic")).unwrap_err();
        assert!(e3.contains("已禁用"), "e3={e3}");

        // 集合内的账号在冷却
        let mut cool = base.clone();
        cool[1].cooldown_until_ms = now_ms() + 60_000;
        let e4 = ordered_pool(&cfg_with(MM_MODE_PINNED, "w2", cool), Some("domestic")).unwrap_err();
        assert!(e4.contains("冷却中"), "e4={e4}");
    }

    #[test]
    fn pin_set_migrates_from_legacy_single_value() {
        // 旧配置只有单值 → 迁移为集合
        assert_eq!(parse_pin_set(None, "a1"), vec!["a1".to_string()]);
        assert_eq!(parse_pin_set(Some(&json!([])), " a1 "), vec!["a1".to_string()]);
        // 新字段存在 → 以它为准（忽略旧单值）
        assert_eq!(
            parse_pin_set(Some(&json!(["a1", "a2"])), "a9"),
            vec!["a1".to_string(), "a2".to_string()]
        );
        // 空白与空串过滤
        assert_eq!(
            parse_pin_set(Some(&json!(["a1", "  ", ""])), ""),
            vec!["a1".to_string()]
        );
        assert!(parse_pin_set(None, "   ").is_empty());
    }

    #[test]
    fn pin_set_normalizes_dedupe_and_drops_dangling() {
        let accounts = vec![acc("a1", "", "domestic", true, 0, "t1"), acc("a2", "", "domestic", true, 0, "t2")];
        let mut ids = vec!["a1".to_string(), "a1".to_string(), "ghost".to_string(), "a2".to_string()];
        normalize_pin_set(&mut ids, &accounts);
        assert_eq!(ids, vec!["a1".to_string(), "a2".to_string()], "去重 + 剔除悬空 + 保序");
        let mut all_ghost = vec!["ghost".to_string()];
        normalize_pin_set(&mut all_ghost, &accounts);
        assert!(all_ghost.is_empty());
    }

    #[test]
    fn pinned_set_returns_all_pinned_in_order() {
        let cfg = cfg_with(
            MM_MODE_PINNED,
            "",
            vec![
                acc("a1", "", "domestic", true, 0, "t1"),
                acc("a2", "", "domestic", true, 0, "t2"),
                acc("a3", "", "domestic", true, 0, "t3"),
            ],
        );
        let mut cfg = cfg;
        cfg.pinned_account_ids = vec!["a2".to_string(), "a1".to_string()];
        let ids: Vec<String> = ordered_pool(&cfg, Some("domestic"))
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec!["a2", "a1"], "同时固定两个：按集合顺序返回，未固定的 a3 不参与");
    }

    #[test]
    fn pinned_set_skips_unusable_but_keeps_others() {
        let mut cfg = cfg_with(
            MM_MODE_PINNED,
            "",
            vec![
                acc("a1", "", "domestic", true, now_ms() + 60_000, "t1"),
                acc("a2", "", "domestic", true, 0, "t2"),
            ],
        );
        cfg.pinned_account_ids = vec!["a1".to_string(), "a2".to_string()];
        let ids: Vec<String> = ordered_pool(&cfg, Some("domestic"))
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec!["a2"], "集合内跳过冷却中的 a1，保留可用的 a2（集合内换号）");
    }

    #[test]
    fn pinned_set_all_unusable_reports_actionable_error() {
        let mut cfg = cfg_with(
            MM_MODE_PINNED,
            "",
            vec![
                acc("a1", "", "domestic", false, 0, "t1"),
                acc("a2", "", "domestic", true, now_ms() + 60_000, "t2"),
            ],
        );
        cfg.pinned_account_ids = vec!["a1".to_string(), "a2".to_string(), "ghost".to_string()];
        let err = ordered_pool(&cfg, Some("domestic")).unwrap_err();
        assert!(err.contains("固定集合"), "err={err}");
        assert!(err.contains("已禁用"), "err={err}");
        assert!(err.contains("冷却中"), "err={err}");
        assert!(err.contains("已删除"), "err={err}");
    }

    #[test]
    fn pinned_set_empty_reports_actionable_error() {
        let cfg = cfg_with(MM_MODE_PINNED, "", vec![acc("a1", "", "domestic", true, 0, "t1")]);
        let err = ordered_pool(&cfg, Some("domestic")).unwrap_err();
        assert!(err.contains("固定集合为空"), "err={err}");
    }

    #[test]
    fn retryable_status_selects_failover_worthy_errors() {
        // 网络错、限流/风控、账号凭据失效（上游 401/403）、服务端错 → 值得换号
        for st in [0u16, 401, 403, 408, 425, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(st), "status {st} 应可换号");
        }
        // 业务参数错 → 换号无用，直接透出
        for st in [400u16, 404, 405, 409, 422] {
            assert!(!is_retryable_status(st), "status {st} 不应换号");
        }
    }

    #[test]
    fn cooldown_uses_exponential_backoff_with_cap() {
        let (base, max) = (60_000u64, 30 * 60_000u64);
        // 普通错误：基数起，按 2^n 放大
        assert_eq!(cooldown_after_failure_with(1, 500, base, max), 60_000);
        assert_eq!(cooldown_after_failure_with(2, 500, base, max), 120_000);
        assert_eq!(cooldown_after_failure_with(3, 500, base, max), 240_000);
        // 限流/风控：起步 ×2
        assert_eq!(cooldown_after_failure_with(1, 429, base, max), 120_000);
        assert_eq!(cooldown_after_failure_with(2, 403, base, max), 240_000);
        // 上限封顶（shift 截到 5、并受 max 限制）
        assert_eq!(cooldown_after_failure_with(99, 429, base, max), max);
        assert_eq!(cooldown_after_failure_with(99, 500, base, max), max);
    }

    #[test]
    fn cooldown_respects_custom_config() {
        // 自定义基数 5s / 上限 60s（可在页面上配）
        let (base, max) = (5_000u64, 60_000u64);
        assert_eq!(cooldown_after_failure_with(1, 500, base, max), 5_000);
        assert_eq!(cooldown_after_failure_with(2, 500, base, max), 10_000);
        assert_eq!(cooldown_after_failure_with(1, 429, base, max), 10_000);
        assert_eq!(cooldown_after_failure_with(9, 500, base, max), 60_000, "受上限封顶");
    }

    #[test]
    fn transient_error_classification() {
        // 全在冷却 → 瞬时（503，可重试）
        assert!(is_transient_pool_error(
            "1 个候选账号均处于冷却保护期（近期触发限流）：待冷却结束自动恢复，或在账号卡片上手动解除"
        ));
        // 配置类 → 409（重试无用）
        assert!(!is_transient_pool_error(
            "已选择「固定账号」，但固定集合为空：请在 MiniMax 页把账号设为「固定」（可同时固定多个）"
        ));
        assert!(!is_transient_pool_error(
            "MiniMax 账号池无可用账号：请先添加账号（网页端 JWT + 指纹三件套）或从桌面端导入登录态"
        ));
    }

    #[test]
    fn failover_attempts_defaults_to_unlimited() {
        // 0 = 不限制 → 试完整个候选池（不再硬编码 3）
        assert_eq!(failover_attempts(1, 0), 1);
        assert_eq!(failover_attempts(2, 0), 2);
        assert_eq!(failover_attempts(5, 0), 5, "固定 5 个账号时 5 个都能轮到");
        assert_eq!(failover_attempts(20, 0), 20);
        // 配成正整数 → 取该值与池子大小的较小者
        assert_eq!(failover_attempts(5, 2), 2);
        assert_eq!(failover_attempts(1, 9), 1);
        // 硬上限兜底（防延迟爆炸）
        assert_eq!(failover_attempts(1000, 0), MAX_FAILOVER_HARD_CAP);
        assert_eq!(failover_attempts(1000, 999), MAX_FAILOVER_HARD_CAP);
        assert_eq!(failover_attempts(0, 0), 0);
    }

    #[test]
    fn empty_pool_message_distinguishes_cooling() {
        let cfg = cfg_with(
            MM_MODE_ROUND_ROBIN,
            "",
            vec![acc("c1", "", "domestic", true, now_ms() + 60_000, "t1")],
        );
        let e = ordered_pool(&cfg, Some("domestic")).unwrap_err();
        assert!(e.contains("冷却"), "e={e}");
    }
}
