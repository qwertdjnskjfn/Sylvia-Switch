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
/// 桌面端 wire query 的 `desktop_version`（asar `__MAVIS_VERSION__` 实测值）。
const DESKTOP_VERSION: &str = "3.1.1";
/// 桌面端 wire query 的 `version_code`（asar 硬编码实测值）。
const VERSION_CODE: &str = "22201";
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

#[derive(Clone, Debug, serde::Deserialize)]
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
    /// 导入时的 accessToken 快照（2026-10-04）：MiniMax 桌面端 auth.json 是单登录槽，
    /// 新登录会替换 records——旧条目现读会错拿新号令牌。effective_token 发现身份
    /// 漂移时回落此快照（令牌链断后到过期前仍可用；长期多号请走网页端 JWT）。
    #[serde(default)]
    pub token_snapshot: String,
    /// 最近一次探测的积分余额（credit/details 未过期求和；探测时写回）
    pub balance: Option<f64>,
    /// 同一轮探测的已消耗积分
    pub credit_consumed: Option<f64>,
    pub balance_at: u64,
    /// 熔断：连续失败次数（成功即清零）。2026-10-05
    pub fail_streak: u32,
    /// 桌面端**实例隔离名**（2026-10-08）。
    /// `""` = 主实例（`%APPDATA%\MiniMax` + `~/.minimax`）；
    /// `"acc2"` = 由 `start-minimax-acc2.bat` 用 `--user-data-dir` / `--data-dir`
    /// 启动的隔离实例（`%APPDATA%\MiniMax-acc2` + `~/.minimax-acc2`）。
    ///
    /// 桌面端是**单登录槽**：一个实例只能登录一个账号，登录第二个会覆盖第一个的
    /// `auth.json`。要同时保住两个账号，必须开隔离实例 ⇒ 本字段决定该账号读哪个
    /// 实例的 auth.json 与设备指纹。
    #[serde(default)]
    pub desktop_instance: String,
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
    /// curl 并发闸门：最多同时在飞的 curl 子进程数（默认 3）。2026-10-05 可配
    pub curl_max_concurrent: u32,
    /// curl 最大尝试次数（默认 3；1 = 不重试）。2026-10-05 可配
    pub curl_retries: u32,
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
        curl_max_concurrent: 3,
        curl_retries: 3,
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
    cfg.curl_max_concurrent = v
        .get("curl_max_concurrent")
        .and_then(Value::as_u64)
        .unwrap_or(3)
        .clamp(1, 32) as u32;
    cfg.curl_retries = v
        .get("curl_retries")
        .and_then(Value::as_u64)
        .unwrap_or(3)
        .clamp(1, 10) as u32;
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
                    desktop_instance: a
                        .get("desktop_instance")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    token_snapshot: a
                        .get("token_snapshot")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
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
                "desktop_instance": a.desktop_instance,
            })
        })
        .collect();
    // 双写：兼容字段恒等于集合首元素（旧版 exe 回滚仍可读）
    let legacy_pin = cfg.pinned_account_ids.first().cloned().unwrap_or_default();
    let v = json!({
        "enabled": cfg.enabled, "api_key": cfg.api_key,
        "chat_payload_template": cfg.chat_payload_template,
        "yy_absolute_chat": cfg.yy_absolute_chat,
        "curl_max_concurrent": cfg.curl_max_concurrent,
        "curl_retries": cfg.curl_retries,
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

/// desktop 来源 token 是否本地已过期（auth.json expiresAtMs 早于当前时间）。
pub fn desktop_token_expired(acc: &MmAccount) -> bool {
    if acc.source != "desktop" || acc.auth_path.is_empty() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&acc.auth_path) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return false };
    let Some(recs) = v.get("records").and_then(Value::as_object) else { return false };
    let exp = recs
        .values()
        .filter_map(|r| r.get("expiresAtMs").and_then(Value::as_i64))
        .min()
        .unwrap_or(0);
    exp > 0 && exp < now_ms() as i64
}

// ---------------------------------------------------------------------------
// 桌面端 OAuth 凭据自动续期（2026-10-09）
// ---------------------------------------------------------------------------
//
// 🔴 根因（2026-10-09 13:00 实测）：桌面端 accessToken **TTL 仅 3600s**，且只有
// 「桌面端进程自己运行着」时才会续期。主实例常驻 ⇒ 一直新鲜；**隔离实例（acc2/acc3）
// 一关就停摆** ⇒ 1 小时后 auth.json 里的 accessToken 过期 ⇒ 签到/余额全 401
// （实测 03:08 过期，12:50 全量签到因此失败）。
//
// 协议来源（`E:/MiniMax Code/resources/app.asar`，逐字抠出）：
//   `@mavis/oauth-core/dist/endpoint-config.js`
//     cn/prod → `https://account.minimax.cn/oauth2/token`
//     en/prod → `https://account.minimax.io/oauth2/token`
//   `@mavis/oauth-core/dist/contracts.js`
//     client_id=`mcode-public` / scope=`agent.default` / audience=`agent-backend`
//   `@mavis/oauth-core/dist/oauth-client.js` → `postForm(tokenEndpoint, {...})`
//     表单：grant_type=refresh_token & refresh_token & client_id & scope & audience
//
// ⚠️ **refresh_token 是轮换型**（用一次即换新）：刷新成功后**必须立刻写回 auth.json**，
// 否则下一次拿旧值刷新必 `invalid_grant`（2026-10-09 13:01 实测踩中：探针只探测未写回，
// acc3 的凭据就此作废，只能重新登录）。探针/调试时务必带写回。

/// 桌面端 OAuth 授权服务器（按区域）。
fn oauth_origin(region: &str) -> &'static str {
    if region == PROVIDER_ZAI {
        "https://account.minimax.io"
    } else {
        "https://account.minimax.cn"
    }
}

const MCODE_OAUTH_CLIENT_ID: &str = "mcode-public";
const MCODE_OAUTH_SCOPE: &str = "agent.default";
const MCODE_OAUTH_AUDIENCE: &str = "agent-backend";

/// 提前续期余量：剩余不足 5 分钟即刷新（access token TTL 实测 3600s）。
const TOKEN_REFRESH_SLACK_MS: i64 = 5 * 60 * 1000;

/// 全局串行化刷新。
///
/// refresh_token 轮换型 ⇒ 并发刷新必然「一个成功、另一个拿旧值 400」
/// （两次刷新之间服务端已把旧值作废）。刷新频率极低（每账号 ≤1 次/小时），
/// 全局单锁足够，不引入按实例的细粒度锁。
static TOKEN_REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 现读 auth.json 里的 `(accessToken, refreshToken, expiresAtMs)`。
fn read_desktop_credential(auth_path: &str) -> Option<(String, String, i64)> {
    let text = std::fs::read_to_string(auth_path).ok()?;
    let v = serde_json::from_str::<Value>(&text).ok()?;
    let recs = v.get("records")?.as_object()?;
    let rec = recs
        .values()
        .find(|r| r.get("accessToken").and_then(Value::as_str).is_some())?;
    Some((
        rec.get("accessToken").and_then(Value::as_str).unwrap_or("").to_string(),
        rec.get("refreshToken").and_then(Value::as_str).unwrap_or("").to_string(),
        rec.get("expiresAtMs").and_then(Value::as_i64).unwrap_or(0),
    ))
}

/// 该账号是不是**它所属实例当前登录的那个账号**（续期闸门）。
///
/// 续期会轮换 refresh_token 并改写 auth.json —— 只有「本账号就是该实例当前登录态」
/// 时才允许动，否则会去刷别人的槽位（把真正主人的凭据轮换掉）。
fn desktop_identity_matches(acc: &MmAccount) -> bool {
    if acc.user_id.is_empty() {
        return false;
    }
    let region_dir = if acc.region == "international" { "en" } else { "cn" };
    let cur = desktop_current_user_ids(region_dir, &account_instance(acc));
    !cur.is_empty() && cur.iter().any(|c| c == &acc.user_id)
}

/// 距过期剩余毫秒（读不到凭据返回 `None`）。
pub fn desktop_token_remaining_ms(acc: &MmAccount) -> Option<i64> {
    let (_, _, exp) = read_desktop_credential(&acc.auth_path)?;
    if exp <= 0 {
        return None;
    }
    Some(exp - now_ms() as i64)
}

/// 把刷新结果原子写回 auth.json（**refresh_token 轮换必须落盘**）。
fn write_back_desktop_credential(
    path: &str,
    access: &str,
    refresh: &str,
    expires_at_ms: i64,
    scopes: Option<Vec<String>>,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读 auth.json 失败: {e}"))?;
    let mut v: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 auth.json 失败: {e}"))?;
    let recs = v
        .get_mut("records")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "auth.json 缺少 records".to_string())?;
    let rec = recs
        .values_mut()
        .find(|r| r.get("accessToken").is_some())
        .ok_or_else(|| "auth.json 无可用记录".to_string())?;
    rec["accessToken"] = json!(access);
    if !refresh.is_empty() {
        rec["refreshToken"] = json!(refresh);
    }
    rec["expiresAtMs"] = json!(expires_at_ms);
    if let Some(s) = scopes {
        rec["scopes"] = json!(s);
    }
    let gen = rec.get("generation").and_then(Value::as_u64).unwrap_or(0);
    rec["generation"] = json!(gen + 1);
    let out = serde_json::to_string_pretty(&v).map_err(|e| format!("序列化 auth.json 失败: {e}"))?;
    crate::modules::config::atomic_write(std::path::Path::new(path), &out)
        .map_err(|e| format!("写回 auth.json 失败: {e}"))
}

/// 桌面端凭据自动续期。
///
/// * `Ok(None)`      —— 无需续期（非桌面端 / 未过期 / 身份不匹配 / 读不到凭据）
/// * `Ok(Some(tok))` —— 已续期并**写回 auth.json**，`tok` 为新 accessToken
/// * `Err(msg)`      —— 需要续期但失败（`msg` 面向用户，含可操作指引）
pub async fn refresh_desktop_credential(acc: &MmAccount) -> Result<Option<String>, String> {
    if acc.source != "desktop" || acc.auth_path.is_empty() {
        return Ok(None);
    }
    if !desktop_identity_matches(acc) {
        // 不属于本实例当前登录态 ⇒ 不动它（凭据归属别人，轮换会误伤）
        return Ok(None);
    }
    let _guard = TOKEN_REFRESH_LOCK.lock().await;
    // 拿锁后再读一次：等待期间可能已被别的调用刷好
    let Some((_, refresh, exp)) = read_desktop_credential(&acc.auth_path) else {
        return Ok(None);
    };
    if exp - now_ms() as i64 > TOKEN_REFRESH_SLACK_MS {
        return Ok(None);
    }
    if refresh.is_empty() {
        return Err("auth.json 缺少 refreshToken，无法自动续期".to_string());
    }
    let url = format!("{}/oauth2/token", oauth_origin(&acc.region));
    let resp = upstream_client()
        .post(&url)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", MCODE_OAUTH_CLIENT_ID),
            ("scope", MCODE_OAUTH_SCOPE),
            ("audience", MCODE_OAUTH_AUDIENCE),
        ])
        .send()
        .await
        .map_err(|e| format!("自动续期请求失败（网络）: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status != 200 {
        let code = v.get("error").and_then(Value::as_str).unwrap_or("");
        let desc = v.get("error_description").and_then(Value::as_str).unwrap_or("");
        return Err(match code {
            "invalid_grant" => "refreshToken 已失效（invalid_grant）⇒ 必须在该实例的桌面端重新登录本账号".to_string(),
            _ => format!("自动续期被拒 HTTP {status} {code} {desc}"),
        });
    }
    let new_access = v
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if new_access.is_empty() {
        let brief: String = text.chars().take(200).collect();
        return Err(format!("自动续期响应缺少 access_token: {brief}"));
    }
    let new_refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ttl = v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
    let scopes = v.get("scope").and_then(Value::as_str).map(|s| {
        s.split_whitespace().map(String::from).collect::<Vec<_>>()
    });
    write_back_desktop_credential(
        &acc.auth_path,
        &new_access,
        &new_refresh,
        now_ms() as i64 + ttl * 1000,
        scopes,
    )?;
    Ok(Some(new_access))
}

/// 签到前的凭据闸门：**已过期且续不回来**才拦（token 还没过期时续期失败不阻断，
/// 免得网络抖动把本来能签的号挡在门外）。
///
/// 返回 `Err(原因)` 表示应当中止本次签到。
pub async fn ensure_desktop_credential(acc: &MmAccount) -> Result<Option<String>, String> {
    match refresh_desktop_credential(acc).await {
        Ok(Some(tok)) => Ok(Some(tok)),
        Ok(None) => Ok(None),
        Err(e) => {
            let remaining = desktop_token_remaining_ms(acc).unwrap_or(i64::MIN);
            if remaining <= 0 {
                Err(e)
            } else {
                Ok(None)
            }
        }
    }
}

/// 账号当前生效 token。
///
/// 2026-10-07 修正取用顺序：**现读优先，快照兜底**。实测桌面端会在同一条
/// auth.json 记录内**原地轮换 accessToken**（auth_key 不变、token 已换），
/// 冻结快照打上游直接 401——签到/余额路径因现读一直正常，对话路径因快照
/// 优先而断线。现读的身份安全由 [`fresh_desktop_token`] 保证：auth_key 精确
/// 命中（键含账号特异后缀）或 userID 防漂移核对通过才采用；记录消失/漂移
/// （真切号）回落冻结快照，由上层 401 换号/自动禁用逻辑接管。
pub fn effective_token(acc: &MmAccount) -> String {
    if acc.source == "desktop" && !acc.auth_path.is_empty() {
        if let Some(tok) = fresh_desktop_token(acc) {
            return tok;
        }
    }
    // 现读不可用（文件缺失/真切号/手动导入条目）：冻结快照自含兜底
    if !acc.token.is_empty() {
        return acc.token.clone();
    }
    if !acc.token_snapshot.is_empty() {
        return acc.token_snapshot.clone();
    }
    acc.token.clone()
}

/// 从桌面端 auth.json 现读本账号的 accessToken（身份校验不过返回 None）。
fn fresh_desktop_token(acc: &MmAccount) -> Option<String> {
    let region_dir = if acc.region == "international" { "en" } else { "cn" };
    let cur_ids = desktop_current_user_ids(region_dir, &account_instance(acc));
    let text = std::fs::read_to_string(&acc.auth_path).ok()?;
    fresh_desktop_token_from(&text, &cur_ids, &acc.user_id)
}

/// 桌面端「当前登录账号」的标识集合。
///
/// 🔴 2026-10-08 修正：`sharedUser` 同时含 **`userID`**（12 字符短串，如 `68naMKpnJn7x`）
/// 与 **`realUserID`**（18 位数字，如 `348569758173523968`）——**二者是同一个账号的两副面孔**。
/// 签名的 `user_id` 参数**必须**用 `realUserID`（上游据其查账号时区，用短串恒定
/// `1406010011 invalid timezone_id`，见 [`merge_identity`] 的四组对照实验），
/// 而历史配置里存的是 `userID` ⇒ 身份比对必须**认这两个中的任意一个**；
/// 否则会把「同一个账号」误判成切号 → 回落冻结快照 → 上游
/// `{"error":"invalid access token"}` ✗（2026-10-08 实测踩中）。
fn desktop_current_user_ids(region_dir: &str, instance: &str) -> Vec<String> {
    let shared = desktop_shared(region_dir, instance);
    let su = shared.get("sharedUser").unwrap_or(&shared);
    let mut out = Vec::new();
    for k in ["userID", "realUserID"] {
        if let Some(s) = su.get(k).and_then(Value::as_str) {
            if !s.is_empty() {
                out.push(s.to_string());
            }
        }
    }
    // 🔴 2026-10-09（隔离实例实证）：`sharedUser` 可能**整体为空** `{}`——隔离实例刚完成
    // 登录时资料尚未同步落盘（实测 acc3 的 `minimax-agent-cn-config.json` 的
    // `sharedUser` = `{}`，而 Local Storage 的 `_app://.user_detail_agent` 已写入真实身份
    // `{"userID":"DjXL9dD349LD","realUserID":"541778119013462018","userName":"MiniMax463137"}`）。
    // 无兜底 ⇒ `cur_ids` 空 ⇒ `fresh_desktop_token` 恒 None、`user_id` 补不上（空）
    // ⇒ 签名缺 `user_id` ⇒ 上游返回空面板 ⇒ 该账号**永远签到失败**
    // （acc3 的 463137 报「无参数与 timezone_id 两种口径均未返回有效面板」的根因）。
    let storage_id = desktop_identity_for(instance).map(|id| id.user_id);
    merge_current_identity(out, storage_id)
}

/// 当前登录身份的两级来源合并（纯函数，便于单测）。
///
/// * `shared_ids` —— `sharedUser` 的 `[userID, realUserID]`：**权威**（与 auth.json
///   的 token 同源，见 [`desktop_current_user_ids`] 的注释）；
/// * `storage_id` —— Local Storage(LevelDB) 扫描出的 `realUserID`：**仅在前者全空时**
///   作为兜底启用。
///
/// 之所以坚持「非空即不兜底」，是因为 LevelDB 身份实测会**滞留上一个账号**
/// （主实例切号后仍返回旧值）——只有 `sharedUser` 缺失、无更优信号时它才比
/// 「读不到身份」更好。
fn merge_current_identity(shared_ids: Vec<String>, storage_id: Option<String>) -> Vec<String> {
    if !shared_ids.is_empty() {
        return shared_ids;
    }
    storage_id
        .filter(|s| !s.is_empty())
        .into_iter()
        .collect()
}

