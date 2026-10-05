//! DuMate 多账号池（契约 docs/unified-accounts-contract.md §1）。
//!
//! 背景：DuMate 桌面端是 Electron 单客户端单登录态，模型引擎（opencode）的
//! INAPP_KEY/USER_ID 绑定桌面端当前登录用户的进程环境 —— 因此**多账号只能在
//! 「签到/积分」（纯 cookie 接口）层落地**，模型调用层仍走本机主账号。
//!
//! - 存储：`~/.wb-switch/dumate_accounts.json`（atomic_write；cookies 明文存盘，
//!   与 trae/autoclaw 池同安全级别，v1 不加密）；
//! - 一号一 IP：每账号可配独立出站代理（`reqwest::Proxy::all`），签到/积分
//!   请求走账号自己的 proxy 切断 IP 维度风控关联；空 = 直连；
//! - id 约定：池账号 `dumate:pool_N`；本机账号仍为 `dumate:local`
//!   （platform_checkin::discover_dumate_account 实时发现，不进池）；
//! - 去重指纹：`bce-user-info` cookie 的值（即 csrftoken 来源值），导入本机
//!   登录态时与既有账号比对，重复直接报「已是同一登录态」；
//! - 签到/积分的幂等口径、成功判定、签到日志与 `dumate:local` **完全同款**
//!   （直接复用 platform_checkin 的同一组函数：loginBonusInfo → hasIssued 短路
//!   → loginBonus → add_checkin_log）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{add_checkin_log, atomic_write, now_ms, now_secs, store_dir};
use crate::modules::dumate::bce_api_with_cookies;
use crate::modules::dumate_credentials::{bce_cookie_header, DumateCookie};
use crate::modules::platform_checkin::{
    assemble_dumate_status, json_code_field, login_bonus_already_text, login_bonus_success,
    PLATFORM_DUMATE,
};

/// 池文件名（位于 `store_dir()`，即 `~/.wb-switch/`）。
const ACCOUNTS_FILE: &str = "dumate_accounts.json";
/// bce 控制台宿主（cookie 头组装用，与 dumate::bce_api 一致）。
const BCE_HOST: &str = "console.bce.baidu.com";
/// 上游签到状态端点（与 platform_checkin::DUMATE_LOGIN_BONUS_INFO 同值）。
const LOGIN_BONUS_INFO: &str = "/api/dumate/points/loginBonusInfo";
/// 上游签到领取端点（与 platform_checkin::DUMATE_LOGIN_BONUS 同值）。
const LOGIN_BONUS: &str = "/api/dumate/points/loginBonus";
/// 积分概览端点（与 dumate::get_credits 同值）。
const QUOTA_OVERVIEW: &str = "/api/dumate/points/quota_overview";

/// 读-改-写串行锁（避免并发导入/更新互相覆盖）。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 池账号条目（契约 §1.1 schema；cookies 元素与 `DumateCookie` 同构）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumatePoolAccount {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cookies: Vec<DumateCookie>,
    /// 一号一 IP：`http://` / `socks5://` 出站代理；空 = 直连。
    #[serde(default)]
    pub proxy: String,
    #[serde(default)]
    pub note: String,
    /// ok | expired | unknown
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub added_at: i64,
    #[serde(default)]
    pub last_checkin_day: String,
}

// ---------------------------------------------------------------------------
// 存储
// ---------------------------------------------------------------------------

fn accounts_path() -> PathBuf {
    store_dir().join(ACCOUNTS_FILE)
}

/// 读取全部池账号（解析失败容错：文件缺失/整体损坏 → 空；单条损坏 → 跳过）。
pub fn load_accounts() -> Vec<DumatePoolAccount> {
    load_accounts_from_text(&std::fs::read_to_string(accounts_path()).unwrap_or_default())
}

