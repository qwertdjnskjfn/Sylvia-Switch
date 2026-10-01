# -*- coding: utf-8 -*-
"""一次性补丁：api.rs 接入 GLM Plan 网关（路由 + 8 个 handler）。"""
import io

p = 'crates/wb-switch-server/src/api.rs'
t = io.open(p, encoding='utf-8').read()

# 1) 路由注册（紧跟 glm-proxy/status 之后）
anchor_route = '.route("/api/glm-proxy/status", get(api_glm_proxy_status))'
routes = anchor_route + '''
        // ── GLM Plan 网关（1.4.0 新通道：GLM Coding Plan 账号池）──
        .route("/api/glmplan/status", get(api_glmplan_status))
        .route("/api/glmplan/config", post(api_glmplan_config_save))
        .route("/api/glmplan/accounts/add", post(api_glmplan_accounts_add))
        .route("/api/glmplan/accounts/remove", post(api_glmplan_accounts_remove))
        .route("/api/glmplan/accounts/enable", post(api_glmplan_accounts_enable))
        .route("/glmplan/v1/models", get(api_glmplan_models))
        .route("/glmplan/v1/chat/completions", post(api_glmplan_chat))
        .route("/glmplan/v1/messages", post(api_glmplan_messages))'''
assert anchor_route in t, 'route anchor'
assert '/glmplan/v1/models' not in t, 'glmplan routes already present'
t = t.replace(anchor_route, routes, 1)

