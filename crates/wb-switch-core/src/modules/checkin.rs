//! 签到：状态查询 / 执行签到 / 自动签到调度。
//!
//! 对照 server.py `get_checkin_status` / `perform_checkin` /
//! `checkin_account` / `run_checkin_cycle` / `_checkin_request` /
//! `_is_unauthorized`。

use chrono::Local;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::modules::account::{account_display_name, build_auth_headers};
use crate::modules::config::{
    add_checkin_log, apply_region_headers, http_request, load_checkin_config, load_checkin_logs,
    now_ms, workbuddy_api_endpoint_for, RunFlagGuard, CHECKIN_API_PREFIX,
};
use crate::modules::platform_checkin::{
    account_platform, all_checkin_accounts, checkin_platform_account, get_platform_status,
    PLATFORM_WORKBUDDY,
};
use crate::modules::refresh::{ensure_fresh_token, refresh_account_token};

static CHECKIN_RUNNING: AtomicBool = AtomicBool::new(false);
static CHECKIN_ACCOUNTS_RUNNING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Automatic recovery cadence shared by every host.
pub const CHECKIN_RECOVERY_INTERVAL: Duration = Duration::from_secs(30 * 60);

// ─────────────────────────────────────────────────────────────────────
// 失败退避 /「需重新登录」状态（2026-10-08）
//
// 修复的现状：登录态过期的账号被当作普通失败，按 5min 节奏**无限重试**
// （实测单日 6 次），徒增请求且可能招致风控；同时「为什么没签、何时再试」
// 没有任何落盘记录。现引入 per-account 退避状态 + 错误分类。
// ─────────────────────────────────────────────────────────────────────

/// 退避状态文件名（与日志/配置同目录，`~/.wb-switch/`）。
const RETRY_STATE_FILE: &str = "checkin_retry_state.json";

/// 单账号签到的**整体墙钟上限**：超时即跳过该账号，不让单账号拖死整轮。
pub const ACCOUNT_CHECKIN_TIMEOUT: Duration = Duration::from_secs(45);

/// 连续「登录态类」失败达到该次数 ⇒ 判定需人工重新登录，**当日不再自动重试**。
pub const AUTH_GIVE_UP_ATTEMPTS: u32 = 2;

/// 错误分类：登录态/凭据类重试无意义，必须人工介入。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckinErrorKind {
    /// 登录态/凭据/令牌过期 —— 重试无用，需重新登录或重新导入 token
    Auth,
    /// 瞬时故障（网络/超时/5xx）—— 退避后可重试
    Transient,
}

/// 按错误文案判定失败性质（**纯函数**）。
pub fn classify_checkin_error(msg: &str) -> CheckinErrorKind {
    let m = msg.to_lowercase();
    const AUTH_HINTS: [&str; 12] = [
        "登录态", "凭据", "过期", "未登录", "重新登录", "登录凭证", "token", "refresh",
        "20403", "auth", "claim_result=0", "paid=false",
    ];
    if AUTH_HINTS.iter().any(|h| m.contains(h)) {
        CheckinErrorKind::Auth
    } else {
        CheckinErrorKind::Transient
    }
}

/// 指数退避时长（毫秒，**纯函数**）：5min → 10 → 20 → 40 …，封顶 6h；
/// `Auth` 类直接给满 6h（当天基本不再打扰）。
pub fn retry_delay_ms(attempts: u32, kind: CheckinErrorKind) -> u64 {
    const BASE_MS: u64 = 5 * 60 * 1000;
    const CAP_MS: u64 = 6 * 60 * 60 * 1000;
    if kind == CheckinErrorKind::Auth {
        return CAP_MS;
    }
    let shift = attempts.saturating_sub(1).min(7); // 5min×2^7 = 640min > 6h 上限 ⇒ 封顶可达
    BASE_MS.saturating_mul(1u64 << shift).min(CAP_MS)
}

fn retry_state_path() -> std::path::PathBuf {
    crate::modules::config::store_dir().join(RETRY_STATE_FILE)
}