/// `load_accounts` 的纯函数内核（便于单测）。
fn load_accounts_from_text(text: &str) -> Vec<DumatePoolAccount> {
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    root.get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<DumatePoolAccount>(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn save_accounts(accounts: &[DumatePoolAccount]) -> Result<(), String> {
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "accounts": accounts,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| format!("写入 DuMate 账号池失败: {e}"))
}

/// 按 id 查找池账号。
pub fn find_account(id: &str) -> Option<DumatePoolAccount> {
    load_accounts().into_iter().find(|a| a.id == id)
}

// ---------------------------------------------------------------------------
// 去重指纹 / cookies 解析
// ---------------------------------------------------------------------------

/// 账号去重指纹：`bce-user-info` cookie 的值（csrftoken 的来源值，
/// 契约 §1.2；一个登录态一个指纹）。
pub fn bce_user_info_of(cookies: &[DumateCookie]) -> String {
    cookies
        .iter()
        .find(|c| c.name == "bce-user-info")
        .map(|c| c.value.clone())
        .unwrap_or_default()
}

/// 从 add/import 的 cookies JSON（数组或 `{cookies:[...]}`）解析 cookie 列表。
/// 逐条校验 name/value 非空，坏条目报出所在序号；全无效 → Err。
fn parse_cookies_json(cookies_json: &Value) -> Result<Vec<DumateCookie>, String> {
    let arr = match cookies_json {
        Value::Array(items) => items.clone(),
        v @ Value::Object(_) => v
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| "cookies JSON 缺少 cookies 数组（应为数组或 {cookies:[...]}）".to_string())?,
        _ => {
            return Err(
                "cookies JSON 格式不对（应为数组或 {cookies:[...]} 对象）".to_string(),
            )
        }
    };
    if arr.is_empty() {
        return Err("cookies 为空数组，请粘贴完整的登录 cookies".to_string());
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let value = item
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() || value.is_empty() {
            return Err(format!("第 {} 条 cookie 缺少 name/value，请检查粘贴内容", i + 1));
        }
        out.push(DumateCookie {
            name,
            value,
            domain: item
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            expiration_date: item.get("expirationDate").and_then(Value::as_f64),
        });
    }
    Ok(out)
}

/// 生成下一个池账号 id：`dumate:pool_{最大序号+1}`（纯函数便于单测）。
fn next_pool_id(accounts: &[DumatePoolAccount]) -> String {
    let mut max = 0u64;
    for a in accounts {
        if let Some(n) = a.id.strip_prefix("dumate:pool_") {
            if let Ok(n) = n.parse::<u64>() {
                max = max.max(n);
            }
        }
    }
    format!("{PLATFORM_DUMATE}:pool_{}", max + 1)
}

/// 缺省显示名（用户没起名时兜底）。
fn default_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "DuMate 账号".to_string()
    } else {
        trimmed.to_string()
    }
}

fn build_account(id: String, name: &str, cookies: Vec<DumateCookie>, proxy: &str, note: &str) -> DumatePoolAccount {
    DumatePoolAccount {
        id,
        name: default_name(name),
        cookies,
        proxy: proxy.trim().to_string(),
        note: note.trim().to_string(),
        status: "unknown".to_string(),
        added_at: now_ms(),
        last_checkin_day: String::new(),
    }
}

// ---------------------------------------------------------------------------
// CRUD（契约 §1.2）
// ---------------------------------------------------------------------------

/// 新增池账号。`cookies_json` 是数组或 `{cookies:[...]}`；逐条校验
/// name/value 非空；id = `pool_{最大序号+1}`。
pub fn add_account(
    name: &str,
    cookies_json: &Value,
    proxy: &str,
    note: &str,
) -> Result<DumatePoolAccount, String> {
    let cookies = parse_cookies_json(cookies_json)?;
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let account = build_account(next_pool_id(&accounts), name, cookies, proxy, note);
    accounts.push(account.clone());
    save_accounts(&accounts)?;
    Ok(account)
}

/// 从本机 DuMate 客户端导入当前登录态到池：
/// 解密 qianfan cookies → 按 `bce-user-info` 值与既有账号去重 → 命名
/// 「本机导入 {MM-DD HH:mm}」入池。
pub fn import_from_client() -> Result<DumatePoolAccount, String> {
    let cookies = crate::modules::dumate_credentials::decrypt_cookies()?;
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let fingerprint = bce_user_info_of(&cookies);
    if !fingerprint.is_empty() {
        for existing in &accounts {
            if bce_user_info_of(&existing.cookies) == fingerprint {
                return Err(format!(
                    "「{}」已是同一登录态，无需重复导入",
                    existing.name
                ));
            }
        }
    }
    let name = format!("本机导入 {}", chrono::Local::now().format("%m-%d %H:%M"));
    let account = build_account(next_pool_id(&accounts), &name, cookies, "", "");
    accounts.push(account.clone());
    save_accounts(&accounts)?;
    Ok(account)
}

