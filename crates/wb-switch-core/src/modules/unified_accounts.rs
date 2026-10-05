//! 统一账号管理聚合（契约 docs/unified-accounts-contract.md §2）。
//!
//! 把**全部平台**账号聚合成统一 schema，供账号管理页一次性渲染：
//! - 数据源：`platform_checkin::all_checkin_accounts`（WorkBuddy 国服账号库 +
//!   AutoClaw / TRAE Work CN / Loomy / ChatGLM / 妙手 / DuMate 本机与池账号）
//!   + Grok 账号池（无签到，仅登录态展示）；
//! - `loginState`：per-account `checkin::get_checkin_status_for`（既有函数）；
//!   WorkBuddy 额外读 auth/refresh 链路的 `needs_relogin` 本机现状，标记失效；
//! - `credits`：per-account `platform_checkin::get_platform_credits`（既有
//!   函数）→ 压缩成 `{available,total,used}`；
//! - `displayName`：**全名无遮罩**（不调用任何 mask 函数）——账号自身
//!   email/nickname 字段；前端 account-extras 备注（localStorage）优先级由
//!   前端按 `note` 字段叠加（后端不持有该存储）；
//! - `proxy`/`note`：仅 DuMate 池账号有值（dumate_accounts 池数据），其余空串；
//! - `gatewayPage`：platform → 前端渠道页路由映射表。

use serde_json::{json, Value};
use std::collections::HashSet;

use crate::modules::account;
use crate::modules::checkin;
use crate::modules::config;
use crate::modules::grok;
use crate::modules::platform_checkin;
use crate::modules::zcode;

/// 统一账号聚合（GET /api/accounts/unified 的数据层）。
/// 返回 `{accounts: [...], summary: {platforms, accounts, healthy, checkedInToday}}`。
pub async fn unified_accounts() -> Value {
    let auto_enabled = config::load_checkin_config()
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut items: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut checkin_accounts: Vec<Value> = Vec::new();

    // ① 签到平台账号（含 WorkBuddy —— unified 语义是账号管理，不跳过；
    //    国际版 .ai WorkBuddy 仍按 all_checkin_accounts 口径排除）。
    for acc in platform_checkin::all_checkin_accounts() {
        let Some(id) = acc.get("id").and_then(Value::as_str).map(String::from) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        checkin_accounts.push(acc);
    }

    // per-account 状态/积分都是网络查询：串行时 26 个账号要 25s+，这里并发聚合。
    let mut rows: Vec<Value> = futures_util::future::join_all(
        checkin_accounts
            .into_iter()
            .map(|acc| async move { checkin_account_item(&acc, auto_enabled).await }),
    )
    .await;
    items.append(&mut rows);

    // ② Grok 账号池：无每日签到（todayCheckedIn=null / credits=null），仅
    //    名称与登录态聚合，让「统一账号管理」覆盖模型池账号。
    for acc in grok::load_accounts() {
        let Some(id) = acc.get("id").and_then(Value::as_str).map(String::from) else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        items.push(grok_account_item(&acc));
    }

    // ②' ZCode 账号池：无签到概念，仅登录身份与额度聚合（契约 zcode-channel-contract §2）。
    //    live 登录态也产出一行（isLive），池内同指纹账号另列 isActive。
    for acc in zcode::list_accounts() {
        let Some(id) = acc.get("id").and_then(Value::as_str).map(String::from) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let identity = acc.get("identity").cloned().unwrap_or(json!({}));
        let label = identity
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "ZCode 账号".to_string());
        let jwt_expired = identity.get("jwtExpired").and_then(Value::as_bool).unwrap_or(false);
        items.push(json!({
            "id": id.clone(),
            "platform": "zcode",
            "displayName": label,
            "region": Value::Null,
            "localLogin": false,
            "loginState": if jwt_expired { "expired" } else { "ok" },
            "todayCheckedIn": Value::Null,
            "credits": Value::Null,
            "proxy": "",
            "note": "",
            "checkinEnabled": false,
            "gatewayPage": "/zcode",
        }));
    }
    if let Some(live) = zcode::live_credentials() {
        if zcode::is_logged_in(&live) {
            let live_hash = zcode::canonical_hash(&live);
            let covered = zcode::list_accounts()
                .iter()
                .any(|acc| acc.get("id").and_then(Value::as_str) == Some(format!("zcode:{live_hash}").as_str()));
            if !covered {
                let identity = zcode::identity_of(&live);
                items.push(json!({
                    "id": "zcode:live",
                    "platform": "zcode",
                    "displayName": identity.label().unwrap_or_else(|| "ZCode 当前登录".to_string()),
                    "region": Value::Null,
                    "localLogin": true,
                    "loginState": if identity.jwt_expired { "expired" } else { "ok" },
                    "todayCheckedIn": Value::Null,
                    "credits": Value::Null,
                    "proxy": "",
                    "note": "",
                    "checkinEnabled": false,
                    "gatewayPage": "/zcode",
                }));
            }
        }
    }

    // ③ 同源标注：同一账号的多个入口（本机 / 池）打上 sameSourceOf，仅标注不隐藏。
    mark_same_source(&mut items);

    // ④ 汇总。
    let mut platforms: HashSet<String> = HashSet::new();
    let mut healthy = 0usize;
    let mut checked_in_today = 0usize;
    for item in &items {
        if let Some(p) = item.get("platform").and_then(Value::as_str) {
            platforms.insert(p.to_string());
        }
        if item.get("loginState").and_then(Value::as_str) == Some("ok") {
            healthy += 1;
        }
        if item.get("todayCheckedIn").and_then(Value::as_bool) == Some(true) {
            checked_in_today += 1;
        }
    }
    json!({
        "accounts": items,
        "summary": {
            "platforms": platforms.len(),
            "accounts": items.len(),
            "healthy": healthy,
            "checkedInToday": checked_in_today,
        },
    })
}

