# -*- coding: utf-8 -*-
"""一次性补丁：minimax.rs desktop 路径重构——纯 Bearer + 正确域名（agent.minimax.cn）。"""
import io

p = 'crates/wb-switch-core/src/modules/minimax.rs'
t = io.open(p, encoding='utf-8').read()

# 1) desktop 域名助手 + 云请求改造
old = '''/// 签名 GET（/minimax-cloud 族；GET 的签名 body 按约定为 "{}"）。
async fn cloud_get(acc: &MmAccount, path: &str, extra_query: Option<&str>) -> Result<Value, String> {
    let host = account_host(acc);
    let (wire, ts, xs, yy) = sign_request(acc, path, "{}", cfg_yy_absolute());
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
}'''
new = '''/// 云端域名：desktop 来源走 agent.minimax.{cn|io}（2026-10-04 实测：桌面 OAuth
/// token 用纯 Bearer 即可通过云端鉴权；签名+token 头形态反而 401）；web 来源
/// 按账号 host（签名路径）。
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

/// 签名 GET（/minimax-cloud 族；GET 的签名 body 按约定为 "{}"）。
/// desktop 来源改走纯 Bearer（cloud_host 换 agent.minimax.cn，无签名头——
/// 实测桌面 OAuth token 在签名+token 头形态下 401，纯 Bearer 200）。
async fn cloud_get(acc: &MmAccount, path: &str, extra_query: Option<&str>) -> Result<Value, String> {
    let host = cloud_host(acc);
    if is_bearer_only(acc) {
        let mut url = format!("{host}{path}");
        if let Some(eq) = extra_query {
            url = format!("{url}&{eq}");
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
}'''
assert old in t, 'cloud_get anchor'
t = t.replace(old, new, 1)

# 2) cloud_post 同改造
old = '''async fn cloud_post(acc: &MmAccount, path: &str, body: &str) -> Result<Value, String> {
    let host = account_host(acc);
    let (wire, ts, xs, yy) = sign_request(acc, path, body, cfg_yy_absolute());
    let url = format!("{host}{path}?{wire}");
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
}'''
new = '''async fn cloud_post(acc: &MmAccount, path: &str, body: &str) -> Result<Value, String> {
    let host = cloud_host(acc);
    if is_bearer_only(acc) {
        let url = format!("{host}{path}");
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
    let url = format!("{host}{path}?{wire}");
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
}'''
assert old in t, 'cloud_post anchor'
t = t.replace(old, new, 1)

# 3) chat_once：desktop 走 mavis 模型端点（Bearer 免签名，已实测 200）
old = '''    // ① 创建会话（路径按区域：国内 archon / 国际 minimax-cloud——snake vs juangchuank 实测口径）
    let (session_path_prefix, stream_host) = if acc.region == PROVIDER_BIGMODEL {
        (format!("{host}/archon/api/v1"), format!("{host}").replace("agent.", "agent-stream."))
    } else {
        (format!("{host}/minimax-cloud/api/v1"), stream_host_for(&host))
    };'''
new = '''    // desktop 来源 → 官方模型直连端点（2026-10-04 实测 200，Bearer 免签名，
    // Anthropic 协议；比网页会话流简单可靠一个量级）
    if is_bearer_only(&acc) {
        let mavis = format!("{}/mavis/api/v1/llm/v1/messages", cloud_model_host(&acc));
        let payload = json!({
            "model": "MiniMax-M3.1-Flash-Preview",
            "max_tokens": 4096,
            "messages": [{ "role": "user", "content": prompt }],
        });
        let resp = upstream_client()
            .post(&mavis)
            .bearer_auth(effective_token(&acc))
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .json(&payload)
            .send()
            .await
            .map_err(|e| (502u16, format!("模型直连失败: {e}")))?;
        let st = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
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
            "model": v.get("model").cloned().unwrap_or(json!("MiniMax-M3.1-Flash-Preview")),
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": v.get("usage").cloned().unwrap_or(json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})),
            "minimax": {"account": acc.label, "region": acc.region, "route": "model-direct"},
        }));
    }

    // ① 创建会话（路径按区域：国内 archon / 国际 minimax-cloud——snake vs juangchuank 实测口径）
    let (session_path_prefix, stream_host) = if acc.region == PROVIDER_BIGMODEL {
        (format!("{host}/archon/api/v1"), format!("{host}").replace("agent.", "agent-stream."))
    } else {
        (format!("{host}/minimax-cloud/api/v1"), stream_host_for(&host))
    };'''
assert old in t, 'chat desktop anchor'
t = t.replace(old, new, 1)

# 4) cloud_model_host 助手（模型端点域名：desktop 国内 = agent.minimaxi.com）
old = '''fn normalize_region(region: Option<&str>) -> Option<&'static str> {'''
new = '''/// 模型直连端点域名（desktop 实测：国内 agent.minimaxi.com 200；国际
/// agent.minimax.io 为对应域——区域绑定，跨区 401 invalid access token）。
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

fn normalize_region(region: Option<&str>) -> Option<&'static str> {'''
assert old in t, 'model host anchor'
t = t.replace(old, new, 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('desktop bearer path done')