/// 更新池账号（仅 name / proxy / note 可改；None = 不变）。
pub fn update_account(
    id: &str,
    name: Option<&str>,
    proxy: Option<&str>,
    note: Option<&str>,
) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let Some(acc) = accounts.iter_mut().find(|a| a.id == id) else {
        return Err(format!("账号不存在: {id}"));
    };
    if let Some(n) = name {
        acc.name = default_name(n);
    }
    if let Some(p) = proxy {
        acc.proxy = p.trim().to_string();
    }
    if let Some(nt) = note {
        acc.note = nt.trim().to_string();
    }
    save_accounts(&accounts)
}

/// 删除池账号。
pub fn remove_account(id: &str) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.id != id);
    if accounts.len() == before {
        return Err(format!("账号不存在: {id}"));
    }
    save_accounts(&accounts)
}

// ---------------------------------------------------------------------------
// 状态落盘辅助（status / lastCheckinDay）
// ---------------------------------------------------------------------------

/// 今日日期（本地时区，YYYY-MM-DD）。
fn today_day() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 更新账号登录态标记（独立小事务；值不变时不落盘）。
fn mark_status(id: &str, status: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(acc) = accounts.iter_mut().find(|a| a.id == id) {
        if acc.status != status {
            acc.status = status.to_string();
            let _ = save_accounts(&accounts);
        }
    }
}

/// 记录「今日已签」日期标记。
fn set_last_checkin_day(id: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(acc) = accounts.iter_mut().find(|a| a.id == id) {
        let day = today_day();
        if acc.last_checkin_day != day {
            acc.last_checkin_day = day;
            let _ = save_accounts(&accounts);
        }
    }
}

// ---------------------------------------------------------------------------
// per-account 签到 / 积分（契约 §1.2：走账号自己的 proxy）
// ---------------------------------------------------------------------------

/// 池账号签到状态：GET loginBonusInfo（账号自己的 cookies + proxy）。
/// 返回契约形态 `{ok, todayCheckedIn, totalTimes, totalPoints, hasIssued, raw}`；
/// 成功把 status 置 ok，登录态失效置 expired。
pub async fn account_status(id: &str) -> Value {
    let Some(acc) = find_account(id) else {
        return json!({"ok": false, "error": format!("DuMate 池账号不存在: {id}")});
    };
    if bce_cookie_header(&acc.cookies, BCE_HOST).0.is_empty() {
        mark_status(id, "expired");
        return json!({
            "ok": false,
            "reason": "login_expired",
            "error": "账号 cookies 已全部失效（过期/域过滤后为空），请重新导入该账号的登录 cookies",
        });
    }
    let (http_status, body) = match bce_api_with_cookies(
        "GET",
        LOGIN_BONUS_INFO,
        None,
        &acc.cookies,
        Some(&acc.proxy),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    // 复用 platform_checkin 的同一组装函数（与 dumate:local 完全同款）。
    let mut status = assemble_dumate_status(http_status, &body);
    if let Some(obj) = status.as_object_mut() {
        // 契约 §1.2：totalTimes / totalPoints / hasIssued 拍平到顶层。
        if let Some(raw) = obj.get("raw").cloned() {
            for key in ["totalTimes", "totalPoints", "hasIssued"] {
                if let Some(val) = raw.get(key) {
                    obj.entry(key.to_string()).or_insert(val.clone());
                }
            }
        }
    }
    if status.get("ok").and_then(Value::as_bool) == Some(true) {
        mark_status(id, "ok");
    } else if status.get("reason").and_then(Value::as_str) == Some("login_expired") {
        mark_status(id, "expired");
    }
    status
}

/// 池账号每日签到：查状态（hasIssued 短路 already）→ POST loginBonus →
/// 成功判定/重复判定/签到日志与 `dumate:local` 完全同款。
/// 返回 `{result: success|already|error, message?, points?, error?}`。
pub async fn account_checkin(id: &str) -> Value {
    let Some(acc) = find_account(id) else {
        return json!({"result": "error", "error": format!("DuMate 池账号不存在: {id}")});
    };
    let email = format!("DuMate · {}", acc.name);

    // 1) 先查状态：hasIssued=true 短路 already（服务端幂等口径，同款）。
    let status = account_status(id).await;
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        return json!({
            "result": "error",
            "error": status
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("查询签到状态失败"),
        });
    }
    if status.get("todayCheckedIn").and_then(Value::as_bool) == Some(true) {
        add_checkin_log(&json!({
            "ts": now_secs(),
            "platform": PLATFORM_DUMATE,
            "accountId": id,
            "email": email,
            "result": "already",
        }));
        set_last_checkin_day(id);
        return json!({"result": "already", "message": "今日已完成 DuMate 签到"});
    }

    // 2) 提交领取：POST loginBonus body {}（走账号自己的 proxy）。
    let acc = find_account(id).unwrap_or(acc);
    let (http_status, body) = match bce_api_with_cookies(
        "POST",
        LOGIN_BONUS,
        Some(json!({})),
        &acc.cookies,
        Some(&acc.proxy),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return json!({"result": "error", "error": e}),
    };
    if http_status == 302 || http_status == 401 || json_code_field(&body, "code") == Some(401) {
        mark_status(id, "expired");
        return json!({
            "result": "error",
            "reason": "login_expired",
            "error": "百度登录态已失效，请重新导入该账号的登录 cookies",
        });
    }

    // 3) 成功判定（同款函数）；重复领取的「已领语义」按文本兜底归 already。
    if login_bonus_success(&body) {
        add_checkin_log(&json!({
            "ts": now_secs(),
            "platform": PLATFORM_DUMATE,
            "accountId": id,
            "email": email,
            "result": "success",
        }));
        mark_status(id, "ok");
        set_last_checkin_day(id);
        let mut out = json!({
            "result": "success",
            "message": "DuMate 每日签到成功，+500 积分",
        });
        if let Some(points) = body.get("points").or_else(|| body.pointer("/result/points")) {
            if points.is_number() {
                out["points"] = points.clone();
            }
        }
        out["raw"] = body.clone();
        out
    } else if login_bonus_already_text(&body) {
        add_checkin_log(&json!({
            "ts": now_secs(),
            "platform": PLATFORM_DUMATE,
            "accountId": id,
            "email": email,
            "result": "already",
        }));
        mark_status(id, "ok");
        set_last_checkin_day(id);
        json!({
            "result": "already",
            "message": "今日已领取过 DuMate 签到积分",
        })
    } else {
        let brief = body.to_string().chars().take(200).collect::<String>();
        json!({
            "result": "error",
            "error": format!("DuMate 签到失败（HTTP {http_status}）: {brief}"),
        })
    }
}