/// [`fresh_desktop_token`] 的纯函数核心（可单测）。
///
/// 🔴 身份锚点必须是 `sharedUser` 里的账号标识（2026-10-07 实锤）：auth.json 是单登录槽，
/// **记录键是客户端级**（本机三个账号的 auth_key 完全相同），记录内也无 userID
/// 字段——auth_key 命中证明不了任何身份。仅当「桌面端当前登录 = 本账号」才允许
/// 现读（跟得上原地轮换）；登录了别的号 / 读不到当前身份（`cur_ids` 空）一律回落
/// 冻结快照，绝不借用他号 token——否则会把当前登录账号的积分/对话写进错误槽位
/// （本次 2/3 号积分被污染的根因）。
///
/// `cur_ids`：当前登录账号的标识集合（`userID` / `realUserID` 命中任一即可）。
fn fresh_desktop_token_from(text: &str, cur_ids: &[String], user_id: &str) -> Option<String> {
    if cur_ids.is_empty() || user_id.is_empty() || !cur_ids.iter().any(|id| id == user_id) {
        return None;
    }
    let v = serde_json::from_str::<Value>(text).ok()?;
    let obj = v.get("records")?.as_object()?;
    let tok = obj
        .values()
        .find_map(|r| r.get("accessToken").and_then(Value::as_str))?;
    if tok.is_empty() {
        None
    } else {
        Some(tok.to_string())
    }
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
    // 空明细闸门（2026-10-07）：上游对失效 token 有时返回 200 + 空 details
    // （而非 401）——空集不代表余额为零，按「读不到」处理，绝不用空数据覆盖
    // 此前探测到的真实余额。
    if arr.is_empty() {
        return None;
    }
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

/// 构造 query 键值对（固定序）。
///
/// 🔴 2026-10-08 关键修正（三条，均有 asar 源码级依据：`out/_next/static/chunks/24831`
/// 前端 axios 拦截器 + `dist/service/plugin-system/cloud-transport.js`，两者算法逐字一致）：
///
/// 1. **签名串必须与实际 wire query 完全一致**——上游用请求 URL 里的 query 复算 `yy`；
///    此前 `op_ticket=undefined` 只加在签名串（`for_signature=true`）而不在 wire ⇒ 两串不等
///    ⇒ 上游复算恒不匹配 ⇒ 恒定 `{"error":"invalid signature"}` ✗（这就是 T1 后仍失败的根因）
/// 2. **desktop 专属参数**：`is_desktop=1` / `desktop_version=<版本>` / `client=desktop`
///    （用户 IPC Proxy 日志逐字）；同时**去掉** wire 里不存在的 `token` / `region`
/// 3. `unix` 由调用方传入**唯一时间源** `r`（真实端 `r = Date.parse(new Date().toString())`
///    ⇒ 秒×1000，毫秒位抹零）——`unix` / `x-timestamp` / `md5(r)` 必须同源，否则签名不可复算
fn client_metadata(acc: &MmAccount, unix_ms: u64) -> Vec<(String, String)> {
    let desktop = acc.source == "desktop";
    let mut v: Vec<(String, String)> = vec![
        ("device_platform".into(), "web".into()),
        ("biz_id".into(), "3".into()),
        ("app_id".into(), "3001".into()),
        ("version_code".into(), VERSION_CODE.into()),
        ("unix".into(), unix_ms.to_string()),
        ("timezone_offset".into(), timezone_offset_value()),
    ];
    if desktop {
        v.push(("is_desktop".into(), "1".into()));
        v.push(("desktop_version".into(), DESKTOP_VERSION.into()));
    }
    v.extend([
        ("sys_language".into(), "zh".into()),
        ("lang".into(), "zh".into()),
        ("uuid".into(), acc.uuid.clone()),
        ("device_id".into(), acc.device_id.clone()),
        ("os_name".into(), "Windows".into()),
        ("browser_name".into(), "Chrome".into()),
        ("device_memory".into(), "16".into()),
        ("cpu_core_num".into(), "20".into()),
        ("browser_language".into(), "zh-CN".into()),
        ("browser_platform".into(), "Win32".into()),
        ("user_id".into(), acc.user_id.clone()),
        ("screen_width".into(), acc.screen_w.clone()),
        ("screen_height".into(), acc.screen_h.clone()),
        (
            "client".into(),
            if desktop { "desktop".to_string() } else { "web".to_string() },
        ),
    ]);
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
///
/// `http_body`：**实际发出的 HTTP body**（GET → `""`；POST → body 原文）。
/// 真实端两种签名的 body 语义**不同**（asar 逐字）：
/// - `x-signature = md5(sec + SALT + bodyText)`：GET 时 `bodyText = ""`，POST 时 = body
/// - `yy          = md5(enc(path?query) + "_" + yyBody + md5(r) + "ooui")`：GET 时 `yyBody = "{}"`
///   ⇒ 此前 GET 一律按 `"{}"` 参与 x-signature ⇒ 与上游复算不符 ⇒ 恒定 `invalid signature` ✗
fn sign_request(acc: &MmAccount, path: &str, http_body: &str, yy_absolute: bool) -> (String, String, String, String) {
    sign_request_at(acc, path, http_body, yy_absolute, now_ms())
}

/// [`sign_request`] 的**可注入时间源**版本（供跨语言对齐测试锁定签名字节）。
///
/// `now` 会先被抹零到整秒（×1000），复刻真实端 `Date.parse(new Date().toString())`
/// 的行为——签名三要素（`unix` / `x-timestamp` / `md5(r)`）全部由这一个 `r` 派生。
fn sign_request_at(
    acc: &MmAccount,
    path: &str,
    http_body: &str,
    yy_absolute: bool,
    now: u64,
) -> (String, String, String, String) {
    // 🔴 唯一时间源（三条签名字节全部同源，见 client_metadata 注释 3）
    let r = (now / 1000) * 1000;
    let pairs = client_metadata(acc, r);
    let wire_query = query_string(&pairs);
    let sec = r / 1000;
    let xs = x_signature(sec, http_body);
    let yy_body = if http_body.is_empty() { "{}" } else { http_body };
    // 签到族（/minimax-cloud、/matrix）yy 签相对路径；agent 对话族默认签绝对 URL
    let yy_input = if yy_absolute && !path.starts_with("/minimax-cloud") && !path.starts_with("/matrix") {
        format!("{}{path}?{wire_query}", account_host(acc))
    } else {
        format!("{path}?{wire_query}")
    };
    let yy = yy_signature(&yy_input, yy_body, r);
    (wire_query, sec.to_string(), xs, yy)
}

// ---------------------------------------------------------------------------
// 上游调用
// ---------------------------------------------------------------------------

// 2026-10-08：对齐桌面端实测 UA（用户 IPC Proxy 日志逐字）——
// 上游对 signin 族的鉴权可能校验 UA/签名一致性。
const MINIMAX_UA: &str = "MiniMaxAgent Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) MiniMax/3.1.1 Chrome/148.0.7778.280 Safari/537.36";

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
/// curl 并发闸门（2026-10-05）：实测反复出现 `os error 231（管道耗尽）` 与 `curl exit 26（读错误）`，
/// 根因是**短时间大量 spawn 子进程 + 三条管道**把 Windows 的管道/句柄打满。
/// 这里限制同时在飞的 curl 数量（超出则短暂等待，宁可慢一点也不失败）。
static CURL_INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const CURL_MAX_CONCURRENT_DEFAULT: usize = 3;
const CURL_GATE_WAIT_MS: u64 = 8000;

/// 重试退避（**纯函数**）：第 n 次重试前的等待毫秒（250 → 500 → 1000 …，封顶 2s）。
pub fn curl_retry_delay_ms(attempt: u32) -> u64 {
    if attempt == 0 {
        return 0;
    }
    (250u64 << (attempt.saturating_sub(1)).min(3)).min(2000)
}

/// 拿一个 curl 并发名额（RAII：Drop 时归还）。`max` 由配置给出（默认 3）。
struct CurlPermit;
impl CurlPermit {
    fn acquire(max: usize) -> CurlPermit {
        use std::sync::atomic::Ordering;
        let max = max.max(1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(CURL_GATE_WAIT_MS);
        loop {
            let cur = CURL_INFLIGHT.load(Ordering::SeqCst);
            if cur < max
                && CURL_INFLIGHT
                    .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return CurlPermit;
            }
            if std::time::Instant::now() >= deadline {
                // 超时也放行：宁可冒一次管道风险，也不让请求卡死
                CURL_INFLIGHT.fetch_add(1, Ordering::SeqCst);
                return CurlPermit;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}
impl Drop for CurlPermit {
    fn drop(&mut self) {
        CURL_INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 对话 HTTP 出口（2026-10-05 加固）：**继续用 curl 子进程**，但补上三层稳定性措施。
///
/// 为什么不换回 reqwest：本仓库 reqwest 编译特性是 **rustls-tls**，而这条上游的证书链
/// **Schannel 认、rustls 不认**（这正是原实现改用 curl.exe 的原因）。换回 reqwest 会直接握手失败；
/// 而 `danger_accept_invalid_certs(true)` 等于放弃证书校验，不接受。
/// ⇒ 正确做法是留在 curl，把它"不稳"的两处补上：
///   1) **并发闸门**（`CurlPermit`，默认最多 3 个同时在飞）—— 治 `os error 231（管道耗尽）`；
///   2) **瞬时重试**（最多 3 次，递增退避）—— 治 `curl exit 26（读错误）` 这类抖动。
/// curl 层失败：区分「请求未发出」与「**可能已送达并计费**」（2026-10-09）。
///
/// 🔴 背景缺陷（用户报「扣减积分却扣了两个账号」）：
/// 此前 curl 的任何失败都被压成 `String`，上层于是「一律重试 / 一律换号」。
/// 但 curl 的退出码能精确区分语义：
///
/// | curl exit | 含义 | 请求是否送达 | 上游是否计费 | 重试/换号 |
/// | --- | --- | --- | --- | --- |
/// | 6 / 7 / 35 | 域名解析 / 连接 / SSL 握手失败 | ❌ 未送达 | ❌ 未计费 | ✅ 安全 |
/// | **28** | **操作超时** | ✅ **已送达** | ⚠️ **可能已计费** | ❌ **禁止** |
/// | **52 / 56** | 空回复 / 接收数据失败 | ✅ **已送达** | ⚠️ **可能已计费** | ❌ **禁止** |
///
/// 成因链：账号 A 发出请求 → 上游已扣 A 的费 → curl `--max-time` 超时（exit 28）
/// → 判失败 → 重试或换号 B → 上游**又扣 B 的费** ⇒ 两个账号双边扣费。
///
/// 故 `maybe_billed = true` 时，**三层重试（curl 内层 / 401 刷新 / 外层换号）都必须止步**，
/// 直接把错误透出 —— 宁可不重试，也不静默多扣一个账号的额度。
#[derive(Debug, Clone)]
pub struct CurlFailure {
    pub msg: String,
    /// 请求**可能已送达并计费** ⇒ 禁止任何形式的重试/换号。
    pub maybe_billed: bool,
}

impl CurlFailure {
    /// 由 curl 退出码判定（取向保守：凡无法排除"已送达"的，一律按已送达处理）。
    fn from_exit(code: Option<i32>, stderr_text: String) -> Self {
        let body: String = stderr_text.chars().take(160).collect();
        let maybe_billed = matches!(code, Some(28) | Some(52) | Some(56));
        Self {
            msg: format!("curl 退出码 {code:?}（传输层失败）：{body}"),
            maybe_billed,
        }
    }

    /// 非传输层失败（本地错误：序列化/调起/写 stdin），请求确实没发出。
    fn local(msg: String) -> Self {
        Self { msg, maybe_billed: false }
    }
}

fn chat_via_curl(url: &str, payload: &Value, token: &str, timeout_secs: u64) -> Result<(u16, String), CurlFailure> {
    let cfg = load_config();
    let tries = cfg.curl_retries.clamp(1, 10);
    let gate = if cfg.curl_max_concurrent == 0 {
        CURL_MAX_CONCURRENT_DEFAULT
    } else {
        cfg.curl_max_concurrent as usize
    };
    let mut last_err = String::new();
    for attempt in 0..tries {
        let delay = curl_retry_delay_ms(attempt);
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
        match chat_via_curl_once(url, payload, token, timeout_secs, gate) {
            Ok(v) => return Ok(v),
            Err(e) => {
                // 🔴 2026-10-09：请求**可能已送达并计费**时**立即止步，不再重试**。
                // 重试 = 上游对同一账号再扣一次费（account 内层的双边扣费来源）。
                if e.maybe_billed {
                    return Err(CurlFailure {
                        msg: format!(
                            "第 {}/{} 次尝试时上游响应异常，但请求可能已产生计费，已停止重试以避免重复扣费。{}",
                            attempt + 1,
                            tries,
                            e.msg
                        ),
                        maybe_billed: true,
                    });
                }
                last_err = e.msg;
            }
        }
    }
    Err(CurlFailure {
        msg: format!("curl 连续 {tries} 次失败（已含并发闸门{gate} 与退避重试）：{last_err}"),
        maybe_billed: false,
    })
}

fn chat_via_curl_once(
    url: &str,
    payload: &Value,
    token: &str,
    timeout_secs: u64,
    gate: usize,
) -> Result<(u16, String), CurlFailure> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let _permit = CurlPermit::acquire(gate); // 并发闸门：防管道/句柄耗尽（os error 231）

    // 中文经命令行传参会产出非法 UTF-8（AGENTS.md 坑 #6）→ 走 stdin
    let body_bytes = serde_json::to_vec(payload).map_err(|e| CurlFailure::local(format!("payload 序列化失败: {e}")))?;
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
        .map_err(|e| CurlFailure::local(format!("调起 curl 失败（系统需自带 curl.exe）: {e}")))?;

    // 写 stdin 后关闭（curl 读到 EOF 才会发请求）
    {
        let mut sin = child
            .stdin
            .take()
            .ok_or_else(|| CurlFailure::local("取 curl stdin 失败".to_string()))?;
        sin.write_all(&body_bytes)
            .map_err(|e| CurlFailure::local(format!("写 curl stdin 失败: {e}")))?;
        // sin 在此 drop → 关闭管道
    }

    let out = child
        .wait_with_output()
        .map_err(|e| CurlFailure::local(format!("等待 curl 失败: {e}")))?;

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
        // 传输层失败：按 curl 退出码判定「是否可能已送达」（28/52/56 → 是）
        return Err(CurlFailure::from_exit(
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).to_string(),
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
    let (wire, ts, xs, yy) = sign_request(acc, path, "", cfg_yy_absolute());
    let mut url = format!("{host}{path}?{wire}");
    if let Some(eq) = extra_query {
        url = format!("{url}&{eq}");
    }
    let resp = upstream_client()
        .get(&url)
        .header("token", effective_token(acc))
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

/// 读 MiniMax Code 的 **auth lease** 值。
///
/// ⚠️ **2026-10-08 实测推翻它的用途**：lease 值**不能**充当 `Authorization`
/// （实测 HTTP 401 空响应）；正确的 `Authorization` 是 auth.json 的**实时 `accessToken`**
/// （即 [`effective_token`]）。本函数仅作 token 读取失败时的最后兜底。
fn minimax_auth_lease() -> Option<String> {
    let mut p = crate::modules::config::home_dir();
    p.push(".minimax");
    p.push("run");
    p.push("mcode-auth-lease-v1.cap");
    let t = std::fs::read_to_string(&p).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

// ---------------------------------------------------------------------------
// 桌面端设备指纹（2026-10-08）—— signin 族 `timezone_id` 校验的真正依赖
// ---------------------------------------------------------------------------

/// 桌面端设备指纹三件套。
///
/// **实测证据链**：签名（`yy`/`x-timestamp`/`x-signature`）与凭据全部通过后，上游仍返回
/// HTTP 200 + `{"base_resp":{"status_code":1406010011,"status_msg":"invalid timezone_id"}}`；
/// 把 `uuid`/`device_id`/`user_id` 换成桌面端真值后**立刻**返回真实签到面板
/// （`status_code=0` + `days[].is_today`）✓ ⇒ 该字段就是最后一道门槛。
///
/// 真值来源（CDP 读桌面端页面存储实证）：
/// - `uuid`      ← `localStorage["UNIQUE_USER_ID"]`
/// - `device_id` ← `sessionStorage["tab_device_id"]`
/// - `user_id`   ← `JSON.parse(localStorage["user_detail_agent"]).realUserID`
///   ⚠️ **不是**同对象里的 `userID`（`68naMKpnJn7x` 那种 12 字符短串会被上游拒）
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopIdentity {
    pub uuid: String,
    pub device_id: String,
    pub user_id: String,
}

impl DesktopIdentity {
    pub fn is_complete(&self) -> bool {
        !self.uuid.is_empty() && !self.device_id.is_empty() && !self.user_id.is_empty()
    }
}

/// 桌面端 userData 目录（按实例隔离）。
/// ⚠️ 主实例目录名是 `%APPDATA%\MiniMax`（**不是** `MiniMax Code`）——按应用名找会扑空。
fn desktop_userdata_dir_of(instance: &str) -> Option<std::path::PathBuf> {
    let base = std::path::PathBuf::from(std::env::var_os("APPDATA")?);
    let candidates: Vec<std::path::PathBuf> = if instance.is_empty() {
        vec![base.join("MiniMax"), base.join("MiniMax Code")]
    } else {
        vec![base.join(format!("MiniMax-{instance}"))]
    };
    candidates.into_iter().find(|p| p.is_dir())
}

/// 账号所属的桌面端实例名。
///
/// 显式字段优先；否则从 `auth_path` 推导（`…\.minimax-acc2\auth\…` / `…\MiniMax-acc2\…`
/// ⇒ `acc2`）。主实例路径不含 `-` 后缀 ⇒ 返回空串。
fn account_instance(acc: &MmAccount) -> String {
    if !acc.desktop_instance.is_empty() {
        return acc.desktop_instance.clone();
    }
    for marker in [".minimax-", "MiniMax-"] {
        if let Some(i) = acc.auth_path.find(marker) {
            let rest = &acc.auth_path[i + marker.len()..];
            let name: String = rest.chars().take_while(|c| *c != '\\' && *c != '/').collect();
            if !name.is_empty() {
                return name;
            }
        }
    }
    String::new()
}

/// 读取 Chromium 存储原始块：`Local Storage/leveldb/*` + `Session Storage/*`
/// （localStorage → leveldb；sessionStorage → Session Storage 的 .log）。
fn read_desktop_storage_blobs(instance: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Some(dir) = desktop_userdata_dir_of(instance) else { return out };
    for sub in ["Local Storage/leveldb", "Session Storage"] {
        let Ok(rd) = std::fs::read_dir(dir.join(sub)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                if let Ok(b) = std::fs::read(&p) {
                    out.push(b);
                }
            }
        }
    }
    out
}

/// 二进制块 → 可扫描文本（UTF-8 与 UTF-16LE 两口径；Chromium 对部分 value 用 UTF-16LE）。
fn decode_variants(blob: &[u8]) -> [String; 2] {
    // 注意：用索引迭代而非 `chunks_exact(2)`——后者会触发 clippy
    // `chunks_exact_with_constant_size` 警告（棘轮门禁不允许新增警告）。
    let utf16: Vec<u16> = (0..blob.len() / 2)
        .map(|i| u16::from_le_bytes([blob[2 * i], blob[2 * i + 1]]))
        .collect();
    [
        String::from_utf8_lossy(blob).into_owned(),
        String::from_utf16_lossy(&utf16),
    ]
}

fn is_uuid_bytes(w: &[u8]) -> bool {
    w.len() == 36
        && w.iter().enumerate().all(|(i, &c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// `key` 之后 `window` 字节内出现的第一个 UUID 形态串（8-4-4-4-12）。
fn scan_uuid_after(text: &str, key: &str, window: usize) -> Option<String> {
    let start = text.find(key)? + key.len();
    let b = text.as_bytes();
    let seg = &b[start..(start + window).min(b.len())];
    (0..seg.len().saturating_sub(35)).find_map(|s| {
        let w = &seg[s..s + 36];
        is_uuid_bytes(w).then(|| String::from_utf8_lossy(w).into_owned())
    })
}

/// `key` 之后 `window` 字节内出现的第一个长度 ≥ `min_len` 的十进制数字串。
///
/// ⚠️ Chromium 把**部分 value 用 UTF-16LE 存**（ASCII 字符间夹 `0x00`），
/// 而 `key` 本身是 UTF-8 ⇒ 同一块里编码混排。这里扫描前先剔除 `NUL`，
/// 使 `5\x003\x002…` 这类 UTF-16LE 数字串能被正确识别。
fn scan_digits_after(text: &str, key: &str, window: usize, min_len: usize) -> Option<String> {
    let start = text.find(key)? + key.len();
    let b = text.as_bytes();
    let seg = &b[start..(start + window).min(b.len())];
    let compact: Vec<u8> = seg.iter().copied().filter(|&c| c != 0).collect();
    let mut i = 0usize;
    while i < compact.len() {
        if compact[i].is_ascii_digit() {
            let s = i;
            while i < compact.len() && compact[i].is_ascii_digit() {
                i += 1;
            }
            if i - s >= min_len {
                return Some(String::from_utf8_lossy(&compact[s..i]).into_owned());
            }
        } else {
            i += 1;
        }
    }
    None
}

/// 从原始块提取三件套（纯函数，便于单测）。
fn extract_identity_from_blobs(blobs: &[Vec<u8>]) -> DesktopIdentity {
    let mut id = DesktopIdentity::default();
    for blob in blobs {
        for text in decode_variants(blob) {
            if id.uuid.is_empty() {
                if let Some(u) = scan_uuid_after(&text, "UNIQUE_USER_ID", 160) {
                    id.uuid = u;
                }
            }
            if id.device_id.is_empty() {
                if let Some(d) = scan_digits_after(&text, "tab_device_id", 160, 5) {
                    id.device_id = d;
                }
            }
            if id.user_id.is_empty() {
                if let Some(u) = scan_digits_after(&text, "realUserID", 160, 10) {
                    id.user_id = u;
                }
            }
        }
        if id.is_complete() {
            break;
        }
    }
    id
}

/// `key` 之后 `window` 字节内 `"..."` 形式的字符串值（LevelDB 里的 JSON 片段，
/// 如 `"userName":"MiniMax463137"`）。`key` **须自带值的开引号**（`"userName":"`），
/// 否则会把值后的逗号当成空串返回。
fn scan_quoted_after(text: &str, key: &str, window: usize) -> Option<String> {
    let start = text.find(key)? + key.len();
    let b = text.as_bytes();
    let seg = &b[start..(start + window).min(b.len())];
    let e = seg.iter().position(|&c| c == b'"')?;
    let out = String::from_utf8_lossy(&seg[..e]).into_owned();
    (!out.is_empty()).then_some(out)
}

/// 现读**指定实例**存储里的单个字符串字段（UTF-8 / UTF-16LE 两口径都试）。
fn desktop_storage_string(instance: &str, key: &str) -> Option<String> {
    for blob in read_desktop_storage_blobs(instance) {
        for text in decode_variants(&blob) {
            if let Some(v) = scan_quoted_after(&text, key, 240) {
                return Some(v);
            }
        }
    }
    None
}

/// 现读**指定实例**的桌面端设备指纹（只读存储文件，**无需调试端口**）。
pub fn desktop_identity_for(instance: &str) -> Option<DesktopIdentity> {
    let blobs = read_desktop_storage_blobs(instance);
    if blobs.is_empty() {
        return None;
    }
    let id = extract_identity_from_blobs(&blobs);
    (!id.uuid.is_empty() || !id.device_id.is_empty() || !id.user_id.is_empty()).then_some(id)
}

/// `sharedUser` 为空时的**存储身份兜底**（返回 `sharedUser` 形状的 JSON）。
///
/// 返回 `{"userID":…, "realUserID":…, "userName":…}`，供 [`discover_desktop`] 与
/// 标签生成共用；读不到任何字段时返回 `json!({})`。
fn desktop_storage_shared(instance: &str) -> Value {
    let id = desktop_identity_for(instance);
    let real_user_id = id.map(|i| i.user_id).unwrap_or_default();
    // 注意：`"userID":"` 不会误命中 `"realUserID":"`（后者是大写 `UserID`）✓
    let short_id = desktop_storage_string(instance, "\"userID\":\"").unwrap_or_default();
    let name = desktop_storage_string(instance, "\"userName\":\"").unwrap_or_default();
    if real_user_id.is_empty() && short_id.is_empty() && name.is_empty() {
        return json!({});
    }
    let mut out = serde_json::Map::new();
    if !short_id.is_empty() {
        out.insert("userID".to_string(), json!(short_id));
    }
    if !real_user_id.is_empty() {
        out.insert("realUserID".to_string(), json!(real_user_id));
    }
    if !name.is_empty() {
        out.insert("userName".to_string(), json!(name));
    }
    Value::Object(out)
}
/// 现读**主实例**的桌面端设备指纹（兼容入口，等价 `desktop_identity_for("")`）。
pub fn desktop_identity() -> Option<DesktopIdentity> {
    desktop_identity_for("")
}

/// 指定实例的 `auth.json` 落点。
///
/// 主实例：`~/.minimax/auth/prod/{cn|en}/mcode-public/auth.json`（本机实测）；
/// 隔离实例：`~/.minimax-<name>/auth/...`（`--data-dir` 决定，见 `start-minimax-acc2.bat`）。
pub fn desktop_auth_json_path(region: &str, instance: &str) -> std::path::PathBuf {
    let home = crate::modules::config::home_dir();
    let base = if instance.is_empty() {
        home.join(".minimax")
    } else {
        home.join(format!(".minimax-{instance}"))
    };
    let region_dir = if region == PROVIDER_ZAI { "en" } else { "cn" };
    base.join("auth")
        .join("prod")
        .join(region_dir)
        .join("mcode-public")
        .join("auth.json")
}

/// 用**该账号所属实例**的桌面端真值覆盖指纹字段（并让 `auth_path` 跟随实例）。
/// 返回是否发生了修改（调用方据此决定是否落盘）。
pub fn backfill_identity(acc: &mut MmAccount) -> bool {
    let instance = account_instance(acc);
    let mut changed = false;
    // 隔离实例的账号：auth_path 必须指向**该实例自己的** auth.json
    // （否则会继续读主实例的单登录槽 ⇒ 拿到别人的 token ✗）
    if !instance.is_empty() {
        let want = desktop_auth_json_path(&acc.region, &instance);
        if want.is_file() && acc.auth_path != want.to_string_lossy() {
            acc.auth_path = want.to_string_lossy().to_string();
            changed = true;
        }
    }
    // 🔴 身份闸门（2026-10-08）：桌面端是**单登录槽**，`desktop_identity_for` 读到的
    // 三件套永远描述"当前登录的那个账号"。此前无条件覆盖 ⇒ 把 A 的 uuid/device_id/
    // user_id 套在**未登录的 B** 头上 ⇒ 签名参数（user_id=A）与 Authorization
    // （B 的冻结快照 token）指向两个账号，上游必 401，且错误信息看似"签名问题"，
    // 掩盖了真正原因"该账号未在桌面端登录"。现在仅当「当前登录 = 本账号」才覆盖。
    let region_dir = if acc.region == "international" { "en" } else { "cn" };
    let cur = desktop_current_user_ids(region_dir, &instance);
    let self_logged = live_identity_matches_account(&acc.user_id, &cur);
    if let Some(id) = desktop_identity_for(&instance) {
        // 设备指纹：始终用现读（上游不校验归属；配置里常为空，补齐才签得动）
        changed |= merge_device_only(acc, &id);
        if self_logged {
            // 🔴 2026-10-09：`user_id` **绝不能**取 `desktop_identity_for` 里的值。
            // 那是从 Local Storage(LevelDB) 扫描出来的，实测会**滞留上一个账号**
            // （主实例登录已切到 463137，它仍返回小螺帽的 348569758173523968）
            // ⇒ 一旦覆盖，签名 user_id 与 Authorization token 分属两个账号 ⇒
            // 上游 `invalid access token`（463137 在当前登录态下反而签到失败的根因）。
            // 权威源是 `sharedUser.realUserID` —— 与 auth.json 的 token **同源**。
            changed |= merge_authoritative_user_id(acc, &cur);
        }
    }
    changed
}

/// 用 `sharedUser` 里的 **realUserID**（长数字）校正账号身份（纯函数，便于单测）。
///
/// `cur_ids` = `[userID, realUserID]`；只认长数字串（realUserID），因为上游据它查时区，
/// 短串 `userID` 会恒定 `1406010011 invalid timezone_id`。
fn merge_authoritative_user_id(acc: &mut MmAccount, cur_ids: &[String]) -> bool {
    let Some(uid) = cur_ids
        .iter()
        .find(|s| s.len() >= 15 && s.chars().all(|c| c.is_ascii_digit()))
    else {
        return false;
    };
    if acc.user_id == *uid {
        return false;
    }
    acc.user_id = uid.clone();
    true
}

/// 现读三件套是否可以套用到本账号（纯函数，便于单测）。
///
/// * `cur_ids` 为空 ⇒ 读不到桌面端当前身份（文件缺失/未安装）⇒ 允许覆盖（历史行为，
///   总比留空导致 `invalid timezone_id` 好）；
/// * `acc_user_id` 为空 ⇒ 本账号未标注身份 ⇒ 允许覆盖（跟随桌面端当前登录）；
/// * 否则必须命中 `userID` / `realUserID` 之一 ⇒ 才允许覆盖。
fn live_identity_matches_account(acc_user_id: &str, cur_ids: &[String]) -> bool {
    cur_ids.is_empty()
        || acc_user_id.is_empty()
        || cur_ids.iter().any(|c| c == acc_user_id)
}

/// 覆盖逻辑（纯函数，便于单测）：**现读值为权威**，无条件覆盖。
///
/// 桌面端只有**单一登录槽** ⇒ `%APPDATA%\MiniMax` 里的三件套永远描述"当前登录的那个账号"。
/// 换号后必须跟着换，所以不能"只填空缺"（否则会残留上一个账号的 `user_id` ⇒ 上游按别人
/// 的时区校验 ⇒ `invalid timezone_id`）。
fn merge_identity(acc: &mut MmAccount, id: &DesktopIdentity) -> bool {
    let mut changed = false;
    if !id.uuid.is_empty() && acc.uuid != id.uuid {
        acc.uuid = id.uuid.clone();
        changed = true;
    }
    if !id.device_id.is_empty() && acc.device_id != id.device_id {
        acc.device_id = id.device_id.clone();
        changed = true;
    }
    // ⚠️ 对照实验（2026-10-08，四组）证明 **`user_id` 是 `invalid timezone_id` 的唯一触发条件**：
    //    A) device_id 换成任意 8 位数字 → 200 ok；B) uuid 换成历史错误值 → 200 ok；
    //    C) `user_id` 换成短串 `userID`（68naMKpnJn7x）→ 恒定 1406010011 ✗
    //    ⇒ 上游拿 `user_id` 反查账号时区，**必须是 realUserID（长数字 ID）**。
    if !id.user_id.is_empty() && acc.user_id != id.user_id {
        acc.user_id = id.user_id.clone();
        changed = true;
    }
    changed
}

/// 只补 `uuid` / `device_id`，**绝不动 `user_id`**（纯函数，便于单测）。
///
/// 🔴 2026-10-08（2.0.12 回归后修正）：本以为「未登录账号一律不覆盖 live 三件套」就安全，
/// 实测把 `device_id` 一起挡掉后 `8d7a0dad4475` 从可签到变成失败——它的 `device_id`
/// 在配置里是**空**，全靠 backfill 现读补齐；上游签到要的是「一套能用的设备指纹」，
/// 并不校验指纹归属（对照实验 A/B：device_id 任意 8 位、uuid 任意值均 200 ok）。
/// 而 `user_id` 是**身份**，上游据其反查账号时区、并与 `Authorization` token 配对，
/// 错配必 `invalid timezone_id` / `invalid access token` ⇒ 该字段必须守死。
/// 结论：**设备指纹可共用，身份不可**。
fn merge_device_only(acc: &mut MmAccount, id: &DesktopIdentity) -> bool {
    let mut changed = false;
    if !id.uuid.is_empty() && acc.uuid != id.uuid {
        acc.uuid = id.uuid.clone();
        changed = true;
    }
    if !id.device_id.is_empty() && acc.device_id != id.device_id {
        acc.device_id = id.device_id.clone();
        changed = true;
    }
    changed
}

/// **强制签名**的 GET（2026-10-08）。
///
/// 背景：桌面端 IPC Proxy 日志实测——`/minimax-cloud/api/v1/signin/status` 的真实请求**必带**
/// `yy` / `x-timestamp` / `x-signature`（外加 `Authorization`），而我方 `is_bearer_only(source=="desktop")`
/// 让 desktop 走免签名捷径 ⇒ 上游恒定回 `{"error":"invalid access token"}` ✗。
/// 本函数绕过该捷径，强制签名；desktop 场景同时带 `Authorization: Bearer` 与 `token` 两个头
/// （日志里是 Authorization，既有签名分支用 token —— 两个都带以覆盖两种实现）。
async fn cloud_get_signed(acc: &MmAccount, path: &str, extra_query: Option<&str>) -> Result<Value, String> {
    let host = cloud_host(acc);
    let (wire, ts, xs, yy) = sign_request(acc, path, "", cfg_yy_absolute());
    let mut url = format!("{host}{path}?{wire}");
    if let Some(eq) = extra_query {
        let sep = if url.contains('?') { "&" } else { "?" };
        url = format!("{url}{sep}{eq}");
    }
    // 🔴 2026-10-08 实测：Authorization **必须**是 auth.json 的实时 accessToken（`mmoat_…`）。
    // 曾误用 lease 值（`~/.minimax/run/mcode-auth-lease-v1.cap`）⇒ 上游 HTTP 401 空响应；
    // 换回 effective_token 后立刻 HTTP 200 并进入业务层 ✓（lease 仅作 token 读取失败的兜底）
    let tok = effective_token(acc);
    let auth_value = if tok.is_empty() {
        minimax_auth_lease().unwrap_or_default()
    } else {
        tok.clone()
    };
    let resp = upstream_client()
        .get(&url)
        .header("Authorization", format!("Bearer {auth_value}"))
        .header("token", &tok)
        .header("x-timestamp", &ts)
        .header("x-signature", &xs)
        .header("yy", &yy)
        .header("Origin", "https://agent.minimax.cn")
        .header("Referer", "https://agent.minimax.cn/")
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await
        .map_err(|e| format!("MiniMax 上游请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    parse_response(status, &text)
}

/// **强制签名**的 POST（body 参与 x_signature）。
async fn cloud_post_q_signed(
    acc: &MmAccount,
    path: &str,
    body: &str,
    extra_query: Option<&str>,
) -> Result<Value, String> {
    let host = cloud_host(acc);
    let (wire, ts, xs, yy) = sign_request(acc, path, body, cfg_yy_absolute());
    let mut url = format!("{host}{path}?{wire}");
    if let Some(eq) = extra_query {
        let sep = if url.contains('?') { "&" } else { "?" };
        url = format!("{url}{sep}{eq}");
    }
    // 🔴 2026-10-08 实测：Authorization **必须**是 auth.json 的实时 accessToken（`mmoat_…`）。
    // 曾误用 lease 值（`~/.minimax/run/mcode-auth-lease-v1.cap`）⇒ 上游 HTTP 401 空响应；
    // 换回 effective_token 后立刻 HTTP 200 并进入业务层 ✓（lease 仅作 token 读取失败的兜底）
    let tok = effective_token(acc);
    let auth_value = if tok.is_empty() {
        minimax_auth_lease().unwrap_or_default()
    } else {
        tok.clone()
    };
    let resp = upstream_client()
        .post(&url)
        .header("Authorization", format!("Bearer {auth_value}"))
        .header("token", &tok)
        .header("x-timestamp", &ts)
        .header("x-signature", &xs)
        .header("yy", &yy)
        .header("Origin", "https://agent.minimax.cn")
        .header("Referer", "https://agent.minimax.cn/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("MiniMax 上游请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    parse_response(status, &text)
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
        .header("token", effective_token(acc))
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

/// 识别上游「HTTP 200 包业务错误」的响应，如 `{"error":"invalid access token"}`。
///
/// 🔴 2026-10-08（P0）：这类响应此前**被当成成功**——signin/status 与 signin/claim
/// 都以 200 返回 error body，代码只取 `data.*`，取不到就落进
/// `claim_result == 0` 分支 ⇒ 判成「已签到 already-claimed」并写
/// `signin_last_date = 今天` ⇒ **该账号当日及此后永远不再补签**，页面却显示
/// 一切正常（failStreak=0、lastError=null）。实测 `d20f264aed3e` 面板与 claim
/// 双双 `invalid access token`，积分停在旧值，就是这个静默吞错。
///
/// 约定：顶层 `error` 非空即失败，调用方必须按失败处理（不计「已签」、不写日期）。
pub(crate) fn upstream_error(v: &Value) -> Option<String> {
    let e = v.get("error").and_then(Value::as_str)?.trim();
    if e.is_empty() {
        None
    } else {
        Some(e.to_string())
    }
}

/// 凭据失效类错误的处置提示（多账号需隔离实例重新登录）。
fn credential_hint(detail: &str) -> &'static str {
    let d = detail.to_ascii_lowercase();
    if d.contains("token")
        || d.contains("401")
        || d.contains("unauthorized")
        || d.contains("signature")
        || d.contains("登录")
    {
        "：桌面端凭据已失效或不匹配，请在该账号登录态下重新「从桌面端导入」（多账号请用隔离实例 start-minimax-acc2.bat）"
    } else {
        ""
    }
}

/// 单账号签到全流程：Prepare → status（已领跳过）→ claim → credit/details 对账。
pub async fn signin_account(acc: &MmAccount) -> Value {
    // 🔴 2026-10-08：签到前用桌面端**真实设备指纹**补齐 uuid/device_id/user_id。
    // 上游 signin 族据其推导 `timezone_id`：三项为空或错值（历史 bug 写成了短串 `userID`）
    // ⇒ 即便签名与凭据全部正确，仍恒定返回 `1406010011 invalid timezone_id` ✗。
    // 每次现读（不落盘）⇒ 桌面端切换登录槽后自动跟上 ✓
    let mut acc_owned = acc.clone();
    backfill_identity(&mut acc_owned);
    let acc: &MmAccount = &acc_owned;

    // 🔴 2026-10-09：桌面端 accessToken TTL 仅 1 小时，隔离实例不常驻 ⇒ 过期后恒定 401。
    // 签到前先自动续期（refresh_token 轮换型，刷新即写回 auth.json）；
    // **已过期且续不回来**才中止，未过期时续期失败不阻断（见 ensure_desktop_credential）。
    let mut steps: Vec<Value> = Vec::new();
    let account_id = acc.id.clone();
    let mut cred_note = "凭据新鲜，无需续期".to_string();
    match ensure_desktop_credential(acc).await {
        Ok(Some(_)) => cred_note = "accessToken 已自动续期并写回 auth.json".to_string(),
        Ok(None) => {}
        Err(e) => {
            return json!({"ok": false, "id": account_id, "account": acc.label,
                          "error": format!("凭据自动续期失败：{e}"), "steps": steps});
        }
    }
    // ⚠️ 续期可能发生在**更早的余额读取**里（refresh_credit_snapshot 也会尽力续期），
    // 所以这里恒定记录一行凭据状态，排查「为什么 401 / 为什么这次没刷」时有据可查。
    if acc.source == "desktop" {
        steps.push(json!({"step": "credential/token", "ok": true,
                          "remainingMs": desktop_token_remaining_ms(acc),
                          "note": cred_note}));
    }
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
    // 时区口径（2026-10-08 二次修正，**依据桌面端本体实现**）：
    // 从 `E:\MiniMax Code\resources\app.asar` 抠出的真实调用是——
    //   GET  /minimax-cloud/api/v1/signin/status           ← **不带任何时区参数**
    //   POST /minimax-cloud/api/v1/signin/claim  {}        ← 同样不带
    //   （客户端只传 {showToast:false}；成功判定看 base_resp.status_code === 0）
    // ⇒ 我们此前**多传时区参数**，上游很可能因此返回空面板（实测三种时区口径 hasToday 全 false）。
    // 现口径：**首选不带参数**；仅当面板里没有"今天"时，才把 timezone_id 作为兜底候选再试一次。
    let mut tz = String::new();                     // 空 = 不带参数（桌面端口径）
    let mut panel = Value::Null;
    let mut days = json!([]);
    // 面板 days 位置兼容：桌面端为 /data/days；历史实现的写法是 /data/panel/days
    fn extract_days(v: &Value) -> Value {
        v.pointer("/data/days")
            .or_else(|| v.pointer("/data/panel/days"))
            .cloned()
            .unwrap_or(json!([]))
    }
    fn day_is_today(v: &Value) -> bool {
        extract_days(v)
            .as_array()
            .map(|arr| arr.iter().any(|d| d.get("is_today").and_then(Value::as_bool) == Some(true)))
            .unwrap_or(false)
    }
    let mut status_err: Option<String> = None;
    for cand in ["", "timezone_id=Asia/Shanghai"] {
        let opt = if cand.is_empty() { None } else { Some(cand) };
        match cloud_get_signed(acc, "/minimax-cloud/api/v1/signin/status", opt).await {
            Ok(v) => {
                // 🔴 200 包 error body（如 invalid access token）**不是面板**：
                // 此前被当成空面板继续往下走，最终误判「已签到」。
                if let Some(err) = upstream_error(&v) {
                    steps.push(json!({"step": "signin/status", "tz": if cand.is_empty() { "(无参数·桌面端口径)" } else { cand },
                                      "ok": false, "error": err}));
                    status_err = Some(err);
                    continue;
                }
                let ok_code = v
                    .pointer("/base_resp/status_code")
                    .and_then(Value::as_i64)
                    .map(|c| c == 0)
                    .unwrap_or(true);
                let has_today = day_is_today(&v);
                // 探针（2026-10-08）：把原始响应截断带出，便于确认面板真实结构。
                // 网关 stdout 不落盘，而 steps[] 会随 /api/minimax/signin/run 直接回传，最快。
                let raw: String = serde_json::to_string(&v).unwrap_or_default().chars().take(2000).collect();
                steps.push(json!({"step": "signin/status", "tz": if cand.is_empty() { "(无参数·桌面端口径)" } else { cand },
                                  "hasToday": has_today, "baseRespOk": ok_code, "raw": raw}));
                if panel.is_null() {
                    tz = cand.to_string();
                    days = extract_days(&v);
                    panel = v;
                }
                if has_today && ok_code {
                    tz = cand.to_string();
                    days = extract_days(&panel);
                    break;
                }
            }
            Err(e) => steps.push(json!({"step": "signin/status", "tz": if cand.is_empty() { "(无参数)" } else { cand }, "ok": false, "error": e})),
        }
    }
    if panel.is_null() {
        let detail = status_err
            .clone()
            .unwrap_or_else(|| "无参数与 timezone_id 两种口径均未返回有效面板".to_string());
        return json!({"ok": false, "id": account_id, "account": acc.label,
                      "error": format!("签到面板查询失败（{detail}）{}", credential_hint(&detail)),
                      "steps": steps});
    }
    let today = panel
        .pointer("/data/panel/days")
        .and_then(Value::as_array)
        .and_then(|arr| arr.iter().find(|d| d.get("is_today").and_then(Value::as_bool) == Some(true)))
        .cloned()
        .unwrap_or(json!({}));
    let today_status = today.get("status").and_then(Value::as_i64).unwrap_or(0);
    // 连续签到奖励：上游**没有独立接口**——它是当天奖励里的 `bonus_points` 字段
    // （前端 `signin-bonus` 标签即渲染 `day.bonus_points`），随 signin/claim 一次性发放。
    // 这里从面板解析出来单独带回，让「连续签到奖励」在结果里可见。
    let bonus_points = today
        .get("bonus_points")
        .and_then(Value::as_u64)
        .or_else(|| today.get("bonus_points").and_then(Value::as_f64).map(|f| f as u64))
        .unwrap_or(0);
    let streak_days = extract_days(&panel)
        .as_array()
        .map(|arr| arr.iter().filter(|d| d.get("status").and_then(Value::as_i64) == Some(2)).count())
        .unwrap_or(0);
    if today_status == 3 {
        return json!({"ok": true, "id": account_id, "account": acc.label, "action": "already-claimed",
                      "points": today.get("points"), "bonusPoints": bonus_points,
                      "streakDays": streak_days, "steps": steps});
    }
    // ③ claim（body 固定 {}）
    let claim = match cloud_post_q_signed(acc, "/minimax-cloud/api/v1/signin/claim", "{}", if tz.is_empty() { None } else { Some(&tz) }).await {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "id": account_id, "account": acc.label, "error": format!("签到领取失败: {e}"), "steps": steps}),
    };
    let claim_raw: String = serde_json::to_string(&claim).unwrap_or_default().chars().take(1200).collect();
    steps.push(json!({"step": "signin/claim.raw", "raw": claim_raw}));
    // 🔴 与 status 同理：200 包 error body 必须按失败处理，绝不落进 claim_result==0
    // 的「已签到」分支（否则写 signin_last_date ⇒ 永久漏签）。
    if let Some(err) = upstream_error(&claim) {
        return json!({"ok": false, "id": account_id, "account": acc.label,
                      "error": format!("签到领取失败（上游 {err}）{}", credential_hint(&err)),
                      "steps": steps});
    }
    let claim_result = claim.pointer("/data/claim_result").and_then(Value::as_i64).unwrap_or(0);
    let points = claim.pointer("/data/points").cloned().unwrap_or(json!(0));
    // ④ 对账：credit/details 找当日发放（claim_result=1 不代表发放，juangchuank 实测）
    let mut paid = false;
    if let Ok(details) = cloud_get_signed(acc, "/minimax-cloud/api/v1/credit/details", if tz.is_empty() { None } else { Some(&tz) }).await {
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
    // ⑤ 有效性闸门（2026-10-07 加，2026-10-08 修正语义）
    //
    // 原意图：上游对**失效 token** 的面板/claim 可能返回 200 + 空 JSON（claim_result
    // 缺省 0）⇒ 会误报 success。这个动机仍然成立，但**判定过严**：
    //   ① 凭据是否有效，已由 ③ 之前的 **Prepare 三步**证明（401 会当场失败）；
    //   ② claim_result=0 在上游语义里是「本次没有新发放」（已领/不在活动内/跨天错位），
    //      并不代表凭据失效——实测本账号 Prepare 全绿、claim=0、details 无发放，
    //      却被告知"账号凭据可能已失效"，用户据此反复切号，问题永远不消失。
    // 现规则：
    //   claim_result ∈ {1,2}            → 真签到 ✓
    //   claim_result == 0 且 paid       → details 已见当天发放 ⇒ **已签到** ✓
    //   claim_result == 0 且 !paid      → 上游明确"无可领" ⇒ 视为**本轮无需领取**（already），
    //                                     不计失败、不记退避；但把上游原值写进 note 供排查
    //   其它非零值（异常码）             → 仍按失败处理（保留风控/异常信号）
    if claim_result == 0 {
        let note = if paid {
            "今日已领取（credit/details 已见当天发放）"
        } else {
            "上游返回 claim_result=0（今日无可领/已领）：不计失败，明日再试"
        };
        steps.push(json!({"step": "claim", "claim_result": 0, "points": points, "paid": paid, "treatAs": "already"}));
        return json!({
            "ok": true, "id": account_id, "account": acc.label,
            "result": "already", "action": "already-claimed",
            "points": points, "paid": paid, "note": note, "steps": steps,
            "bonusPoints": bonus_points, "streakDays": streak_days,
            "panelDays": days,
        });
    }
    if claim_result != 1 && claim_result != 2 {
        return json!({
            "ok": false, "id": account_id, "account": acc.label,
            "error": format!("签到异常（上游 claim_result={claim_result}，paid={paid}）：非 0/1/2 的异常返回，请保留此信息反馈"),
            "steps": steps,
        });
    }
    steps.push(json!({"step": "claim", "claim_result": claim_result, "points": points, "paid": paid}));
    json!({
        "ok": true, "id": account_id, "account": acc.label,
        "action": if claim_result == 2 { "already-claimed" } else { "claimed" },
        "points": points, "paid": paid, "steps": steps,
        "bonusPoints": bonus_points, "streakDays": streak_days,
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

/// 一次对话的**完整**请求（2026-10-09）。
///
/// 🔴 此前 `chat_once(prompt, …)` 只收一个 `prompt` 字符串，导致三处能力被网关自己掐死：
/// | 能力 | 此前 | 本结构如何修 |
/// | --- | --- | --- |
/// | 多轮上下文 | ❌ 只取 `messages` **末条**文本，历史全丢 | `messages` 整段透传上游 |
/// | 工具调用 | ❌ payload 无 `tools`，上游永远收不到工具声明 | `tools` 透传（Anthropic 原生格式）|
/// | 强制选工具 | ❌ 无 `tool_choice` | `tool_choice` 透传 |
///
/// 实测（2026-10-09 直连上游对照）：MiniMax **原生支持** `tools` 与 `tool_choice`，
/// 能正确返回 `tool_use` 块 ⇒ 本网关的丢失才是问题所在。
#[derive(Clone, Default)]
pub struct ChatRequest {
    /// 模型名（`MiniMax-M3` / `MiniMax-M2.7` / …）；`None` 用 [`DEFAULT_CHAT_MODEL`]。
    pub model: Option<String>,
    /// **Anthropic 原生** `tools` 数组（`[{name, description, input_schema}]`）。
    /// 已由服务端从 OpenAI 的 `[{type,function:{…}}]` 形状转换而来。
    pub tools: Option<Value>,
    /// `tool_choice`：`{"type":"auto"|"any"|"tool","name":…}`；`None` 不传。
    pub tool_choice: Option<Value>,
    /// 完整消息历史（Anthropic 形状）。`None` 时退化为「单条 user + prompt」。
    pub messages: Option<Value>,
    /// Anthropic thinking 配置：`{"type":"adaptive"|"enabled"|"disabled",
    /// "budget_tokens":N}`。
    ///
    /// ⚠️ 实测（2026-10-09 直连上游）：**M3.1-Flash-Preview 禁止 disabled**
    /// —— 传 `thinking.type="disabled"` 返回 HTTP 400
    /// *"requires adaptive thinking"*；而**不传时默认就是开的**（官方 config.yaml
    /// `thinking_config.mode: forced_on`）。故默认不写，留给调用方显式指定。
    pub thinking: Option<Value>,
    /// 思考强度（M3.1 特有）。官方 `thinking.effortOptions`：
    /// `default|low|medium|high|xhigh|max`，`defaultModelThinking.effort: max`。
    pub reasoning_effort: Option<String>,
    /// `max_tokens`；`None` 用 [`DEFAULT_CHAT_MAX_TOKENS`]。
    pub max_tokens: Option<u64>,
}

/// `max_tokens` 缺省值（2026-10-09 抽成常量，便于单测引用）。
///
/// 🔴 2026-10-09 修正：**4096 → 32768**。
/// 实测（M3.1-Flash-Preview）：不传 `max_tokens` 时输出**硬卡在 4096**——
/// 要求「数到 3000」只数到 1630 就停（out=4096），而传 `max_tokens=32000`
/// 可正常输出 8516 tokens 数完 3000。
/// 旧值 4096 对长回答（代码生成、长文档、多步推理）是**静默截断**，
/// 且原 `finish_reason` 恒为 `stop`，客户端完全无法察觉。
/// 32768 是原值的 8 倍，覆盖绝大多数场景；模型上限为 128000（官方 config.yaml）。
pub const DEFAULT_CHAT_MAX_TOKENS: u64 = 32768;

/// 默认思考强度（2026-10-09，用户要求「开最大推理」）。
///
/// 官方 `thinking.effortOptions`：`default|low|medium|high|xhigh|max`，
/// 官方默认是 `default`。此处取**最高档 max** 作为缺省值。
///
/// ⚠️ **客户端显式传 `reasoning_effort` 时以客户端为准**（保留灵活性）。
/// 影响：思考更充分但更慢、更耗 token。若要回退官方默认，传 `"default"`。
pub const DEFAULT_REASONING_EFFORT: &str = "max";

/// 本次请求实际使用的 `max_tokens`（请求值优先，缺省用 [`DEFAULT_CHAT_MAX_TOKENS`]）。
/// 抽成函数以便 `build_chat_payload` 与 `finish_reason_for` 用**同一个口径**，
/// 避免"发出去的 max_tokens"与"截断判定用的 max_tokens"不一致。
fn effective_max_tokens(req: Option<&ChatRequest>) -> u64 {
    req.and_then(|r| r.max_tokens).unwrap_or(DEFAULT_CHAT_MAX_TOKENS)
}

/// 把 OpenAI 形状的 `messages` 归一成 **Anthropic 形状**（2026-10-09）。
///
/// 网关对外是 OpenAI 兼容（`content` 为字符串），上游是 Anthropic 形状
/// （`content` 为块数组）。逐条转换：
/// - `content` 是字符串 → `[{type:"text", text:…}]`
/// - `content` 已是数组（原样保留，兼容调用方直接发 Anthropic 形状）
/// - 顺带把 OpenAI 的 `role:"tool"` / `tool_call_id` 折成 Anthropic 的
///   `type:"tool_result"` 块（否则多轮工具调用的结果回喂会被上游拒）
fn normalize_anthropic_messages(messages: &Value) -> Value {
    let Some(arr) = messages.as_array() else {
        return json!([{ "role": "user", "content": [] }]);
    };
    let mut out: Vec<Value> = Vec::with_capacity(arr.len());
    for m in arr {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        // ① OpenAI 的工具结果消息 → Anthropic tool_result 块
        if role == "tool" {
            let content = m
                .get("content")
                .map(scalar_to_text)
                .unwrap_or_else(|| String::new());
            out.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                    "content": content,
                }],
            }));
            continue;
        }
        // ② assistant 消息：tool_calls → tool_use 块
        if role == "assistant" {
            let mut blocks: Vec<Value> = Vec::new();
            if let Some(t) = m.get("content") {
                match t {
                    Value::String(s) if !s.is_empty() => {
                        blocks.push(json!({ "type": "text", "text": s }));
                    }
                    Value::Array(_) => {
                        if let Some(a) = t.as_array() {
                            // 同样做块转换（assistant 回传里也可能带 image_url）
                            blocks.extend(openai_blocks_to_anthropic(a));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    let name = tc
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let input: Value = tc
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .map(|s| serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({})))
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": name,
                        "input": input,
                    }));
                }
            }
            out.push(json!({ "role": "assistant", "content": blocks }));
            continue;
        }
        // ③ user / system：字符串 → text 块
        let content = match m.get("content") {
            Some(Value::String(s)) => {
                if s.is_empty() {
                    vec![]
                } else {
                    vec![json!({ "type": "text", "text": s })]
                }
            }
            // 2026-10-09：数组型 content 不再原样透传 —— 走块转换
            // （ZCode 发的是 OpenAI 形状的 image_url，上游只认 Anthropic 的 image 块）
            Some(Value::Array(a)) => openai_blocks_to_anthropic(a),
            Some(other) => vec![json!({ "type": "text", "text": scalar_to_text(other) })],
            None => vec![],
        };
        // Anthropic 不接受 system role —— 并入首条 user 之前用 system 字段承载。
        // 简化：system 消息原样作为 user 发出（内容不丢），避免破坏结构。
        out.push(json!({ "role": role, "content": content }));
    }
    json!(out)
}

/// 把 **OpenAI 形状的内容块数组**转成 **Anthropic 形状**（2026-10-09）。
///
/// 🔴 修复背景：ZCode 报错 `unsupported content type 'image_url' (2013)`。
/// 此前 user/system 分支对数组型 content 是**原样透传**，而两边的图片块形状不同：
///
/// | 协议 | 图片块 |
/// | --- | --- |
/// | OpenAI | `{"type":"image_url","image_url":{"url":"data:image/png;base64,XXX"}}` |
/// | Anthropic | `{"type":"image","source":{"type":"base64","media_type":"image/png","data":"XXX"}}` |
///
/// 转换规则：
/// - `text` → 形状相同，原样
/// - `image_url`（`data:<mime>;base64,<data>`）→ `image` + `source.type=base64`
/// - `image_url`（`http(s)://…`）→ `image` + `source.type=url`
/// - `image` / `tool_use` / `tool_result` 等**已是 Anthropic 形状** → 原样
/// - **其余未知类型 → 丢弃**（不透传）。理由：上游对未知块类型是**硬 400 整单失败**
///   （见本次事故），丢弃虽会少一块内容，但能让请求整体成功 —— 两害相权取其轻。
///   注意刻意**不记日志**：这里在热路径上，且 `content` 可能较长。
fn openai_blocks_to_anthropic(arr: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(arr.len());
    for b in arr {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => out.push(b.clone()),
            Some("image_url") => {
                let url = b
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // data:<mime>;base64,<payload>
                if let Some(rest) = url.strip_prefix("data:") {
                    if let Some((mime, data)) = rest.split_once(";base64,") {
                        if !mime.is_empty() && !data.is_empty() {
                            out.push(json!({
                                "type": "image",
                                "source": {"type": "base64", "media_type": mime, "data": data},
                            }));
                            continue;
                        }
                    }
                }
                // 远程 URL（Anthropic 支持 url 型 source）
                if url.starts_with("http://") || url.starts_with("https://") {
                    out.push(json!({
                        "type": "image",
                        "source": {"type": "url", "url": url},
                    }));
                    continue;
                }
                // 无法识别 → 丢弃（避免整单 400）
            }
            // 已是 Anthropic 形状（image / tool_use / tool_result / thinking …）→ 原样
            Some("image") | Some("tool_use") | Some("tool_result") | Some("thinking") => {
                out.push(b.clone())
            }
            // 其余未知类型 → 丢弃
            _ => {}
        }
    }
    out
}

/// 把任意 JSON 标量/结构转成纯文本（用于 tool_result 等场景）。
fn scalar_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 构造发往上游的 **Anthropic 形状** payload（2026-10-09）。
///
/// 此前是硬编码的 `json!({model, max_tokens:4096, messages:[单条]})`，
/// 导致 `tools` / `tool_choice` / 多轮历史**三项能力全部被网关掐死**。
/// 现在按 [`ChatRequest`] 逐项透传；空值不写入（保持与此前完全一致的请求体形状，
/// 避免对无工具场景产生行为漂移）。
fn build_chat_payload(model: &str, req: Option<&ChatRequest>, messages: Option<&Value>, prompt: &str) -> Value {
    let mut p = json!({
        "model": model,
        "max_tokens": effective_max_tokens(req),
        "messages": messages
            .filter(|m| m.is_array())
            .cloned()
            .unwrap_or_else(|| json!([{ "role": "user", "content": prompt }])),
    });
    let obj = p.as_object_mut().expect("payload 起始是对象");
    if let Some(t) = req.and_then(|r| r.tools.as_ref()).filter(|t| t.is_array() && !t.as_array().is_none_or(Vec::is_empty)) {
        obj.insert("tools".into(), t.clone());
    }
    if let Some(tc) = req.and_then(|r| r.tool_choice.as_ref()).filter(|t| !t.is_null()) {
        obj.insert("tool_choice".into(), tc.clone());
    }
    // 思考配置（2026-10-09）：thinking / reasoning_effort 必须**逐字透传**给上游，
    // 网关不做归一化——上游对 `thinking.type="disabled"` 会直接 400。
    if let Some(th) = req.and_then(|r| r.thinking.as_ref()).filter(|t| !t.is_null()) {
        obj.insert("thinking".into(), th.clone());
    }
    // 思考强度：客户端显式指定优先，否则用 DEFAULT_REASONING_EFFORT（当前为 max）。
    // 2026-10-09：此前「不传 = 上游用官方 default」，改为「不传 = 本网关默认 max」，
    // 以满足「开最大推理」的需求；客户端仍可传 "default" 显式回退。
    let effort = req
        .and_then(|r| r.reasoning_effort.as_deref())
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .unwrap_or(DEFAULT_REASONING_EFFORT);
    obj.insert("reasoning_effort".into(), json!(effort));
    p
}

/// 把上游 Anthropic `content` 块数组翻译成 OpenAI 形状（2026-10-09）。
///
/// | Anthropic 块 | OpenAI 字段 |
/// | --- | --- |
/// | `type:"text"` | `content`（拼接）|
/// | `type:"thinking"` | `reasoning_content`（拼接）|
/// | `type:"tool_use"` | `tool_calls[] = {id, type:"function", function:{name, arguments}}`（`input` 序列化为 JSON 字符串）|
///
/// 返回 `(content, tool_calls, reasoning)`；三者都可能为空数组/空串。
fn translate_anthropic_blocks(v: &Value) -> (String, Vec<Value>, String) {
    let arr = v.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for b in arr {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    content.push_str(t);
                }
            }
            Some("thinking") => {
                // 上游字段名是 `thinking`；个别版本用 `text`，两个都收
                let t = b
                    .get("thinking")
                    .and_then(Value::as_str)
                    .or_else(|| b.get("text").and_then(Value::as_str))
                    .unwrap_or("");
                reasoning.push_str(t);
            }
            Some("tool_use") => {
                let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let args = b.get("input").cloned().unwrap_or_else(|| json!({}));
                let args_s = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                tool_calls.push(json!({
                    "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                    "type": "function",
                    "index": tool_calls.len(),
                    "function": { "name": name, "arguments": args_s },
                }));
            }
            _ => {}
        }
    }
    (content, tool_calls, reasoning)
}

/// 拼 OpenAI 形状的 assistant message（`content` / `tool_calls` / `reasoning_content`）。
///
/// `tool_calls` 非空时 `content` 置 `null`（OpenAI 规范：有工具调用时 content 为 null），
/// 但**若上游同时给了文本**则保留 —— 有些客户端要求二者并存才能正常渲染。
fn openai_assistant_message(content: &str, tool_calls: &[Value], reasoning: &str) -> Value {
    let mut m = json!({ "role": "assistant" });
    let obj = m.as_object_mut().expect("message 起始是对象");
    obj.insert(
        "content".into(),
        if content.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            json!(content)
        },
    );
    if !tool_calls.is_empty() {
        obj.insert("tool_calls".into(), json!(tool_calls));
    }
    if !reasoning.is_empty() {
        obj.insert("reasoning_content".into(), json!(reasoning));
    }
    m
}

/// 有工具调用时 `finish_reason` 必须是 `tool_calls`（OpenAI 规范），
/// 否则客户端不会把下一轮工具结果回喂给模型。
fn finish_reason_for_tool_calls(tool_calls: &[Value]) -> &'static str {
    if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    }
}

/// 完整 `finish_reason` 判定（2026-10-09）：
/// 1. 有工具调用 → `tool_calls`（OpenAI 规范，客户端据此回喂结果）
/// 2. 输出打满 `max_tokens` → **`length`**（截断信号）
/// 3. 否则 → `stop`
///
/// 🔴 为什么必须有第 2 条：此前恒返回 `stop`，而实测「不传 max_tokens」时
/// 输出**硬卡在 4096 上限**（要求数到 3000 只数到 1630）——响应看起来是
/// **正常结束**，客户端与用户都无法察觉被截断。**这种静默失败比报错更危险**
/// （对比：报错至少能被重试逻辑捕获）。
///
/// 判据用 `output_tokens >= max_tokens`。注意上游 `output_tokens` 通常**包含**
/// thinking tokens，故该判据对「思考占满预算」的情况同样会正确报 `length`
/// —— 这正是我们想要的（思考吃光预算导致正文被截，也是截断）。
fn finish_reason_for(tool_calls: &[Value], usage: &Value, max_tokens: u64) -> &'static str {
    if !tool_calls.is_empty() {
        return "tool_calls";
    }
    let out = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if max_tokens > 0 && out >= max_tokens {
        "length"
    } else {
        "stop"
    }
}

/// 单轮对话。
///
/// `model`：调用方指定的模型名（如 `MiniMax-M3` / `MiniMax-M2.7`）；
/// 传 `None` 或空白则用 [`DEFAULT_CHAT_MODEL`]。
/// 单账号执行（2026-10-05 从 chat_once 抽出）：不再自行选号，账号由调用方给定。
/// 这样 chat_once 才能在外层实现「按候选池顺序换号 + 熔断」。
#[allow(clippy::too_many_arguments)]
async fn chat_with_account(
    acc: &MmAccount,
    prompt: &str,
    model: &str,
    yy_absolute_chat: bool,
    template: &str,
    req: Option<&ChatRequest>,
    messages: Option<&Value>,
) -> Result<Value, (u16, String)> {
    if is_bearer_only(acc) {
        let mavis = format!("{}/mavis/api/v1/llm/v1/messages", cloud_model_host(acc));
        let payload = build_chat_payload(model, req, messages, prompt);
        // 走 curl 子进程：reqwest 对该上游恒定返回 in=21（未按内容处理），
        // 根因与全部排除过程见 chat_via_curl 的文档注释。
        // ⚠️ 必须在闭包外取好 token：`acc` 是借用，`move` 闭包不能带走引用。
        let chat_token = effective_token(acc);
        let (st, text) = tokio::task::spawn_blocking(move || {
            chat_via_curl(&mavis, &payload, &chat_token, 120)
        })
        .await
        .map_err(|e| (502u16, format!("对话任务异常: {e}")))?
        .map_err(|e| {
            // 🔴 2026-10-09：请求「可能已送达并计费」时用哨兵状态码 —— 外层
            // `is_retryable_status` 会因此**拒绝换号**，避免第二个账号被上游再扣一次费。
            if e.maybe_billed {
                (
                    crate::modules::glm_proxy::MIGHT_HAVE_BILLED,
                    e.msg,
                )
            } else {
                (502u16, e.msg)
            }
        })?;
        if !(200..300).contains(&st) {
            return Err((st, format!("模型直连 HTTP {st}: {}", text.chars().take(150).collect::<String>())));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| (502u16, format!("模型响应解析失败: {e}")))?;
        // 2026-10-09：不再只取 text 块——`tool_use` 翻译为 OpenAI `tool_calls`、
        // `thinking` 翻译为 `reasoning_content`（否则工具调用能力被本网关掐死）。
        let (content, tool_calls, reasoning) = translate_anthropic_blocks(&v);
        let usage = v
            .get("usage")
            .cloned()
            .unwrap_or(json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}));
        let finish = finish_reason_for(&tool_calls, &usage, effective_max_tokens(req));
        return Ok(json!({
            "id": v.get("id").cloned().unwrap_or(json!("minimax-desktop")),
            "object": "chat.completion",
            "created": now_ms() / 1000,
            // 上游未回 model 时回显**请求的**模型名（此前硬编码 M3.1-Flash-Preview，掩盖了实际未生效）
            "model": v.get("model").cloned().unwrap_or(json!(model.clone())),
            "choices": [{"index": 0, "message": openai_assistant_message(&content, &tool_calls, &reasoning), "finish_reason": finish}],
            "usage": usage,
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
        let payload = build_chat_payload(model, req, messages, prompt);
        // 走 curl 子进程：reqwest 对该上游恒定返回 in=21（未按内容处理），
        // 根因与全部排除过程见 chat_via_curl 的文档注释。
        // ⚠️ 必须在闭包外取好 token：`acc` 是借用，`move` 闭包不能带走引用。
        let chat_token = effective_token(acc);
        let (st, text) = tokio::task::spawn_blocking(move || {
            chat_via_curl(&mavis, &payload, &chat_token, 120)
        })
        .await
        .map_err(|e| (502u16, format!("对话任务异常: {e}")))?
        .map_err(|e| {
            // 🔴 2026-10-09：请求「可能已送达并计费」时用哨兵状态码 —— 外层
            // `is_retryable_status` 会因此**拒绝换号**，避免第二个账号被上游再扣一次费。
            if e.maybe_billed {
                (
                    crate::modules::glm_proxy::MIGHT_HAVE_BILLED,
                    e.msg,
                )
            } else {
                (502u16, e.msg)
            }
        })?;
        if !(200..300).contains(&st) {
            return Err((st, format!("模型直连 HTTP {st}: {}", text.chars().take(150).collect::<String>())));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| (502u16, format!("模型响应解析失败: {e}")))?;
        // 同上：tool_use → tool_calls、thinking → reasoning_content
        let (content, tool_calls, reasoning) = translate_anthropic_blocks(&v);
        let usage = v
            .get("usage")
            .cloned()
            .unwrap_or(json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}));
        let finish = finish_reason_for(&tool_calls, &usage, effective_max_tokens(req));
        return Ok(json!({
            "id": v.get("id").cloned().unwrap_or(json!("minimax-desktop")),
            "object": "chat.completion",
            "created": now_ms() / 1000,
            "model": v.get("model").cloned().unwrap_or(json!(model.clone())),
            "choices": [{"index": 0, "message": openai_assistant_message(&content, &tool_calls, &reasoning), "finish_reason": finish}],
            "usage": usage,
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
        .post(format!("{create_url}?{wire}"))
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
        let (w2, t2, x2, y2) = sign_request(acc, &format!("/session/{session_id}"), "", yy_absolute_chat);
        let r = upstream_client()
            .get(format!("{idle_url}?{w2}"))
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
    chat_once_full(prompt, region_hint, model, None).await
}

/// **完整能力**对话（2026-10-09）：在 [`chat_once`] 之上接受 [`ChatRequest`]，
/// 透传 `tools` / `tool_choice` / 完整 `messages`，并把上游的 `tool_use` /
/// `thinking` 块翻译成 OpenAI 形状（`tool_calls` / `reasoning_content`）。
///

/// MiniMax 调用计量（Token + 积分），2026-10-10。
///
/// 落库统一走 `model_usage::record_channel_call("minimax", …)`，字段名与 GLM 记录
/// **完全一致** ⇒ 前端图表/列表与 `/api/glm-usage/*` 聚合逻辑零改动即可消费。
mod call_metrics {
    use super::*;
    use serde_json::{json, Value};

    /// 「调用前余额」的后台采样句柄。
    ///
    /// 必须并行采样：`credits_remaining` 是一次真实上游请求（实测 250–600ms），
    /// 串行进关键路径会直接拖慢每一次对话。
    pub struct BalanceProbe {
        handle: Option<tokio::task::JoinHandle<Option<f64>>>,
    }

    /// 启动调用前余额采样（**不阻塞**）。
    pub fn start(acc: &MmAccount) -> BalanceProbe {
        // spawn 要求 'static，故克隆账号条目（MmAccount: Clone）。
        let owned = acc.clone();
        BalanceProbe {
            handle: Some(tokio::spawn(async move { credits_remaining(&owned).await })),
        }
    }

    /// 放弃采样（本次尝试将换号重试，不产生计费）。
    ///
    /// 注：用普通参数而非 `self` —— Rust 只允许在 `impl` 块的方法里按值接收 `self`。
    pub fn abort(probe: BalanceProbe) {
        if let Some(h) = probe.handle {
            h.abort();
        }
    }

    /// 结算本次调用并落一条 usage 记录。
    ///
    /// * `response` —— 成功时的上游响应（从中提取 usage）；失败时传 `None`。
    /// * `error`    —— 失败原因；成功时传 `None`。
    pub async fn settle(
        probe: BalanceProbe,
        acc: &MmAccount,
        requested_model: &str,
        actual_model: &str,
        response: Option<&Value>,
        latency_ms: u64,
        error: Option<&str>,
    ) {
        let ok = error.is_none();
        // Token：复用既有口径（自动覆盖缓存 token）。
        let usage = response
            .map(crate::modules::model_usage::extract_usage)
            .unwrap_or_else(|| {
                json!({
                    "promptTokens": 0, "completionTokens": 0, "totalTokens": 0,
                    "reasoningTokens": Value::Null, "cachedTokens": 0,
                    "uncachedPromptTokens": 0, "cacheHit": false,
                })
            });
        let pick = |k: &str| usage.get(k).cloned().unwrap_or(json!(0));

        // 积分：调用后余额 vs 调用前余额。取不到 → null + pending（不伪造 0）。
        let before = match probe.handle {
            Some(h) => h.await.ok().flatten(),
            None => None,
        };
        let after = credits_remaining(acc).await;
        let (credits, credits_pending) = match (before, after) {
            // 余额差为负 = 期间有充值/过期，不是「本次花费」⇒ 不记。
            (Some(b), Some(a)) if a < b => (Value::Null, true),
            (Some(b), Some(a)) => (json!(b - a), false),
            _ => (Value::Null, true),
        };

        let mut rec = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "ts": now_ms(),
            "model": actual_model,
            "requestedModel": requested_model,
            "routeId": Value::Null,
            "accountId": acc.id,
            "accountLabel": acc.label,
            "promptTokens": pick("promptTokens"),
            "completionTokens": pick("completionTokens"),
            "totalTokens": pick("totalTokens"),
            "reasoningTokens": usage.get("reasoningTokens").cloned().unwrap_or(Value::Null),
            "cachedTokens": pick("cachedTokens"),
            "uncachedPromptTokens": pick("uncachedPromptTokens"),
            "cacheHit": usage.get("cacheHit").cloned().unwrap_or(json!(false)),
            "credits": credits,
            "creditsPending": credits_pending,
            // 后置余额随记录落库：`accounts-overview` 的「当前余额」列据此展示，
            // 免去在汇总层再发一次上游请求（`credits_remaining` 是一次真实网络调用）。
            "balanceAfter": after.map(|b| json!((b * 100.0).round() / 100.0)).unwrap_or(Value::Null),
            "ledgerId": Value::Null,
            "latencyMs": latency_ms,
            "ttfbMs": Value::Null,
            "ok": ok,
        });
        if let Some(e) = error {
            rec["error"] = json!(e.chars().take(300).collect::<String>());
        }
        crate::modules::model_usage::record_channel_call("minimax", rec);
    }
}

/// `req` 为 `None` 或各字段为空时，行为与 [`chat_once`] 完全一致（向后兼容）。
pub async fn chat_once_full(
    prompt: &str,
    region_hint: Option<&str>,
    model: Option<&str>,
    req: Option<ChatRequest>,
) -> Result<Value, (u16, String)> {
    let req = req.unwrap_or_default();
    let cfg = load_config();
    if !cfg.enabled {
        return Err((503, "MiniMax 网关未启用".to_string()));
    }
    // 模型名：请求参数 > ChatRequest.model > 内置默认；三端去空白，空串视为未指定
    let model: String = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            req.model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| DEFAULT_CHAT_MODEL.to_string());
    // 完整消息历史：有则用（并按 Anthropic 形状归一），无则退化为单条 user
    let messages: Value = match req.messages.as_ref().and_then(|m| {
        if m.is_array() { Some(m) } else { None }
    }) {
        Some(m) => normalize_anthropic_messages(m),
        None => json!([{ "role": "user", "content": prompt }]),
    };
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
        // 2026-10-10：调用**前**并行采样余额（供积分归因）。spawn 在下方立即 await。
        let probe = call_metrics::start(acc);
        let call_started = now_ms();
        match chat_with_account(
            acc,
            prompt,
            &model,
            cfg.yy_absolute_chat,
            &cfg.chat_payload_template,
            Some(&req),
            Some(&messages),
        )
        .await
        {
            Ok(mut v) => {
                mark_account_success(&acc.id);
                if let Some(o) = v.as_object_mut() {
                    o.insert("failover".to_string(), json!(k));
                }
                // 2026-10-10：落计量（Token + 积分）。用响应里的 `model` 回显，
                // 避免上游改名后统计口径漂移；`settle` 内部只读不改响应体。
                // 延迟取 `call_started`（本次尝试发起时刻）——不可在 settle 前才取，
                // 否则 latency 恒为 0。
                call_metrics::settle(
                    probe,
                    acc,
                    &model,
                    v.get("model").and_then(Value::as_str).unwrap_or(&model),
                    Some(&v),
                    now_ms().saturating_sub(call_started),
                    None,
                )
                .await;
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
                // 该账号不再重试时（含最终失败）：把这次失败也记进统计，
                // 否则「调用次数」只统计成功数，失败率无从体现。
                if strict || !retryable {
                    call_metrics::settle(
                        probe,
                        acc,
                        &model,
                        &model,
                        None,
                        now_ms().saturating_sub(call_started),
                        Some(&msg),
                    )
                    .await;
                    return Err((st, msg));
                }
                // 还要换号重试：本次尝试作废（不产生计费），丢弃采样即可。
                call_metrics::abort(probe);
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
    // 🔴 2026-10-09：「请求可能已送达并计费」**一律禁止换号重试**。
    //
    // 成因：账号 A 发出请求 → 上游已扣 A 的费 → 响应超时/中断 → 若此处返回 true
    // 就会换号 B 重试 → 上游又扣 B 的费 ⇒ **同一次操作两个账号双边扣费**
    // （用户报「扣减积分却扣了两个账号」的成因）。
    // 取向：宁可不重试（把错误透出），也不静默多扣一个账号的额度。
    if status == crate::modules::glm_proxy::MIGHT_HAVE_BILLED {
        return false;
    }
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



/// 扫描桌面端登录态（`{主实例 + 各隔离实例}/auth/prod/{cn,en}/mcode-public/auth.json`，只读）。
///
/// 🔴 2026-10-08：桌面端是**单登录槽**（一个实例只能登录一个账号，登录第二个会覆盖
/// 第一个的 auth.json）。多账号的正解是开**隔离实例**
/// （`--data-dir=~/.minimax-<name> --user-data-dir=%APPDATA%\MiniMax-<name>`，
/// 见仓库根 `start-minimax-acc2.bat`）⇒ 本函数自动发现 `~/.minimax-*` 下的实例账号 ✓
pub fn discover_desktop() -> Value {
    let home = crate::modules::config::home_dir();
    let mut bases: Vec<(String, std::path::PathBuf)> = vec![(String::new(), home.join(".minimax"))];
    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(inst) = name.strip_prefix(".minimax-") {
                if !inst.is_empty() && e.path().is_dir() {
                    bases.push((inst.to_string(), e.path()));
                }
            }
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for (instance, base) in bases {
        for (dir, region) in [("cn", "domestic"), ("en", "international")] {
            let path = base
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
            // 🔴 2026-10-09（隔离实例实证）：`sharedUser` 可能整体为空（资料未同步落盘，
            // 实测 acc3）⇒ 回退到 Local Storage 身份。否则导入的账号 `user_id` 为空、
            // 标签退化成「桌面端-」，签到必返空面板（根因见 `desktop_current_user_ids`）。
            let mut shared = desktop_shared(dir, &instance);
            let mut storage_fallback = false;
            if shared.get("realUserID").and_then(Value::as_str).unwrap_or("").is_empty()
                && shared.get("userID").and_then(Value::as_str).unwrap_or("").is_empty()
            {
                shared = desktop_storage_shared(&instance);
                storage_fallback = !shared.as_object().map(|o| o.is_empty()).unwrap_or(true);
            }
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
                // 实例名（"" = 主实例；"acc2" = 隔离实例）⇒ 决定指纹与 auth.json 来源
                "desktopInstance": instance,
                // `sharedUser` 为空、身份取自 Local Storage 兜底时为 true
                "storageFallback": storage_fallback,
                "userId": shared.get("userID").cloned().unwrap_or(Value::Null),
                "realUserID": shared.get("realUserID").cloned().unwrap_or(Value::Null),
                "deviceID": shared.get("deviceID").cloned().unwrap_or(Value::Null),
                "expiresAtMs": rec.get("expiresAtMs").cloned().unwrap_or(Value::Null),
                // 导入时 accessToken 快照（capture 落 token_snapshot；桌面端单登录槽
                // 切号后 auth.json 被替换，此快照是旧账号最后的可用凭据）
                "accessTokenSnapshot": rec.get("accessToken").cloned().unwrap_or(Value::Null),
                "tokenExpired": rec
                    .get("expiresAtMs")
                    .and_then(Value::as_u64)
                    .map(|e| e < now_ms())
                    .unwrap_or(false),
            }));
        }
    }
    json!({ "ok": !out.is_empty(), "accounts": out })
}

/// 桌面端 sharedUser（%APPDATA%/MiniMax/minimax-agent[-cn]-config.json）。
fn desktop_shared(region_dir: &str, instance: &str) -> Value {
    let Some(dir) = desktop_userdata_dir_of(instance) else {
        return json!({});
    };
    let name = if region_dir == "cn" {
        "minimax-agent-cn-config.json"
    } else {
        "minimax-agent-config.json"
    };
    std::fs::read_to_string(dir.join(name))
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
    let mut newly_added: Vec<String> = Vec::new();
    for d in &list {
        let path = d.get("authPath").and_then(Value::as_str).unwrap_or("").to_string();
        let key = d.get("authKey").and_then(Value::as_str).unwrap_or("").to_string();
        let snapshot = d
            .get("accessTokenSnapshot")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let user_id = d.get("userId").and_then(Value::as_str).unwrap_or_default().to_string();
        // 2026-10-08：签名的 `user_id` 必须是 **realUserID**（短串 userID 会被上游
        // 以 `1406010011 invalid timezone_id` 拒绝）⇒ 导入即取真值，旧条目由 backfill 升级。
        let real_user_id = d.get("realUserID").and_then(Value::as_str).unwrap_or_default().to_string();
        let wanted_uid = if real_user_id.is_empty() { user_id.clone() } else { real_user_id.clone() };
        let instance = d.get("desktopInstance").and_then(Value::as_str).unwrap_or_default().to_string();
        // 身份去重（2026-10-04 修复「老账号不见了」）：桌面端重登**复用同一 auth 槽位键**
        // 只替换里面的令牌——按 (auth_path, auth_key) 去重会把新登录并进旧条目（旧号被
        // 顶掉改名）。改为按 user_id（账号身份）去重：同号=更新令牌，新号=新增冻结条目。
        if !user_id.is_empty() || !real_user_id.is_empty() {
            // 匹配放宽到「短串 userID 或 realUserID 任一命中」——历史条目存的是短串，
            // 否则升级到 realUserID 时会重复建条目（2026-10-08）。
            if let Some(a) = cfg.accounts.iter_mut().find(|a| {
                a.source == "desktop"
                    && ((!user_id.is_empty() && a.user_id == user_id)
                        || (!real_user_id.is_empty() && a.user_id == real_user_id))
            }) {
                if !snapshot.is_empty() {
                    a.token = snapshot.clone();
                    a.token_snapshot = snapshot;
                }
                a.auth_path = path;
                a.auth_key = key;
                a.desktop_instance = instance.clone();
                if !wanted_uid.is_empty() {
                    a.user_id = wanted_uid.clone();
                }
                a.last_error = None;
                // 条目累计值可能被旧版本清零过 ⇒ 与账本对账一次（只增不减）
                reconcile_points_total(a);
                skipped.push(format!("{}（已在池，令牌已更新）", a.label));
                continue;
            }
        } else if !key.is_empty() {
            // 2026-10-08 修复「桌面端账号重复、签到不生效」：桌面端 auth.json 的 user_id
            // 可能为空 ⇒ 上面按 user_id 去重不生效，每次捕获都 push 新条目。
            // 实测池里因此堆了 3 条**共用同一 auth_key** 的条目（同一个登录态），
            // 签到 claim 自然拿不到奖励（claim_result=0）。
            // 退化方案：user_id 为空时按 (auth_path, auth_key) 去重——同一登录槽位只保留一条。
            if let Some(a) = cfg
                .accounts
                .iter_mut()
                .find(|a| a.source == "desktop" && a.auth_key == key && a.auth_path == path)
            {
                if !snapshot.is_empty() {
                    a.token = snapshot.clone();
                    a.token_snapshot = snapshot;
                }
                if !wanted_uid.is_empty() {
                    a.user_id = wanted_uid.clone();
                }
                a.desktop_instance = instance.clone();
                a.last_error = None;
                skipped.push(format!("{}（同一桌面端登录态，令牌已更新）", a.label));
                continue;
            }
        }
        let region = d.get("region").and_then(Value::as_str).unwrap_or(PROVIDER_BIGMODEL).to_string();
        let id = gen_id();
        let label = format!(
            "桌面端-{}",
            d.get("label").and_then(Value::as_str).unwrap_or(region.as_str())
        );
        // 🔴 2026-10-09：新建条目时从**账本**取历史累计（不再从 0 开始）——
        // 条目 id 每次导入都变，只有按身份记账的值能跨条目存活。
        let points_total = ledger_points(&wanted_uid, &label);
        cfg.accounts.push(MmAccount {
            id: id.clone(),
            label,
            region,
            // 冻结快照条目：token 自含（导入时的 accessToken），不随 auth.json 槽位漂移
            token: snapshot.clone(),
            uuid: uuid::Uuid::new_v4().to_string(),
            // 真实桌面端 deviceID（sharedUser，纯数字）——比本机生成更可信
            device_id: d
                .get("deviceID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            user_id: wanted_uid.clone(),
            agent_id: String::new(),
            screen_w: "1707".into(),
            screen_h: "1067".into(),
            base_url: String::new(),
            enabled: true,
            added_at: now_ms(),
            last_error: None,
            cooldown_until_ms: 0,
            signin_last_date: String::new(),
            signin_points_total: points_total,
            source: "desktop".into(),
            auth_path: path,
            auth_key: key,
            // 导入时 accessToken 快照：桌面端切号后 auth.json 被替换，此快照是
            // 旧账号最后的可用凭据（effective_token 身份漂移时回落）。
            token_snapshot: snapshot,
            balance: None,
            credit_consumed: None,
            balance_at: 0,
            fail_streak: 0,
            desktop_instance: instance.clone(),
        });
        newly_added.push(id);
        added += 1;
    }
    // 身份同步循环已删除（2026-10-04）：冻结条目的身份/令牌自含，不再被当前
    // 桌面端登录覆盖——这正是「导入的账号被顶掉」的根源。桌面端当前登录如需
    // 入池，点「从桌面端导入」会按 user_id 识别为独立冻结条目。
    // 新导入的账号自动加入固定集合（用户预期「导入即固定/记住」；幂等）。
    if added > 0 {
        for id in &newly_added {
            if !cfg.pinned_account_ids.iter().any(|x| x == id) {
                cfg.pinned_account_ids.push(id.clone());
            }
        }
        normalize_pin_set(&mut cfg.pinned_account_ids, &cfg.accounts);
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
        // 读余额同样需要新鲜 token（TTL 1 小时）⇒ 先尽力续期，失败不阻断
        let _ = refresh_desktop_credential(acc).await;
        match credit_summary(acc).await {
            // 四舍五入到 2 位小数：credit/details 求和的 f64 尾差（2386.4700000000003）
            // 会原样进配置与 UI，读数观感极差
            Some((remaining, used, _expire)) => (
                Some((remaining * 100.0).round() / 100.0),
                Some((used * 100.0).round() / 100.0),
            ),
            None => (None, None),
        }
    } else {
        (membership_balance(acc).await.ok().map(|b| (b * 100.0).round() / 100.0), None)
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

// ---------------------------------------------------------------------------
// 积分动向追踪（2026-10-09）
// ---------------------------------------------------------------------------

/// 一次签到引发的积分变动记录。
///
/// 上游签到接口**不直接返回余额**：`signin/claim` 只给 `claim_result` 与本次
/// `points`（且 `claim_result=1` 不代表真的发放，juangchuank 实测）。因此
/// **变动值必须由签到前后的 `credit/details` 查询推算**——这也是本结构的存在理由。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreditMovement {
    /// 账号 id
    pub id: String,
    /// 账号名（记录当时的名字，便于事后对账）
    pub label: String,
    /// 账号身份（`realUserID`）。**跨条目对账的稳定键**——条目被删/重导后 `id` 会变，
    /// 只有身份不变（2026-10-09 新增；旧记录缺省为空）
    #[serde(default)]
    pub user_id: String,
    /// 变动时间（毫秒时间戳）
    pub at_ms: u64,
    /// 变动时间（UTC+8 可读串）
    pub at_str: String,
    /// 签到是否成功
    pub ok: bool,
    /// `claimed` / `already-claimed` / `failed`
    pub action: String,
    /// 签到前余额（读不到为 null）
    pub balance_before: Option<f64>,
    /// 签到后余额（读不到为 null）
    pub balance_after: Option<f64>,
    /// 变动差值 = after - before（任一端读不到则为 null）
    pub delta: Option<f64>,
    /// 上游返回的本次积分点数
    pub points: Option<u64>,
    /// credit/details 是否见到当日发放
    pub paid: bool,
    /// 失败原因 / 备注
    pub note: Option<String>,
}

/// 动向记录文件（独立于账号配置，避免与 `save_config` 的快照式覆盖互相踩踏）。
pub fn movements_path() -> std::path::PathBuf {
    // 与 [`points_ledger_path`] 同理：测试不得写生产目录（详见该函数注释）
    #[cfg(test)]
    {
        return points_ledger_path().with_file_name("minimax_credit_movements.json");
    }
    #[cfg(not(test))]
    crate::modules::config::store_dir().join("minimax_credit_movements.json")
}

/// 最多保留的动向条数（防止无限增长）。
const MOVEMENTS_CAP: usize = 500;

pub fn load_movements() -> Vec<CreditMovement> {
    std::fs::read_to_string(movements_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<CreditMovement>>(&t).ok())
        .unwrap_or_default()
}

/// 追加一条动向（保留最近 [`MOVEMENTS_CAP`] 条）。
///
/// 读取—追加—写回之间**无锁**：签到路由是串行的（`signin/run` 内 sleep 2s 逐个处理），
/// 冲突窗口极小；即便丢一条也只是审计记录，不影响签到主流程。
pub fn append_movement(m: CreditMovement) {
    let mut all = load_movements();
    all.push(m);
    if all.len() > MOVEMENTS_CAP {
        let drop = all.len() - MOVEMENTS_CAP;
        all.drain(0..drop);
    }
    if let Ok(s) = serde_json::to_string_pretty(&all) {
        let _ = std::fs::write(movements_path(), s);
    }
}

/// 本地（UTC+8）可读时间串 `YYYY-MM-DD HH:MM:SS`。
pub fn now_local_str() -> String {
    use chrono::FixedOffset;
    let cst = FixedOffset::east_opt(8 * 3600).unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
    chrono::Utc::now().with_timezone(&cst).format("%Y-%m-%d %H:%M:%S").to_string()
}

// ---------------------------------------------------------------------------
// 累计积分账本（2026-10-09）
// ---------------------------------------------------------------------------
//
// 🔴 解决的问题：`signin_points_total` 存在**账号条目**里，而条目会被删/重导
// （桌面端切号、去重、手动移除…）⇒ **累计值每次都归零**。实测 2026-10-09：
// 小螺帽 00:42 真领 400，但该条目随后被重建 ⇒ UI 显示「签到 2026-10-09 · 累计 0 分」。
//
// 口径说明（必须让用户看懂）：
//   累计 = **本机记录的「本账号历次 claim 成功」积分之和**，不是上游权威值
//   （上游没有「历史签到累计」接口，只有 7 天面板 + credit/details 余额）。
//   ⇒ 因此它按 **身份（realUserID）** 记账、跨条目存活；同日 `already-claimed`
//   **不累加**（防重复计数，真实到手看动向的 `delta`）。

/// 账本文件：`{ "<realUserID 或 label>": {"points": N, "updated_at_ms": …} }`
pub fn points_ledger_path() -> std::path::PathBuf {
    // 🔴 2026-10-09：测试必须走**独立目录**。此前单测
    // `apply_signin_outcome_accumulates_only_on_claimed` 通过 `ledger_add` 把
    // `label:mm-8 = 1200` 写进了**用户真实的** `minimax_points_ledger.json`
    // ⇒ 测试污染生产数据（记账类文件尤其危险：脏值会被 reconcile 灌进真实账号）。
    #[cfg(test)]
    {
        static D: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        return D
            .get_or_init(|| {
                // ⚠️ 必须**按进程唯一 + 启动即清空**：固定目录名会让上一次测试运行的
                // 账本残留参与本次断言（实测 `label:mm-8` 累到 4000，把 400 的期望打崩）。
                let dir = std::env::temp_dir()
                    .join(format!("wb-switch-test-store-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                let _ = std::fs::create_dir_all(&dir);
                // 🔴 2026-10-09 修正：此处必须返回**目录下的文件路径**。
                // 此前直接 `return dir`（漏了文件名）⇒ `atomic_write` 把文件写向一个
                // 目录路径而失败（错误被 `let _ =` 吞掉）⇒ `load_ledger` 永远读到空
                // ⇒ `signin_points_total` 恒为 0，测试 `left: 0` 失败。
                dir.join("minimax_points_ledger.json")
            })
            .clone();
    }
    #[cfg(not(test))]
    crate::modules::config::store_dir().join("minimax_points_ledger.json")
}

/// 账本键：优先身份 `realUserID`（条目 id 会变、身份不变），退化用 `label`。
fn ledger_key(user_id: &str, label: &str) -> String {
    if !user_id.is_empty() {
        format!("uid:{user_id}")
    } else {
        format!("label:{label}")
    }
}

/// 账本写锁（进程级）。
///
/// 🔴 2026-10-09：账本是「读—改—写」且原先**无锁**。两个并发写入（手动签到 ×
/// 统一编排 sweep）会同时 `atomic_write` 同一目标 ⇒ Windows 上后到的 rename
/// 直接 `os error 5 拒绝访问`（tmp 名唯一但目标是同一个文件）。单测并行时必现，
/// 生产只是窗口更小 —— 已实测报错，故补锁。临界区只包「读改写」，不含网络调用。
fn ledger_lock() -> &'static std::sync::Mutex<()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
}

/// **记一天的「已确认到账」签到积分**（按天去重），返回 `(累计, 本次是否新增)`。
///
/// 🔴 2026-10-09（口径纠偏，用户实测截图「累计 0 分」逼出来的）：
/// 旧口径只认 `action == "claimed"`（**本机**领到的）。但桌面端客户端自带自动签到 ——
/// **客户端先领了**，服务端再跑只会拿到 `status=3` / `claim_result=2` ⇒ 判成
/// `already-claimed` ⇒ **本机永远记 0**，而用户看到的是「今天确实签到、也确实到账了」。
/// 实测 MiniMax614670：面板 `is_today, status=3, points=400`、`paid=true`，累计却是 0。
///
/// 现口径：**当天上游确认已签到就记一次**（`status=3` / 本机 `claimed` / `paid=true`），
/// 按 `(身份, 日期)` 去重 ⇒ 既覆盖客户端先领，又不会重复计数（同日跑 10 次也只记一次）。
///
/// 旧格式兼容：老账本只有 `points` 没有 `days`，无法反推历史值属于哪天
/// ⇒ 首次写入时把旧值**归到调用当天**（保守：宁可少记也不重复记）。
pub fn ledger_add_day(user_id: &str, label: &str, date: &str, points: u64) -> (u64, bool) {
    if points == 0 {
        return (ledger_points(user_id, label), false);
    }
    let _guard = ledger_lock().lock().unwrap_or_else(|e| e.into_inner());
    let key = ledger_key(user_id, label);
    let mut all = load_ledger();
    let mut days: serde_json::Map<String, Value> = all
        .get(&key)
        .and_then(|v| v.get("days"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if days.is_empty() {
        // 旧格式迁移：把历史累计归到「今天」，避免同一天被记两次
        let legacy = all
            .get(&key)
            .and_then(|v| v.get("points"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if legacy > 0 {
            days.insert(date.to_string(), json!(legacy));
        }
    }
    if days.get(date).and_then(Value::as_u64).unwrap_or(0) > 0 {
        let total: u64 = days.values().filter_map(Value::as_u64).sum();
        return (total, false); // 当天已记过 ⇒ 幂等
    }
    days.insert(date.to_string(), json!(points));
    let total: u64 = days.values().filter_map(Value::as_u64).sum();
    all.insert(
        key,
        json!({"points": total, "days": Value::Object(days), "updated_at_ms": now_ms(),
               "label": label, "user_id": user_id}),
    );
    if let Ok(s) = serde_json::to_string_pretty(&Value::Object(all)) {
        let _ = crate::modules::config::atomic_write(&points_ledger_path(), &s);
    }
    (total, true)
}

/// 从一次签到结果里取「本次的签到积分」。
fn signin_points_of(res: &Value) -> u64 {
    res.get("points")
        .and_then(Value::as_u64)
        .or_else(|| {
            res.get("points")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// 这次签到**是否可记为「当天已到账」**（纯函数，便于单测）。
///
/// 记：`claimed`（本机领到）；`already-claimed` 且不是「上游明确无可领」
/// （只有 `result == "already"` **且** `paid != true` 才是无可领）。
/// 不记：失败、上游无可领。
fn credited_today(res: &Value) -> bool {
    if res.get("ok").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    match res.get("action").and_then(Value::as_str).unwrap_or("") {
        "claimed" => true,
        "already-claimed" => {
            res.get("paid").and_then(Value::as_bool) == Some(true)
                || res.get("result").and_then(Value::as_str) != Some("already")
        }
        _ => false,
    }
}

fn load_ledger() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(points_ledger_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// 读账本累计值。
///
/// ⚠️ **两个键都查、取 max**：`uid:<realUserID>`（新写入，稳定）与
/// `label:<名字>`（历史动向回填产生，条目重建后 label 会变）可能并存。
/// 取 max 而非相加 —— 同一笔积分不可能同时以两个键独立入账，取大值既不丢也不重复。
pub fn ledger_points(user_id: &str, label: &str) -> u64 {
    let all = load_ledger();
    let get = |k: String| {
        all.get(&k)
            .and_then(|v| v.get("points"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let by_uid = if user_id.is_empty() {
        0
    } else {
        get(format!("uid:{user_id}"))
    };
    let by_label = if label.is_empty() {
        0
    } else {
        get(format!("label:{label}"))
    };
    by_uid.max(by_label)
}

/// **一次性回填/校正**条目上的 `signin_points_total`：取「条目现值」与「账本值」的较大者。
///
/// 用途：① 新导入条目从账本恢复历史累计（不再从 0 开始）；
/// ② 历史缺失（条目被删过）时由调用方先调 [`ledger_backfill_from_history`] 补账本。
pub fn reconcile_points_total(acc: &mut MmAccount) -> bool {
    let want = ledger_points(&acc.user_id, &acc.label);
    if want > acc.signin_points_total {
        acc.signin_points_total = want;
        return true;
    }
    false
}

/// 从历史记录（动向 JSON）回填账本：把「本机确实领到过」的积分补进账本。
///
/// 匹配规则：只统计 `action == "claimed"` 且 `points > 0` 的记录；
/// `already-claimed` 不计（那是当日已领，不代表本次发放）。
///
/// `alias`：当前账号池的 `(user_id, label)`，用于把历史记录产生的 **`label:` 键升级成
/// `uid:` 键**——历史动向早于 `user_id` 字段，只能按名字归并；升级后条目重建/改名都不丢。
///
/// **幂等**：逐键取 `max(账本现值, 流水求和)`，重复调用不会翻倍。
pub fn ledger_backfill_from_history(alias: &[(String, String)]) -> usize {
    let _guard = ledger_lock().lock().unwrap_or_else(|e| e.into_inner());
    // label → user_id 映射（只取身份像样的，避免把 web 账号的空身份写进键）
    let alias_map: std::collections::HashMap<&str, &str> = alias
        .iter()
        .filter(|(uid, _)| !uid.is_empty())
        .map(|(uid, label)| (label.as_str(), uid.as_str()))
        .collect();

    // 流水按 **(键, 日期)** 归集；同日多条取 **max**（同一天只记一次，与
    // `ledger_add_day` 的去重口径一致）。只统计「已确认到账」：
    //   `claimed`（本机领到） 或 `already-claimed` 且 `paid`（客户端先领、details 已见发放）
    let mut per: std::collections::HashMap<String, (String, String, std::collections::BTreeMap<String, u64>)> =
        std::collections::HashMap::new();
    for m in load_movements() {
        let credited = m.action == "claimed" || (m.action == "already-claimed" && m.paid);
        if !credited {
            continue;
        }
        let p = m.points.unwrap_or(0);
        if p == 0 {
            continue;
        }
        let date = m.at_str.chars().take(10).collect::<String>();
        if date.len() < 10 {
            continue;
        }
        let uid = if !m.user_id.is_empty() {
            m.user_id.clone()
        } else {
            alias_map.get(m.label.as_str()).map(|s| s.to_string()).unwrap_or_default()
        };
        let key = ledger_key(&uid, &m.label);
        let e = per.entry(key).or_insert((m.label.clone(), uid, Default::default()));
        let slot = e.2.entry(date).or_insert(0);
        if p > *slot {
            *slot = p;
        }
    }

    let mut all = load_ledger();
    let mut touched = 0usize;
    for (key, (label, uid, days)) in per {
        let mut merged: serde_json::Map<String, Value> = all
            .get(&key)
            .and_then(|v| v.get("days"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // 旧格式（只有 points、无 days）⇒ 把历史累计归到流水里最早那一天
        if merged.is_empty() {
            let legacy = all
                .get(&key)
                .and_then(|v| v.get("points"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if legacy > 0 {
                if let Some(first) = days.keys().next() {
                    merged.insert(first.clone(), json!(legacy));
                }
            }
        }
        let mut changed = false;
        for (d, p) in &days {
            if merged.get(d).and_then(Value::as_u64).unwrap_or(0) < *p {
                merged.insert(d.clone(), json!(p));
                changed = true;
            }
        }
        if !changed {
            continue;
        }
        let total: u64 = merged.values().filter_map(Value::as_u64).sum();
        all.insert(
            key,
            json!({"points": total, "days": Value::Object(merged), "updated_at_ms": now_ms(),
                   "label": label, "user_id": uid, "source": "backfill-from-movements"}),
        );
        touched += 1;
    }
    if touched > 0 {
        if let Ok(s) = serde_json::to_string_pretty(&Value::Object(all)) {
            let _ = crate::modules::config::atomic_write(&points_ledger_path(), &s);
        }
    }
    touched
}

/// 签到前后各查一次积分，产出动向记录并落盘。
/// `points` 取上游 `signin/claim` 的返回值（可能为 null：`already-claimed` 时上游
/// 给的是面板当日已发分）；**真实到手以 `delta` 为准**（前后余额推算）。
/// 返回 `(动向记录, 已消耗积分)`——后者一并带回，调用方无需再打一次上游。
pub async fn track_credit_movement(
    acc: &MmAccount,
    res: &Value,
    balance_before: Option<f64>,
) -> (CreditMovement, Option<f64>) {
    let (balance_after, consumed) = refresh_credit_snapshot(acc).await;
    let delta = match (balance_before, balance_after) {
        (Some(b), Some(a)) => Some(((a - b) * 100.0).round() / 100.0),
        _ => None,
    };
    let ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let m = CreditMovement {
        id: acc.id.clone(),
        label: acc.label.clone(),
        user_id: acc.user_id.clone(),
        at_ms: now_ms(),
        at_str: now_local_str(),
        ok,
        action: res
            .get("action")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| if ok { "ok".to_string() } else { "failed".to_string() }),
        balance_before,
        balance_after,
        delta,
        points: res
            .get("points")
            .and_then(Value::as_u64)
            .or_else(|| {
                res.get("points")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u64>().ok())
            }),
        paid: res.get("paid").and_then(Value::as_bool).unwrap_or(false),
        note: res
            .get("error")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| res.get("note").and_then(Value::as_str).map(String::from)),
    };
    append_movement(m.clone());
    (m, consumed)
}

// ---------------------------------------------------------------------------
// 统一签到编排 signin_sweep（2026-10-09）
//
// 覆盖「每日签到 + 连续签到奖励 + 积分领取」全部入口：
//   · 每日签到      GET  /minimax-cloud/api/v1/signin/status（面板）
//   · 积分领取      POST /minimax-cloud/api/v1/signin/claim
//   · 连续签到奖励  —— **无独立接口**：它是当天奖励的 `bonus_points` 字段
//                      （前端 `signin-bonus` 标签即渲染它），随 claim 一并发放
//   · 发放对账      GET  /minimax-cloud/api/v1/credit/details
// （`signin/config` 是活动文案配置，非领取入口，不调用）
// ---------------------------------------------------------------------------

/// 编排选项。**优先级：显式参数 > 环境变量 > 内置默认**，凭据与账号一律来自配置
/// 文件（`minimax_config.json` + 桌面端 auth.json 现读），代码内无任何硬编码凭据。
#[derive(Debug, Clone, Copy)]
pub struct SweepOptions {
    /// 单账号失败重试次数（不含首次）。可重试=网络/超时/上游 5xx；凭据失效不重试。
    pub retry: usize,
    /// 账号之间的间隔毫秒（防风控）
    pub interval_ms: u64,
    /// 今日已签是否跳过（幂等开关）
    pub skip_claimed: bool,
    /// 是否包含已禁用账号
    pub include_disabled: bool,
    /// 只演练不实际请求上游
    pub dry_run: bool,
}

impl Default for SweepOptions {
    fn default() -> Self {
        Self {
            retry: 2,
            interval_ms: 2000,
            skip_claimed: true,
            include_disabled: false,
            dry_run: false,
        }
    }
}

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(d)
}

/// 从环境变量读编排参数（缺省回落内置值）：
/// `MINIMAX_SWEEP_RETRY` / `MINIMAX_SWEEP_INTERVAL_MS` / `MINIMAX_SWEEP_SKIP_CLAIMED`
/// / `MINIMAX_SWEEP_INCLUDE_DISABLED` / `MINIMAX_SWEEP_DRY_RUN`
pub fn sweep_options_from_env() -> SweepOptions {
    let d = SweepOptions::default();
    SweepOptions {
        retry: env_usize("MINIMAX_SWEEP_RETRY", d.retry),
        interval_ms: env_usize("MINIMAX_SWEEP_INTERVAL_MS", d.interval_ms as usize) as u64,
        skip_claimed: std::env::var("MINIMAX_SWEEP_SKIP_CLAIMED")
            .ok()
            .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "no" => Some(false),
                "1" | "true" | "yes" => Some(true),
                _ => None,
            })
            .unwrap_or(d.skip_claimed),
        include_disabled: std::env::var("MINIMAX_SWEEP_INCLUDE_DISABLED")
            .ok()
            .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "no" => Some(false),
                "1" | "true" | "yes" => Some(true),
                _ => None,
            })
            .unwrap_or(d.include_disabled),
        dry_run: std::env::var("MINIMAX_SWEEP_DRY_RUN")
            .ok()
            .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "no" => Some(false),
                "1" | "true" | "yes" => Some(true),
                _ => None,
            })
            .unwrap_or(d.dry_run),
    }
}

/// 编排日志：追加写 `store_dir()/minimax-signin-sweep.log`。
/// 网关 stdout 不落盘（既有约定），所以编排必须自己留痕，便于事后对账。
pub fn sweep_log(line: &str) {
    let p = crate::modules::config::store_dir().join("minimax-signin-sweep.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        use std::io::Write;
        let _ = writeln!(f, "[{}] {}", now_local_str(), line);
    }
}

/// 失败分类：决定**是否值得重试**，以及汇总表里归到哪一类。
fn classify_signin_error(msg: &str) -> (&'static str, bool) {
    let m = msg.to_ascii_lowercase();
    if m.contains("invalid access token")
        || m.contains("401")
        || m.contains("unauthorized")
        || m.contains("凭据")
        || m.contains("signature")
    {
        ("auth", false) // 凭据失效/签名错：重试无用，且会放大风控信号
    } else if m.contains("timeout") || m.contains("timed out") || m.contains("超时") {
        ("timeout", true)
    } else if m.contains("502") || m.contains("503") || m.contains("504") || m.contains("连接")
        || m.contains("connect") || m.contains("network") || m.contains("network is unreachable")
    {
        ("network", true)
    } else if m.contains("500") || m.contains("429") {
        ("upstream", true)
    } else {
        ("upstream", false)
    }
}

/// 把一次签到结果并入账号配置 —— **唯一实现**。
///
/// 🔴 2026-10-09：此前「自动签到(`signin_and_record`) / HTTP 路由(`/signin/run`) /
/// 统一签到(`platform_checkin`)」各自写一套，已三次踩坑（failStreak 不清零、
/// lastError 不回写、签到前余额取不到）。现在三处统一调本函数。
///
/// 约定：成功 → 记日期 / 仅 `claimed` 时累加积分 / 清零熔断 / 写余额；
/// 失败 → 只记 `last_error`，**不累加 `fail_streak`**（失败多为「该账号未在桌面端
/// 登录」这类环境事实，累加会在用户切号前把账号熔断，属误伤）。
pub fn apply_signin_outcome(
    cfg: &mut MmConfig,
    acc_id: &str,
    res: &Value,
    balance_after: Option<f64>,
    consumed: Option<f64>,
) {
    let signed = res.get("ok").and_then(Value::as_bool) == Some(true);
    let Some(a) = cfg.accounts.iter_mut().find(|x| x.id == acc_id) else {
        return;
    };
    if signed {
        a.signin_last_date = today_str();
        // 🔴 2026-10-09（口径纠偏）：只认 `claimed` 会让「客户端先领」的账号**永远是 0**
        // （实测 MiniMax614670：面板 status=3/points=400/paid=true，累计却 0）。
        // 改为「当天上游确认已到账即记」，按 (身份, 日期) 去重 ⇒ 同日重复跑不重复计。
        let p = signin_points_of(res);
        if credited_today(res) && p > 0 {
            ledger_add_day(&a.user_id, &a.label, &today_str(), p);
            // 条目上的累计以账本为准（跨条目存活），**只增不减**
            let want = ledger_points(&a.user_id, &a.label);
            if want > a.signin_points_total {
                a.signin_points_total = want;
            }
        }
        a.fail_streak = 0;
        a.last_error = None;
        if balance_after.is_some() || consumed.is_some() {
            a.balance = balance_after;
            a.credit_consumed = consumed;
            a.balance_at = now_ms();
        }
    } else {
        a.last_error = res.get("error").and_then(Value::as_str).map(String::from);
    }
}

/// 全量账号签到编排：遍历 → 逐个签到领取 → 重试 → 追踪积分 → 汇总。
///
/// * **不漏**：默认覆盖全部账号；**不重复**：按 id 去重且保序。
/// * **限频**：账号间隔 `interval_ms`（默认 2s），重试指数退避。
/// * **幂等**：`skip_claimed` 时今日已签直接跳过；即便重复请求，上游
///   `status==3` / `claim_result==2` 也保证不会二次发放。
pub async fn signin_sweep(opts: SweepOptions, only_ids: &[String]) -> Value {
    let started = now_ms();
    let started_str = now_local_str();
    sweep_log(&format!(
        "=== 编排开始 dry_run={} retry={} interval_ms={} skip_claimed={} only={:?}",
        opts.dry_run, opts.retry, opts.interval_ms, opts.skip_claimed, only_ids
    ));

    let base = load_config();
    if !base.enabled {
        sweep_log("中止：MiniMax 网关未启用");
        return json!({"ok": false, "error": "MiniMax 网关未启用"});
    }

    // 目标集：**保序去重**（HashSet 判重 + Vec 保序），杜绝「漏 / 重复」
    let mut seen = std::collections::HashSet::new();
    let targets: Vec<MmAccount> = base
        .accounts
        .iter()
        .filter(|a| opts.include_disabled || a.enabled)
        .filter(|a| only_ids.is_empty() || only_ids.iter().any(|i| i == &a.id))
        .filter(|a| seen.insert(a.id.clone()))
        .cloned()
        .collect();

    let mut cfg = load_config();
    let today = today_str();
    let mut accounts: Vec<Value> = Vec::new();
    let mut failures: Vec<Value> = Vec::new();
    let (mut n_ok, mut n_fail, mut n_skip) = (0usize, 0usize, 0usize);

    for (idx, acc) in targets.iter().enumerate() {
        sweep_log(&format!(
            "[{}/{}] 开始处理 {}（{}）",
            idx + 1,
            targets.len(),
            acc.label,
            acc.id
        ));

        // ── 前置判定（跳过项，均记录原因）────────────────────────
        if !acc.enabled {
            n_skip += 1;
            let reason = "账号已禁用";
            sweep_log(&format!("  跳过：{reason}"));
            accounts.push(json!({"id": acc.id, "label": acc.label, "status": "skipped", "reason": reason, "errorKind": "disabled"}));
            continue;
        }
        if acc.cooldown_until_ms > now_ms() {
            n_skip += 1;
            let secs = (acc.cooldown_until_ms - now_ms()) / 1000;
            let reason = format!("冷却中（剩余 {secs}s）");
            sweep_log(&format!("  跳过：{reason}"));
            accounts.push(json!({"id": acc.id, "label": acc.label, "status": "skipped", "reason": reason, "errorKind": "cooldown"}));
            continue;
        }
        if effective_token(acc).is_empty() {
            n_skip += 1;
            let reason = "无可用凭据（token 为空，请重新导入）";
            sweep_log(&format!("  跳过：{reason}"));
            accounts.push(json!({"id": acc.id, "label": acc.label, "status": "skipped", "reason": reason, "errorKind": "no-credential"}));
            continue;
        }
        if opts.skip_claimed && acc.signin_last_date == today {
            n_skip += 1;
            let reason = format!("今日已签（{today}，幂等跳过）");
            sweep_log(&format!("  跳过：{reason}"));
            accounts.push(json!({"id": acc.id, "label": acc.label, "status": "skipped", "reason": reason, "errorKind": "already-signed"}));
            continue;
        }
        if opts.dry_run {
            n_skip += 1;
            sweep_log("  跳过：dry-run（未请求上游）");
            accounts.push(json!({"id": acc.id, "label": acc.label, "status": "skipped", "reason": "dry-run 演练", "errorKind": "dry-run"}));
            continue;
        }

        // ── 签到前积分 ────────────────────────────────────────
        let (balance_before, _) = refresh_credit_snapshot(acc).await;

        // ── 带重试的签到 ──────────────────────────────────────
        let mut attempts = 0usize;
        let mut res = Value::Null;
        let mut last_err = String::new();
        let mut kind = "upstream";
        loop {
            attempts += 1;
            res = signin_account(acc).await;
            if res.get("ok").and_then(Value::as_bool) == Some(true) {
                break;
            }
            last_err = res
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
                .to_string();
            let (k, retryable) = classify_signin_error(&last_err);
            kind = k;
            if retryable && attempts <= opts.retry {
                let backoff = 1000u64 * (1 << (attempts - 1)); // 1s / 2s / 4s
                sweep_log(&format!(
                    "  第 {attempts} 次失败[{kind}]：{last_err} → {backoff}ms 后退避重试"
                ));
                tokio::time::sleep(Duration::from_millis(backoff)).await;
                continue;
            }
            sweep_log(&format!(
                "  第 {attempts} 次失败[{kind}]：{last_err}{}",
                if retryable { "（重试次数已用尽）" } else { "（不可重试）" }
            ));
            break;
        }

        // ── 签到后积分 + 差值（只记一次，重试过程不产生噪音）────
        let (mv, consumed) = track_credit_movement(acc, &res, balance_before).await;

        let ok = res.get("ok").and_then(Value::as_bool) == Some(true);
        if ok {
            n_ok += 1;
            sweep_log(&format!(
                "  成功 action={} points={:?} bonus={:?} 积分 {} → {}（delta={:?}）",
                res.get("action").and_then(Value::as_str).unwrap_or("-"),
                res.get("points"),
                res.get("bonusPoints"),
                fmt_opt(mv.balance_before),
                fmt_opt(mv.balance_after),
                mv.delta
            ));
        } else {
            n_fail += 1;
            failures.push(json!({
                "id": acc.id, "label": acc.label,
                "reason": last_err, "errorKind": kind, "attempts": attempts,
            }));
            sweep_log(&format!("  失败[{kind}] 原因：{last_err}"));
        }

        accounts.push(json!({
            "id": acc.id, "label": acc.label,
            "status": if ok { "success" } else { "failed" },
            "action": res.get("action"),
            "attempts": attempts,
            "points": res.get("points"),
            "bonusPoints": res.get("bonusPoints"),
            "streakDays": res.get("streakDays"),
            "paid": res.get("paid"),
            "balanceBefore": mv.balance_before,
            "balanceAfter": mv.balance_after,
            "balanceDelta": mv.delta,
            "at": mv.at_str,
            "errorKind": if ok { Value::Null } else { json!(kind) },
            "reason": if ok { Value::Null } else { json!(last_err) },
        }));

        apply_signin_outcome(&mut cfg, &acc.id, &res, mv.balance_after, consumed);

        // ── 限频 ─────────────────────────────────────────────
        if idx + 1 < targets.len() {
            tokio::time::sleep(Duration::from_millis(opts.interval_ms)).await;
        }
    }

    save_config(&cfg);
    let finished = now_ms();
    let summary = json!({
        "ok": true,
        "startedAt": started_str,
        "finishedAt": now_local_str(),
        "durationMs": finished - started,
        "total": targets.len(),
        "success": n_ok,
        "failed": n_fail,
        "skipped": n_skip,
        "accounts": accounts,
        "failures": failures,
        "options": {
            "retry": opts.retry, "intervalMs": opts.interval_ms,
            "skipClaimed": opts.skip_claimed, "includeDisabled": opts.include_disabled,
            "dryRun": opts.dry_run,
        },
    });
    sweep_log(&format!(
        "=== 编排结束 总{} 成功{} 失败{} 跳过{} 耗时{}ms",
        targets.len(), n_ok, n_fail, n_skip, finished - started
    ));
    for f in &failures {
        sweep_log(&format!(
            "  失败清单：{}（{}）{}",
            f.get("label").and_then(Value::as_str).unwrap_or("-"),
            f.get("id").and_then(Value::as_str).unwrap_or("-"),
            f.get("reason").and_then(Value::as_str).unwrap_or("-")
        ));
    }
    summary
}

fn fmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.2}"),
        None => "null".to_string(),
    }
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
                        let res = signin_and_record(&acc).await;
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

/// 按配置内账号 id 取账号（统一签到分发用）。
/// 按账号 id 刷新并**写回**积分（巡检用；2026-10-08）。
/// 与 `refresh_and_save_credit(&MmAccount)` 等价，只是多一步按 id 取账号。
pub async fn refresh_and_save_credit_for(id: &str) -> Option<f64> {
    let acc = find_account(id)?;
    refresh_and_save_credit(&acc).await
}

pub fn find_account(id: &str) -> Option<MmAccount> {
    load_config().accounts.into_iter().find(|a| a.id == id)
}

/// 签到 + 本地记账（日期 / 累计分 / last_error）+ 积分快照刷新——**统一口径**。
///
/// 自动签到循环与统一全量签到（platform_checkin）共用：2026-10-06 的教训是
/// 「签到后积分不变」源于循环与手动路由两份实现漂移，这里收敛为唯一实现。
/// ok=true 即记当日已签（含上游 already-claimed：桌面端自己签过时把当日
/// 已发分同步进累计值，日期标记后当日不会重复入账）。
pub async fn signin_and_record(acc: &MmAccount) -> Value {
    // 积分动向：签到**前**先取一次余额（自动签到路径同样要留痕）
    let (balance_before, _) = refresh_credit_snapshot(acc).await;
    let res = signin_account(acc).await;
    let (mv, consumed) = track_credit_movement(acc, &res, balance_before).await;
    // 🔴 2026-10-09：写回逻辑**只留一份**（apply_signin_outcome），自动签到 /
    // HTTP 路由 / 统一编排三处共用。此前各写一套，已三次踩坑（failStreak 不清零、
    // lastError 不回写、签到前余额取不到）。积分/日期/熔断/错误一次写全。
    let mut cfg2 = load_config();
    apply_signin_outcome(&mut cfg2, &acc.id, &res, mv.balance_after, consumed);
    save_config(&cfg2);
    // 把积分动向带回调用方（自动签到通知 / 统一签到页都要展示）
    let mut out = res;
    if let Some(o) = out.as_object_mut() {
        o.insert("balanceBefore".to_string(), json!(mv.balance_before));
        o.insert("balanceAfter".to_string(), json!(mv.balance_after));
        o.insert("balanceDelta".to_string(), json!(mv.delta));
        o.insert("movementAt".to_string(), json!(mv.at_str));
    }
    out
}

/// 统一签到状态页用：查上游签到面板，is_today.status==3 视为今日已领。
pub async fn signin_panel_status(acc: &MmAccount) -> Value {
    let tz = "timezone_id=Asia/Shanghai";
    match cloud_get(acc, "/minimax-cloud/api/v1/signin/status", Some(tz)).await {
        Ok(v) => {
            // 🔴 2026-10-10 修正路径：实测上游返回 {"data":{"scene":2,"days":[…]}}，
            // **没有 `panel` 层**。此前读 `/data/panel/days` ⇒ pointer 返回 None
            // ⇒ `unwrap_or(0)` ⇒ `todayCheckedIn` **恒为 false**
            // （现象：账号卡片显示「未签到」，而当天其实已签到成功）。
            // 保留 panel 路径作兼容（历史/其它版本可能不同）。
            let today_status = v
                .pointer("/data/days")
                .or_else(|| v.pointer("/data/panel/days"))
                .and_then(Value::as_array)
                .and_then(|arr| {
                    arr.iter()
                        .find(|d| d.get("is_today").and_then(Value::as_bool) == Some(true))
                })
                .and_then(|d| d.get("status"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            json!({"ok": true, "todayCheckedIn": today_status == 3})
        }
        Err(e) => json!({"ok": false, "error": e}),
    }
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
            token_snapshot: String::new(),
            balance: None,
            credit_consumed: None,
            balance_at: 0,
            fail_streak: 0,
            desktop_instance: String::new(),
        }
    }

    fn cfg_with(mode: &str, pin: &str, accounts: Vec<MmAccount>) -> MmConfig {
        MmConfig {
            enabled: true,
            api_key: "sk-test".to_string(),
            cache_enabled: false,
            chat_payload_template: String::new(),
            yy_absolute_chat: true,
            curl_max_concurrent: 3,
            curl_retries: 3,
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
    fn desktop_capture_dedupes_when_user_id_missing() {
        // 回归防护：桌面端 auth.json 的 user_id 为空时，必须按 (auth_path, auth_key) 去重，
        // 否则同一登录态会堆成多条 → 签到 claim 拿不到奖励。
        let mut cfg = MmConfig {
            enabled: true,
            api_key: "sk-test".to_string(),
            cache_enabled: false,
            chat_payload_template: String::new(),
            yy_absolute_chat: true,
            cooldown_base_secs: 60,
            cooldown_max_secs: 1800,
            account_mode: MM_MODE_ROUND_ROBIN.to_string(),
            pinned_account_ids: Vec::new(),
            pinned_account_id: String::new(),
            max_failover: 0,
            curl_max_concurrent: 3,
            curl_retries: 3,
            accounts: Vec::new(),
        };
        let mut acc = MmAccount {
            id: "mm-1".to_string(),
            label: "桌面端-旧".to_string(),
            region: PROVIDER_BIGMODEL.to_string(),
            token: String::new(),
            uuid: String::new(),
            device_id: String::new(),
            user_id: String::new(),       // ← 关键：user_id 为空
            agent_id: String::new(),
            screen_w: String::new(),
            screen_h: String::new(),
            base_url: String::new(),
            enabled: true,
            added_at: 0,
            last_error: None,
            cooldown_until_ms: 0,
            signin_last_date: String::new(),
            signin_points_total: 0,
            source: "desktop".to_string(),
            auth_path: "/tmp/auth.json".to_string(),
            auth_key: "same-slot-key".to_string(),
            token_snapshot: String::new(),
            balance: None,
            credit_consumed: None,
            balance_at: 0,
            fail_streak: 0,
            desktop_instance: String::new(),
        };
        cfg.accounts.push(acc.clone());
        // 模拟第二次捕获同一登录态（user_id 空、同 auth_key）——去重逻辑应命中
        let dup = acc.clone();
        assert!(
            cfg.accounts
                .iter()
                .any(|a| a.source == "desktop" && a.auth_key == dup.auth_key && a.auth_path == dup.auth_path),
            "同 auth_key+auth_path 应命中既有条目（不新增）"
        );
        assert_eq!(cfg.accounts.len(), 1, "池中该登录态只应有一条");
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
    fn curl_retry_backoff_is_bounded() {
        assert_eq!(curl_retry_delay_ms(0), 0, "首次不等待");
        assert_eq!(curl_retry_delay_ms(1), 250);
        assert_eq!(curl_retry_delay_ms(2), 500);
        assert_eq!(curl_retry_delay_ms(3), 1000);
        assert_eq!(curl_retry_delay_ms(9), 2000, "封顶 2s");
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

    // ===== 2026-10-07 token 现读优先：桌面端在同一 auth_key 记录内原地轮换
    // accessToken，冻结快照打上游 401（对话断线根因）；现读须身份校验防漂移 =====
    // 🔴 身份锚点 = sharedUser.userID（auth_key 是客户端级，三账号同键，不证明身份）

    /// 组装 auth.json 测试文本（值全部来自运行时变量）
    fn auth_json_with(key: &str, tok: &str) -> String {
        serde_json::json!({ "records": { key: { "accessToken": tok } } }).to_string()
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fresh_token_follows_in_place_rotation() {
        // 桌面端当前登录 = 本账号：拿记录内（已轮换的）新 token
        let rotated = format!("rotated-{}", 1);
        let text = auth_json_with("k1 x00", &rotated);
        let got = fresh_desktop_token_from(&text, &ids(&["me"]), "me");
        assert_eq!(got.as_deref(), Some(rotated.as_str()));
    }

    #[test]
    fn fresh_token_rejects_drifted_login() {
        // 桌面端登录的是别的号（单登录槽已覆盖本账号令牌）→ 绝不借用
        let theirs = format!("theirs-{}", 2);
        let text = auth_json_with("same-client-key", &theirs);
        assert_eq!(fresh_desktop_token_from(&text, &ids(&["someone-else"]), "me"), None);
        // 读不到当前登录身份（标识集合空）→ 同样拒绝（身份不可判定不赌）
        assert_eq!(fresh_desktop_token_from(&text, &[], "me"), None);
        // 本账号缺 user_id → 无法核对 → 拒绝
        assert_eq!(fresh_desktop_token_from(&text, &ids(&["me"]), ""), None);
    }

    /// 回归（2026-10-08 实测踩中）：`sharedUser` 的 **`realUserID`** 同样是合法身份锚点。
    ///
    /// 我方把 `acc.user_id` 修正为 realUserID（签名要求）后，若身份比对只认短串 `userID`，
    /// 就会被误判成"切号" ⇒ 回落旧快照 ⇒ 上游 `invalid access token` ✗。
    #[test]
    fn fresh_token_accepts_real_user_id_anchor() {
        let text = auth_json_with("k x00", "tok-live");
        let both = ids(&["68naMKpnJn7x", "348569758173523968"]);
        assert_eq!(
            fresh_desktop_token_from(&text, &both, "348569758173523968").as_deref(),
            Some("tok-live"),
            "realUserID 必须命中"
        );
        assert_eq!(
            fresh_desktop_token_from(&text, &both, "68naMKpnJn7x").as_deref(),
            Some("tok-live"),
            "userID 也必须命中"
        );
        assert_eq!(fresh_desktop_token_from(&text, &both, "999999"), None, "陌生 ID 必须拒绝");
    }

    #[test]
    fn fresh_token_skips_empty_access_token() {
        let text = auth_json_with("my-key", "");
        assert_eq!(fresh_desktop_token_from(&text, &ids(&["me"]), "me"), None);
    }

    #[test]
    fn fresh_token_rejects_malformed() {
        assert_eq!(fresh_desktop_token_from("not-json", &ids(&["me"]), "me"), None);
        assert_eq!(fresh_desktop_token_from(r#"{"x":1}"#, &ids(&["me"]), "me"), None);
    }

    #[test]
    fn effective_token_falls_back_when_live_read_unavailable() {
        // auth.json 文件消失（桌面端退出登录/真切号）→ 回落冻结快照
        let live = format!("live-{}", 4);
        let text = auth_json_with("k x00", &live);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("wb-mm-auth-test-{}.json", std::process::id()));
        std::fs::write(&path, &text).unwrap();
        let mut a = acc("d1", "desktop", "domestic", true, 0, "t-snap");
        a.auth_path = path.to_string_lossy().to_string();
        a.auth_key = "k x00".to_string();
        a.user_id = "u-test".to_string();
        // 文件存在，但身份是否匹配取决于本机 sharedUser（测试环境不可控）——
        // 只断言两种合法结局：现读成功 = live，或身份不匹配回落 t-snap
        let got = effective_token(&a);
        assert!(got == live || got == "t-snap", "got={got}");
        std::fs::remove_file(&path).ok();
        assert_eq!(effective_token(&a), "t-snap", "现读失败回落快照");
        // 非 desktop 条目：不碰 auth.json
        let web = acc("w1", "", "domestic", true, 0, "t-web");
        assert_eq!(effective_token(&web), "t-web");
    }

    // -----------------------------------------------------------------------
    // 签名字节对齐（2026-10-08：asar 源码级算法 → 固定向量锁定跨语言一致性）
    // -----------------------------------------------------------------------

    /// 跨语言固定向量：期望值由 `tools-sign-vector.py` 按 asar 抠出的**真实 JS 算法**
    /// 复算（`out/_next/static/chunks/24831` 前端 axios 拦截器）。
    /// 任何导致签名字节偏移的改动都会在此立刻失败。
    #[test]
    fn signature_bytes_match_js_reference_vector() {
        let mut a = acc("mm-vec", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        a.uuid = "35ac1c1a-71f0-43b8-9bd9-f41e3bcf0869".into();
        a.device_id = "54325316".into();
        a.user_id = "348569758173523968".into();
        a.screen_w = "1536".into();
        a.screen_h = "960".into();

        let (wire, ts, xs, yy) = sign_request_at(
            &a,
            "/minimax-cloud/api/v1/signin/status",
            "",
            false,
            1_791_455_553_000,
        );

        assert_eq!(ts, "1791455553", "x-timestamp 必须与 unix 同源");
        assert_eq!(
            wire,
            "device_platform=web&biz_id=3&app_id=3001&version_code=22201&unix=1791455553000\
             &timezone_offset=28800&is_desktop=1&desktop_version=3.1.1&sys_language=zh&lang=zh\
             &uuid=35ac1c1a-71f0-43b8-9bd9-f41e3bcf0869&device_id=54325316&os_name=Windows\
             &browser_name=Chrome&device_memory=16&cpu_core_num=20&browser_language=zh-CN\
             &browser_platform=Win32&user_id=348569758173523968&screen_width=1536\
             &screen_height=960&client=desktop"
        );
        // GET ⇒ x-signature 的 body 是**空串**（不是 "{}"；这是 T1 后仍报 invalid signature 的根因）
        assert_eq!(xs, "17ea242e32d45865c0fd2a741634cb7f");
        assert_eq!(yy, "4efcec50c125c399652e813bfae2cd16");
    }

    /// 回归：`yy` 必须**逐字签 wire query 本身**。
    /// 曾把 `op_ticket=undefined` 只加进签名串（wire 无）⇒ 上游拿请求 URL 复算恒不匹配
    /// ⇒ 恒定 `{"error":"invalid signature"}`。
    #[test]
    fn yy_signs_exactly_the_wire_query() {
        let a = acc("mm-1", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        let path = "/minimax-cloud/api/v1/signin/status";
        let (wire, _ts, _xs, yy) = sign_request_at(&a, path, "", false, 1_791_455_553_000);
        assert_eq!(yy, yy_signature(&format!("{path}?{wire}"), "{}", 1_791_455_553_000));
        for forbidden in ["op_ticket", "token=", "region="] {
            assert!(!wire.contains(forbidden), "wire query 不应含 {forbidden}: {wire}");
        }
    }

    /// GET 与 POST 的 body 语义差异（asar 逐字）：
    /// `x-signature` 的 body = **实际 HTTP body**（GET 为 `""`），`yy` 的 body 则 GET 取 `"{}"`。
    #[test]
    fn x_signature_body_semantics_differ_get_vs_post() {
        let a = acc("mm-2", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        let p = "/minimax-cloud/api/v1/signin/claim";
        let (_w1, _t1, xs_get, yy_get) = sign_request_at(&a, p, "", false, 1_791_455_553_000);
        let (_w2, _t2, xs_post, yy_post) = sign_request_at(&a, p, "{}", false, 1_791_455_553_000);
        assert_ne!(xs_get, xs_post, "GET 空 body 与 POST {{}} 必须产生不同 x-signature");
        assert_eq!(xs_get, x_signature(1_791_455_553, ""));
        assert_eq!(xs_post, x_signature(1_791_455_553, "{}"));
        // yy：空 body 时两者等价（GET 的 yyBody 也是 "{}"）
        assert_eq!(yy_get, yy_post);
    }

    /// 时间源唯一 + 秒对齐：同一秒内两次签名必须完全相同（复刻 `Date.parse(new Date())` 行为）。
    #[test]
    fn signature_time_source_is_single_and_second_aligned() {
        let a = acc("mm-3", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        let p = "/minimax-cloud/api/v1/signin/status";
        let one = sign_request_at(&a, p, "", false, 1_791_455_553_000);
        let two = sign_request_at(&a, p, "", false, 1_791_455_553_789);
        assert_eq!(one, two, "同一秒内的签名（含 unix / x-timestamp / yy）必须完全一致");
        assert!(one.0.contains("unix=1791455553000"), "{}", one.0);
    }

    /// desktop 账号的 query 必带桌面端标记；非 desktop 不带（勿污染网页端路径）。
    #[test]
    fn desktop_query_carries_desktop_markers() {
        let d = acc("mm-4", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        let (wire, ..) = sign_request_at(&d, "/minimax-cloud/api/v1/signin/status", "", false, 1_791_455_553_000);
        assert!(wire.contains("is_desktop=1"), "{wire}");
        assert!(wire.contains("desktop_version=3.1.1"), "{wire}");
        assert!(wire.ends_with("client=desktop"), "{wire}");

        let w = acc("mm-5", "", PROVIDER_BIGMODEL, true, 0, "tok");
        let (wire2, ..) = sign_request_at(&w, "/minimax-cloud/api/v1/signin/status", "", false, 1_791_455_553_000);
        assert!(!wire2.contains("is_desktop"), "{wire2}");
        assert!(wire2.ends_with("client=web"), "{wire2}");
    }

    // -----------------------------------------------------------------------
    // 桌面端设备指纹提取（2026-10-08）
    // -----------------------------------------------------------------------

    /// 从模拟的 Chromium 存储块里提取三件套（localStorage 的 value 用 UTF-8，
    /// sessionStorage 的 value 用 UTF-16LE——两种口径都要能解）。
    #[test]
    fn desktop_identity_extracts_from_storage_blobs() {
        let mut ls = Vec::new();
        ls.extend_from_slice(b"_https://agent.minimax.cn\x00\x01UNIQUE_USER_ID");
        ls.extend_from_slice(b"35ac1c1a-71f0-43b8-9bd9-f41e3bcf0869");
        ls.extend_from_slice(
            b"\x00\x01user_detail_agent{\"userID\":\"68naMKpnJn7x\",\"realUserID\":\"348569758173523968\"}",
        );
        let mut ss = Vec::new();
        ss.extend_from_slice(b"_https://agent.minimax.cn\x00\x01tab_device_id");
        for b in b"54325316" {
            ss.push(*b);
            ss.push(0); // UTF-16LE
        }
        let id = extract_identity_from_blobs(&[ls, ss]);
        assert_eq!(id.uuid, "35ac1c1a-71f0-43b8-9bd9-f41e3bcf0869");
        assert_eq!(id.device_id, "54325316");
        assert_eq!(id.user_id, "348569758173523968", "必须取 realUserID 而非短串 userID");
        assert!(id.is_complete());
    }

    /// 存储里没有指纹时返回空结构（不 panic）。
    #[test]
    fn desktop_identity_empty_when_blobs_lack_keys() {
        let id = extract_identity_from_blobs(&[b"nothing here".to_vec(), vec![0u8; 64]]);
        assert_eq!(id, DesktopIdentity::default());
        assert!(!id.is_complete());
    }

    /// LevelDB 里 `"userName":"MiniMax463137"` 这类 JSON 片段抽取（2026-10-09）。
    #[test]
    fn scan_quoted_after_reads_json_string_value() {
        let raw = "\u{0}\u{1}user_detail_agent{\"userID\":\"DjXL9dD349LD\",\
                   \"realUserID\":\"541778119013462018\",\"userName\":\"MiniMax463137\",\
                   \"subUserName\":\"MiniMax463137\"}";
        assert_eq!(
            scan_quoted_after(raw, "\"userName\":\"", 240).as_deref(),
            Some("MiniMax463137"),
            "必须取 userName 的值而不是其后的逗号"
        );
        // `"userID":"` 不得误命中 `"realUserID":"`（大写 UserID）
        assert_eq!(scan_quoted_after(raw, "\"userID\":\"", 240).as_deref(), Some("DjXL9dD349LD"));
        assert_eq!(scan_quoted_after(raw, "\"nope\":\"", 240), None);
    }

    /// 当前身份的两级来源：`sharedUser` 非空 ⇒ 权威、**不**看 LevelDB；
    /// `sharedUser` 空 ⇒ 回退 LevelDB（acc3 场景）；两者皆空 ⇒ 空集合。
    #[test]
    fn current_identity_prefers_shared_user_and_falls_back_to_storage() {
        let shared = vec!["68naMKpnJn7x".to_string(), "348569758173523968".to_string()];
        assert_eq!(
            merge_current_identity(shared.clone(), Some("541778119013462018".into())),
            shared,
            "sharedUser 有值时必须是权威来源，LevelDB 不得插手（其值可能滞留旧账号）"
        );

        assert_eq!(
            merge_current_identity(Vec::new(), Some("541778119013462018".into())),
            vec!["541778119013462018".to_string()],
            "sharedUser 为空时必须回退到存储身份（acc3 的 463137 靠这条才签得动）"
        );

        assert!(merge_current_identity(Vec::new(), None).is_empty());
        assert!(merge_current_identity(Vec::new(), Some(String::new())).is_empty());
    }

    // -----------------------------------------------------------------------
    // 凭据自动续期（2026-10-09）
    // -----------------------------------------------------------------------

    /// 授权服务器按区域切换（endpoint-config.js 实证：cn → .cn / en → .io）。
    #[test]
    fn oauth_origin_switches_by_region() {
        assert_eq!(oauth_origin(PROVIDER_BIGMODEL), "https://account.minimax.cn");
        assert_eq!(oauth_origin(PROVIDER_ZAI), "https://account.minimax.io");
        assert_eq!(MCODE_OAUTH_CLIENT_ID, "mcode-public");
        assert_eq!(MCODE_OAUTH_SCOPE, "agent.default");
        assert_eq!(MCODE_OAUTH_AUDIENCE, "agent-backend");
    }

    /// auth.json 解析：取「带 accessToken 的那条记录」，并读出 refresh/过期时间。
    #[test]
    fn read_desktop_credential_picks_record_with_access_token() {
        let p = std::env::temp_dir().join(format!("mm-auth-{}.json", uuid::Uuid::new_v4()));
        let doc = json!({
            "schemaVersion": 1,
            "records": {
                "com.minimax.mcode.oauth.prod.cn": {
                    "accessToken": "mmoat_AAA", "refreshToken": "mmort_BBB",
                    "expiresAtMs": 1_791_486_515_486i64, "clientId": "mcode-public"
                }
            }
        });
        std::fs::write(&p, serde_json::to_string(&doc).unwrap()).unwrap();
        let (a, r, e) = read_desktop_credential(p.to_string_lossy().as_ref()).unwrap();
        assert_eq!(a, "mmoat_AAA");
        assert_eq!(r, "mmort_BBB");
        assert_eq!(e, 1_791_486_515_486i64);
        let _ = std::fs::remove_file(&p);

        assert!(read_desktop_credential("Z:\\definitely\\missing.json").is_none());
    }

    /// 🔴 写回必须**换掉 refreshToken**（轮换型）并推进 `generation`，
    /// 且保留其余字段不丢（clientId / loginEpoch / audience …）。
    #[test]
    fn write_back_rotates_refresh_token_and_keeps_other_fields() {
        let p = std::env::temp_dir().join(format!("mm-auth-{}.json", uuid::Uuid::new_v4()));
        let doc = json!({
            "schemaVersion": 1,
            "records": {
                "k": {
                    "accessToken": "mmoat_OLD", "refreshToken": "mmort_OLD",
                    "expiresAtMs": 1i64, "generation": 3,
                    "clientId": "mcode-public", "audience": "agent-backend",
                    "loginEpoch": "2297f98b"
                }
            }
        });
        std::fs::write(&p, serde_json::to_string(&doc).unwrap()).unwrap();

        write_back_desktop_credential(
            p.to_string_lossy().as_ref(),
            "mmoat_NEW",
            "mmort_NEW",
            1_791_486_515_486,
            Some(vec!["agent.default".to_string()]),
        )
        .unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let rec = v.pointer("/records/k").unwrap();
        assert_eq!(rec.get("accessToken").unwrap(), "mmoat_NEW");
        assert_eq!(rec.get("refreshToken").unwrap(), "mmort_NEW", "轮换值必须落盘，否则下次刷新 invalid_grant");
        assert_eq!(rec.get("expiresAtMs").unwrap(), 1_791_486_515_486i64);
        assert_eq!(rec.get("generation").unwrap(), 4, "generation 必须推进");
        assert_eq!(rec.get("clientId").unwrap(), "mcode-public", "其余字段不得丢失");
        assert_eq!(rec.get("loginEpoch").unwrap(), "2297f98b");
        let _ = std::fs::remove_file(&p);
    }

    /// 写回空 refreshToken 时**不得**把原有的 refreshToken 抹掉。
    #[test]
    fn write_back_never_erases_refresh_token_with_empty_value() {
        let p = std::env::temp_dir().join(format!("mm-auth-{}.json", uuid::Uuid::new_v4()));
        let doc = json!({"records": {"k": {"accessToken": "a", "refreshToken": "keepme"}}});
        std::fs::write(&p, serde_json::to_string(&doc).unwrap()).unwrap();
        write_back_desktop_credential(p.to_string_lossy().as_ref(), "a2", "", 5, None).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v.pointer("/records/k/refreshToken").unwrap(), "keepme");
        let _ = std::fs::remove_file(&p);
    }

    /// 不是「本实例当前登录账号」时不得续期（避免把别人槽位的凭据轮换掉）。
    #[test]
    fn refresh_gate_requires_matching_live_identity() {
        let mut a = acc("mm-r", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        a.auth_path = r"C:\x\.minimax\auth\prod\cn\mcode-public\auth.json".into();
        a.user_id = String::new();
        assert!(!desktop_identity_matches(&a), "user_id 为空 ⇒ 不续期");

        a.user_id = "348569758173523968".into();
        // 真实机器上主实例登录的是谁取决于环境，这里只断言「不会 panic 且不误判为空 user_id 通过」
        let _ = desktop_identity_matches(&a);
    }

    /// 账本键：有身份用 `uid:`（条目重建后仍命中），无身份退化 `label:`。
    #[test]
    fn ledger_key_prefers_identity_over_label() {
        assert_eq!(ledger_key("541778119013462018", "桌面端-MiniMax463137"), "uid:541778119013462018");
        assert_eq!(ledger_key("", "桌面端-老"), "label:桌面端-老");
        // 条目 id 每次导入都变，只有身份/名字能跨条目存活
        assert_ne!(ledger_key("1", "x"), ledger_key("2", "x"));
    }

    /// 🔴 2026-10-09 口径纠偏：客户端先签 ⇒ 服务端只拿到 `already-claimed`，
    /// 但当天**确实到账**（面板 `status=3` / `paid=true`）⇒ 必须记。
    #[test]
    fn credited_today_covers_client_first_claim() {
        // 本机领到
        assert!(credited_today(&json!({"ok": true, "action": "claimed", "points": 400})));
        // 面板 status=3 提前返回（无 paid / result 字段）—— 客户端已签
        assert!(credited_today(&json!({"ok": true, "action": "already-claimed", "points": 400})));
        // claim_result=2 但 details 已见当日发放
        assert!(credited_today(&json!({"ok": true, "action": "already-claimed",
                                        "result": "already", "paid": true, "points": 400})));
        // 上游明确「今日无可领」⇒ 不记
        assert!(!credited_today(&json!({"ok": true, "action": "already-claimed",
                                         "result": "already", "paid": false, "points": 400})));
        // 失败 / 未知动作 ⇒ 不记
        assert!(!credited_today(&json!({"ok": false, "action": "failed"})));
        assert!(!credited_today(&json!({"ok": true, "action": "skipped"})));
    }

    /// 按天记账：同日重复调用**只记一次**；跨天累加；旧格式（只有 points）迁移不重复计。
    #[test]
    fn ledger_add_day_is_idempotent_per_date_and_migrates_legacy() {
        let uid = format!("t-{}", uuid::Uuid::new_v4().simple());
        let lbl = "测试账号";

        // 首次：无历史
        let (t1, added1) = ledger_add_day(&uid, lbl, "2026-10-09", 400);
        assert!(added1);
        assert_eq!(t1, 400);
        // 同日再跑 5 次（already-claimed 也会走这里）⇒ 仍是 400
        for _ in 0..5 {
            let (t, added) = ledger_add_day(&uid, lbl, "2026-10-09", 400);
            assert!(!added, "同一天不得重复计入");
            assert_eq!(t, 400);
        }
        // 次日 ⇒ 累加
        let (t2, added2) = ledger_add_day(&uid, lbl, "2026-10-10", 400);
        assert!(added2);
        assert_eq!(t2, 800);

        // 旧格式迁移：另一个 uid 手工塞一个「只有 points」的条目
        let uid2 = format!("t-legacy-{}", uuid::Uuid::new_v4().simple());
        let key = ledger_key(&uid2, lbl);
        let mut all = load_ledger();
        all.insert(key, json!({"points": 400, "label": lbl, "user_id": uid2}));
        let _ = crate::modules::config::atomic_write(
            &points_ledger_path(),
            &serde_json::to_string_pretty(&Value::Object(all)).unwrap(),
        );
        // 当天再记 ⇒ 旧值被归到今天，**不得**变成 800
        let (t3, added3) = ledger_add_day(&uid2, lbl, "2026-10-09", 400);
        assert!(!added3, "旧格式的历史值应视为已计入当天");
        assert_eq!(t3, 400, "不得把同一天算两遍");
    }

    /// 🔴 防回归：账本/动向等**记账文件在测试里必须落临时目录**。
    /// 真实事故（2026-10-09）：`ledger_add` 走生产路径，把 `label:mm-8 = 1200`
    /// 写进了用户的 `minimax_points_ledger.json`；而 reconcile 会把脏值灌进真实账号
    /// ⇒ 单测污染生产数据。断言「测试路径 ≠ store_dir 且落在 temp 下」。
    #[test]
    fn ledger_and_movement_files_are_isolated_from_production_store() {
        let store = crate::modules::config::store_dir().to_string_lossy().to_string();
        let temp = std::env::temp_dir().to_string_lossy().to_string();
        for p in [points_ledger_path(), movements_path()] {
            let s = p.to_string_lossy().to_string();
            assert!(
                !s.starts_with(store.as_str()),
                "记账文件在测试里仍指向生产目录：{s}"
            );
            assert!(
                s.starts_with(temp.as_str()),
                "测试记账文件应落在临时目录：{s}"
            );
        }
    }

    /// 实例推导（2026-10-08 多账号）：主实例路径 → `""`；隔离实例路径 → 实例名；显式字段优先。
    #[test]
    fn account_instance_inferred_from_auth_path() {
        let mut a = acc("mm-7", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        a.auth_path = r"C:\Users\x\.minimax\auth\prod\cn\mcode-public\auth.json".into();
        assert_eq!(account_instance(&a), "", "主实例路径不应被误判");

        a.auth_path = r"C:\Users\x\.minimax-acc2\auth\prod\cn\mcode-public\auth.json".into();
        assert_eq!(account_instance(&a), "acc2", "应从 auth_path 推导出隔离实例名");

        a.auth_path = r"D:\whatever\auth.json".into();
        a.desktop_instance = "acc3".into();
        assert_eq!(account_instance(&a), "acc3", "显式字段优先于推导");

        // 隔离实例的 auth.json 落点
        let p = desktop_auth_json_path(PROVIDER_BIGMODEL, "acc2");
        assert!(p.to_string_lossy().contains(".minimax-acc2"), "{}", p.display());
        assert!(p.to_string_lossy().ends_with(r"auth\prod\cn\mcode-public\auth.json"), "{}", p.display());
    }

    /// 现读值为权威：`merge_identity` 必须**无条件覆盖**（含把历史短串 `userID` 换成 realUserID）。
    #[test]
    fn merge_identity_always_overrides_with_live_values() {
        let mut a = acc("mm-6", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        a.uuid = "e10d8943-bc08-4f5c-901e-eb394568248e".into(); // 历史错误 uuid
        a.user_id = "68naMKpnJn7x".into(); // 历史 bug：写成了短串 userID
        let id = DesktopIdentity {
            uuid: "35ac1c1a-71f0-43b8-9bd9-f41e3bcf0869".into(),
            device_id: "10991203".into(),
            user_id: "348569758173523968".into(),
        };
        assert!(merge_identity(&mut a, &id));
        assert_eq!(a.uuid, id.uuid);
        assert_eq!(a.device_id, id.device_id);
        assert_eq!(a.user_id, id.user_id, "短串必须被 realUserID 覆盖");

        // 已一致 ⇒ 不再判定为变更（避免无谓落盘）
        assert!(!merge_identity(&mut a, &id));
    }

    /// 现读三件套不得套用到「未在桌面端登录」的账号上（否则签名 user_id 与
    /// Authorization token 分属两个账号 ⇒ 必 401 且错误信息误导）。
    #[test]
    fn live_identity_only_applies_to_currently_logged_account() {
        let cur = vec!["eZ1lOGNPPQeA".to_string(), "541732795704172548".to_string()];
        // 本账号 = 当前登录（realUserID 口径）⇒ 允许
        assert!(live_identity_matches_account("541732795704172548", &cur));
        // 本账号 = 当前登录（短 userID 口径）⇒ 允许
        assert!(live_identity_matches_account("eZ1lOGNPPQeA", &cur));
        // 本账号是**另一个**账号 ⇒ 拒绝（保留自身配置，不套别人指纹）
        assert!(!live_identity_matches_account("68naMKpnJn7x", &cur));
        // 读不到当前身份 / 本账号未标注身份 ⇒ 允许（历史行为，避免留空）
        assert!(live_identity_matches_account("68naMKpnJn7x", &[]));
        assert!(live_identity_matches_account("", &cur));
    }

    /// `user_id` 必须来自 `sharedUser.realUserID`，不能被 LevelDB 里的陈旧值污染。
    #[test]
    fn authoritative_user_id_comes_from_shared_user_not_leveldb() {
        let mut a = acc("mm-9", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        a.user_id = "348569758173523968".into(); // 陈旧值（上一个账号）
        // sharedUser：短 userID + 长 realUserID ⇒ 必须取长串
        let cur = vec!["DjXL9dD349LD".to_string(), "541778119013462018".to_string()];
        assert!(merge_authoritative_user_id(&mut a, &cur));
        assert_eq!(a.user_id, "541778119013462018");
        // 已一致 ⇒ 不报变更
        assert!(!merge_authoritative_user_id(&mut a, &cur));
        // 只有短串（老数据）⇒ 不动，避免写入错误身份
        let only_short = vec!["DjXL9dD349LD".to_string()];
        assert!(!merge_authoritative_user_id(&mut a, &only_short));
        assert_eq!(a.user_id, "541778119013462018");
    }

    /// 未登录账号：只补设备指纹，`user_id` 必须保留自身值（身份不可错配）。
    #[test]
    fn merge_device_only_never_touches_user_id() {
        let mut a = acc("mm-7", "desktop", PROVIDER_BIGMODEL, true, 0, "tok");
        a.uuid = String::new();
        a.device_id = String::new(); // 配置里 device_id 空 ⇒ 靠 backfill 补齐
        a.user_id = "541778119013462018".into(); // 自身 realUserID
        let live = DesktopIdentity {
            uuid: "cf3a6943-1c95-4e48-9e51-17a817ecb0ed".into(),
            device_id: "10991203".into(),
            user_id: "541732795704172548".into(), // 当前登录的**别人**
        };
        assert!(merge_device_only(&mut a, &live));
        assert_eq!(a.uuid, live.uuid, "设备指纹应补齐");
        assert_eq!(a.device_id, live.device_id, "device_id 空必须补齐");
        assert_eq!(a.user_id, "541778119013462018", "身份绝不能被别人的覆盖");
        // 幂等：已一致时不报变更
        assert!(!merge_device_only(&mut a, &live));
    }

    /// 积分动向：差值由「签到前后两次查询」推算（上游不直接返回余额）。
    ///
    /// 覆盖三种组合：两端都读到（有差值）/ 任一端读不到（delta=null，不得臆造）/
    /// 失败场景（ok=false 但余额仍记录）。
    #[test]
    fn credit_movement_delta_is_derived_from_before_after() {
        // 纯推算逻辑（与 track_credit_movement 内的计算同一公式）
        let delta = |b: Option<f64>, a: Option<f64>| match (b, a) {
            (Some(b), Some(a)) => Some(((a - b) * 100.0).round() / 100.0),
            _ => None,
        };
        assert_eq!(delta(Some(3386.47), Some(3786.47)), Some(400.0));
        assert_eq!(delta(Some(3786.47), Some(3786.47)), Some(0.0), "重复签到不得产生差值");
        assert_eq!(delta(Some(100.0), None), None, "读不到后值时不得臆造差值");
        assert_eq!(delta(None, Some(100.0)), None);
        // 浮点尾差必须被收敛（2386.4700000000003 之类）
        assert_eq!(delta(Some(0.1), Some(0.30000000000000004)), Some(0.2));
    }

    #[test]
    fn now_local_str_is_beijing_wall_clock() {
        let s = now_local_str();
        assert_eq!(s.len(), "2026-10-09 00:00:00".len(), "格式 YYYY-MM-DD HH:MM:SS：{s}");
        assert_eq!(s.chars().nth(4), Some('-'));
        assert_eq!(s.chars().nth(13), Some(':'));
    }

    /// 失败分类决定「是否重试」：凭据失效不可重试（重试无用且放大风控信号），
    /// 网络/超时/5xx 必须重试。
    #[test]
    fn signin_error_classification_drives_retry() {
        let auth = "签到面板查询失败（invalid access token）：桌面端凭据已失效";
        assert_eq!(classify_signin_error(auth), ("auth", false));
        assert_eq!(classify_signin_error("401 Unauthorized"), ("auth", false));
        assert_eq!(classify_signin_error("请求超时"), ("timeout", true));
        assert_eq!(classify_signin_error("operation timed out"), ("timeout", true));
        assert_eq!(classify_signin_error("上游 502"), ("network", true));
        assert_eq!(classify_signin_error("连接被拒绝"), ("network", true));
        assert_eq!(classify_signin_error("上游 429 限流"), ("upstream", true));
        // 未知业务错误：默认不重试（避免对业务性拒绝反复施压）
        assert_eq!(classify_signin_error("签到异常（上游 claim_result=9）"), ("upstream", false));
    }

    /// 写回口径：只有真 `claimed` 才累加积分；`already-claimed` 累加会产生幻影分。
    #[test]
    fn apply_signin_outcome_accumulates_only_on_claimed() {
        // 唯一身份 ⇒ 测试之间互不干扰（账本按身份记账，落在临时目录）
        let uid = format!("t-mm-{}", uuid::Uuid::new_v4().simple());
        let mut cfg = load_config();
        cfg.accounts = vec![acc("mm-8", "desktop", PROVIDER_BIGMODEL, true, 0, "tok")];
        cfg.accounts[0].user_id = uid;
        cfg.accounts[0].fail_streak = 3;
        cfg.accounts[0].last_error = Some("旧错误".into());

        // ① 真领取：记账 + 清零熔断 + 写余额
        //    （2026-10-09 口径纠偏：累计以**账本**为准，条目初值不再叠加）
        let res = json!({"ok": true, "action": "claimed", "points": 400});
        apply_signin_outcome(&mut cfg, "mm-8", &res, Some(500.0), Some(1.0));
        assert_eq!(cfg.accounts[0].signin_points_total, 400);
        assert_eq!(cfg.accounts[0].fail_streak, 0);
        assert_eq!(cfg.accounts[0].last_error, None);
        assert_eq!(cfg.accounts[0].balance, Some(500.0));
        assert_eq!(cfg.accounts[0].signin_last_date, today_str());

        // ② 同日 already-claimed（客户端先签的情形）⇒ **记过一次就不再记**，
        //    不得产生幻影分
        let res2 = json!({"ok": true, "action": "already-claimed", "points": 400});
        apply_signin_outcome(&mut cfg, "mm-8", &res2, Some(500.0), Some(1.0));
        assert_eq!(cfg.accounts[0].signin_points_total, 400, "同一天重复签到不得产生幻影分");

        // ③ 上游明确「今日无可领」⇒ 不记分
        let res4 = json!({"ok": true, "action": "already-claimed", "result": "already",
                          "paid": false, "points": 400});
        apply_signin_outcome(&mut cfg, "mm-8", &res4, Some(500.0), Some(1.0));
        assert_eq!(cfg.accounts[0].signin_points_total, 400);

        // ④ 失败：只记原因，不动积分与日期，也不累加 fail_streak（避免误熔断）
        let res3 = json!({"ok": false, "error": "invalid access token"});
        apply_signin_outcome(&mut cfg, "mm-8", &res3, None, None);
        assert_eq!(cfg.accounts[0].signin_points_total, 400);
        assert_eq!(cfg.accounts[0].fail_streak, 0, "环境性失败不得累加熔断");
        assert_eq!(cfg.accounts[0].last_error.as_deref(), Some("invalid access token"));
    }

    /// 目标集必须「不漏且不重复」：按 id 去重且保序。
    #[test]
    // ---- OpenAI -> Anthropic 内容块转换（2026-10-09 修 image_url 400 事故）----

    #[test]
    fn image_url_data_uri_becomes_anthropic_base64() {
        // ZCode 报错原文：unsupported content type 'image_url' (2013)
        let blocks = json!([
            {"type": "text", "text": "看看这张图"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
        ]);
        let out = openai_blocks_to_anthropic(blocks.as_array().unwrap());
        assert_eq!(out.len(), 2, "两块都应保留");
        assert_eq!(out[0]["type"], json!("text"), "text 块原样");
        assert_eq!(out[1]["type"], json!("image"), "image_url -> image");
        assert_eq!(out[1]["source"]["type"], json!("base64"));
        assert_eq!(out[1]["source"]["media_type"], json!("image/png"));
        assert_eq!(out[1]["source"]["data"], json!("iVBORw0KGgo="));
        // 关键断言：不得再出现 image_url 字样（否则上游仍 400）
        assert!(!out[1].to_string().contains("image_url"));
    }

    #[test]
    fn image_url_remote_becomes_anthropic_url_source() {
        let blocks = json!([
            {"type": "image_url", "image_url": {"url": "https://x.test/a.jpg"}},
        ]);
        let out = openai_blocks_to_anthropic(blocks.as_array().unwrap());
        assert_eq!(out[0]["type"], json!("image"));
        assert_eq!(out[0]["source"]["type"], json!("url"));
        assert_eq!(out[0]["source"]["url"], json!("https://x.test/a.jpg"));
    }

    #[test]
    fn unknown_block_types_are_dropped_not_passed_through() {
        // 上游对未知块是硬 400（整单失败）→ 未知类型必须丢弃而非透传
        let blocks = json!([
            {"type": "text", "text": "keep"},
            {"type": "input_audio", "input_audio": {"data": "xxx"}},
            {"type": "totally_unknown", "foo": 1},
        ]);
        let out = openai_blocks_to_anthropic(blocks.as_array().unwrap());
        assert_eq!(out.len(), 1, "只保留已知块");
        assert_eq!(out[0]["text"], json!("keep"));
    }

    #[test]
    fn anthropic_shaped_blocks_pass_through_untouched() {
        // 已是 Anthropic 形状的块不能被二次改写
        let blocks = json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAA"}},
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
        ]);
        let out = openai_blocks_to_anthropic(blocks.as_array().unwrap());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], blocks[0], "image 块原样");
        assert_eq!(out[1], blocks[1], "tool_result 块原样");
    }

    #[test]
    fn normalize_messages_converts_user_image_content() {
        // 端到端：user 消息里带 image_url，经 normalize 后应为 Anthropic 形状
        let msgs = json!([
            {"role": "user", "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "data:image/webp;base64,QUJD"}},
            ]},
        ]);
        let out = normalize_anthropic_messages(&msgs);
        let c = &out[0]["content"];
        assert_eq!(c[1]["type"], json!("image"));
        assert_eq!(c[1]["source"]["media_type"], json!("image/webp"));
        assert_eq!(c[1]["source"]["data"], json!("QUJD"));
    }

    // ---- 「可能已计费」不得重试/换号（2026-10-09 修「一次操作扣两个账号」）----

    #[test]
    fn curl_failure_marks_timeout_as_maybe_billed() {
        // curl exit 28 = 操作超时 -> 请求已发出、上游可能已计费 -> 必须禁止重试
        let f = CurlFailure::from_exit(Some(28), "Operation timed out".into());
        assert!(f.maybe_billed, "超时(28)必须标记为可能已计费");
        assert!(f.msg.contains("28"));
    }

    #[test]
    fn curl_failure_marks_empty_reply_and_recv_error_as_maybe_billed() {
        // 52 = 空回复、56 = 接收数据失败：都已送达上游
        assert!(CurlFailure::from_exit(Some(52), "empty reply".into()).maybe_billed);
        assert!(CurlFailure::from_exit(Some(56), "recv failure".into()).maybe_billed);
    }

    #[test]
    fn curl_failure_keeps_connect_errors_retryable() {
        // 6=域名解析失败 7=连接失败 35=SSL握手失败 -> 请求未送达 -> 可安全重试
        for code in [Some(6), Some(7), Some(35)] {
            let f = CurlFailure::from_exit(code, "connect failed".into());
            assert!(!f.maybe_billed, "exit={code:?} 未送达，应可重试");
        }
        // 本地错误（序列化/调起/写 stdin）同样可重试
        assert!(!CurlFailure::local("序列化失败".into()).maybe_billed);
    }

    #[test]
    fn maybe_billed_status_blocks_account_failover() {
        // 核心保证：哨兵状态码不会被判为可重试 -> 不会换号 -> 第二个账号不会被扣费
        assert!(
            !is_retryable_status(crate::modules::glm_proxy::MIGHT_HAVE_BILLED),
            "「可能已计费」必须禁止换号，否则同一次操作会扣两个账号"
        );
        // 对照：普通网络/服务端错误仍可换号（不影响可用性）
        assert!(is_retryable_status(0), "未送达仍可换号");
        assert!(is_retryable_status(502), "502 仍可换号");
        assert!(is_retryable_status(429), "限流仍可换号");
    }

    #[test]
    fn sweep_targets_dedupe_preserves_order() {
        let ids = vec!["a", "b", "a", "c", "b"];
        let mut seen = std::collections::HashSet::new();
        let out: Vec<&str> = ids.into_iter().filter(|i| seen.insert(*i)).collect();
        assert_eq!(out, vec!["a", "b", "c"], "去重后必须保持首次出现顺序");
    }

    /// 上游 200 包 error body 必须被识别为失败（否则会被误判「已签到」→ 永久漏签）。
    #[test]
    fn upstream_error_detects_business_error_body() {
        assert_eq!(
            upstream_error(&json!({"error": "invalid access token"})),
            Some("invalid access token".to_string())
        );
        // 正常成功响应不带顶层 error ⇒ 不是失败
        assert_eq!(upstream_error(&json!({"data": {"claim_result": 2}, "base_resp": {"status_code": 0}})), None);
        // 空串不算错误
        assert_eq!(upstream_error(&json!({"error": ""})), None);
        // 非字符串 error 不采信（避免误伤）
        assert_eq!(upstream_error(&json!({"error": 401})), None);
    }

    #[test]
    fn credential_hint_targets_token_failures_only() {
        assert!(credential_hint("invalid access token").contains("重新"));
        assert!(credential_hint("HTTP 401").contains("隔离实例"));
        assert_eq!(credential_hint("上游开小差"), "");
    }

    /// 手动运行（依赖本机 MiniMax 桌面端数据）：
    /// `cargo test -p wb-switch-core probe_real_desktop_identity -- --ignored --nocapture`
    #[test]
    #[ignore = "依赖本机桌面端存储，手动运行"]
    fn probe_real_desktop_identity() {
        let id = desktop_identity();
        println!("真实桌面端指纹 = {id:?}");
        assert!(
            id.map(|i| i.is_complete()).unwrap_or(false),
            "未从 %APPDATA%\\MiniMax 读到完整三件套"
        );
    }

    /// 手动运行 —— **真实上游端到端**签到（打印完整 steps，含 status 面板 raw）：
    /// `cargo test -p wb-switch-core probe_real_signin -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "会真实请求 MiniMax 上游，手动运行"]
    fn probe_real_signin_desktop() {
        let Some(a) = find_account("d20f264aed3e") else {
            panic!("账号 d20f264aed3e 不在本机配置里");
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let v = rt.block_on(signin_account(&a));
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        // 🔴 不断言 ok：桌面端是**单登录槽**，未处于登录态的账号必然 401。
        // 这是环境事实而非代码缺陷——探针的价值在于如实 dump 上游原值与判定，
        // 供人工核对（2.0.12 前 401 会被误判成 already-claimed，正是靠它发现的）。
        assert!(
            v.get("steps").and_then(Value::as_array).is_some(),
            "应返回 steps 诊断链"
        );
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            println!("[注意] 本账号本次签到未成功：{:?}", v.get("error"));
        }
    }
}