/// 读退避状态（缺失/损坏时返回空对象，不阻断签到）。
pub fn load_retry_state() -> Value {
    let path = retry_state_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

/// 写退避状态（沿用既有原子写）。
pub fn save_retry_state(state: &Value) {
    if let Some(obj) = state.as_object() {
        if obj.is_empty() {
            return;
        }
    }
    let text = serde_json::to_string_pretty(state).unwrap_or_default();
    if let Err(e) = crate::modules::config::atomic_write(&retry_state_path(), &text) {
        eprintln!("[自动签到] 退避状态写入失败: {e}");
    }
}

/// 取某账号的退避条目。
pub fn retry_entry<'a>(state: &'a Value, account_id: &str) -> Option<&'a Value> {
    state.get(account_id).filter(|v| v.is_object())
}

/// 是否允许此刻尝试该账号（**幂等核心**，纯函数）：
/// - 无条目 ⇒ 允许
/// - 当日 `needsRelogin` ⇒ 一律不再自动重试
/// - 否则比较 `nextRetryAtMs`
pub fn should_attempt(entry: Option<&Value>, now_ms: i64) -> bool {
    let Some(e) = entry else { return true };
    let same_day = e.get("day").and_then(Value::as_str).unwrap_or("") == current_local_date();
    if same_day && e.get("needsRelogin").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let next = e.get("nextRetryAtMs").and_then(Value::as_i64).unwrap_or(0);
    now_ms >= next
}

