//! OAuth 扫码登录采集（复刻 cockpit 流程）。
//!
//! 对照 server.py `oauth_start` / `oauth_poll`。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::modules::account;
use crate::modules::config::{
    apply_region_headers, http_request, norm_ts, now_ms, now_secs, workbuddy_api_endpoint_for,
    OAUTH_TIMEOUT_SECONDS, WORKBUDDY_API_PREFIX, WORKBUDDY_PLATFORM,
};

/// 登录目标区域归一化：`intl`（大小写 / 首尾空白不敏感）→ 国际版，其余 → 国服。
///
/// 口径与池的 region 派生（`domain` 后缀 `.ai`）保持一致，避免两处判定打架。
fn normalize_login_region(region: &str) -> &'static str {
    if region.trim().eq_ignore_ascii_case("intl") {
        "intl"
    } else {
        "cn"
    }
}

/// 该区域账号的 `domain` 兜底值。
///
/// 🔴 为什么必须有兜底：`oauth_poll` 原先只取上游 `data.domain`；若国际版
/// 响应里没有该字段，记录的 domain 就是**空串** ⇒ 池按后缀派生 region 时
/// 落到 `cn` ⇒ **国际号被当成国内号**，症状是「明明登了国际版，请求却全走国服」。
fn default_domain_for(region: &str) -> &'static str {
    if region == "intl" {
        "www.workbuddy.ai"
    } else {
        "www.workbuddy.cn"
    }
}

/// 按登录区域选 API 基址。
///
/// 复用 `workbuddy_api_endpoint_for`（refresh / checkin / chat 共用的**唯一**判定），
/// 不在这里另写一套「`.ai` ⇒ intl」——否则又多一处会漂移的口径。
fn endpoint_for_region(region: &str) -> &'static str {
    workbuddy_api_endpoint_for(&json!({ "domain": default_domain_for(region) }))
}

#[derive(Default)]
struct OAuthInfo {
    state: String,
    expires_at: i64,
    done: bool,
    result: Option<Value>,
    error: Option<String>,
    /// 本次登录请求的**目标区域**（`cn` / `intl`）。
    ///
    /// 必须随 state 一起存：`oauth_poll` 是**另一次 HTTP 调用**，只拿到 `login_id`，
    /// 若不在这里记住区域，轮询时就会退回硬编码的国服端点。
    region: String,
}

static OAUTH_STATES: OnceLock<Mutex<HashMap<String, OAuthInfo>>> = OnceLock::new();

fn oauth_states() -> &'static Mutex<HashMap<String, OAuthInfo>> {
    OAUTH_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 发起登录：向官方申请 state，返回 loginId / verificationUri / expiresIn。
///
/// `region` 决定 API 基址与区域头（`cn` / `intl`）。**缺省 `cn`**（保持旧行为）。
pub async fn oauth_start(region: &str) -> Result<Value, String> {
    let region = normalize_login_region(region);
    let endpoint = endpoint_for_region(region);
    let login_id = format!("wb_{}", uuid::Uuid::new_v4().simple());
    let url = format!("{endpoint}{WORKBUDDY_API_PREFIX}/auth/state?platform={WORKBUDDY_PLATFORM}");
    let resp = http_request(&url, "POST", Some(json!({})), None).await;
    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let state = data
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if state.is_empty() {
        let snippet = serde_json::to_string(&resp)
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect::<String>();
        return Err(format!("auth/state 响应缺少 state: {snippet}"));
    }
    let auth_url = data
        .get("authUrl")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("auth_url").and_then(|v| v.as_str()))
        .or_else(|| data.get("url").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{endpoint}/login?state={state}"));

    let mut map = oauth_states().lock().unwrap();
    map.insert(
        login_id.clone(),
        OAuthInfo {
            state,
            expires_at: now_secs() + OAUTH_TIMEOUT_SECONDS,
            region: region.to_string(),
            ..Default::default()
        },
    );
    drop(map);

    Ok(json!({
        "loginId": login_id,
        "verificationUri": auth_url,
        "expiresIn": OAUTH_TIMEOUT_SECONDS,
        "region": region,
        "endpoint": endpoint,
    }))
}

