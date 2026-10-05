# -*- coding: utf-8 -*-
"""Remove the DuMate multi-account management block from AccountsPage.tsx (UI entry only).

Scope (keep): unified accounts table dumate rows, backend /api/dumate/* endpoints,
DuMatePage independent page, api.ts dumate wrappers, data files.
This script only deletes page-local state/functions/JSX plus the now-unused Plus icon import.
"""

import io
import re
import sys

PATH = r"E:\WorkBuddy自动签到助手\workbuddy-switch\src\pages\AccountsPage.tsx"
with io.open(PATH, encoding="utf-8") as f:
    t = f.read()

BYTES_BEFORE = len(t.encode("utf-8"))


def rep(old: str, new: str, tag: str, count: int = 1) -> None:
    global t
    n = t.count(old)
    assert n == count, f"[{tag}] expected {count} match, got {n}"
    t = t.replace(old, new)
    print(f"[ok] {tag}")


# 1) Plus icon import is only used inside the DuMate block -> remove import.
rep("  Plus,\n", "", "import Plus")

# 2) parseDumateCookies helper + surrounding blank line.
rep(
    "  return pkgs.some((p) => p.expiringSoon);\n"
    "}\n"
    "\n"
    "/** 解析并校验 DuMate cookies JSON（数组或 {cookies:[...]}，逐条 name/value 非空）。 */\n"
    "function parseDumateCookies(text: string): { ok: true; parsed: unknown } | { ok: false; error: string } {\n"
    "  let parsed: unknown;\n"
    "  try {\n"
    "    parsed = JSON.parse(text);\n"
    "  } catch {\n"
    "    return { ok: false, error: 'cookies JSON 解析失败：请粘贴完整的 JSON 数组或 {\"cookies\":[...]}' };\n"
    "  }\n"
    "  const list = Array.isArray(parsed)\n"
    "    ? parsed\n"
    '    : parsed && typeof parsed === "object" && Array.isArray((parsed as { cookies?: unknown }).cookies)\n'
    "      ? (parsed as { cookies: unknown[] }).cookies\n"
    "      : null;\n"
    "  if (!list || list.length === 0) {\n"
    "    return { ok: false, error: '未识别到 cookie 列表：支持 JSON 数组或 {\"cookies\":[...]}' };\n"
    "  }\n"
    "  const bad = list.findIndex((c) => {\n"
    '    if (!c || typeof c !== "object") return true;\n'
    "    const o = c as { name?: unknown; value?: unknown };\n"
    '    return typeof o.name !== "string" || !o.name || typeof o.value !== "string" || !o.value;\n'
    "  });\n"
    "  if (bad >= 0) {\n"
    "    return { ok: false, error: `第 ${bad + 1} 条 cookie 缺少非空的 name / value 字段` };\n"
    "  }\n"
    "  return { ok: true, parsed };\n"
    "}\n"
    "\n"
    "export default function AccountsPage() {",
    "  return pkgs.some((p) => p.expiringSoon);\n"
    "}\n"
    "\n"
    "export default function AccountsPage() {",
    "parseDumateCookies",
)

# 3) DuMate pool state block (319-326 + trailing blank line).
rep(
    "  // ---- DuMate 多账号池（契约 §1/§4.3；多账号仅用于签到/积分，模型调用走本机主账号）----\n"
    "  const [dumateError, setDumateError] = useState<string | null>(null);\n"
    "  const [dumateBusy, setDumateBusy] = useState<string | null>(null);\n"
    "  const [dumateAddOpen, setDumateAddOpen] = useState(false);\n"
    '  const [dumateAddName, setDumateAddName] = useState("");\n'
    '  const [dumateAddCookies, setDumateAddCookies] = useState("");\n'
    '  const [dumateAddProxy, setDumateAddProxy] = useState("");\n'
    "  const [dumateAdding, setDumateAdding] = useState(false);\n"
    "\n",
    "",
    "state block",
)

# 4) loadDumatePool + double doc comment (438-447 + trailing blank line).
rep(
    "  /** DuMate 账号池列表（失败落 dumateError，卡内展示，不打断其他区块）。 */\n"
    "  /** 账号池可加载性探测（账号列表已在「统一账号」表展示，此处只记加载错误）。 */\n"
    "  async function loadDumatePool() {\n"
    "    try {\n"
    "      await api.getDumateAccounts();\n"
    "      setDumateError(null);\n"
    "    } catch (e) {\n"
    "      setDumateError(api.asError(e));\n"
    "    }\n"
    "  }\n"
    "\n",
    "",
    "loadDumatePool",
)