/// 签到平台账号 → 统一 schema 行（per-account 状态 + 积分实时查询）。
async fn checkin_account_item(acc: &Value, auto_enabled: bool) -> Value {
    let platform = platform_checkin::account_platform(acc);
    let status = checkin::get_checkin_status_for(acc).await;
    let credits = platform_checkin::get_platform_credits(acc).await;
    json!({
        "id": acc.get("id").cloned().unwrap_or(Value::Null),
        "platform": platform,
        "displayName": account::account_display_name(acc),
        "region": region_of(platform, acc),
        "localLogin": platform_checkin::is_local_login_account(acc),
        "loginState": login_state_from_status(platform, acc, &status),
        "todayCheckedIn": status.get("todayCheckedIn").cloned().unwrap_or(Value::Null),
        "credits": credits_summary(&credits),
        // 仅 DuMate 池账号候选里带 proxy/note（dumate_accounts 池数据）；其余空串。
        "proxy": acc.get("proxy").and_then(Value::as_str).unwrap_or(""),
        "note": acc.get("note").and_then(Value::as_str).unwrap_or(""),
        // 自动签到开关现状（全局开关；妙手无签到功能恒为 false）。
        "checkinEnabled": auto_enabled && platform != platform_checkin::PLATFORM_MIAOSHOU,
        "gatewayPage": gateway_page(platform),
    })
}

/// Grok 池账号 → 统一 schema 行（无签到链路，status 字段映射登录态）。
fn grok_account_item(acc: &Value) -> Value {
    let status = acc.get("status").and_then(Value::as_str).unwrap_or("ok");
    let login_state = match status {
        "error" => "expired",
        "ok" => "ok",
        _ => "unknown",
    };
    let display = {
        let name = acc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if name.is_empty() {
            acc.get("email")
                .and_then(Value::as_str)
                .unwrap_or("Grok 账号")
        } else {
            name
        }
    };
    json!({
        "id": acc.get("id").cloned().unwrap_or(Value::Null),
        "platform": "grok",
        "displayName": display,
        "region": Value::Null,
        "localLogin": false,
        "loginState": login_state,
        "todayCheckedIn": Value::Null,
        "credits": Value::Null,
        "proxy": "",
        "note": "",
        "checkinEnabled": false,
        "gatewayPage": "/grok",
    })
}

/// 区域归一（契约 §2：cn|intl|null）。
/// AutoClaw 池存 international/domestic，映射为 intl/cn；
/// WorkBuddy 按 domain 后缀派生（.ai → intl，与 workbuddy_gateway 口径一致）；
/// 其余平台无区域语义 → null。
fn region_of(platform: &str, account: &Value) -> Value {
    match platform {
        platform_checkin::PLATFORM_AUTOCLAW => {
            match account.get("region").and_then(Value::as_str) {
                Some("international") => json!("intl"),
                Some("domestic") => json!("cn"),
                _ => Value::Null,
            }
        }
        platform_checkin::PLATFORM_WORKBUDDY => {
            let intl = account
                .get("domain")
                .and_then(Value::as_str)
                .map(|d| d.to_ascii_lowercase().ends_with(".ai"))
                .unwrap_or(false);
            json!(if intl { "intl" } else { "cn" })
        }
        _ => Value::Null,
    }
}