# 2) handlers：插到 api_glm_proxy_status 之前
anchor_handler = 'async fn api_glm_proxy_status() -> Response {'
handlers = '''// ---------------------------------------------------------------------------
// GLM Plan 网关（1.4.0 新通道：GLM Coding Plan 账号池 → OpenAI/Anthropic）
// ---------------------------------------------------------------------------

fn glmplan_mask_key(key: &str) -> String {
    if key.len() <= 10 {
        "*".repeat(key.len())
    } else {
        format!("{}...{}", &key[..6], &key[key.len() - 4..])
    }
}

fn glmplan_key_ok(headers: &HeaderMap) -> bool {
    let cfg = glmplan::load_config();
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    matches!(got, Some(k) if k == cfg.api_key)
}

async fn api_glmplan_status() -> Response {
    let cfg = glmplan::load_config();
    let stats = glmplan::cache_stats();
    let accounts: Vec<Value> = cfg
        .accounts
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "label": a.label,
                "provider": a.provider,
                "providerLabel": glmplan::provider_label(&a.provider),
                "apiKeyMasked": glmplan_mask_key(&a.api_key),
                "enabled": a.enabled,
                "lastError": a.last_error,
                "cooling": a.cooldown_until_ms > glmplan::now_ms(),
            })
        })
        .collect();
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(json!({
        "enabled": cfg.enabled,
        "apiKeyMasked": glmplan_mask_key(&cfg.api_key),
        "defaultProvider": cfg.default_provider,
        "defaultModel": cfg.default_model,
        "models": cfg.models,
        "accounts": accounts,
        "cache": {
            "enabled": cfg.cache_enabled,
            "hits": stats.hits,
            "misses": stats.misses,
            "entries": stats.entries,
        },
        "endpoint": format!("http://127.0.0.1:{port}/glmplan/v1"),
    }))
}

async fn api_glmplan_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = glmplan::load_config();
    if let Some(b) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = b;
    }
    if let Some(p) = body.get("defaultProvider").and_then(Value::as_str) {
        if glmplan::provider_info(p).is_some() {
            cfg.default_provider = p.to_string();
        }
    }
    if let Some(m) = body.get("defaultModel").and_then(Value::as_str) {
        if !m.is_empty() {
            cfg.default_model = m.to_string();
        }
    }
    if let Some(b) = body.get("cacheEnabled").and_then(Value::as_bool) {
        cfg.cache_enabled = b;
    }
    if let Some(b) = body.get("spoof").and_then(Value::as_bool) {
        cfg.spoof = b;
    }
    if body.get("regenerateKey").and_then(Value::as_bool).unwrap_or(false) {
        cfg.api_key = glmplan::gen_api_key();
    }
    glmplan::save_config(&cfg);
    json_ok(json!({ "ok": true }))
}

async fn api_glmplan_accounts_add(Json(body): Json<Value>) -> Response {
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if glmplan::provider_info(&provider).is_none() {
        return json_err(
            "provider 必须为 bigmodel（国内版）或 zai（国际版）".to_string(),
            StatusCode::BAD_REQUEST,
        );
    }
    let api_key = body
        .get("apiKey")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if api_key.is_empty() {
        return json_err("缺少 apiKey".to_string(), StatusCode::BAD_REQUEST);
    }
    let label = body
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("未命名")
        .trim()
        .to_string();
    let mut cfg = glmplan::load_config();
    if cfg.accounts.iter().any(|a| a.api_key == api_key) {
        return json_err("该 API Key 已在账号池中".to_string(), StatusCode::BAD_REQUEST);
    }
    let id = glmplan::gen_id();
    cfg.accounts.push(glmplan::PlanAccount {
        id: id.clone(),
        label,
        provider,
        api_key,
        enabled: true,
        added_at: glmplan::now_ms(),
        last_error: None,
        cooldown_until_ms: 0,
    });
    if !cfg.enabled {
        cfg.enabled = true;
    }
    glmplan::save_config(&cfg);
    json_ok(json!({ "ok": true, "id": id }))
}

async fn api_glmplan_accounts_remove(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let mut cfg = glmplan::load_config();
    let before = cfg.accounts.len();
    cfg.accounts.retain(|a| a.id != id);
    if cfg.accounts.len() == before {
        return json_err("账号不存在".to_string(), StatusCode::NOT_FOUND);
    }
    glmplan::save_config(&cfg);
    json_ok(json!({ "ok": true }))
}

async fn api_glmplan_accounts_enable(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let mut cfg = glmplan::load_config();
    match cfg.accounts.iter_mut().find(|a| a.id == id) {
        Some(a) => {
            a.enabled = enabled;
            a.cooldown_until_ms = 0;
            glmplan::save_config(&cfg);
            json_ok(json!({ "ok": true }))
        }
        None => json_err("账号不存在".to_string(), StatusCode::NOT_FOUND),
    }
}

async fn api_glmplan_models(headers: HeaderMap, RawQuery(raw_query): RawQuery) -> Response {
    if !glmplan_key_ok(&headers) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let cfg = glmplan::load_config();
    let region = raw_query.as_deref().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("region=").map(|v| v.to_string()))
    });
    json_ok(glmplan::models_payload_filtered(&cfg, region.as_deref()))
}

async fn api_glmplan_chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !glmplan_key_ok(&headers) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if streaming {
        return match glmplan::chat_stream(body).await {
            Ok(resp) => {
                let ct = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .cloned()
                    .unwrap_or(header::HeaderValue::from_static("text/event-stream"));
                let stream = futures_util::stream::unfold(resp, |mut r| async move {
                    match r.chunk().await {
                        Ok(Some(c)) => Some((Ok::<_, String>(c), r)),
                        Ok(None) => None,
                        Err(e) => Some((Err(format!("GLM Plan 流中断: {e}")), r)),
                    }
                });
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, ct)],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
            Err((s, e)) => {
                glm_proxy_error(e, StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY))
            }
        };
    }
    match glmplan::chat_completions(body).await {
        Ok(v) => json_ok(v),
        Err((s, e)) => glm_proxy_error(e, StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY)),
    }
}

async fn api_glmplan_messages(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !glmplan_key_ok(&headers) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match glmplan::anthropic_messages(body).await {
        Ok((status, text)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            [(header::CONTENT_TYPE, "application/json")],
            text,
        )
            .into_response(),
        Err((s, e)) => glm_proxy_error(e, StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY)),
    }
}

''' + anchor_handler
assert anchor_handler in t, 'handler anchor'
t = t.replace(anchor_handler, handlers, 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('api.rs glmplan wired')