# 5) useEffect: three loadDumatePool() call sites.
rep(
    "    void loadUnified(true);\n    void loadDumatePool();\n",
    "    void loadUnified(true);\n",
    "useEffect initial",
)
rep(
    "      void loadUnified();\n      void loadDumatePool();\n",
    "      void loadUnified();\n",
    "useEffect interval",
)
rep(
    "        void loadUnified();\n        void loadDumatePool();\n",
    "        void loadUnified();\n",
    "useEffect visibility",
)

# 6) refreshUnifiedAll: drop dumate part.
rep(
    "  /** 手动刷新：统一聚合 + DuMate 账号池一起重拉。 */\n"
    "  async function refreshUnifiedAll() {\n"
    "    await Promise.all([loadUnified(true), loadDumatePool()]);\n"
    "  }",
    "  /** 手动刷新：统一聚合。 */\n"
    "  async function refreshUnifiedAll() {\n"
    "    await loadUnified(true);\n"
    "  }",
    "refreshUnifiedAll",
)

# 7) saveUnifiedProxy & onUnifiedRemoveDumate: both Promise.all([...loadDumatePool()]) -> loadUnified().
rep(
    "      await Promise.all([loadUnified(), loadDumatePool()]);\n",
    "      await loadUnified();\n",
    "Promise.all->loadUnified",
    count=2,
)

# 8) onDumateImportClient + onDumateAdd + section comment (540-594 + blank-line tail).
rep(
    "  }\n"
    "\n"
    "  // ---------------------------------------------------------------------------\n"
    "  // DuMate 多账号池：导入 / 添加 / 签到 / 代理 / 删除\n"
    "  // ---------------------------------------------------------------------------\n"
    "\n"
    "  /** 导入本机客户端登录态为池账号（后端按 bce-user-info 去重，重复直接报错）。 */\n"
    "  async function onDumateImportClient() {\n"
    '    setDumateBusy("import");\n'
    "    try {\n"
    "      const res = await api.importDumateClient();\n"
    '      toast.success(res.message || "已导入本机登录态");\n'
    "      await Promise.all([loadDumatePool(), loadUnified()]);\n"
    "    } catch (e) {\n"
    "      toast.error(api.asError(e));\n"
    "    } finally {\n"
    "      setDumateBusy(null);\n"
    "    }\n"
    "  }\n"
    "\n"
    "  /** 添加池账号：客户端先校验 cookies JSON（数组或 {cookies:[...]}，逐条 name/value 非空）。 */\n"
    "  async function onDumateAdd() {\n"
    "    const name = dumateAddName.trim();\n"
    "    if (!name) {\n"
    '      toast.error("请填写账号名称");\n'
    "      return;\n"
    "    }\n"
    "    const text = dumateAddCookies.trim();\n"
    "    if (!text) {\n"
    '      toast.error("请粘贴 cookies JSON");\n'
    "      return;\n"
    "    }\n"
    "    const parsed = parseDumateCookies(text);\n"
    "    if (!parsed.ok) {\n"
    "      toast.error(parsed.error);\n"
    "      return;\n"
    "    }\n"
    "    setDumateAdding(true);\n"
    "    try {\n"
    "      await api.addDumateAccount({\n"
    "        name,\n"
    "        cookies: parsed.parsed,\n"
    "        proxy: dumateAddProxy.trim(),\n"
    '        note: "",\n'
    "      });\n"
    "      toast.success(`已添加账号「${name}」`);\n"
    '      setDumateAddName("");\n'
    '      setDumateAddCookies("");\n'
    '      setDumateAddProxy("");\n'
    "      setDumateAddOpen(false);\n"
    "      await Promise.all([loadDumatePool(), loadUnified()]);\n"
    "    } catch (e) {\n"
    "      toast.error(api.asError(e));\n"
    "    } finally {\n"
    "      setDumateAdding(false);\n"
    "    }\n"
    "  }\n"
    "\n"
    "\n"
    "\n"
    "\n"
    "  async function loadPlatformCheckins() {",
    "  }\n\n  async function loadPlatformCheckins() {",
    "import+add handlers",
)

