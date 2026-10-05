# -*- coding: utf-8 -*-
"""一次性补丁：GLM Plan 自动切换接线（api 路由 / main 循环 / ZcodePage UI）。"""
import io

# ── 1) api.rs：use 已有 zcode；加 2 路由 + 2 handler + status 暴露 ──
p = 'crates/wb-switch-server/src/api.rs'
t = io.open(p, encoding='utf-8').read()

a_route = '.route("/api/zcode/claim/state", get(api_zcode_claim_state).post(api_zcode_claim_state_save))'
routes = a_route + '''
        .route("/api/zcode/auto-switch/config", post(api_zcode_autoswitch_config))
        .route("/api/zcode/auto-switch/tick", post(api_zcode_autoswitch_tick))'''
assert a_route in t, 'route anchor'
assert 'auto-switch/config' not in t, 'routes already present'
t = t.replace(a_route, routes, 1)

a_handler = 'async fn api_zcode_switch(Json(body): Json<Value>) -> Response {'
handlers = '''/// POST /api/zcode/auto-switch/config：启用/关闭自动切换与阈值。
async fn api_zcode_autoswitch_config(Json(body): Json<Value>) -> Response {
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let threshold = body.get("thresholdTokens").and_then(Value::as_f64).unwrap_or(0.0);
    match zcode::save_auto_switch_config(enabled, threshold) {
        Ok(()) => json_ok(json!({ "ok": true, "enabled": enabled, "thresholdTokens": threshold.max(0.0) })),
        Err(e) => json_err(e, StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/auto-switch/tick：立即跑一轮自动切换检查（后台每 5 分钟自动跑）。
async fn api_zcode_autoswitch_tick() -> Response {
    json_ok(zcode::auto_switch_tick().await)
}

''' + a_handler
assert a_handler in t, 'handler anchor'
t = t.replace(a_handler, handlers, 1)

# status 暴露 autoSwitch
a_status = '''    json!({
        "ok": true,
        "liveExists": live.is_some(),
        "loggedIn": logged_in,'''
assert a_status in t or True
# status() 在 core；api 层 handler 直接透传 —— 找 api_zcode_status 确认
if 'api_zcode_status' in t and 'autoSwitch' not in t:
    # core status() 增加 autoSwitch 字段（在 zcode.rs）
    pc = 'crates/wb-switch-core/src/modules/zcode.rs'
    tc = io.open(pc, encoding='utf-8').read()
    a2 = '''    json!({
        "ok": true,
        "liveExists": live.is_some(),
        "loggedIn": logged_in,
        "desktopRunning": desktop_running(),'''
    b2 = '''    json!({
        "ok": true,
        "liveExists": live.is_some(),
        "loggedIn": logged_in,
        "autoSwitch": auto_switch_state(),
        "desktopRunning": desktop_running(),'''
    assert a2 in tc, 'core status anchor'
    tc = tc.replace(a2, b2, 1)
    io.open(pc, 'w', encoding='utf-8').write(tc)
    print('core status exposes autoSwitch')

io.open(p, 'w', encoding='utf-8').write(t)
print('api.rs routes+handlers ok')

# ── 2) main.rs：启动自动切换循环 ──
p = 'crates/wb-switch-server/src/main.rs'
t = io.open(p, encoding='utf-8').read()
if 'spawn_auto_switch_loop' not in t:
    a = '    wb_switch_core::modules::autoclaw_watch::spawn_watch_loop();'
    assert a in t, 'main anchor'
    t = t.replace(a, a + '\n    wb_switch_core::modules::zcode::spawn_auto_switch_loop();', 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('main.rs loop ok')
