# -*- coding: utf-8 -*-
"""Remove the DuMate multi-account pool section + related UI code from AccountsPage.tsx.

Strategy: bottom-up marker-based cuts, each with a uniqueness assertion; the file is
written back ONLY when every step passes. Nothing else is touched (api.ts, DuMatePage,
backend dumate.rs, PLATFORM_LABELS's dumate entry, unified non-pool rows stay).
"""

import io, sys

PATH = "src/pages/AccountsPage.tsx"
with io.open(PATH, "r", encoding="utf-8") as f:
    t = f.read()
orig_len = len(t)


def cut(text, start, end, label):
    i = text.find(start)
    assert i != -1, f"{label}: START not found -> {start[:70]!r}"
    j = text.find(end, i)
    assert j != -1, f"{label}: END not found -> {end[:70]!r}"
    assert j > i, f"{label}: END before START"
    return text[:i] + text[j:]


def repl(text, old, new, label, count=1):
    n = text.count(old)
    assert n == count, (
        f"{label}: expected {count} occurrence(s), got {n} -> {old[:70]!r}"
    )
    return text.replace(old, new)


# 1) Whole DuMate card section (1465-1557), keep the "添加与迁移账号" div that follows.
t = cut(
    t,
    "      {/* DuMate \u591a\u8d26\u53f7\u5361\uff08\u5951\u7ea6 \u00a74.3\uff09\uff1a\u591a\u8d26\u53f7\u4ec5\u7528\u4e8e\u7b7e\u5230/\u79ef\u5206\uff08\u4e00\u53f7\u4e00 IP\uff09\uff0c\u6a21\u578b\u8c03\u7528\u8d70\u672c\u673a\u4e3b\u8d26\u53f7 */}",
    '      <div className="relative mb-6 overflow-visible rounded-2xl',
    "1-card",
)

# 2) Inline proxy-edit ternary head (unifiedProxyEdit?.id === acc.id ? (...) : (<>)
t = cut(
    t,
    "{unifiedProxyEdit?.id === acc.id ? (",
    "\n                    ) : (\n                        <>",
    "2-proxy-ternary",
)

# 3) Proxy display span in the row meta block
t = repl(
    t,
    '                        {isPool && (\n                          <span className="max-w-[220px] truncate">\n                            {acc.proxy ? `\u4ee3\u7406 ${acc.proxy}` : "\u76f4\u8fde"}\n                          </span>\n                        )}\n',
    "",
    "3-proxy-span",
)

# 4) Proxy-edit button (+ tooltip) and remove button (+ tooltip)
t = repl(
    t,
    """                            {isPool && (
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Button
                                    size="sm"
                                    variant="ghost"
                                    className="h-7 px-1.5 text-xs"
                                    onClick={() => setUnifiedProxyEdit({ id: acc.id, value: acc.proxy ?? "" })}
                                    aria-label={`\u7f16\u8f91 ${displayName} \u4ee3\u7406`}
                                  >
                                    <Globe className="size-3.5" />
                                  </Button>
                                </TooltipTrigger>
                                <TooltipContent side="top">\u7f16\u8f91\u51fa\u7ad9\u4ee3\u7406\uff08\u4e00\u53f7\u4e00 IP\uff09</TooltipContent>
                              </Tooltip>
                            )}
                            {isPool && (
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Button
                                    size="sm"
                                    variant="ghost"
                                    className="h-7 px-1.5 text-xs text-destructive hover:text-destructive"
                                    onClick={() => void onUnifiedRemoveDumate(acc)}
                                    disabled={unifiedBusyId !== null}
                                    aria-label={`\u5220\u9664 ${displayName}`}
                                  >
                                    <Trash2 className="size-3.5" />
                                  </Button>
                                </TooltipTrigger>
                                <TooltipContent side="top">\u5220\u9664\u8be5 DuMate \u6c60\u8d26\u53f7</TooltipContent>
                              </Tooltip>
                            )}
""",
    "",
    "4-row-buttons",
)

# 5) isPool local decl
t = repl(
    t,
    "                  const isPool = isDumatePool(acc);\n",
    "",
    "5-isPool-decl",
)

# 6) savingProxy local decl
t = repl(
    t,
    "                  const savingProxy = unifiedBusyId === `proxy:${acc.id}`;\n",
    "",
    "6-savingProxy-decl",
)

# 7) isDumatePool helper definition + trailing blank line
t = repl(
    t,
    '  /** \u7edf\u4e00\u8d26\u53f7\u662f\u5426\u4e3a DuMate \u6c60\u8d26\u53f7\uff08\u5951\u7ea6 \u00a72\uff1a\u4ec5\u6c60\u8d26\u53f7\u6709 proxy \u5b57\u6bb5\uff09\u3002 */\n  const isDumatePool = (acc: UnifiedAccount): boolean => acc.id.startsWith("dumate:pool_");\n\n',
    "",
    "7-isDumatePool",
)

# 8) Header subtitle rewrite (no DuMate mention)
t = repl(
    t,
    "\u7edf\u4e00\u7ba1\u7406\u5168\u90e8\u5e73\u53f0\u8d26\u53f7\uff1a\u7b7e\u5230\u3001\u79ef\u5206\u3001\u767b\u5f55\u6001\u4e0e DuMate \u4e00\u53f7\u4e00 IP \u51fa\u7ad9\u4ee3\u7406\uff0c\u5168\u540d\u76f4\u663e\u65e0\u906e\u7f69\u3002",
    "\u7edf\u4e00\u7ba1\u7406\u5168\u90e8\u5e73\u53f0\u8d26\u53f7\uff1a\u7b7e\u5230\u3001\u79ef\u5206\u3001\u767b\u5f55\u6001\u4e0e\u51fa\u7ad9\u4ee3\u7406\uff0c\u5168\u540d\u76f4\u663e\u65e0\u906e\u7f69\u3002",
    "8-subtitle",
)