/// 登录态映射（契约 §2：已有 checkin status 的 ok/失败原因 → ok/expired/unknown）。
/// WorkBuddy 额外读 auth/refresh 链路的 `needs_relogin` 本机现状（契约 §2：
/// 「workbuddy 的 loginState 读 auth_file 现状」）。
fn login_state_from_status(platform: &str, account: &Value, status: &Value) -> &'static str {
    if platform == platform_checkin::PLATFORM_WORKBUDDY
        && account.get("needs_relogin").and_then(Value::as_bool) == Some(true)
    {
        return "expired";
    }
    if status.get("ok").and_then(Value::as_bool) == Some(true) {
        return "ok";
    }
    let reason = status.get("reason").and_then(Value::as_str).unwrap_or("");
    let error = status.get("error").and_then(Value::as_str).unwrap_or("");
    let error_lower = error.to_ascii_lowercase();
    if reason == "login_expired"
        || error.contains("失效")
        || error.contains("重新登录")
        || error.contains("401")
        || error_lower.contains("unauthorized")
        || error_lower.contains("invalid token")
    {
        "expired"
    } else {
        "unknown"
    }
}

/// get_platform_credits payload → 统一积分视图（契约 §2 + 积分包明细）：
/// `{available,total,used, packages:[{name,remaining,total,used,expireAt,expiringSoon,kind}],
///    expiringSoonRemaining, soonestExpireAt}`。
/// packages 提取两条来源：
/// ① CreditExpiry.resources（AutoClaw/WorkBuddy/TRAE 等既有结构）；
/// ② DuMate 的 subscription[]/incremental[]（百度云 quota_overview 原始包列表）。
/// 各平台余额字段名不一（remainingTotal/availablePoints/balance），按序兜底。
fn credits_summary(credits: &Value) -> Value {
    if credits.get("ok").and_then(Value::as_bool) != Some(true) {
        return Value::Null;
    }
    let num = |keys: &[&str]| -> Option<f64> {
        for k in keys {
            if let Some(v) = credits.get(*k) {
                if let Some(f) = v.as_f64() {
                    return Some(f);
                }
                if let Some(s) = v.as_str() {
                    if let Ok(f) = s.replace(',', "").trim().parse::<f64>() {
                        return Some(f);
                    }
                }
            }
        }
        None
    };
    let available = num(&["remainingTotal", "availablePoints", "balance"]).unwrap_or(0.0);
    let total = num(&["totalRemaining", "totalPoints"]).unwrap_or(available);
    let used = (total - available).max(0.0);

    // 窗口只读一次配置，避免每个包都去读文件（26 个账号 × 最多 10 个包）。
    let soon_days = config::expiring_soon_days();
    let mut packages = packages_from_resources(credits, soon_days);
    if packages.is_empty() {
        packages = packages_from_dumate(credits, soon_days);
    }
    // 排序：临期优先，其次按到期时间升序（无到期时间的排最后）。
    packages.sort_by(|a, b| {
        let sa = a.get("expiringSoon").and_then(Value::as_bool).unwrap_or(false);
        let sb = b.get("expiringSoon").and_then(Value::as_bool).unwrap_or(false);
        sb.cmp(&sa)
            .then_with(|| {
                let ea = a.get("expireAt").and_then(Value::as_i64);
                let eb = b.get("expireAt").and_then(Value::as_i64);
                match (ea, eb) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            })
    });
    packages.truncate(10);

    let soonest = packages
        .iter()
        .filter_map(|p| p.get("expireAt").and_then(Value::as_i64))
        .min();
    let expiring_soon_remaining = {
        let from_field = num(&["expiringSoonRemaining"]);
        let from_packages: f64 = packages
            .iter()
            .filter(|p| p.get("expiringSoon").and_then(Value::as_bool) == Some(true))
            .filter_map(|p| p.get("remaining").and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok()))
            .sum();
        let sum = from_packages + from_field.unwrap_or(0.0);
        if sum > 0.0 { Some(sum) } else { None }
    };

    json!({
        "available": fmt_num(available),
        "total": fmt_num(total),
        "used": fmt_num(used),
        "packages": packages,
        "expiringSoonRemaining": expiring_soon_remaining.map(fmt_num),
        "soonestExpireAt": soonest,
    })
}