/// 轮询一次官方 token 接口。成功则拉取账号信息并入库。
pub async fn oauth_poll(login_id: &str) -> Value {
    let (state, region) = {
        let mut map = oauth_states().lock().unwrap();
        let Some(info) = map.get_mut(login_id) else {
            return json!({"done": true, "error": "登录请求不存在"});
        };
        if info.done {
            return json!({"done": true, "result": info.result.clone(), "error": info.error.clone()});
        }
        if now_secs() > info.expires_at {
            info.done = true;
            info.error = Some("登录超时".to_string());
            return json!({"done": true, "error": "登录超时"});
        }
        let region = if info.region.is_empty() {
            // 兼容本次升级前遗留的 state（没有 region 字段）⇒ 按国服处理。
            "cn".to_string()
        } else {
            info.region.clone()
        };
        (info.state.clone(), region)
    };
    let endpoint = endpoint_for_region(&region);

    let url = format!("{endpoint}{WORKBUDDY_API_PREFIX}/auth/token?state={state}");
    let resp = http_request(&url, "GET", None, None).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 && code != 200 {
        return json!({"done": false});
    }
    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let access_token = data
        .get("accessToken")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("access_token").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        return json!({"done": false});
    }

    // 拉取账号信息
    let account_url = format!("{endpoint}{WORKBUDDY_API_PREFIX}/login/account?state={state}");
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!("Bearer {access_token}"),
    );
    // 🔴 domain 兜底必须在**建 headers 之前**算好：`apply_region_headers` 正是按
    // domain 判定区域并注入 Origin/Referer/X-Auth-Refresh-Source 的。
    // 上游若不回 domain，国际号会被当成国服发请求 ⇒ 被上游按 Origin 拒。
    let domain = data
        .get("domain")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_domain_for(&region).to_string());
    let account_stub = json!({ "domain": domain });
    apply_region_headers(&account_stub, &mut headers);
    headers.insert("X-Domain".to_string(), domain.clone());

    let acc_resp = http_request(&account_url, "GET", None, Some(&headers)).await;
    let acc_data = acc_resp.get("data").cloned().unwrap_or_else(|| json!({}));

    let uid = acc_data
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let nickname = acc_data
        .get("nickname")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let email = oauth_profile_email(&acc_data);

    let expires_at = norm_ts(data.get("expiresAt").or_else(|| data.get("expires_at")));
    let expires_at = match expires_at {
        Some(v) => Some(v),
        None => data
            .get("expiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };
    let refresh_expires_at = norm_ts(
        data.get("refreshExpiresAt")
            .or_else(|| data.get("refresh_expires_at")),
    );
    let refresh_expires_at = match refresh_expires_at {
        Some(v) => Some(v),
        None => data
            .get("refreshExpiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };

    let account = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "uid": uid,
        "nickname": nickname,
        "email": email,
        "enterpriseName": acc_data.get("enterpriseName"),
        "enterpriseId": acc_data.get("enterpriseId"),
        "access_token": access_token,
        "refresh_token": data.get("refreshToken").and_then(|v| v.as_str())
            .or_else(|| data.get("refresh_token").and_then(|v| v.as_str()))
            .map(|s| s.to_string()),
        "token_type": data.get("tokenType").and_then(|v| v.as_str())
            .or_else(|| data.get("token_type").and_then(|v| v.as_str()))
            .unwrap_or("Bearer")
            .to_string(),
        "domain": domain.to_string(),
        "expiresAt": expires_at,
        "refreshExpiresAt": refresh_expires_at,
        "auth_raw": data,
        "profile_raw": acc_data,
        "createdAt": now_ms(),
    });

    let account = match account::save_collected_account(account) {
        Ok(saved) => saved,
        Err(error) => {
            let error = format!("保存账号失败: {error}");
            let mut map = oauth_states().lock().unwrap();
            if let Some(info) = map.get_mut(login_id) {
                info.done = true;
                info.error = Some(error.clone());
            }
            return json!({"done": true, "error": error});
        }
    };

    let result = account::account_meta(&account);
    let mut map = oauth_states().lock().unwrap();
    if let Some(info) = map.get_mut(login_id) {
        info.done = true;
        info.result = Some(result.clone());
    }
    drop(map);

    json!({
        "done": true,
        "result": result,
        // 回传区域供界面确认「确实登进了国际版」，避免用户以为登错区。
        "region": region,
        "domain": domain,
    })
}

fn oauth_profile_email(profile: &Value) -> Option<String> {
    profile
        .get("email")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    // 断言用别名：与 `config` 里的名字保持一一对应，同时避免遮蔽模块内的其它引用。
    use crate::modules::config::{
        WORKBUDDY_API_ENDPOINT as WORKBUDDY_API_ENDPOINT_CN, WORKBUDDY_API_ENDPOINT_INTL,
    };

    #[test]
    fn login_region_normalization_is_case_and_space_insensitive() {
        assert_eq!(normalize_login_region("intl"), "intl");
        assert_eq!(normalize_login_region("  INTL "), "intl");
        assert_eq!(normalize_login_region("Intl"), "intl");
        // 缺省 / 未知值一律按国服（保持升级前的旧行为）
        assert_eq!(normalize_login_region(""), "cn");
        assert_eq!(normalize_login_region("cn"), "cn");
        assert_eq!(normalize_login_region("garbage"), "cn");
    }

    #[test]
    fn login_region_selects_distinct_endpoints() {
        // 🔴 本次问题的核心断言：两个区域必须打到**不同**的 API 基址。
        // 升级前 oauth.rs 全部硬编码国服端点 ⇒ 国际版无从登录。
        let cn = endpoint_for_region("cn");
        let intl = endpoint_for_region("intl");
        assert_ne!(cn, intl, "两区端点必须不同，否则等于没支持国际版");
        assert_eq!(cn, WORKBUDDY_API_ENDPOINT_CN);
        assert_eq!(intl, WORKBUDDY_API_ENDPOINT_INTL);
        assert!(intl.contains("workbuddy.ai"), "国际端点应为 workbuddy.ai");
    }

    #[test]
    fn login_domain_fallback_prevents_intl_account_being_treated_as_cn() {
        // 🔴 上游不回 domain 时若留空串，池按后缀派生 region 会落回 cn
        // ⇒ 国际号被当成国内号。兜底必须给出真实域。
        assert_eq!(default_domain_for("intl"), "www.workbuddy.ai");
        assert_eq!(default_domain_for("cn"), "www.workbuddy.cn");
        // 兜底域必须能被既有判定识别为 intl
        assert_eq!(
            workbuddy_api_endpoint_for(&json!({"domain": default_domain_for("intl")})),
            WORKBUDDY_API_ENDPOINT_INTL
        );
    }

    #[test]
    fn intl_login_sends_region_headers() {
        // 国际版上游按 Origin/Referer 判定来源区域，缺失会被拒。
        let mut h = HashMap::new();
        apply_region_headers(&json!({"domain": "www.workbuddy.ai"}), &mut h);
        assert_eq!(
            h.get("Origin").map(String::as_str),
            Some(WORKBUDDY_API_ENDPOINT_INTL)
        );
        assert_eq!(h.get("X-Auth-Refresh-Source").map(String::as_str), Some("workbuddy"));
        // 国服不带这些头（保持现状）
        let mut h2 = HashMap::new();
        apply_region_headers(&json!({"domain": "www.workbuddy.cn"}), &mut h2);
        assert!(h2.is_empty(), "国服不应注入国际区域头");
    }

    #[test]
    fn oauth_profile_without_email_does_not_use_nickname_or_uid() {
        let profile = json!({"uid": "u-1", "nickname": "同名用户"});
        assert_eq!(oauth_profile_email(&profile), None);
    }

    #[test]
    fn oauth_profile_keeps_factual_email() {
        let profile = json!({"email": " user@example.com "});
        assert_eq!(
            oauth_profile_email(&profile).as_deref(),
            Some("user@example.com")
        );
    }
}
