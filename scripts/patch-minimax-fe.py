# -*- coding: utf-8 -*-
"""一次性补丁：MiniMax 前端接线（api.ts 封装 / gateway-registry / App.tsx 路由 / test 端点）。"""
import io, re

# ── 1) api.ts：端点表 + 封装 ──
p = 'src/lib/api.ts'
t = io.open(p, encoding='utf-8').read()
if 'get_minimax_status' not in t:
    a = '  tick_zcode_autoswitch: { method: "POST", path: "/api/zcode/auto-switch/tick" },'
    b = a + '''
  get_minimax_status: { method: "GET", path: "/api/minimax/status" },
  save_minimax_config: { method: "POST", path: "/api/minimax/config" },
  add_minimax_account: { method: "POST", path: "/api/minimax/accounts/add" },
  remove_minimax_account: { method: "POST", path: "/api/minimax/accounts/remove" },
  enable_minimax_account: { method: "POST", path: "/api/minimax/accounts/enable" },
  run_minimax_signin: { method: "POST", path: "/api/minimax/signin/run" },'''
    assert a in t, 'api map anchor'
    t = t.replace(a, b, 1)
if 'export async function getMinimaxStatus' not in t:
    anchor = 'export async function tickZcodeAutoSwitch() {'
    add = '''/** MiniMax 通道（1.6.0）状态 */
export async function getMinimaxStatus() {
  return call("get_minimax_status");
}
/** 保存 MiniMax 通道配置 */
export async function saveMinimaxConfig(body: Record<string, unknown>) {
  return call("save_minimax_config", body);
}
/** 加入 MiniMax 账号池 */
export async function addMinimaxAccount(body: Record<string, string>) {
  return call("add_minimax_account", body);
}
/** 移出 MiniMax 账号池 */
export async function removeMinimaxAccount(body: { id: string }) {
  return call("remove_minimax_account", body);
}
/** 启用/停用 MiniMax 账号 */
export async function enableMinimaxAccount(body: { id: string; enabled: boolean }) {
  return call("enable_minimax_account", body);
}
/** MiniMax 立即全量签到 */
export async function runMinimaxSignin(body: { id?: string }) {
  return call("run_minimax_signin", body);
}
''' + anchor
    assert anchor in t, 'api fn anchor'
    t = t.replace(anchor, add, 1)
io.open(p, 'w', encoding='utf-8').write(t)
print('api.ts ok')

# ── 2) gateway-registry：侧栏导航 ──
p = 'src/lib/gateway-registry.ts'
t = io.open(p, encoding='utf-8').read()
if 'glmplan' in t and 'minimax' not in t:
    a = '  { id: "glmplan", path: "/glm-plan", label: "GLM Plan", icon: Layers },'
    b = a + '\n  { id: "minimax", path: "/minimax", label: "MiniMax", icon: Bot },'
    assert a in t, 'registry anchor'
    t = t.replace(a, b, 1)
    t = t.replace('  Network,\n', '  Bot,\n  Network,\n', 1) if 'Bot,' not in t else t
    io.open(p, 'w', encoding='utf-8').write(t)
print('gateway-registry ok')

# ── 3) App.tsx：路由 ──
p = 'src/App.tsx'
t = io.open(p, encoding='utf-8').read()
if 'MiniMaxPage' not in t:
    a = 'import GlmPlanPage from "@/pages/GlmPlanPage";'
    assert a in t, 'app import anchor'
    t = t.replace(a, a + '\nimport MiniMaxPage from "@/pages/MiniMaxPage";', 1)
    a2 = '            <Route path="/glm-plan" element={<GlmPlanPage />} />'
    assert a2 in t, 'app route anchor'
    t = t.replace(a2, a2 + '\n            <Route path="/minimax" element={<MiniMaxPage />} />', 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('App.tsx ok')

# ── 4) api.rs：补 /api/minimax/test 端点（页面试一试）──
p = 'crates/wb-switch-server/src/api.rs'
t = io.open(p, encoding='utf-8').read()
if 'api_minimax_test' not in t:
    a = '.route("/api/minimax/signin/run", post(api_minimax_signin_run))'
    assert a in t, 'test route anchor'
    t = t.replace(a, a + '\n        .route("/api/minimax/test", post(api_minimax_test))', 1)
    a2 = 'async fn api_minimax_signin_run(Json(body): Json<Value>) -> Response {'
    h = '''async fn api_minimax_test(Json(body): Json<Value>) -> Response {
    // 本地管理端点（与 /api/glm-proxy/test 同口径）：不走网关密钥
    let prompt = body.get("prompt").and_then(Value::as_str).unwrap_or("只回复 OK").to_string();
    let started = std::time::Instant::now();
    match minimax::chat_once(&prompt, None).await {
        Ok(v) => {
            let content = v
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            json_ok(json!({ "ok": true, "content": content, "latencyMs": started.elapsed().as_millis() as u64 }))
        }
        Err((s, e)) => json_ok(json!({ "ok": false, "error": e, "status": s })),
    }
}

async fn api_minimax_signin_run(Json(body): Json<Value>) -> Response {'''
    assert a2 in t, 'test handler anchor'
    t = t.replace(a2, h, 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('test endpoint ok')