# 9) Five handler functions + section comment block, up to (not incl.) loadPlatformCheckins
t = cut(
    t,
    "  /** \u4fdd\u5b58\u7edf\u4e00\u8868\u884c\u5185\u7f16\u8f91\u7684\u51fa\u7ad9\u4ee3\u7406\uff08\u7a7a\u4e32 = \u76f4\u8fde\uff1b\u4ec5 dumate \u6c60\u8d26\u53f7\uff09\u3002 */",
    "  async function loadPlatformCheckins() {",
    "9-handlers",
)

# 10) loadDumatePool function block, up to (not incl.) the useEffect
t = cut(
    t,
    "  /** DuMate \u8d26\u53f7\u6c60\u5217\u8868\uff08\u5931\u8d25\u843d dumateError\uff0c\u5361\u5185\u5c55\u793a\uff0c\u4e0d\u6253\u65ad\u5176\u4ed6\u533a\u5757\uff09\u3002 */\n  /** \u8d26\u53f7\u6c60\u53ef\u52a0\u8f7d\u6027\u63a2\u6d4b\uff08\u8d26\u53f7\u5217\u8868\u5df2\u5728\u300c\u7edf\u4e00\u8d26\u53f7\u300d\u8868\u5c55\u793a\uff0c\u6b64\u5904\u53ea\u8bb0\u52a0\u8f7d\u9519\u8bef\uff09\u3002 */",
    "  useEffect(() => {\n    void loadUnified(true);",
    "10-loadDumatePool",
)

# 11) Three call sites inside the useEffect (keep loadUnified on the same line)
t = repl(
    t,
    "    void loadUnified(true);\n    void loadDumatePool();",
    "    void loadUnified(true);",
    "11a-call1",
)
t = repl(
    t,
    "      void loadUnified();\n      void loadDumatePool();",
    "      void loadUnified();",
    "11b-call2",
)
t = repl(
    t,
    "        void loadUnified();\n        void loadDumatePool();",
    "        void loadUnified();",
    "11c-call3",
)

# 12) refreshUnifiedAll body/comment
t = repl(
    t,
    "  /** \u624b\u52a8\u5237\u65b0\uff1a\u7edf\u4e00\u805a\u5408 + DuMate \u8d26\u53f7\u6c60\u4e00\u8d77\u91cd\u62c9\u3002 */\n  async function refreshUnifiedAll() {\n    await Promise.all([loadUnified(true), loadDumatePool()]);\n  }",
    "  /** \u624b\u52a8\u5237\u65b0\uff1a\u7edf\u4e00\u805a\u5408\u8d26\u53f7\u91cd\u62c9\u3002 */\n  async function refreshUnifiedAll() {\n    await loadUnified(true);\n  }",
    "12-refreshUnifiedAll",
)

# 13) unifiedProxyEdit state (with its comment)
t = repl(
    t,
    "  /** \u7edf\u4e00\u8868\u884c\u5185\u4ee3\u7406\u7f16\u8f91\uff08\u4ec5 dumate \u6c60\u8d26\u53f7\uff09 */\n  const [unifiedProxyEdit, setUnifiedProxyEdit] = useState<{ id: string; value: string } | null>(null);\n\n",
    "",
    "13-unifiedProxyEdit",
)

# 14) dumate* state block, down to (not incl.) the useEffect head
t = repl(
    t,
    '  // ---- DuMate \u591a\u8d26\u53f7\u6c60\uff08\u5951\u7ea6 \u00a71/\u00a74.3\uff1b\u591a\u8d26\u53f7\u4ec5\u7528\u4e8e\u7b7e\u5230/\u79ef\u5206\uff0c\u6a21\u578b\u8c03\u7528\u8d70\u672c\u673a\u4e3b\u8d26\u53f7\uff09----\n  const [dumateError, setDumateError] = useState<string | null>(null);\n  const [dumateBusy, setDumateBusy] = useState<string | null>(null);\n  const [dumateAddOpen, setDumateAddOpen] = useState(false);\n  const [dumateAddName, setDumateAddName] = useState("");\n  const [dumateAddCookies, setDumateAddCookies] = useState("");\n  const [dumateAddProxy, setDumateAddProxy] = useState("");\n  const [dumateAdding, setDumateAdding] = useState(false);\n\n  useEffect(() => {',
    "  useEffect(() => {",
    "14-dumate-states",
)

# 15) parseDumateCookies function + its doc comment, up to (not incl.) AccountsPage default export
t = cut(
    t,
    "/** \u89e3\u6790\u5e76\u6821\u9a8c DuMate cookies JSON",
    "export default function AccountsPage()",
    "15-parseDumateCookies",
)

# Final guard: none of the deleted identifiers may survive (outside PLATFORM_LABELS dumate label)
for ident in [
    "isDumatePool(",
    "unifiedProxyEdit",
    "loadDumatePool",
    "onDumateAdd",
    "onDumateImportClient",
    "onUnifiedRemoveDumate",
    "saveUnifiedProxy",
    "dumateAddOpen",
    "dumateError",
    "dumateBusy",
    "parseDumateCookies",
    "isPool =",
    "savingProxy =",
]:
    assert ident not in t, f"guard: {ident} still present"

assert len(t) < orig_len, "file should have shrunk"

with io.open(PATH, "w", encoding="utf-8", newline="") as f:
    f.write(t)
print("PATCH_OK removed_bytes=%d" % (orig_len - len(t)))