/// 来源 ①：CreditExpiry.resources（AutoClaw / WorkBuddy / TRAE 等）。
fn packages_from_resources(credits: &Value, soon_days: i64) -> Vec<Value> {
    let Some(list) = credits.get("resources").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|r| {
            let remaining = json_num(r.get("remaining")).unwrap_or(0.0);
            let total = json_num(r.get("total")).unwrap_or(remaining);
            let used = json_num(r.get("used")).unwrap_or((total - remaining).max(0.0));
            if remaining <= 0.0 {
                return None;
            }
            let name = r
                .get("packageName")
                .and_then(Value::as_str)
                .or_else(|| r.get("packageCode").and_then(Value::as_str))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "积分包".to_string());
            let expire_at = json_num(r.get("expireAt")).map(|f| normalize_epoch(f as i64));
            // 上游 expiringSoon 为权威标志；缺失时按到期时间兜底计算。
            let expiring_soon = r.get("expiringSoon").and_then(Value::as_bool) == Some(true)
                || is_expiring_within_days(expire_at, soon_days);
            Some(json!({
                "name": name,
                "remaining": fmt_num(remaining),
                "total": fmt_num(total),
                "used": fmt_num(used),
                "expireAt": expire_at,
                "expiringSoon": expiring_soon,
                "kind": "resource",
            }))
        })
        .collect()
}

/// 来源 ②：DuMate quota_overview 的 subscription[]/incremental[]。
fn packages_from_dumate(credits: &Value, soon_days: i64) -> Vec<Value> {
    let mut out = Vec::new();
    for (key, kind) in [("subscription", "subscription"), ("incremental", "incremental")] {
        let Some(list) = credits.get(key).and_then(Value::as_array) else {
            continue;
        };
        for p in list {
            let total = json_num(p.get("totalPoints")).unwrap_or(0.0);
            let used = json_num(p.get("usedPoints")).unwrap_or(0.0);
            let remaining = (total - used).max(0.0);
            if remaining <= 0.0 {
                continue;
            }
            let expire_at = json_num(p.get("expireDate")).map(|f| normalize_epoch(f as i64));
            let source = p.get("source").and_then(Value::as_str).unwrap_or("");
            let pkg_type = p.get("packageType").and_then(Value::as_str).unwrap_or("");
            let name = dumate_package_name(pkg_type, source);
            Some(json!({
                "name": name,
                "remaining": fmt_num(remaining),
                "total": fmt_num(total),
                "used": fmt_num(used),
                "expireAt": expire_at,
                "expiringSoon": is_expiring_within_days(expire_at, soon_days),
                "kind": kind,
            }))
            .map(|v| out.push(v));
        }
    }
    out
}

/// DuMate 包类型/来源 → 中文展示名（未知组合回落原文）。
fn dumate_package_name(pkg_type: &str, source: &str) -> String {
    let mapped = match pkg_type {
        "plan_experience" => "订阅体验包",
        "grant_point" => "赠送积分",
        "event_bonus" => "活动奖励",
        "login_bonus" => "签到奖励",
        _ => "",
    };
    if !mapped.is_empty() {
        return mapped.to_string();
    }
    let s = if source.is_empty() { pkg_type } else { source };
    if s.is_empty() {
        "积分包".to_string()
    } else {
        s.to_string()
    }
}

/// 「近期到期」窗口（天）。
/// 2026-10-01 由 **7 天放宽到 30 天**：7 天过窄，像 10/31 到期（29 天）这类包
/// 在用户直觉里就算「近期」，但按 7 天口径不会亮 —— 实测全站仅 1 个包命中。
/// 「近期到期」窗口天数由 **`config::expiring_soon_days()`** 提供（可配置，默认 30）。
///
/// 历史：初版 7 天过窄 —— 像 10/31 到期（29 天）这类包用户直觉算「近期」却不亮，
/// 实测全站仅 1 个包命中；2026-10-01 先放宽到 30，随后改为配置化（设置页可改，
/// 改完即时生效、不需重新构建）。
///
/// ⚠️ 不要再在这里写常量：窗口口径必须与 `credits.rs` 一致，统一走配置。