# 9) JSX block (1484-1575 + trailing blank line).
rep(
    "      {/* DuMate 多账号卡（契约 §4.3）：多账号仅用于签到/积分（一号一 IP），模型调用走本机主账号 */}\n"
    '      <section className="mb-8 min-w-0" aria-labelledby="dumate-pool-title">\n'
    '        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">\n'
    "          {/* 工具条：导入本机登录态 / 添加账号 */}\n"
    '          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">\n'
    '            <div className="flex min-w-0 items-center gap-2">\n'
    '              <Users className="size-4 shrink-0 text-muted-foreground" />\n'
    '              <h2 id="dumate-pool-title" className="text-sm font-medium">\n'
    "                DuMate 多账号管理\n"
    "              </h2>\n"
    "              {dumateError ? (\n"
    '                <Badge variant="destructive" className="text-[10px]">加载失败</Badge>\n'
    "              ) : null}\n"
    "            </div>\n"
    '            <div className="flex shrink-0 items-center gap-2">\n'
    "              <DemoAction>\n"
    "                <Button\n"
    '                  size="sm"\n'
    '                  variant="outline"\n'
    "                  onClick={() => void onDumateImportClient()}\n"
    '                  disabled={dumateBusy === "import"}\n'
    '                  title="把本机 DuMate 客户端当前登录的百度账号导入账号池（与既有账号同一登录态时会提示）"\n'
    "                >\n"
    '                  {dumateBusy === "import" ? <Loader2 className="animate-spin" /> : <Download />}\n'
    "                  导入本机登录态\n"
    "                </Button>\n"
    "              </DemoAction>\n"
    "              <Button\n"
    '                size="sm"\n'
    '                variant={dumateAddOpen ? "secondary" : "outline"}\n'
    "                onClick={() => setDumateAddOpen((v) => !v)}\n"
    "              >\n"
    "                <Plus />\n"
    '                {dumateAddOpen ? "收起" : "添加账号"}\n'
    "              </Button>\n"
    "            </div>\n"
    "          </div>\n"
    "\n"
    "          {/* 添加账号（折叠表单：名称 / cookies JSON / 代理 URL） */}\n"
    "          {dumateAddOpen ? (\n"
    '            <div className="space-y-3 border-t border-border/50 bg-muted/30 px-4 py-4 sm:px-5">\n'
    '              <div className="grid gap-3 sm:grid-cols-2">\n'
    "                <div>\n"
    '                  <label className="text-xs font-medium text-muted-foreground">账号名称</label>\n'
    "                  <Input\n"
    '                    className="mt-1"\n'
    '                    placeholder="如：二号机"\n'
    "                    value={dumateAddName}\n"
    "                    onChange={(e) => setDumateAddName(e.target.value)}\n"
    "                  />\n"
    "                </div>\n"
    "                <div>\n"
    '                  <label className="text-xs font-medium text-muted-foreground">\n'
    "                    出站代理 URL（一号一 IP，可选）\n"
    "                  </label>\n"
    "                  <Input\n"
    '                    className="mt-1"\n'
    '                    placeholder="http:// 或 socks5://，留空 = 直连"\n'
    "                    value={dumateAddProxy}\n"
    "                    onChange={(e) => setDumateAddProxy(e.target.value)}\n"
    "                  />\n"
    "                </div>\n"
    "              </div>\n"
    "              <div>\n"
    '                <label className="text-xs font-medium text-muted-foreground">\n'
    "                  Cookies JSON（浏览器 DevTools 导出的百度登录 cookie）\n"
    "                </label>\n"
    "                <textarea\n"
    "                  rows={4}\n"
    '                  placeholder=\'[{"name":"...","value":"...","domain":"..."},...] 或 {"cookies":[...]}\'\n'
    "                  value={dumateAddCookies}\n"
    "                  onChange={(e) => setDumateAddCookies(e.target.value)}\n"
    '                  className="mt-1 block w-full rounded-md border border-input bg-background p-2.5 font-mono text-xs shadow-sm focus:border-primary focus:outline-none focus:ring-1 focus:ring-primary"\n'
    "                />\n"
    "              </div>\n"
    '              <div className="flex items-center gap-2">\n'
    '                <Button size="sm" onClick={() => void onDumateAdd()} disabled={dumateAdding}>\n'
    '                  {dumateAdding ? <Loader2 className="animate-spin" /> : <Plus />}保存账号\n'
    "                </Button>\n"
    '                <Button size="sm" variant="ghost" onClick={() => setDumateAddOpen(false)}>\n'
    "                  取消\n"
    "                </Button>\n"
    "              </div>\n"
    "            </div>\n"
    "          ) : null}\n"
    "\n"
    '          <p className="px-4 pb-3 text-[11px] leading-5 text-muted-foreground sm:px-5">\n'
    "            多账号仅用于签到与积分查询（每个账号可配独立出站代理，实现一号一 IP，降低同 IP 关联风控）；\n"
    "            模型调用仍走本机主账号。账号已在「统一账号」表展示并参与统一签到，此处不再重复列出。\n"
    "          </p>\n"
    "        </Card>\n"
    "      </section>\n"
    "\n",
    "",
    "JSX block",
)

# 10) sanity: no stray blank-line runs where the big handler block was removed.
t = re.sub(r"\n{4,}", "\n\n\n", t)

with io.open(PATH, "w", encoding="utf-8", newline="\n") as f:
    f.write(t)

print(f"[done] bytes {BYTES_BEFORE} -> {len(t.encode('utf-8'))}")
