# -*- coding: utf-8 -*-
"""一次性补丁：MiniMax 桌面端导入接线（api 路由/handler/status 暴露 source + FE 导入按钮）。"""
import io

# ── 1) api.rs：路由 + 2 handler + status 暴露 source ──
p = 'crates/wb-switch-server/src/api.rs'
t = io.open(p, encoding='utf-8').read()

a_route = '.route("/api/minimax/test", post(api_minimax_test))'
if 'capture-desktop' not in t:
    assert a_route in t, 'route anchor'
    t = t.replace(a_route, a_route + '''
        .route("/api/minimax/desktop/discover", get(api_minimax_desktop_discover))
        .route("/api/minimax/desktop/capture", post(api_minimax_desktop_capture))''', 1)

a_handler = 'async fn api_minimax_signin_run(Json(body): Json<Value>) -> Response {'
if 'api_minimax_desktop_discover' not in t:
    h = '''async fn api_minimax_desktop_discover() -> Response {
    json_ok(minimax::discover_desktop())
}

async fn api_minimax_desktop_capture() -> Response {
    json_ok(minimax::capture_from_desktop())
}

''' + a_handler
    assert a_handler in t, 'handler anchor'
    t = t.replace(a_handler, h, 1)

# status 暴露 source
a_src = '"tokenMasked": minimax_mask(&a.token),'
if '"source": a.source' not in t:
    assert a_src in t, 'status src anchor'
    t = t.replace(a_src, a_src + '\n                "source": a.source,', 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('api.rs ok')

# ── 2) api.ts：封装 ──
p = 'src/lib/api.ts'
t = io.open(p, encoding='utf-8').read()
if 'capture_minimax_desktop' not in t:
    a = '  run_minimax_signin: { method: "POST", path: "/api/minimax/signin/run" },'
    assert a in t, 'map anchor'
    t = t.replace(a, a + '''
  capture_minimax_desktop: { method: "POST", path: "/api/minimax/desktop/capture" },''', 1)
    a2 = 'export async function runMinimaxSignin(body: { id?: string }) {'
    add = '''/** 从桌面端登录态导入 MiniMax 账号（只读共享） */
export async function captureMinimaxDesktop() {
  return call("capture_minimax_desktop");
}
''' + a2
    assert a2 in t, 'fn anchor'
    t = t.replace(a2, add, 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('api.ts ok')

# ── 3) MiniMaxPage：导入按钮 + 桌面端徽标 ──
p = 'src/pages/MiniMaxPage.tsx'
t = io.open(p, encoding='utf-8').read()

if '从桌面端导入' not in t:
    a0 = '''          <div className="flex flex-wrap items-center gap-2 px-4 py-3 sm:px-5">
            <Button size="sm" disabled={busy === "signin" || !status.enabled} onClick={() => void runSignin()}>'''
    b0 = '''          <div className="flex flex-wrap items-center gap-2 px-4 py-3 sm:px-5">
            <Button
              size="sm"
              variant="outline"
              disabled={busy === "capture"}
              onClick={() =>
                void act("capture", {}, "").then((r) => {
                  const res = r as { added?: number; skipped?: string[] } | undefined;
                  if (res?.added) setMsg({ type: "ok", text: `已从桌面端导入 ${res.added} 个账号（只读共享登录态，桌面端续期自动跟上）` });
                })
              }
            >
              {busy === "capture" ? <Loader2 className="animate-spin" /> : <Download />}从桌面端导入
            </Button>
            <Button size="sm" disabled={busy === "signin" || !status.enabled} onClick={() => void runSignin()}>'''
    assert a0 in t, 'page btn anchor'
    t = t.replace(a0, b0, 1)

# act 返回值：让 act 把响应返回
old = '''      const fn = actions[name];
      if (!fn) throw new Error(`未知操作: ${name}`);
      await fn();
      if (name !== "probe") setMsg({ type: "ok", text: okText });
      await refresh();'''
new = '''      const fn = actions[name];
      if (!fn) throw new Error(`未知操作: ${name}`);
      const result = await fn();
      if (name !== "probe") setMsg({ type: "ok", text: okText });
      await refresh();
      return result;'''
if 'const result = await fn();' not in t:
    assert old in t, 'act return anchor'
    t = t.replace(old, new, 1)
# capture action 映射
a_act = '        saveMinimaxConfig: () => api.saveMinimaxConfig(body),'
if 'captureMinimaxDesktop' not in t:
    assert a_act in t, 'act map anchor'
    t = t.replace(a_act, a_act + '''
        capture: () => api.captureMinimaxDesktop(),''', 1)
# async function act 签名加返回类型
t = t.replace('  async function act(name: string, body: Record<string, unknown>, okText: string) {',
              '  async function act(name: string, body: Record<string, unknown>, okText: string): Promise<unknown> {')

# 桌面端徽标（账号行）
a_badge = '''                  <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">
                    签到 {a.signinLastDate ? a.signinLastDate : "未签"} · 累计 {a.signinPointsTotal} 分
                  </Badge>'''
if '桌面端共享' not in t:
    assert a_badge in t, 'badge anchor'
    t = t.replace(a_badge, '''                  {(a as { source?: string }).source === "desktop" ? (
                    <Badge variant="outline" className="h-5 px-1.5 text-[10px]">桌面端共享</Badge>
                  ) : null}
''' + a_badge, 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('MiniMaxPage ok')