/// 同源标注：同一平台内「积分视图逐项一致」的账号，视为同一账号的不同入口。
///
/// 实测 `dumate:local`（本机账号）与 `dumate:pool_1`（池主号）的可用积分与
/// 全部积分包（名称/剩余/到期）完全相同 —— 是同一账号的两个入口。
/// 这里**只打标记不隐藏**，是否隐藏由前端决定（避免后端擅自删数据）。
///
/// 指纹只用「可用积分 + 积分包明细」，无积分的账号（如 Grok）返回 None 不参与分组，
/// 否则大量空积分账号会被误聚成一组。
fn mark_same_source(items: &mut Vec<Value>) {
    let fingerprint = |item: &Value| -> Option<String> {
        let credits = item.get("credits")?;
        if credits.is_null() {
            return None;
        }
        let available = credits.get("available").and_then(Value::as_str).unwrap_or("");
        // 🔴 只有**有到期时间**的包才参与指纹。
        // 反例：AutoClaw 的「奖励积分」各账号都是 1400 且 expireAt=null，
        // 若纳入指纹会把两个不同账号误判成同源（1.0.17 实测踩到）。
        // 无到期时间的额度包是通用额度，各账号雷同属正常，不足以证明同一账号。
        let packages: Vec<String> = credits
            .get("packages")
            .and_then(Value::as_array)?
            .iter()
            .filter(|p| p.get("expireAt").and_then(Value::as_i64).is_some())
            .map(|p| {
                format!(
                    "{}|{}|{}",
                    p.get("name").and_then(Value::as_str).unwrap_or(""),
                    p.get("remaining").and_then(Value::as_str).unwrap_or(""),
                    p.get("expireAt")
                        .and_then(Value::as_i64)
                        .map(|v| v.to_string())
                        .unwrap_or_default()
                )
            })
            .collect();
        // 无到期包 → 指纹不足以证明同源，不参与分组。
        if packages.is_empty() {
            return None;
        }
        Some(format!(
            "{}|{}|{}",
            item.get("platform").and_then(Value::as_str).unwrap_or(""),
            available,
            packages.join(",")
        ))
    };

    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for item in items.iter_mut() {
        let Some(key) = fingerprint(item) else {
            continue;
        };
        let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        let first = seen.get(&key).cloned();
        match first {
            Some(anchor) if anchor != id => {
                item["sameSourceOf"] = json!(anchor);
            }
            Some(_) => {}
            None => {
                seen.insert(key, id);
            }
        }
    }
}

/// 到期时间（秒级 epoch）是否在 `days` 天内（含已过期）。
fn is_expiring_within_days(expire_at: Option<i64>, days: i64) -> bool {
    let Some(at) = expire_at else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    at < now.saturating_add(days * 86400)
}

/// 归一化 epoch 到**秒**：上游口径不一——WorkBuddy/AutoClaw 的 CreditExpiry
/// 用毫秒（前端 `new Date(ts)` 直读），DuMate quota_overview 用秒。
/// 判据：> 1e11 视为毫秒（秒级 epoch 到 5138 年才会到 1e11）。
fn normalize_epoch(v: i64) -> i64 {
    if v > 100_000_000_000 {
        v / 1000
    } else {
        v
    }
}

fn json_num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|x| {
        x.as_f64()
            .or_else(|| x.as_str().and_then(|s| s.replace(',', "").trim().parse::<f64>().ok()))
    })
}

/// 数值 → 展示串（整数值不带小数位，其余保留两位）。
fn fmt_num(f: f64) -> String {
    if (f - f.round()).abs() < f64::EPSILON {
        format!("{}", f.round() as i64)
    } else {
        format!("{f:.2}")
    }
}

/// platform → 前端渠道页路由映射表（契约 §2 gatewayPage）。
fn gateway_page(platform: &str) -> &'static str {
    match platform {
        platform_checkin::PLATFORM_AUTOCLAW => "/autoclaw",
        platform_checkin::PLATFORM_TRAE_WORK_CN => "/trae-work-cn",
        platform_checkin::PLATFORM_DUMATE => "/dumate",
        platform_checkin::PLATFORM_LOOMY => "/loomy",
        platform_checkin::PLATFORM_CHATGLM => "/glmweb",
        platform_checkin::PLATFORM_MIAOSHOU => "/miaoshou",
        platform_checkin::PLATFORM_WORKBUDDY => "/workbuddy",
        "grok" => "/grok",
        "qwen" => "/qwen",
        "qoder" => "/qoder",
        "kimi" => "/kimi",
        "chatgpt" => "/chatgpt",
        _ => "/",
    }
}