/// 池账号积分概览：GET quota_overview → dumate::get_credits 同构 payload
/// （含 totalRemaining/remainingTotal 数值镜像，供 credit_usage 快照与统一视图）。
pub async fn account_credits(id: &str) -> Value {
    let Some(acc) = find_account(id) else {
        return json!({"ok": false, "error": format!("DuMate 池账号不存在: {id}")});
    };
    let (http_status, body) = match bce_api_with_cookies(
        "GET",
        QUOTA_OVERVIEW,
        None,
        &acc.cookies,
        Some(&acc.proxy),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "reason": "credentials_missing", "error": e}),
    };
    if http_status == 302 || http_status == 401 {
        mark_status(id, "expired");
        return json!({
            "ok": false,
            "reason": "login_expired",
            "error": "百度登录态已失效，请重新导入该账号的登录 cookies",
        });
    }
    match crate::modules::dumate::code_num(&body) {
        Some(0) => {
            mark_status(id, "ok");
            let credits = crate::modules::dumate::assemble_credits(
                body.get("result").unwrap_or(&Value::Null),
            );
            with_credit_mirrors(credits)
        }
        Some(401) => {
            mark_status(id, "expired");
            json!({
                "ok": false,
                "reason": "login_expired",
                "error": "百度登录态已失效，请重新导入该账号的登录 cookies",
            })
        }
        _ => {
            let brief = body.to_string().chars().take(200).collect::<String>();
            json!({
                "ok": false,
                "reason": "upstream_error",
                "error": format!("积分查询失败（HTTP {http_status}）: {brief}"),
            })
        }
    }
}

