# -*- coding: utf-8 -*-
"""一次性补丁：ZcodePage 自动切换卡片（占位符版，避免 f-string 花括号冲突）。"""
import io, re

# ── 1) types.ts ──
p = 'src/lib/types.ts'
t = io.open(p, encoding='utf-8').read()
if 'ZcodeAutoSwitchState' not in t:
    old = '''export interface ZcodeStatus {
  /** credentials.json 文件存在 */
  liveExists: boolean;
  /** 能解析出登录身份（= 已登录） */
  loggedIn: boolean;'''
    new = '''export interface ZcodeAutoSwitchState {
  config?: { enabled: boolean; thresholdTokens: number };
  lastTickAt?: number | null;
  lastState?: string | null;
  lastSwitchAt?: number | null;
  lastSwitchTo?: string | null;
  lastMessage?: string | null;
  events?: { ts: number; message: string }[];
}

export interface ZcodeStatus {
  /** credentials.json 文件存在 */
  liveExists: boolean;
  /** 能解析出登录身份（= 已登录） */
  loggedIn: boolean;
  /** 1.5.2 多账号自动管理（额度耗尽自动切换） */
  autoSwitch?: ZcodeAutoSwitchState;'''
    assert old in t, 'types anchor'
    t = t.replace(old, new, 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('types.ts ok')

# ── 2) api.ts ──
p = 'src/lib/api.ts'
t = io.open(p, encoding='utf-8').read()
if 'save_zcode_autoswitch_config' not in t:
    anchor = 'export async function getGlmplanStatus() {'
    add = '''/** 保存 ZCode 自动切换配置（额度耗尽自动热切换） */
export async function saveZcodeAutoSwitchConfig(body: { enabled: boolean; thresholdTokens: number }) {
  return call("save_zcode_autoswitch_config", body);
}
/** 立即跑一轮 ZCode 自动切换检查 */
export async function tickZcodeAutoSwitch() {
  return call("tick_zcode_autoswitch");
}
''' + anchor
    assert anchor in t, 'api anchor'
    t = t.replace(anchor, add, 1)
    a = '  get_glmplan_status: { method: "GET", path: "/api/glmplan/status" },'
    b = a + '''
  save_zcode_autoswitch_config: { method: "POST", path: "/api/zcode/auto-switch/config" },
  tick_zcode_autoswitch: { method: "POST", path: "/api/zcode/auto-switch/tick" },'''
    assert a in t, 'api map anchor'
    t = t.replace(a, b, 1)
    io.open(p, 'w', encoding='utf-8').write(t)
print('api.ts ok')

# ── 3) ZcodePage ──
p = 'src/pages/ZcodePage.tsx'
t = io.open(p, encoding='utf-8').read()

if '额度耗尽自动切换' not in t:
    a0 = '  const [zcodeProxyEdit, setZcodeProxyEdit] = useState<{ id: string; value: string } | null>(null);'
    b0 = a0 + '''
  // 1.5.2 额度耗尽自动切换
  const [asEnabled, setAsEnabled] = useState<boolean | null>(null);
  const [asThreshold, setAsThreshold] = useState("0");
  const [asBusy, setAsBusy] = useState(false);'''
    assert a0 in t, 'a0'
    t = t.replace(a0, b0, 1)

    a1 = '    const st = await api.getZcodeStatus();'
    b1 = '''    const st = await api.getZcodeStatus();
    if (st.autoSwitch?.config) {
      setAsEnabled(st.autoSwitch.config.enabled);
      setAsThreshold(String(st.autoSwitch.config.thresholdTokens ?? 0));
    }'''
    assert a1 in t, 'a1'
    t = t.replace(a1, b1, 1)

    m = re.search(r'\n(\s*)<Section\s+title="未检测到登录态：请先打开 ZCode 登录账号"', t)
    assert m, 'ui anchor'
    INDENT = m.group(1)

    card = '''
@@<Section
@@  title="额度耗尽自动切换"
@@  description="后台每 5 分钟检查当前账号剩余额度；低于阈值时自动热切换到还有余额的池账号（复用已存凭据，无需重新登录，会话不中断）；全部耗尽时桌面通知提示补充"
@@>
@@  <CardContent className="p-0">
@@    <div className="flex flex-wrap items-center gap-3 px-4 py-3 sm:px-5">
@@      <Switch
@@        checked={asEnabled ?? false}
@@        onCheckedChange={(checked) => {
@@          setAsBusy(true);
@@          api.saveZcodeAutoSwitchConfig({ enabled: checked, thresholdTokens: Number(asThreshold) || 0 })
@@            .then(() => setAsEnabled(checked))
@@            .catch((e) => setMsg({ type: "err", text: String(e) }))
@@            .finally(() => setAsBusy(false));
@@        }}
@@      />
@@      <span className="text-xs text-muted-foreground">{asEnabled ? "已启用（后台每 5 分钟检查）" : "已停用"}</span>
@@      <span className="text-xs text-muted-foreground">耗尽阈值（tokens）</span>
@@      <Input
@@        className="w-32"
@@        value={asThreshold}
@@        onChange={(e) => setAsThreshold(e.target.value.replace(/[^0-9]/g, ""))}
@@        placeholder="0"
@@      />
@@      <Button
@@        variant="outline"
@@        size="sm"
@@        disabled={asBusy}
@@        onClick={() => {
@@          setAsBusy(true);
@@          api.saveZcodeAutoSwitchConfig({ enabled: asEnabled ?? false, thresholdTokens: Number(asThreshold) || 0 })
@@            .then(() => setMsg({ type: "ok", text: "阈值已保存" }))
@@            .catch((e) => setMsg({ type: "err", text: String(e) }))
@@            .finally(() => setAsBusy(false));
@@        }}
@@      >
@@        保存阈值
@@      </Button>
@@      <Button
@@        variant="outline"
@@        size="sm"
@@        disabled={asBusy}
@@        onClick={() => {
@@          setAsBusy(true);
@@          api.tickZcodeAutoSwitch()
@@            .then((r) => {
@@              const res = r as { action?: string; message?: string };
@@              setMsg({ type: "ok", text: res.message || `本轮结果: ${res.action ?? "none"}` });
@@              void refresh();
@@            })
@@            .catch((e) => setMsg({ type: "err", text: String(e) }))
@@            .finally(() => setAsBusy(false));
@@        }}
@@      >
@@        立即检查
@@      </Button>
@@    </div>
@@    {status?.autoSwitch?.lastMessage ? (
@@      <div className="mx-4 mb-3 rounded-lg bg-muted/60 px-3 py-2 text-xs text-muted-foreground sm:mx-5">
@@        最近状态：{status.autoSwitch.lastState ?? "—"}
@@        {status.autoSwitch.lastSwitchTo ? ` · 已切到「${status.autoSwitch.lastSwitchTo}」` : ""}
@@        <br />
@@        {status.autoSwitch.lastMessage}
@@      </div>
@@    ) : null}
@@  </CardContent>
@@</Section>

'''
    card = card.replace('@@', INDENT)
    t = t[:m.start()] + card + t[m.start():]
    io.open(p, 'w', encoding='utf-8').write(t)
print('ZcodePage card ok')