// ---------------------------------------------------------------------------
// 单测（纯函数；不依赖网络 / 真实账号库）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiring_soon_window_default_is_thirty_days() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // 用**默认值**做确定性断言（不读用户可能改过的配置）。
        let d = config::DEFAULT_EXPIRING_SOON_DAYS;
        assert_eq!(d, 30);
        // 29 天后的包必须算「近期」，31 天后的不算。
        assert!(is_expiring_within_days(Some(now + 29 * 86400), d));
        assert!(!is_expiring_within_days(Some(now + 31 * 86400), d));
        // 无到期时间不算近期（如 AutoClaw 奖励积分）。
        assert!(!is_expiring_within_days(None, d));
        // 已过期仍算近期（前端据此标「已过期」）。
        assert!(is_expiring_within_days(Some(now - 86400), d));
    }

    #[test]
    fn expiring_soon_config_clamps_and_defaults() {
        // 越界值被夹住：0（永不亮）→ 1，过大 → 365；非对象/缺字段 → 默认 30。
        assert_eq!(config::merge_expiring_soon_config(&json!({"days": 0}))["days"], 1);
        assert_eq!(config::merge_expiring_soon_config(&json!({"days": 9999}))["days"], 365);
        assert_eq!(config::merge_expiring_soon_config(&json!({"days": 14}))["days"], 14);
        assert_eq!(config::merge_expiring_soon_config(&json!({}))["days"], 30);
        assert_eq!(config::merge_expiring_soon_config(&json!("bad"))["days"], 30);
    }

    #[test]
    fn marks_duplicate_entrances_of_the_same_account() {
        let pkg = json!([{
            "name": "订阅体验包", "remaining": "5885.49", "expireAt": 1791445280,
        }]);
        let mut items = vec![
            json!({"id": "dumate:local", "platform": "dumate",
                   "credits": {"available": "9105.49", "packages": pkg}}),
            json!({"id": "dumate:pool_1", "platform": "dumate",
                   "credits": {"available": "9105.49", "packages": pkg}}),
        ];
        mark_same_source(&mut items);
        // 第一条是锚点，不打标；第二条指向锚点。
        assert_eq!(items[0]["sameSourceOf"], Value::Null);
        assert_eq!(items[1]["sameSourceOf"], json!("dumate:local"));
    }

    #[test]
    fn different_accounts_are_not_marked_as_same_source() {
        let mut items = vec![
            json!({"id": "dumate:local", "platform": "dumate",
                   "credits": {"available": "9105.49",
                               "packages": [{"name": "订阅体验包", "remaining": "5885.49", "expireAt": 1791445280}]}}),
            json!({"id": "dumate:pool_1", "platform": "dumate",
                   "credits": {"available": "1200",
                               "packages": [{"name": "订阅体验包", "remaining": "1200", "expireAt": 1791445280}]}}),
        ];
        mark_same_source(&mut items);
        assert_eq!(items[0]["sameSourceOf"], Value::Null);
        assert_eq!(items[1]["sameSourceOf"], Value::Null);
    }

    #[test]
    fn identical_creditless_expiry_packages_are_not_same_source() {
        // 1.0.17 实测踩到的误判：两个不同 AutoClaw 账号都是「奖励积分 1400、无到期时间」。
        // 无到期时间的通用额度包雷同属正常，不能据此认定同源。
        let reward = json!([{"name": "奖励积分", "remaining": "1400", "expireAt": Value::Null}]);
        let mut items = vec![
            json!({"id": "autoclaw:aaa", "platform": "autoclaw",
                   "credits": {"available": "1400", "packages": reward}}),
            json!({"id": "autoclaw:bbb", "platform": "autoclaw",
                   "credits": {"available": "1400", "packages": reward}}),
        ];
        mark_same_source(&mut items);
        assert_eq!(items[0]["sameSourceOf"], Value::Null);
        assert_eq!(items[1]["sameSourceOf"], Value::Null);
    }

    #[test]
    fn creditless_accounts_are_never_grouped_as_same_source() {
        // Grok 等无积分账号指纹为 None —— 不能因为它们都没有积分就被聚成一组。
        let mut items = vec![
            json!({"id": "grok_a", "platform": "grok", "credits": Value::Null}),
            json!({"id": "grok_b", "platform": "grok", "credits": Value::Null}),
        ];
        mark_same_source(&mut items);
        assert_eq!(items[0]["sameSourceOf"], Value::Null);
        assert_eq!(items[1]["sameSourceOf"], Value::Null);
    }

    #[test]
    fn login_state_maps_status_reasons() {
        assert_eq!(
            login_state_from_status(
                platform_checkin::PLATFORM_DUMATE,
                &json!({}),
                &json!({"ok": true, "todayCheckedIn": false}),
            ),
            "ok"
        );
        // login_expired reason（dumate 上游 302/401）。
        assert_eq!(
            login_state_from_status(
                platform_checkin::PLATFORM_DUMATE,
                &json!({}),
                &json!({"ok": false, "reason": "login_expired", "error": "百度登录态已失效，请打开 DuMate 客户端重新登录"}),
            ),
            "expired"
        );
        // 文案关键词兜底（无 reason 字段）。
        assert_eq!(
            login_state_from_status(
                platform_checkin::PLATFORM_AUTOCLAW,
                &json!({}),
                &json!({"ok": false, "error": "AutoClaw 登录态已过期，请打开 AutoClaw 重新登录"}),
            ),
            "expired"
        );
        // 其他失败 → unknown。
        assert_eq!(
            login_state_from_status(
                platform_checkin::PLATFORM_AUTOCLAW,
                &json!({}),
                &json!({"ok": false, "error": "网络超时"}),
            ),
            "unknown"
        );
    }

    #[test]
    fn workbuddy_needs_relogin_forces_expired() {
        let account = json!({"id": "wb-1", "needs_relogin": true});
        // 即便签到状态 ok（缓存/探测窗口），本机 refresh 链路已标记 → expired。
        assert_eq!(
            login_state_from_status(
                platform_checkin::PLATFORM_WORKBUDDY,
                &account,
                &json!({"ok": true, "todayCheckedIn": false}),
            ),
            "expired"
        );
        // 其他平台不受 needs_relogin 影响。
        assert_eq!(
            login_state_from_status(
                platform_checkin::PLATFORM_AUTOCLAW,
                &json!({"needs_relogin": true}),
                &json!({"ok": true}),
            ),
            "ok"
        );
    }

    #[test]
    fn credits_summary_maps_common_fields() {
        // dumate 形态（字符串数字 availablePoints/totalPoints）。
        let v = credits_summary(&json!({
            "ok": true, "availablePoints": "15964.27", "totalPoints": "16020.00",
        }));
        assert_eq!(v.get("available").and_then(Value::as_str), Some("15964.27"));
        assert_eq!(v.get("total").and_then(Value::as_str), Some("16020"), "整数值去尾零");
        assert_eq!(v.get("used").and_then(Value::as_str), Some("55.73"));
        // loomy/chatglm 形态（remainingTotal 数值）。
        let v = credits_summary(&json!({"ok": true, "remainingTotal": 120.0, "totalRemaining": 120.0}));
        assert_eq!(v.get("available").and_then(Value::as_str), Some("120"));
        // miaoshou 形态（仅 balance）。
        let v = credits_summary(&json!({"ok": true, "balance": 88.5}));
        assert_eq!(v.get("available").and_then(Value::as_str), Some("88.50"));
        assert_eq!(v.get("total").and_then(Value::as_str), Some("88.50"));
        assert_eq!(v.get("used").and_then(Value::as_str), Some("0"));
        // 失败 → null。
        assert!(credits_summary(&json!({"ok": false, "error": "x"})).is_null());
    }

    #[test]
    fn region_maps_autoclaw_and_workbuddy_domain() {
        assert_eq!(
            region_of(platform_checkin::PLATFORM_AUTOCLAW, &json!({"region": "international"})),
            json!("intl")
        );
        assert_eq!(
            region_of(platform_checkin::PLATFORM_AUTOCLAW, &json!({"region": "domestic"})),
            json!("cn")
        );
        assert!(region_of(platform_checkin::PLATFORM_AUTOCLAW, &json!({})).is_null());
        assert_eq!(
            region_of(platform_checkin::PLATFORM_WORKBUDDY, &json!({"domain": "www.workbuddy.ai"})),
            json!("intl")
        );
        assert_eq!(
            region_of(platform_checkin::PLATFORM_WORKBUDDY, &json!({"domain": "www.workbuddy.cn"})),
            json!("cn")
        );
        assert!(region_of(platform_checkin::PLATFORM_DUMATE, &json!({})).is_null());
    }

    #[test]
    fn gateway_page_routes_to_channel_pages() {
        assert_eq!(gateway_page(platform_checkin::PLATFORM_AUTOCLAW), "/autoclaw");
        assert_eq!(gateway_page(platform_checkin::PLATFORM_TRAE_WORK_CN), "/trae-work-cn");
        assert_eq!(gateway_page(platform_checkin::PLATFORM_DUMATE), "/dumate");
        assert_eq!(gateway_page(platform_checkin::PLATFORM_CHATGLM), "/glmweb");
        assert_eq!(gateway_page("grok"), "/grok");
        assert_eq!(gateway_page(platform_checkin::PLATFORM_MIAOSHOU), "/miaoshou");
        assert_eq!(gateway_page(platform_checkin::PLATFORM_WORKBUDDY), "/workbuddy");
        assert_eq!(gateway_page("who-knows"), "/");
    }

    #[test]
    fn grok_item_maps_status_to_login_state_without_checkin() {
        let item = grok_account_item(&json!({"id": "g1", "name": "主号", "status": "ok"}));
        assert_eq!(item.get("platform").and_then(Value::as_str), Some("grok"));
        assert_eq!(item.get("displayName").and_then(Value::as_str), Some("主号"));
        assert_eq!(item.get("loginState").and_then(Value::as_str), Some("ok"));
        assert!(item.get("todayCheckedIn").unwrap().is_null());
        assert!(item.get("credits").unwrap().is_null());
        assert_eq!(item.get("checkinEnabled").and_then(Value::as_bool), Some(false));
        let item = grok_account_item(&json!({"id": "g2", "status": "error"}));
        assert_eq!(item.get("loginState").and_then(Value::as_str), Some("expired"));
        // 名称缺省：email 兜底，再兜底平台默认名。
        let item = grok_account_item(&json!({"id": "g3", "email": "a@b.c"}));
        assert_eq!(item.get("displayName").and_then(Value::as_str), Some("a@b.c"));
    }

    #[test]
    fn fmt_num_trims_integer_fraction() {
        assert_eq!(fmt_num(120.0), "120");
        assert_eq!(fmt_num(88.5), "88.50");
        assert_eq!(fmt_num(0.0), "0");
    }

    #[test]
    fn credits_packages_from_resources_and_dumate() {
        // CreditExpiry.resources 形态：包名/剩余/到期/临期映射。
        let v = credits_summary(&json!({
            "ok": true, "remainingTotal": 100.0, "totalRemaining": 300.0,
            "resources": [
                {"packageName": "月度权益", "total": 300.0, "remaining": 100.0, "used": 200.0, "expireAt": 4102444800i64, "expiringSoon": true},
                {"packageCode": "new_user", "total": 50.0, "remaining": 50.0, "used": 0.0, "expireAt": null},
            ],
        }));
        let pkgs = v.get("packages").and_then(Value::as_array).unwrap();
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].get("name").and_then(Value::as_str), Some("月度权益"), "临期包排最前");
        assert_eq!(pkgs[0].get("expiringSoon").and_then(Value::as_bool), Some(true));
        assert_eq!(pkgs[0].get("remaining").and_then(Value::as_str), Some("100"));
        // DuMate 形态：subscription/incremental + 中文包名 + 剩余=总-已用。
        let v = credits_summary(&json!({
            "ok": true, "availablePoints": "15964.27", "totalPoints": "16020.00",
            "subscription": [{"packageType": "plan_experience", "source": "event_bonus", "totalPoints": "10000.00", "usedPoints": "67.43", "expireDate": 4102444800i64}],
            "incremental": [{"packageType": "grant_point", "source": "login_bonus", "totalPoints": "500.00", "usedPoints": "0.00", "expireDate": 4102444800i64}],
        }));
        let pkgs = v.get("packages").and_then(Value::as_array).unwrap();
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].get("name").and_then(Value::as_str), Some("订阅体验包"));
        assert_eq!(pkgs[0].get("remaining").and_then(Value::as_str), Some("9932.57"));
        assert_eq!(pkgs[1].get("name").and_then(Value::as_str), Some("赠送积分"));
        assert!(v.get("soonestExpireAt").and_then(Value::as_i64).is_some());
        // 毫秒口径（WorkBuddy/AutoClaw CreditExpiry）归一为秒。
        let v = credits_summary(&json!({
            "ok": true, "remainingTotal": 10.0, "totalRemaining": 10.0,
            "resources": [{"packageName": "月度", "total": 10.0, "remaining": 10.0, "expireAt": 4102444800000i64}],
        }));
        let pkgs = v.get("packages").and_then(Value::as_array).unwrap();
        assert_eq!(pkgs[0].get("expireAt").and_then(Value::as_i64), Some(4102444800i64));
        // 无包数据的平台：packages 空数组但 available 仍在。
        let v = credits_summary(&json!({"ok": true, "balance": 88.5}));
        assert_eq!(v.get("packages").and_then(Value::as_array).map(|a| a.len()), Some(0));
        assert_eq!(v.get("available").and_then(Value::as_str), Some("88.50"));
    }

    #[test]
    fn is_expiring_within_days_boundaries() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(is_expiring_within_days(Some(now + 3 * 86400), 7));
        assert!(!is_expiring_within_days(Some(now + 30 * 86400), 7));
        assert!(is_expiring_within_days(Some(now - 86400), 7), "已过期也算临期");
        assert!(!is_expiring_within_days(None, 7));
    }
}