/// 是否**所有未完成账号都处于退避/待重新登录** ⇒ 调度可放宽到 30min 兜底档，
/// 避免"因为某几个号一直失败 → 每 5 分钟重刷全部账号"。
pub fn all_pending_accounts_backing_off() -> bool {
    let state = load_retry_state();
    let today = current_local_date();
    let now = now_ms() as i64;
    let logs = load_checkin_logs();
    let pending: Vec<Value> = all_checkin_accounts()
        .into_iter()
        .filter(|acc| !accounts_checked_in_today(std::slice::from_ref(acc), &logs, &today))
        .collect();
    if pending.is_empty() {
        return true;
    }
    pending.iter().all(|acc| match acc.get("id").and_then(Value::as_str) {
        Some(id) => !should_attempt(retry_entry(&state, id), now),
        None => true,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckinCycleMode {
    /// Always verify every account against the server after a host starts.
    StartupVerify,
    /// Re-verify every account against the server during background recovery.
    PeriodicRecovery,
}

#[derive(Debug, Eq, PartialEq)]
enum StatusDecision {
    Already,
    Submit,
    Error(String),
}

struct AccountRunGuard {
    key: String,
}

impl AccountRunGuard {
    fn try_acquire(account: &Value) -> Option<Self> {
        let key = account
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(String::from)
            .unwrap_or_else(|| account_display_name(account));
        let mut running = CHECKIN_ACCOUNTS_RUNNING
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap();
        if !running.insert(key.clone()) {
            return None;
        }
        Some(Self { key })
    }
}

impl Drop for AccountRunGuard {
    fn drop(&mut self) {
        if let Some(running) = CHECKIN_ACCOUNTS_RUNNING.get() {
            running.lock().unwrap().remove(&self.key);
        }
    }
}

/// 判断是否因 token 失效被拒（用于触发刷新重试）。
fn is_unauthorized(resp: &Value) -> bool {
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 401 || code == 403 {
        return true;
    }
    let msg = resp
        .get("message")
        .or_else(|| resp.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    ["unauthorized", "401", "登录", "失效", "过期", "token"]
        .iter()
        .any(|k| msg.contains(k))
}

/// 发签到相关请求；遇到未授权且存在 refresh token 时刷新一次并重试。
/// 端点按账号 domain 选区域（国际账号打 workbuddy.ai，此前硬打国服端点
/// 会对国际账号返回 openresty 401，被当成签到/登录失败展示）。
async fn checkin_request(path: &str, account: &Value) -> Value {
    let base = workbuddy_api_endpoint_for(account);
    let url = format!("{base}{path}");
    let mut headers = build_auth_headers(account);
    apply_region_headers(account, &mut headers);
    let mut resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    if is_unauthorized(&resp)
        && !account
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        let refreshed = refresh_account_token(account.clone()).await;
        let mut headers = build_auth_headers(&refreshed);
        apply_region_headers(&refreshed, &mut headers);
        resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    }
    resp
}

/// 查询签到状态：新接口 checkin-activity-status，失败回退 checkin-status。
pub async fn get_checkin_status(account: &Value) -> Value {
    let resp = checkin_request(
        &format!("{CHECKIN_API_PREFIX}/checkin-activity-status"),
        account,
    )
    .await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 0 || code == 200 {
        let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
        return json!({
            "ok": true,
            "todayCheckedIn": data.get("today_checked_in")
                .or_else(|| data.get("todayCheckedIn"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            "raw": data,
        });
    }
    let resp2 = checkin_request(&format!("{CHECKIN_API_PREFIX}/checkin-status"), account).await;
    let code2 = resp2.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code2 == 0 || code2 == 200 {
        let data = resp2.get("data").cloned().unwrap_or_else(|| json!({}));
        return json!({
            "ok": true,
            "todayCheckedIn": data.get("today_checked_in")
                .or_else(|| data.get("todayCheckedIn"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            "raw": data,
        });
    }
    json!({
        "ok": false,
        "error": resp2.get("message")
            .or_else(|| resp2.get("msg"))
            .and_then(|v| v.as_str())
            .unwrap_or(&format!("code={code2}"))
            .to_string(),
    })
}

/// 执行签到（POST daily-checkin）；服务端返回已签到提示按成功处理。
pub async fn perform_checkin(account: &Value) -> Value {
    let resp = checkin_request(&format!("{CHECKIN_API_PREFIX}/daily-checkin"), account).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 0 || code == 200 {
        return json!({"ok": true, "raw": resp.get("data").cloned().unwrap_or_else(|| json!({}))});
    }
    let msg = resp
        .get("message")
        .or_else(|| resp.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or(&format!("code={code}"))
        .to_string();
    if msg.contains("已签到") || msg.to_lowercase().contains("repeat") {
        return json!({"ok": true, "already": true, "message": msg});
    }
    json!({"ok": false, "error": msg})
}

/// 签到状态查询（按平台路由）：WorkBuddy 走计费接口，平台账号走各自
/// 的客户端签到接口。返回结构统一为 ok / todayCheckedIn / raw。
pub async fn get_checkin_status_for(account: &Value) -> Value {
    if account_platform(account) == PLATFORM_WORKBUDDY {
        get_checkin_status(account).await
    } else {
        get_platform_status(account).await
    }
}

fn decide_from_status(status: &Value) -> StatusDecision {
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        return StatusDecision::Error(
            status
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("查询签到状态失败")
                .to_string(),
        );
    }
    if status.get("todayCheckedIn").and_then(Value::as_bool) == Some(true) {
        StatusDecision::Already
    } else {
        StatusDecision::Submit
    }
}

/// 对单个账号执行完整签到流程：惰性刷新 → 查状态 → 未签到时提交 → 写提交日志。
/// WorkBuddy 以外的平台账号（AutoClaw / TRAE Work CN）由 platform_checkin 处理。
pub async fn checkin_account(account: &Value) -> Value {
    let Some(_account_guard) = AccountRunGuard::try_acquire(account) else {
        return json!({"result": "error", "error": "该账号正在签到，请稍后再试"});
    };
    // 国际版 WorkBuddy（domain .ai）无签到功能：直接返回 unsupported，
    // 不拿国际版 token 去打国服 codebuddy.cn 签到端点（必败且污染失败通知）。
    if crate::modules::platform_checkin::is_intl_workbuddy(account) {
        let entry = json!({
            "result": "unsupported",
            "ts": now_ms(),
            "accountId": account.get("id").cloned().unwrap_or(Value::Null),
            "email": account_display_name(account),
            "error": "国际版 WorkBuddy 账号没有签到功能",
        });
        add_checkin_log(&entry);
        return entry;
    }
    if let Some(store_id) = account.get("autoclawStoreId").and_then(Value::as_str) {
        // AutoClaw 账号池账号：用自身 token 签到（401 自动续期重试）。
        let res = crate::modules::autoclaw_accounts::checkin_stored(store_id).await;
        let result = res
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string();
        let error = res.get("error").and_then(Value::as_str).map(String::from);
        let mut entry = json!({
            "result": result,
            "ts": now_ms(),
            "accountId": account.get("id").cloned().unwrap_or(Value::Null),
            "email": account_display_name(account),
        });
        if let Some(e) = error.clone() {
            entry["error"] = json!(e);
        }
        add_checkin_log(&entry);
        return res;
    }
    if account_platform(account) != PLATFORM_WORKBUDDY {
        let res = checkin_platform_account(account).await;
        let result = res
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string();
        let error = res.get("error").and_then(Value::as_str).map(String::from);
        let mut entry = json!({
            "result": result,
            "ts": now_ms(),
            "accountId": account.get("id").cloned().unwrap_or(Value::Null),
            "email": account_display_name(account),
        });
        if let Some(e) = error.clone() {
            entry["error"] = json!(e);
        }
        add_checkin_log(&entry);
        return res;
    }
    let cfg = load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;
    let status = get_checkin_status(&acc).await;
    match decide_from_status(&status) {
        StatusDecision::Already => {
            let entry = json!({
                "result": "already",
                "ts": now_ms(),
                "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
                "email": account_display_name(&acc),
            });
            add_checkin_log(&entry);
            return json!({"result": "already"});
        }
        StatusDecision::Error(error) => {
            let entry = json!({
                "result": "error",
                "ts": now_ms(),
                "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
                "email": account_display_name(&acc),
                "error": error.clone(),
            });
            add_checkin_log(&entry);
            return json!({"result": "error", "error": error});
        }
        StatusDecision::Submit => {}
    }

    let entry = json!({
        "ts": now_ms(),
        "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
        "email": account_display_name(&acc),
    });
    let res = perform_checkin(&acc).await;
    let result = if res.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        if res.get("already").and_then(|v| v.as_bool()) == Some(true) {
            "already"
        } else {
            "success"
        }
    } else {
        "error"
    };
    let error = if result == "error" {
        res.get("error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };
    let mut entry_map = json!({
        "result": result,
        "ts": entry["ts"],
        "accountId": entry["accountId"],
        "email": entry["email"],
    });
    if let Some(e) = error.clone() {
        entry_map["error"] = json!(e);
    }
    add_checkin_log(&entry_map);
    json!({"result": result, "error": error})
}

pub fn date_str(ts_ms: Option<i64>) -> String {
    let dt = Local::now();
    if let Some(ms) = ts_ms {
        let secs = ms / 1000;
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| dt.format("%Y-%m-%d").to_string())
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

/// 计算当前本地时间距离下一个午夜（次日 00:00:05）的等待时长。
/// 额外保留 5 秒等待外部平台跨天日切完成。最小为 5 秒。
pub fn duration_until_next_midnight() -> Duration {
    let now = Local::now();
    let tomorrow = (now.date_naive() + chrono::Days::new(1))
        .and_hms_opt(0, 0, 5)
        .and_then(|naive| naive.and_local_timezone(Local).single())
        .unwrap_or_else(|| now + chrono::Duration::hours(24));
    let diff = tomorrow.signed_duration_since(now);
    let secs = diff.num_seconds().max(5);
    Duration::from_secs(secs as u64)
}

/// 获取当前本地日期字符串（YYYY-MM-DD）。
pub fn current_local_date() -> String {
    date_str(None)
}

/// 账号库丢失本机 WorkBuddy 账号时自动补导。
///
/// 2026-09-14 实测：账号库当天不含本机 WorkBuddy 账号（平台账号池照常），
/// 自动签到全天静默跳过 WorkBuddy，用户只能手动签到。账号库会被导入 /
/// 切换 / 轮换等流程改写，一旦丢了本机账号就没有任何环节会补回来；而本机
/// 登录（auth.json）仍在，说明账号只是“掉库”而不是被注销，因此每轮开始
/// 核对一次，缺了就立即补导。本机未登录 WorkBuddy 时属正常情况，静默跳过。
/// 返回是否发生了补导。
fn heal_missing_local_account(accounts: &mut Vec<Value>) -> bool {
    if accounts
        .iter()
        .any(|acc| account_platform(acc) == PLATFORM_WORKBUDDY)
    {
        return false;
    }
    let Ok(meta) = crate::modules::account::import_local() else {
        return false;
    };
    eprintln!(
        "[自动签到] 账号库缺少本机 WorkBuddy 账号，已自动补导：{}",
        meta.get("nickname")
            .or_else(|| meta.get("email"))
            .and_then(Value::as_str)
            .unwrap_or("未知")
    );
    *accounts = all_checkin_accounts();
    true
}

/// 执行一轮自动签到。启动与周期轮次均逐账号查询服务端状态。
///
/// 并发锁防止与手动签到/上一轮重复运行。
/// 一轮签到的聚合结果（供两条调用路径复用）。
pub struct CheckinRoundOutcome {
    pub results: Vec<Value>,
    pub succeeded: usize,
    pub already: usize,
    pub failures: Vec<String>,
    pub backing_off: Vec<String>,
    pub need_relogin: Vec<String>,
}

/// 逐账号执行签到的**唯一实现**（2026-10-08）：
/// ① 退避/需重新登录 ⇒ 跳过（幂等，不徒增请求）
/// ② 每个账号套 `ACCOUNT_CHECKIN_TIMEOUT` 墙钟 ⇒ 单账号卡死不影响其余
/// ③ 失败写退避状态（分类 + 指数退避 + Auth 类连续 2 次 ⇒ 当日停试）
/// ④ 成功/已签到 ⇒ 立即清零退避状态
async fn run_accounts_hardened(accounts: Vec<Value>) -> CheckinRoundOutcome {
    let mut out = CheckinRoundOutcome {
        results: Vec::with_capacity(accounts.len()),
        succeeded: 0,
        already: 0,
        failures: Vec::new(),
        backing_off: Vec::new(),
        need_relogin: Vec::new(),
    };
    let mut retry_state = load_retry_state();
    let today = current_local_date();
    let now = now_ms() as i64;
    for acc in accounts {
        let name = account_display_name(&acc);
        let aid = acc
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !aid.is_empty() && !should_attempt(retry_entry(&retry_state, &aid), now) {
            out.backing_off.push(name.clone());
            out.results.push(json!({
                "accountId": aid, "email": name, "result": "backoff", "error": Value::Null,
            }));
            continue;
        }
        let result = match tokio::time::timeout(ACCOUNT_CHECKIN_TIMEOUT, checkin_account(&acc)).await {
            Ok(r) => r,
            Err(_) => json!({
                "result": "error",
                "error": format!(
                    "该账号签到超过 {}s 未返回，已跳过本轮（其余账号不受影响）",
                    ACCOUNT_CHECKIN_TIMEOUT.as_secs()
                ),
            }),
        };
        let kind_of = result.get("result").and_then(Value::as_str).unwrap_or("");
        match kind_of {
            "success" => out.succeeded += 1,
            "already" => out.already += 1,
            _ => {}
        }
        if kind_of == "error" {
            let err = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
                .to_string();
            out.failures.push(format!("{name}（{err}）"));
            if !aid.is_empty() {
                let kind = classify_checkin_error(&err);
                let prev = retry_entry(&retry_state, &aid)
                    .and_then(|e| e.get("attempts"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                let attempts = prev + 1;
                let needs = kind == CheckinErrorKind::Auth && attempts >= AUTH_GIVE_UP_ATTEMPTS;
                let entry = json!({
                    "attempts": attempts,
                    "kind": match kind { CheckinErrorKind::Auth => "auth", _ => "transient" },
                    "day": today,
                    "nextRetryAtMs": now + retry_delay_ms(attempts, kind) as i64,
                    "needsRelogin": needs,
                    "lastError": err,
                    "updatedAtMs": now,
                });
                if let Some(obj) = retry_state.as_object_mut() {
                    obj.insert(aid.clone(), entry);
                }
                if needs {
                    out.need_relogin.push(format!("{name}（{err}）"));
                }
            }
        } else if !aid.is_empty() {
            if let Some(obj) = retry_state.as_object_mut() {
                obj.remove(&aid);
            }
        }
        out.results.push(json!({
            "accountId": aid,
            "email": name,
            "result": result.get("result").cloned().unwrap_or(Value::Null),
            "error": result.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
    save_retry_state(&retry_state);
    out
}

/// 把一轮签到的**汇总**也写进签到日志（2026-10-08）。
///
/// 背景：网关由 launcher 启动，**stdout 不落盘** ⇒ 调度层的 `println!` 排障时看不见。
/// 这里以一条 `result = "round"` 的汇总条目落进既有日志（页面 / `GET /api/checkin/logs` 可见），
/// 避免"调度是否跑了、跑了什么"只能靠猜。
pub fn log_checkin_round_summary(round: &CheckinRoundOutcome) {
    let summary = format!(
        "成功 {} · 已签 {} · 失败 {} · 退避跳过 {} · 需重新登录 {}",
        round.succeeded,
        round.already,
        round.failures.len(),
        round.backing_off.len(),
        round.need_relogin.len()
    );
    add_checkin_log(&json!({
        "result": "round",
        "ts": now_ms(),
        "accountId": "__round__",
        "email": "[轮次汇总]",
        "error": summary,
    }));
    if !round.need_relogin.is_empty() {
        add_checkin_log(&json!({
            "result": "round",
            "ts": now_ms(),
            "accountId": "__need_relogin__",
            "email": "[需重新登录]",
            "error": round.need_relogin.join("；"),
        }));
    }
}

pub async fn run_checkin_cycle(_mode: CheckinCycleMode) -> Value {
    let Some(_guard) = RunFlagGuard::try_acquire(&CHECKIN_RUNNING) else {
        return json!({"status": "skipped", "reason": "already_running"});
    };
    let cfg = load_checkin_config();
    if cfg.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
        eprintln!("[自动签到] 本轮跳过：自动签到未启用");
        return json!({"status": "disabled"});
    }
    // 自愈：此前因瞬时网络错误被标记失效的 AutoClaw 池账号，先尝试续期复活。
    crate::modules::autoclaw_accounts::recover_invalid_accounts().await;
    let mut accounts = all_checkin_accounts();
    let healed_local_account = heal_missing_local_account(&mut accounts);
    if accounts.is_empty() {
        eprintln!("[自动签到] 本轮跳过：没有任何可签到的账号");
        return json!({"status": "no_accounts"});
    }
    let total = accounts.len();
    let mut summary = json!({
        "status": "ok",
        "healedLocalAccount": healed_local_account,
        "accounts": [],
    });
    let outcome = run_accounts_hardened(accounts).await;
    log_checkin_round_summary(&outcome);
    let failures = outcome.failures;
    let succeeded = outcome.succeeded;
    let already = outcome.already;
    let backing_off = outcome.backing_off;
    let need_relogin = outcome.need_relogin;
    for r in outcome.results {
        summary["accounts"].as_array_mut().unwrap().push(r);
    }
    summary["backoffSkipped"] = json!(backing_off);
    summary["needRelogin"] = json!(need_relogin);
    // 有账号签到失败时推送桌面通知（同类 10 分钟节流，避免刷屏）。
    if !failures.is_empty() {
        crate::modules::notify::notify(
            "checkin-failure",
            "自动签到异常",
            &format!("{} 个账号签到失败：{}", failures.len(), failures.join("；")),
        );
    }
    if !need_relogin.is_empty() {
        crate::modules::notify::notify(
            "checkin-need-relogin",
            "签到需重新登录",
            &format!(
                "{} 个账号登录态/凭据已失效，已暂停今日自动重试：{}",
                need_relogin.len(),
                need_relogin.join("；")
            ),
        );
    }
    eprintln!(
        "[自动签到] 本轮 {} 个账号：成功 {}，已签 {}，失败 {}{}{}{}",
        total,
        succeeded,
        already,
        failures.len(),
        if healed_local_account {
            "（已自动补导本机账号）"
        } else {
            ""
        },
        if backing_off.is_empty() {
            String::new()
        } else {
            format!("，退避跳过 {}", backing_off.len())
        },
        if need_relogin.is_empty() {
            String::new()
        } else {
            format!("，需重新登录 {}", need_relogin.len())
        }
    );
    summary
}

/// True when every stored account has a today's log of `success` or `already`.
///
/// Empty account list is false so the tray keeps offering 一键签到.
pub fn all_accounts_checked_in_today() -> bool {
    accounts_checked_in_today(
        &all_checkin_accounts(),
        &load_checkin_logs(),
        &date_str(None),
    )
}

pub fn accounts_checked_in_today(accounts: &[Value], logs: &[Value], today: &str) -> bool {
    if accounts.is_empty() {
        return false;
    }
    accounts.iter().all(|account| {
        let Some(id) = account.get("id").and_then(Value::as_str) else {
            return false;
        };
        latest_today_result(logs, id, today)
            .map(|result| result == "success" || result == "already")
            .unwrap_or(false)
    })
}

fn latest_today_result<'a>(logs: &'a [Value], account_id: &str, today: &str) -> Option<&'a str> {
    logs.iter()
        .rev()
        .find(|entry| {
            entry.get("accountId").and_then(Value::as_str) == Some(account_id)
                && date_str(entry.get("ts").and_then(Value::as_i64)) == today
        })
        .and_then(|entry| entry.get("result").and_then(Value::as_str))
}

/// 对全部账号立即签到（前端一键签到），含本机发现的平台账号。
pub async fn run_checkin_all() -> Value {
    let Some(_guard) = RunFlagGuard::try_acquire(&CHECKIN_RUNNING) else {
        return json!({"accounts": [], "status": "skipped", "reason": "already_running"});
    };
    let mut accounts = all_checkin_accounts();
    heal_missing_local_account(&mut accounts);
    // 2026-10-08：与自动调度**共用**同一加固循环（超时隔离 / 退避跳过 / 状态落盘 / 成功清零），
    // 此前手动「一键签到」是裸循环 ⇒ 过期账号照样被反复硬刷。
    let outcome = run_accounts_hardened(accounts).await;
    log_checkin_round_summary(&outcome);
    json!({
        "accounts": outcome.results,
        "succeeded": outcome.succeeded,
        "already": outcome.already,
        "failures": outcome.failures,
        "backoffSkipped": outcome.backing_off,
        "needRelogin": outcome.need_relogin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_status_returns_already_without_submission() {
        assert_eq!(
            decide_from_status(&json!({"ok": true, "todayCheckedIn": true})),
            StatusDecision::Already
        );
    }

    #[test]
    fn failed_status_returns_error_without_submission() {
        assert_eq!(
            decide_from_status(&json!({"ok": false, "error": "offline"})),
            StatusDecision::Error("offline".to_string())
        );
    }

    #[test]
    fn unchecked_status_is_the_only_path_to_submission() {
        assert_eq!(
            decide_from_status(&json!({"ok": true, "todayCheckedIn": false})),
            StatusDecision::Submit
        );
    }

    #[test]
    fn same_account_cannot_acquire_two_operation_guards() {
        let account = json!({"id": "checkin-guard-test-account"});
        let first = AccountRunGuard::try_acquire(&account).expect("first operation acquires guard");
        assert!(AccountRunGuard::try_acquire(&account).is_none());
        drop(first);
        assert!(AccountRunGuard::try_acquire(&account).is_some());
    }

    #[tokio::test]
    async fn manual_all_reports_busy_when_cycle_is_running() {
        let _cycle_guard =
            RunFlagGuard::try_acquire(&CHECKIN_RUNNING).expect("test acquires cycle guard");
        let result = run_checkin_all().await;

        assert_eq!(result["accounts"], json!([]));
        assert_eq!(result["status"], "skipped");
        assert_eq!(result["reason"], "already_running");
    }

    #[test]
    fn is_unauthorized_detects_code() {
        assert!(is_unauthorized(&json!({"code": 401})));
        assert!(is_unauthorized(&json!({"code": 403})));
        assert!(!is_unauthorized(&json!({"code": 0})));
    }

    #[test]
    fn checked_in_today_requires_every_account() {
        let accounts = vec![json!({"id": "a"}), json!({"id": "b"})];
        let logs = vec![
            json!({"accountId": "a", "result": "success", "ts": 1_700_000_000_000_i64}),
            json!({"accountId": "b", "result": "already", "ts": 1_700_000_100_000_i64}),
        ];
        let today = date_str(Some(1_700_000_000_000));
        assert!(accounts_checked_in_today(&accounts, &logs, &today));
    }

    #[test]
    fn checked_in_today_false_when_one_failed_last() {
        let accounts = vec![json!({"id": "a"})];
        let logs = vec![
            json!({"accountId": "a", "result": "success", "ts": 1_700_000_000_000_i64}),
            json!({"accountId": "a", "result": "error", "ts": 1_700_000_200_000_i64}),
        ];
        let today = date_str(Some(1_700_000_200_000));
        assert!(!accounts_checked_in_today(&accounts, &logs, &today));
    }

    #[test]
    fn checked_in_today_false_when_empty_or_missing() {
        assert!(!accounts_checked_in_today(&[], &[], "2026-08-19"));
        let accounts = vec![json!({"id": "a"})];
        assert!(!accounts_checked_in_today(&accounts, &[], "2026-08-19"));
    }

    #[test]
    fn test_duration_until_next_midnight_is_reasonable() {
        let dur = duration_until_next_midnight();
        assert!(dur.as_secs() >= 5);
        assert!(dur.as_secs() <= 86405);
        let cur_date = current_local_date();
        assert_eq!(cur_date.len(), 10);
    }
}

#[cfg(test)]
mod checkin_retry_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_auth_errors() {
        for msg in [
            "登录态续期失败：刷新暂时不可用（用户未登录，请重新登录）",
            "DuMate 签到失败（HTTP 200）: {\"message\":{\"global\":\"登录凭证已过期，请重新登录\"}}",
            "TRAE 刷新令牌已失效（code=20403），请打开 TRAE Work CN 重新登录",
            "签到未生效（claim_result=0，paid=false）：账号凭据可能已失效",
        ] {
            assert_eq!(classify_checkin_error(msg), CheckinErrorKind::Auth, "msg={msg}");
        }
        for msg in ["connection reset by peer", "上游 502", "请求超时"] {
            assert_eq!(classify_checkin_error(msg), CheckinErrorKind::Transient, "msg={msg}");
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        let k = CheckinErrorKind::Transient;
        assert_eq!(retry_delay_ms(1, k), 5 * 60_000);
        assert_eq!(retry_delay_ms(2, k), 10 * 60_000);
        assert_eq!(retry_delay_ms(3, k), 20 * 60_000);
        assert_eq!(retry_delay_ms(99, k), 6 * 60 * 60_000, "封顶 6h");
        // 登录态类直接满 6h（当天不再自动重试）
        assert_eq!(retry_delay_ms(1, CheckinErrorKind::Auth), 6 * 60 * 60_000);
    }

    #[test]
    fn should_attempt_respects_backoff_and_relogin_flag() {
        let now = 1_000_000i64;
        assert!(should_attempt(None, now), "无条目 ⇒ 允许");
        // 未到重试时间 ⇒ 跳过
        let e = json!({"day": current_local_date(), "attempts": 1, "nextRetryAtMs": now + 60_000, "needsRelogin": false});
        assert!(!should_attempt(Some(&e), now));
        // 到点 ⇒ 允许
        assert!(should_attempt(Some(&e), now + 60_001));
        // 当日 needsRelogin ⇒ 永不再试（即使已过重试时间）
        let e2 = json!({"day": current_local_date(), "attempts": 2, "nextRetryAtMs": 0, "needsRelogin": true});
        assert!(!should_attempt(Some(&e2), now + 86_400_000));
        // 隔天 ⇒ needsRelogin 失效，恢复尝试（跨天新的一天重新开始）
        let e3 = json!({"day": "2000-01-01", "attempts": 9, "nextRetryAtMs": 0, "needsRelogin": true});
        assert!(should_attempt(Some(&e3), now));
    }

    #[test]
    fn account_timeout_is_bounded() {
        assert!(ACCOUNT_CHECKIN_TIMEOUT <= Duration::from_secs(60), "单账号必须有墙钟上限，防单点拖死整轮");
        assert!(AUTH_GIVE_UP_ATTEMPTS >= 1);
    }
}
