# -*- coding: utf-8 -*-
"""一次性补丁：api.rs 接入 MiniMax 通道（路由 + 9 个 handler）+ main.rs 循环。"""
import io, re

# ── 1) api.rs：use + 路由 + handlers ──
p = 'crates/wb-switch-server/src/api.rs'
t = io.open(p, encoding='utf-8').read()
mm = re.search(r'use wb_switch_core::modules::\{([^}]+)\};', t)
inner = mm.group(1)
if 'minimax' not in inner:
    new_inner = inner.replace('loomy,', 'loomy, minimax,', 1) if 'loomy,' in inner else inner.rstrip() + ', minimax'
    t = t[:mm.start(1)] + new_inner + t[mm.end(1):]

a_route = '.route("/api/autoclaw/watch", get(api_autoclaw_watch_state))'
routes = '''.route("/api/minimax/status", get(api_minimax_status))
        .route("/api/minimax/config", post(api_minimax_config_save))
        .route("/api/minimax/accounts/add", post(api_minimax_accounts_add))
        .route("/api/minimax/accounts/remove", post(api_minimax_accounts_remove))
        .route("/api/minimax/accounts/enable", post(api_minimax_accounts_enable))
        .route("/api/minimax/signin/run", post(api_minimax_signin_run))
        .route("/api/minimax/probe", post(api_minimax_probe))
        .route("/minimax/v1/models", get(api_minimax_models))
        .route("/minimax/v1/chat/completions", post(api_minimax_chat))
        ''' + a_route
assert a_route in t, 'route anchor'
assert '/api/minimax/status' not in t, 'already present'
t = t.replace(a_route, routes, 1)