/// 给 assemble_credits 的 payload 补 totalRemaining/remainingTotal 数值镜像
/// （与 platform_checkin::get_dumate_credits 同款，credit_usage 快照有数可记）。
fn with_credit_mirrors(mut credits: Value) -> Value {
    let parse = |key: &str| -> f64 {
        credits
            .get(key)
            .and_then(Value::as_str)
            .map(|s| s.replace(',', ""))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let total_remaining = parse("totalPoints");
    let remaining_total = parse("availablePoints");
    credits["totalRemaining"] = json!(total_remaining);
    credits["remainingTotal"] = json!(remaining_total);
    credits
}

// ---------------------------------------------------------------------------
// 列表摘要（契约 §1.5：不回传 cookie 明文，只回 cookieCount）
// ---------------------------------------------------------------------------

/// 单个池账号 → 列表摘要行。
pub fn summary_of(acc: &DumatePoolAccount) -> Value {
    json!({
        "id": acc.id,
        "name": acc.name,
        "proxy": acc.proxy,
        "note": acc.note,
        "status": acc.status,
        "addedAt": acc.added_at,
        "lastCheckinDay": acc.last_checkin_day,
        "cookieCount": acc.cookies.len(),
        "isLocal": false,
        "credits": latest_cached_credits(&acc.id),
    })
}

/// 最新积分缓存（credit_usage 快照里该账号最近一条；没有 → null）。
fn latest_cached_credits(account_id: &str) -> Value {
    let latest = crate::modules::credit_usage::load_snapshots()
        .into_iter()
        .filter(|s| s.get("accountId").and_then(Value::as_str) == Some(account_id))
        .max_by_key(|s| s.get("ts").and_then(Value::as_i64).unwrap_or(0));
    match latest {
        Some(s) => {
            let total = s.get("total").and_then(Value::as_f64).unwrap_or(0.0);
            let remaining = s.get("remaining").and_then(Value::as_f64).unwrap_or(0.0);
            json!({
                "available": remaining,
                "total": total,
                "used": (total - remaining).max(0.0),
                "ts": s.get("ts").cloned().unwrap_or(Value::Null),
            })
        }
        None => Value::Null,
    }
}

/// 全量列表（池账号 + 本机账号行）：`isLocal=true` 标记 `dumate:local`。
/// 本机行不回传 cookie（cookieCount=0，凭据是否齐备看 status）。
pub fn list_entries() -> Vec<Value> {
    let mut out = Vec::new();
    if crate::modules::dumate_credentials::credentials_present() {
        out.push(json!({
            "id": format!("{PLATFORM_DUMATE}:local"),
            "name": "本机账号（桌面端登录）",
            "proxy": "",
            "note": "模型调用走本机主账号",
            "status": "ok",
            "addedAt": 0,
            "lastCheckinDay": "",
            "cookieCount": 0,
            "isLocal": true,
            "credits": Value::Null,
        }));
    }
    for acc in load_accounts() {
        out.push(summary_of(&acc));
    }
    out
}

// ---------------------------------------------------------------------------
// 单测（纯函数；不依赖网络 / 真实凭据文件）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str, domain: &str, exp: Option<f64>) -> DumateCookie {
        DumateCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string(),
            expiration_date: exp,
        }
    }

    #[test]
    fn parse_cookies_accepts_array_and_wrapper() {
        // 数组形态。
        let arr = json!([
            {"name": "BDUSS", "value": "abc", "domain": ".baidu.com", "expirationDate": 1893456000},
            {"name": "bce-user-info", "value": "csrf-1"},
        ]);
        let cookies = parse_cookies_json(&arr).unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "BDUSS");
        assert_eq!(cookies[0].expiration_date, Some(1893456000.0));
        assert_eq!(cookies[1].domain, "", "缺 domain 兜底空串");

        // {cookies:[...]} 包装形态。
        let wrapped = json!({"cookies": [{"name": "A", "value": "1"}]});
        assert_eq!(parse_cookies_json(&wrapped).unwrap().len(), 1);

        // 空数组 / 非法形态报错。
        assert!(parse_cookies_json(&json!([])).is_err());
        assert!(parse_cookies_json(&json!({"nope": 1})).is_err());
        assert!(parse_cookies_json(&json!("junk")).is_err());
    }

    #[test]
    fn parse_cookies_rejects_missing_name_or_value() {
        let bad = json!([
            {"name": "OK", "value": "1"},
            {"name": "", "value": "2"},
        ]);
        let err = parse_cookies_json(&bad).unwrap_err();
        assert!(err.contains("第 2 条"), "{err}");
        let bad2 = json!([{"name": "X"}]);
        assert!(parse_cookies_json(&bad2).is_err());
    }

    #[test]
    fn bce_user_info_fingerprint_extracts_bce_user_info_value() {
        let cookies = vec![
            cookie("BDUSS", "abc", ".baidu.com", None),
            cookie("bce-user-info", "csrf-xyz%3D", ".bce.baidu.com", None),
        ];
        assert_eq!(bce_user_info_of(&cookies), "csrf-xyz%3D");
        assert_eq!(bce_user_info_of(&[]), "");
        assert_eq!(
            bce_user_info_of(&[cookie("other", "v", ".baidu.com", None)]),
            "",
            "无 bce-user-info → 空指纹"
        );
    }

    #[test]
    fn next_pool_id_increments_max_sequence() {
        let accounts = vec![
            DumatePoolAccount {
                id: "dumate:pool_1".into(),
                name: "a".into(),
                cookies: vec![],
                proxy: String::new(),
                note: String::new(),
                status: "ok".into(),
                added_at: 0,
                last_checkin_day: String::new(),
            },
            DumatePoolAccount {
                id: "dumate:pool_3".into(),
                name: "b".into(),
                cookies: vec![],
                proxy: String::new(),
                note: String::new(),
                status: "ok".into(),
                added_at: 0,
                last_checkin_day: String::new(),
            },
        ];
        assert_eq!(next_pool_id(&accounts), "dumate:pool_4");
        assert_eq!(next_pool_id(&[]), "dumate:pool_1", "空池从 1 起");
        // 非法序号不参与 max。
        let junk = vec![DumatePoolAccount {
            id: "dumate:pool_x".into(),
            name: String::new(),
            cookies: vec![],
            proxy: String::new(),
            note: String::new(),
            status: String::new(),
            added_at: 0,
            last_checkin_day: String::new(),
        }];
        assert_eq!(next_pool_id(&junk), "dumate:pool_1");
    }

    #[test]
    fn load_accounts_tolerates_broken_text_and_entries() {
        assert!(load_accounts_from_text("").is_empty(), "空文本 → 空");
        assert!(load_accounts_from_text("not json {").is_empty(), "整体坏 → 空");
        assert!(load_accounts_from_text(r#"{"accounts": 42}"#).is_empty());
        // 单条损坏跳过，好条目保留（缺字段 serde default 兜底）。
        let text = r#"{"accounts":[
            {"id": "dumate:pool_1", "name": "好号", "cookies": [
                {"name": "BDUSS", "value": "abc", "domain": ".baidu.com", "expirationDate": 1893456000}
            ]},
            {"id": 12345, "name": {"bad": true}}
        ]}"#;
        let accounts = load_accounts_from_text(text);
        assert_eq!(accounts.len(), 1, "坏条目跳过");
        assert_eq!(accounts[0].id, "dumate:pool_1");
        assert_eq!(accounts[0].name, "好号");
        assert_eq!(accounts[0].cookies.len(), 1);
        assert_eq!(accounts[0].status, "", "缺字段兜底");
    }

    #[test]
    fn default_name_and_cookie_count_summary() {
        let acc = DumatePoolAccount {
            id: "dumate:pool_2".into(),
            name: "二号机".into(),
            cookies: vec![cookie("A", "1", ".baidu.com", None), cookie("B", "2", "", None)],
            proxy: "http://127.0.0.1:7890".into(),
            note: "备注".into(),
            status: "ok".into(),
            added_at: 1727800000,
            last_checkin_day: "2026-10-01".into(),
        };
        assert_eq!(default_name(""), "DuMate 账号");
        assert_eq!(default_name("  "), "DuMate 账号");
        assert_eq!(default_name(" 二号机 "), "二号机");
        let v = summary_of(&acc);
        assert_eq!(v.get("id").and_then(Value::as_str), Some("dumate:pool_2"));
        assert_eq!(v.get("cookieCount").and_then(Value::as_i64), Some(2));
        assert_eq!(v.get("isLocal").and_then(Value::as_bool), Some(false));
        assert_eq!(v.get("proxy").and_then(Value::as_str), Some("http://127.0.0.1:7890"));
        // 摘要不回传 cookie 明文。
        assert!(v.get("cookies").is_none(), "summary 不得携带 cookies: {v}");
    }

    #[test]
    fn cookie_serde_round_trip_uses_camel_case() {
        let acc = DumatePoolAccount {
            id: "dumate:pool_1".into(),
            name: "n".into(),
            cookies: vec![cookie("BDUSS", "v", ".baidu.com", Some(1893456000.0))],
            proxy: String::new(),
            note: String::new(),
            status: "unknown".into(),
            added_at: 1,
            last_checkin_day: String::new(),
        };
        let text = serde_json::to_string(&acc).unwrap();
        assert!(text.contains("expirationDate"), "落盘键为 camelCase: {text}");
        let back: DumatePoolAccount = serde_json::from_str(&text).unwrap();
        assert_eq!(back.cookies[0].expiration_date, Some(1893456000.0));
    }
}
