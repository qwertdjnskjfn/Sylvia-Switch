# -*- coding: utf-8 -*-
"""一次性补丁：接入 AutoClaw 封禁监控（模块注册/探测端点/后台循环/路由）。"""
import io

# 1) mod.rs 注册
p = 'crates/wb-switch-core/src/modules/mod.rs'
t = io.open(p, encoding='utf-8').read()
if 'pub mod autoclaw_watch;' not in t:
    a = 'pub mod autoclaw_accounts;'
    assert a in t
    t = t.replace(a, a + '\npub mod autoclaw_watch;', 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('mod.rs ok')

# 2) glm_proxy.rs：公开账号健康探测函数
p = 'crates/wb-switch-core/src/modules/glm_proxy.rs'
t = io.open(p, encoding='utf-8').read()
anchor = '/// 轮询计数：多账号时按请求轮换。'
probe_fn = '''/// 1.5.0：账号健康探测（autoclaw_watch 专用）——最小 chat 请求，返回 (状态码, 响应原文)。
/// 不走账号池/缓存/重试，纯单次直连，用于区分封禁(410004)/额度(810001)/登录态(401)。
pub async fn probe_account_chat(region_str: &str, token: &str) -> (u16, String) {
    let region = AccountRegion::from_str(region_str).unwrap_or(AccountRegion::Domestic);
    let payload = json!({
        "model": "glm-5.3",
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": false,
        "max_tokens": 16,
    });
    match send_once(&payload, token, "zaicoding_glm-5.3", region).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            (status, text)
        }
        Err(e) => (0, e),
    }
}

''' + anchor
assert anchor in t
t = t.replace(anchor, probe_fn, 1)
io.open(p, 'w', encoding='utf-8').write(t)
print('glm_proxy probe fn ok')

# 3) main.rs：启动后台循环
p = 'crates/wb-switch-server/src/main.rs'
t = io.open(p, encoding='utf-8').read()
if 'spawn_watch_loop' not in t:
    a = '    wb_switch_core::modules::grok_pool::spawn_health_check_loop();'
    assert a in t
    t = t.replace(a, a + '\n    wb_switch_core::modules::autoclaw_watch::spawn_watch_loop();', 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('main.rs ok')

# 4) api.rs：use + 路由 + handler
p = 'crates/wb-switch-server/src/api.rs'
t = io.open(p, encoding='utf-8').read()
m = None
import re
mm = re.search(r'use wb_switch_core::modules::\{([^}]+)\};', t)
inner = mm.group(1)
if 'autoclaw_watch' not in inner:
    new_inner = inner.replace('account_protection,', 'account_protection, autoclaw_watch,', 1) if 'account_protection,' in inner else inner.rstrip() + ', autoclaw_watch'
    t = t[:mm.start(1)] + new_inner + t[mm.end(1):]

a_route = '.route("/api/glmplan/status", get(api_glmplan_status))'
routes = '''.route("/api/autoclaw/watch", get(api_autoclaw_watch_state))
        .route("/api/autoclaw/watch/probe", post(api_autoclaw_watch_probe))
        ''' + a_route
assert a_route in t
t = t.replace(a_route, routes, 1)

a_handler = 'async fn api_glmplan_status() -> Response {'
handlers = '''async fn api_autoclaw_watch_state() -> Response {
    // 只读存储的监控状态（不触发网络探测；探测走 POST /probe）
    json_ok(autoclaw_watch::watch_state())
}

async fn api_autoclaw_watch_probe() -> Response {
    json_ok(autoclaw_watch::probe_now().await)
}

''' + a_handler
assert a_handler in t
t = t.replace(a_handler, handlers, 1)
io.open(p, 'w', encoding='utf-8').write(t)
print('api.rs ok')