a_handler = 'async fn api_autoclaw_watch_state() -> Response {'
handlers = '''// ---------------------------------------------------------------------------
// MiniMax 通道（1.6.0：Agent 网页端反代 + 账号池 + 每日签到）
// ---------------------------------------------------------------------------

fn minimax_mask(key: &str) -> String {
    if key.len() <= 12 {
        "*".repeat(key.len())
    } else {
        format!("{}...{}", &key[..8], &key[key.len() - 4..])
    }
}

fn minimax_key_ok(headers: &HeaderMap) -> bool {
    let cfg = minimax::load_config();
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    matches!(got, Some(k) if k == cfg.api_key)
}

async fn api_minimax_status() -> Response {
    let cfg = minimax::load_config();
    let accounts: Vec<Value> = cfg
        .accounts
        .iter()
        .map(|a| {
            json!({
                "id": a.id, "label": a.label, "region": a.region,
                "regionLabel": minimax::provider_label(&a.region),
                "tokenMasked": minimax_mask(&a.token),
                "uuid": a.uuid, "deviceId": a.device_id, "userId": a.user_id,
                "agentId": a.agent_id, "baseUrl": a.base_url,
                "enabled": a.enabled, "lastError": a.last_error,
                "cooldown": a.cooldown_until_ms > minimax::now_ms(),
                "signinLastDate": a.signin_last_date, "signinPointsTotal": a.signin_points_total,
            })
        })
        .collect();
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(json!({
        "enabled": cfg.enabled,
        "apiKey": cfg.api_key,
        "defaultModel": "MiniMax-M3",
        "models": ["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"],
        "accounts": accounts,
        "chatExperimental": true,
        "endpoint": format!("http://127.0.0.1:{port}/minimax/v1"),
        "payloadTemplate": cfg.chat_payload_template,
    }))
}

async fn api_minimax_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = minimax::load_config();
    if let Some(b) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = b;
    }
    if let Some(b) = body.get("regenerateKey").and_then(Value::as_bool).unwrap_or(false) {
        cfg.api_key = minimax::gen_api_key();
    }
    if let Some(t) = body.get("chatPayloadTemplate").and_then(Value::as_str) {
        if !t.is_empty() {
            cfg.chat_payload_template = t.to_string();
        }
    }
    minimax::save_config(&cfg);
    json_ok(json!({ "ok": true }))
}

async fn api_minimax_accounts_add(Json(body): Json<Value>) -> Response {
    let region = body.get("region").and_then(Value::as_str).unwrap_or("domestic").to_string();
    if !matches!(region.as_str(), "domestic" | "international") {
        return json_err("region 必须为 domestic（国内版）或 international（国际版）".to_string(), StatusCode::BAD_REQUEST);
    }
    let token = body.get("token").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if token.is_empty() {
        return json_err("缺少 token（网页端 JWT，localStorage._token）".to_string(), StatusCode::BAD_REQUEST);
    }
    let mut cfg = minimax::load_config();
    let id = minimax::gen_id();
    cfg.accounts.push(minimax::MmAccount {
        id: id.clone(),
        label: body.get("label").and_then(Value::as_str).unwrap_or("未命名").trim().to_string(),
        region,
        token,
        uuid: body.get("uuid").and_then(Value::as_str).unwrap_or("").to_string(),
        device_id: body.get("deviceId").and_then(Value::as_str).unwrap_or("").to_string(),
        user_id: body.get("userId").and_then(Value::as_str).unwrap_or("").to_string(),
        agent_id: body.get("agentId").and_then(Value::as_str).unwrap_or("").to_string(),
        screen_w: body.get("screenW").and_then(Value::as_str).unwrap_or("1707").to_string(),
        screen_h: body.get("screenH").and_then(Value::as_str).unwrap_or("1067").to_string(),
        base_url: body.get("baseUrl").and_then(Value::as_str).unwrap_or("").to_string(),
        enabled: true,
        added_at: minimax::now_ms(),
        last_error: None,
        cooldown_until_ms: 0,
        signin_last_date: String::new(),
        signin_points_total: 0,
    });
    if !cfg.enabled {
        cfg.enabled = true;
    }
    minimax::save_config(&cfg);
    json_ok(json!({ "ok": true, "id": id }))
}

async fn api_minimax_accounts_remove(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let mut cfg = minimax::load_config();
    let before = cfg.accounts.len();
    cfg.accounts.retain(|a| a.id != id);
    if cfg.accounts.len() == before {
        return json_err("账号不存在".to_string(), StatusCode::NOT_FOUND);
    }
    minimax::save_config(&cfg);
    json_ok(json!({ "ok": true }))
}

async fn api_minimax_accounts_enable(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let mut cfg = minimax::load_config();
    match cfg.accounts.iter_mut().find(|a| a.id == id) {
        Some(a) => {
            a.enabled = enabled;
            a.cooldown_until_ms = 0;
            minimax::save_config(&cfg);
            json_ok(json!({ "ok": true }))
        }
        None => json_err("账号不存在".to_string(), StatusCode::NOT_FOUND),
    }
}

async fn api_minimax_signin_run(Json(body): Json<Value>) -> Response {
    let only_id = body.get("id").and_then(Value::as_str).map(String::from);
    let cfg = minimax::load_config();
    if !cfg.enabled {
        return json_err("MiniMax 网关未启用".to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    let mut results = Vec::new();
    for acc in &cfg.accounts {
        if !acc.enabled {
            continue;
        }
        if let Some(id) = &only_id {
            if &acc.id != id {
                continue;
            }
        }
        let res = minimax::signin_account(acc).await;
        results.push(res);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    // 签到结果回写签到日期
    let mut cfg = minimax::load_config();
    for res in &results {
        if res.get("ok").and_then(Value::as_bool) == Some(true) {
            let label = res.get("account").and_then(Value::as_str).unwrap_or("");
            if let Some(a) = cfg.accounts.iter_mut().find(|a| a.label == label) {
                a.signin_last_date = a.signin_last_date.clone();
            }
        }
    }
    json_ok(json!({ "ok": true, "results": results }))
}

async fn api_minimax_probe(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let cfg = minimax::load_config();
    let Some(acc) = cfg.accounts.iter().find(|a| a.id == id).cloned() else {
        return json_err("账号不存在".to_string(), StatusCode::NOT_FOUND);
    };
    let started = std::time::Instant::now();
    let (status, detail) = minimax::probe_account(&acc).await;
    let balance = if status == 200 {
        // 探测成功顺带返回余额
        match minimax::membership_balance(&acc).await {
            Ok(v) => Some(v),
            None => None,
        }
    } else {
        None
    };
    json_ok(json!({
        "ok": status == 200, "account": acc.label, "status": status,
        "detail": detail, "balance": balance,
        "latencyMs": started.elapsed().as_millis() as u64,
    }))
}

async fn api_minimax_models(headers: HeaderMap, RawQuery(raw_query): RawQuery) -> Response {
    if !minimax_key_ok(&headers) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let cfg = minimax::load_config();
    let region = raw_query.as_deref().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("region=").map(|v| v.to_string()))
    });
    json_ok(minimax::models_payload_filtered(&cfg, region.as_deref()))
}

async fn api_minimax_chat(headers: HeaderMap, Json(mut body): Json<Value>) -> Response {
    if !minimax_key_ok(&headers) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let region = body.get("region").and_then(Value::as_str).map(String::from);
    let prompt = body
        .pointer("/messages/-1/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    body["region"] = json!(region.unwrap_or_default());
    body["prompt"] = json!(prompt);
    match minimax::chat_once(&prompt, region.as_deref()).await {
        Ok(v) => json_ok(v),
        Err((s, e)) => glm_proxy_error(e, StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY)),
    }
}

''' + a_handler
assert a_handler in t, 'handler anchor'
t = t.replace(a_handler, handlers, 1)
io.open(p, 'w', encoding='utf-8').write(t)
print('api.rs minimax wired')

# ── 2) main.rs：启动每日签到循环 ──
p = 'crates/wb-switch-server/src/main.rs'
t = io.open(p, encoding='utf-8').read()
if 'spawn_signin_loop' not in t:
    a = '    wb_switch_core::modules::zcode::spawn_auto_switch_loop();'
    assert a in t, 'main anchor'
    t = t.replace(a, a + '\n    wb_switch_core::modules::minimax::spawn_signin_loop();', 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('main.rs loop ok')
