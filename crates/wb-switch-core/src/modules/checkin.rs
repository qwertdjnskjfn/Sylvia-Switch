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
    add_checkin_log, http_request, load_checkin_config, load_checkin_logs, now_ms, RunFlagGuard,
    CHECKIN_API_PREFIX, WORKBUDDY_API_ENDPOINT,
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
async fn checkin_request(path: &str, account: &Value) -> Value {
    let url = format!("{WORKBUDDY_API_ENDPOINT}{path}");
    let headers = build_auth_headers(account);
    let mut resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    if is_unauthorized(&resp)
        && !account
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        let refreshed = refresh_account_token(account.clone()).await;
        let headers = build_auth_headers(&refreshed);
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
    let mut failures: Vec<String> = Vec::new();
    let mut succeeded = 0usize;
    let mut already = 0usize;
    for acc in accounts {
        let result = checkin_account(&acc).await;
        match result.get("result").and_then(Value::as_str) {
            Some("success") => succeeded += 1,
            Some("already") => already += 1,
            _ => {}
        }
        if result.get("result").and_then(Value::as_str) == Some("error") {
            failures.push(format!(
                "{}（{}）",
                account_display_name(&acc),
                result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
            ));
        }
        summary["accounts"].as_array_mut().unwrap().push(json!({
            "email": account_display_name(&acc),
            "result": result.get("result").cloned().unwrap_or(Value::Null),
            "error": result.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
    // 有账号签到失败时推送桌面通知（同类 10 分钟节流，避免刷屏）。
    if !failures.is_empty() {
        crate::modules::notify::notify(
            "checkin-failure",
            "自动签到异常",
            &format!("{} 个账号签到失败：{}", failures.len(), failures.join("；")),
        );
    }
    eprintln!(
        "[自动签到] 本轮 {} 个账号：成功 {}，已签 {}，失败 {}{}",
        total,
        succeeded,
        already,
        failures.len(),
        if healed_local_account {
            "（已自动补导本机账号）"
        } else {
            ""
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
    let mut results: Vec<Value> = Vec::new();
    for acc in accounts {
        let r = checkin_account(&acc).await;
        results.push(json!({
            "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
            "email": account_display_name(&acc),
            "result": r.get("result").cloned().unwrap_or(Value::Null),
            "error": r.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
    json!({"accounts": results})
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
