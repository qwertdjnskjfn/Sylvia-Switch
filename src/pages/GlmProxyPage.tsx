import { useEffect, useState } from "react";
import { Activity, Check, CircleCheck, Loader2, PlugZap, RefreshCw, Send, ShieldAlert, ShieldCheck, Trash2, UserPlus } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import type {
  AccountProtectionStatusInfo,
  AutoclawAccountInfo,
  AutoclawCheckinResult,
  GlmProxyConfigInfo,
  GlmProxyStatus,
  ModelMonitorItem,
} from "@/lib/types";
import { cn } from "@/lib/utils";

/** 各模型的使用建议（依据本机实测：速度 / 消耗等级 / 模态）。 */
const MODEL_ADVICE: Record<string, string> = {
  auto: "日常默认：实测最快、消耗低",
  "auto-fast": "日常默认：快且支持看图",
  "glm-5.3-flash": "看图/多模态专用；纯文本建议换 Auto",
  "glm-5.3": "仅复杂重构；首字延迟约为 Auto 的 2 倍、消耗等级高",
};

function formatNumber(value: number): string {
  return Number.isInteger(value) ? value.toLocaleString("zh-CN") : value.toLocaleString("zh-CN", { maximumFractionDigits: 1 });
}

function formatTokens(value: number): string {
  const v = value ?? 0;
  const abs = Math.abs(v);
  const sign = v < 0 ? "-" : "";
  if (abs >= 100_000_000) return `${sign}${(abs / 100_000_000).toFixed(1)}亿`;
  if (abs >= 10_000) return `${sign}${(abs / 10_000).toFixed(1)}万`;
  return formatNumber(v);
}

function formatLatency(ms: number): string {
  if (!ms) return "—";
  return ms >= 1000 ? `${(ms / 1000).toFixed(ms >= 10_000 ? 0 : 1)}s` : `${Math.round(ms)}ms`;
}

interface SectionProps {
  title: string;
  description?: string;
  children: React.ReactNode;
}

// v1.0+: 区域图标和标签映射
const REGION_ICONS = {
  international: '🌐',
  domestic: '🇨🇳',
};

const REGION_LABELS = {
  international: '国际版',
  domestic: '国内版',
};

function Section({ title, description, children }: SectionProps) {
  return (
    <section className="min-w-0 space-y-2.5" aria-labelledby={title}>
      <div className="px-1">
        <h2 className="text-[13px] font-medium leading-5">{title}</h2>
        {description ? <p className="mt-0.5 text-xs text-muted-foreground">{description}</p> : null}
      </div>
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="p-0">{children}</CardContent>
      </Card>
    </section>
  );
}

function Row({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="mx-4 flex min-w-0 flex-wrap items-center justify-between gap-2 border-b border-border/50 py-3 sm:mx-5">
      <div className="min-w-0">
        <p className="text-sm font-medium leading-5">{label}</p>
        {hint ? <p className="mt-0.5 text-xs text-muted-foreground">{hint}</p> : null}
      </div>
      {children}
    </div>
  );
}

const HERMES_SNIPPET = `model:
  default: "glm-5.3"
  provider: "custom"
  base_url: "http://127.0.0.1:57890/v1"
  api_key: "<访问密钥>"`;

export default function GlmProxyPage() {
  const [cfg, setCfg] = useState<GlmProxyConfigInfo | null>(null);
  const [status, setStatus] = useState<GlmProxyStatus | null>(null);
  const [probing, setProbing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [loadModel, setLoadModel] = useState("glm-5.3");
  const [testPrompt, setTestPrompt] = useState("只回复四个字：测试成功");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [accounts, setAccounts] = useState<AutoclawAccountInfo[]>([]);
  const [checkinResults, setCheckinResults] = useState<Record<string, AutoclawCheckinResult>>({});
  const [accountsBusy, setAccountsBusy] = useState<string | null>(null);
  const [checkinAllRunning, setCheckinAllRunning] = useState(false);
  const [showAddForm, setShowAddForm] = useState(false);
  const [newLabel, setNewLabel] = useState("");
  const [newAccess, setNewAccess] = useState("");
  const [newRefresh, setNewRefresh] = useState("");
  const [newDevice, setNewDevice] = useState("");
  /** 添加账号的区域分入口：国际版 / 国内版（默认国际版） */
  const [newRegion, setNewRegion] = useState<'international' | 'domestic'>('international');
  /** 模型 id -> 近 7 天实测统计（用于「模型选型参考」表格） */
  const [speedStats, setSpeedStats] = useState<Record<string, ModelMonitorItem>>({});
  const [protection, setProtection] = useState<AccountProtectionStatusInfo | null>(null);
  const [protectionBusy, setProtectionBusy] = useState(false);

  async function reloadProtection() {
    try {
      const res = await api.getProtectionStatus();
      setProtection(res);
    } catch {
      // 忽略错误
    }
  }

  async function toggleProtection(enabled: boolean) {
    if (!protection) return;
    setProtectionBusy(true);
    try {
      const updated = await api.saveProtectionConfig({ ...protection.config, enabled });
      setProtection((prev) =>
        prev
          ? {
              ...prev,
              config: updated.config,
              status: updated.config.enabled ? "active" : "disabled",
            }
          : null,
      );
    } finally {
      setProtectionBusy(false);
    }
  }

  async function toggleKeepalive(keepaliveEnabled: boolean) {
    if (!protection) return;
    setProtectionBusy(true);
    try {
      const updated = await api.saveProtectionConfig({ ...protection.config, keepaliveEnabled });
      setProtection((prev) => (prev ? { ...prev, config: updated.config } : null));
    } finally {
      setProtectionBusy(false);
    }
  }

  async function triggerKeepaliveNow() {
    setProtectionBusy(true);
    try {
      await api.triggerProtectionKeepalive();
      await reloadProtection();
    } finally {
      setProtectionBusy(false);
    }
  }

  async function clearCooldown(accountId: string) {
    setProtectionBusy(true);
    try {
      await api.clearProtectionCooldown(accountId);
      await reloadProtection();
    } finally {
      setProtectionBusy(false);
    }
  }

  async function loadAccounts() {
    const res = await api.getAutoclawAccounts();
    setAccounts(res.accounts ?? []);
  }

  useEffect(() => {
    if (!api.isWebui() || api.isDemoMode()) return;
    void api
      .getGlmProxyConfig()
      .then((c) => {
        setCfg(c);
        if (c.models.length > 0 && !c.models.some((m) => m.id === "glm-5.3")) {
          setLoadModel(c.models[0].id);
        }
      })
      .catch((e) => setLoadError(api.asError(e)));
    void loadAccounts().catch(() => undefined);
    void probe();
    void loadSpeedStats();
    void reloadProtection();

    const interval = setInterval(() => {
      void reloadProtection();
    }, 15000);
    return () => clearInterval(interval);
  }, []);

  /** 近 7 天实测：把监控数据按模型 id 建索引，供选型参考表使用。 */
  async function loadSpeedStats() {
    try {
      const monitor = await api.getGlmUsageMonitor(168);
      const map: Record<string, ModelMonitorItem> = {};
      for (const item of monitor.models) {
        map[item.model] = item;
      }
      setSpeedStats(map);
    } catch {
      // 统计不可用时表格降级显示「—」，不阻塞页面。
    }
  }

  async function probe() {
    setProbing(true);
    try {
      setStatus(await api.getGlmProxyStatus());
    } catch (e) {
      setLoadError(api.asError(e));
    } finally {
      setProbing(false);
    }
  }

  async function save(patch: {
    enabled?: boolean;
    apiKey?: string;
    regenerateKey?: boolean;
    speedFirst?: boolean;
    speedFirstTarget?: string;
    /** 账号策略：round_robin / local_first / local_only / pinned */
    accountMode?: string;
    /** pinned 策略使用的账号 id */
    pinnedAccountId?: string;
  }) {
    setSaving(true);
    try {
      setCfg(await api.saveGlmProxyConfig(patch));
    } catch (e) {
      setLoadError(api.asError(e));
    } finally {
      setSaving(false);
    }
  }

  async function runTest() {
    if (!testPrompt.trim() || testing) return;
    setTesting(true);
    setTestResult(null);
    try {
      const res = await callTest(loadModel, testPrompt.trim());
      setTestResult({
        ok: true,
        text: `${res.content || "（空回复）"}\n\n—— ${res.model} · ${res.latencyMs}ms`,
      });
    } catch (e) {
      setTestResult({ ok: false, text: api.asError(e) });
    } finally {
      setTesting(false);
    }
  }

  async function checkinOne(id: string) {
    setAccountsBusy(id);
    try {
      const res = await api.autoclawCheckinOne(id);
      setCheckinResults((prev) => ({ ...prev, [id]: res }));
    } catch (e) {
      setCheckinResults((prev) => ({
        ...prev,
        [id]: { id, label: id, result: "error", error: api.asError(e) },
      }));
    } finally {
      setAccountsBusy(null);
    }
  }

  async function checkinAll() {
    setCheckinAllRunning(true);
    try {
      const res = await api.autoclawCheckinAll();
      const map: Record<string, AutoclawCheckinResult> = {};
      for (const item of res.accounts ?? []) map[item.id] = item;
      setCheckinResults(map);
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setCheckinAllRunning(false);
    }
  }

  async function refreshAccount(id: string) {
    setAccountsBusy(id);
    try {
      await api.refreshAutoclawAccount(id);
      await loadAccounts();
    } catch (e) {
      setMsg({ type: "err", text: `刷新失败：${api.asError(e)}` });
    } finally {
      setAccountsBusy(null);
    }
  }

  async function removeAccount(id: string) {
    setAccountsBusy(id);
    try {
      await api.removeAutoclawAccount(id);
      await loadAccounts();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setAccountsBusy(null);
    }
  }

  async function importLocal() {
    setAccountsBusy("import");
    try {
      await api.importAutoclawLocal();
      await loadAccounts();
      setMsg({ type: "ok", text: "已导入本机当前登录的 AutoClaw 账号" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setAccountsBusy(null);
    }
  }

  async function submitAdd() {
    if (!newAccess.trim() || !newRefresh.trim()) {
      setMsg({ type: "err", text: "access token 与 refresh token 均必填" });
      return;
    }
    setAccountsBusy("add");
    try {
      await api.addAutoclawAccount({
        label: newLabel.trim() || undefined,
        accessToken: newAccess.trim(),
        refreshToken: newRefresh.trim(),
        deviceId: newDevice.trim() || undefined,
        region: newRegion,
      });
      setNewLabel("");
      setNewAccess("");
      setNewRefresh("");
      setNewDevice("");
      setShowAddForm(false);
      await loadAccounts();
      setMsg({ type: "ok", text: `账号已加入${REGION_LABELS[newRegion]}账号池` });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setAccountsBusy(null);
    }
  }

  /** 手动指定账号归属区域（防误判修正） */
  async function changeRegion(id: string, region: 'international' | 'domestic') {
    setAccountsBusy(id);
    try {
      await api.setAutoclawAccountRegion(id, region);
      await loadAccounts();
      setMsg({ type: "ok", text: `账号已归入${REGION_LABELS[region]}（签到与代理将路由到${REGION_LABELS[region]}网关）` });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setAccountsBusy(null);
    }
  }

  if (!api.isWebui() || api.isDemoMode()) {
    return (
      <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
        <Alert>
          <AlertTitle>GLM 代理</AlertTitle>
          <AlertDescription>该功能仅在 webui 后端（Sylvia Switch 启动器）下可用。</AlertDescription>
        </Alert>
      </div>
    );
  }

  if (loadError && !cfg) {
    return (
      <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
        <Alert>
          <AlertTitle>GLM 代理</AlertTitle>
          <AlertDescription>配置加载失败：{loadError}</AlertDescription>
        </Alert>
      </div>
    );
  }

  return (
    <div className="mx-auto min-w-0 w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-8 flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-2xl font-semibold tracking-tight">
            <PlugZap className="size-6 text-primary" />
            GLM 代理
          </h1>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            把 AutoClaw 订阅的 GLM 模型暴露为本机 OpenAI 兼容接口，供 Hermes
            等任意 OpenAI 客户端使用；登录态失效自动续期。
          </p>
        </div>
        {cfg ? (
          <div className="flex shrink-0 items-center gap-2 pt-1">
            <span className="text-xs text-muted-foreground">{cfg.enabled ? "已启用" : "已停用"}</span>
            <Switch
              checked={cfg.enabled}
              disabled={saving}
              onCheckedChange={(v) => void save({ enabled: v })}
              aria-label="启用 GLM 代理"
            />
          </div>
        ) : null}
      </header>

      <div className="min-w-0 space-y-10">
        <Section title="网关状态" description="探测 AutoClaw 登录态与上游网关可达性（只读请求，不消耗积分）">
          <CardContent className="p-0">
            <div className="mx-4 flex min-w-0 flex-wrap items-center justify-between gap-3 px-0 py-3.5 sm:mx-5">
              <div className="flex min-w-0 flex-wrap items-center gap-2">
                {status ? (
                  <>
                    <Badge variant={status.tokenReady ? "success" : "destructive"}>
                      {status.tokenReady ? "登录态有效" : "登录态不可用"}
                    </Badge>
                    <Badge variant={status.gatewayOk ? "success" : "destructive"}>
                      {status.gatewayOk ? `网关可达 · ${status.latencyMs}ms` : "网关异常"}
                    </Badge>
                    <Badge variant="secondary">{status.models} 个模型</Badge>
                  </>
                ) : (
                  <span className="text-sm text-muted-foreground">尚未探测</span>
                )}
                {status && !status.gatewayOk && status.error ? (
                  <span className="min-w-0 truncate text-xs text-destructive" title={status.error}>
                    {status.error}
                  </span>
                ) : null}
              </div>
              <Button size="sm" variant="outline" onClick={() => void probe()} disabled={probing}>
                {probing ? <Loader2 className="animate-spin" /> : <RefreshCw />}检测
              </Button>
            </div>
          </CardContent>
        </Section>

        {cfg ? (
          <>
            <Section
              title="账号保护与主动保活"
              description="全账号防封与异常规避保护体系：官方桌面客户端指纹拟真、前瞻式静默保活、单账号并发控制与 403 限流自适应冷却熔断"
            >
              <CardContent className="p-0">
                <Row
                  label="账号主动保护状态"
                  hint="开启后对反代请求拟真官方桌面端指纹、控制并发槽位并在 403 限流时自适应熔断"
                >
                  <div className="flex items-center gap-3">
                    <Badge variant={protection?.config.enabled ? "success" : "secondary"}>
                      <ShieldCheck className="mr-1 size-3.5" />
                      {protection?.config.enabled ? "防护已激活" : "防护已暂停"}
                    </Badge>
                    <Switch
                      checked={protection?.config.enabled ?? true}
                      disabled={protectionBusy}
                      onCheckedChange={(v) => void toggleProtection(v)}
                      aria-label="切换账号主动保护"
                    />
                  </div>
                </Row>

                <Row
                  label="后台主动保活心跳"
                  hint={`每 ${protection?.config.keepaliveIntervalMinutes ?? 120} 分钟轻量巡检：前瞻式刷新即将过期的 JWT 并向官方发送只读心跳（零积分消耗）`}
                >
                  <div className="flex items-center gap-2">
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() => void triggerKeepaliveNow()}
                      disabled={protectionBusy}
                    >
                      {protectionBusy ? <Loader2 className="animate-spin" /> : <Activity className="size-3.5" />}
                      立即巡检保活
                    </Button>
                    <Switch
                      checked={protection?.config.keepaliveEnabled ?? true}
                      disabled={protectionBusy}
                      onCheckedChange={(v) => void toggleKeepalive(v)}
                      aria-label="切换后台保活心跳"
                    />
                  </div>
                </Row>

                <Row
                  label="官方客户端 User-Agent 拟真"
                  hint="向上游网关发送请求时注入 Electron/Chromium 真实桌面端指纹，规避 reqwest 爬虫特征检测"
                >
                  <div className="flex max-w-md items-center gap-1.5 overflow-hidden text-xs text-muted-foreground">
                    <span className="truncate font-mono text-[11px]">
                      {protection?.simulatedUserAgent || "AutoClaw/Electron Desktop UA"}
                    </span>
                    {protection?.simulatedUserAgent ? (
                      <CopyButton text={protection.simulatedUserAgent} label="User-Agent" />
                    ) : null}
                  </div>
                </Row>

                <Row
                  label="单账号并发槽位限制"
                  hint="限制单账号最大并发请求数（默认 2），外部 Agent 突发流量自动排队或平摊至池内空闲账号"
                >
                  <Badge variant="outline" className="font-mono">
                    上限: {protection?.config.maxConcurrencyPerAccount ?? 2} 并发 / 账号
                  </Badge>
                </Row>

                {protection && protection.coolingAccounts.length > 0 ? (
                  <div className="border-t border-destructive/20 bg-destructive/5 p-4 sm:px-5">
                    <div className="flex items-center gap-2 text-destructive">
                      <ShieldAlert className="size-4 shrink-0" />
                      <span className="text-xs font-medium">
                        检测到 {protection.coolingAccounts.length} 个账号触发限流自适应冷却保护（防封熔断中）
                      </span>
                    </div>
                    <div className="mt-2.5 space-y-2">
                      {protection.coolingAccounts.map((acc) => (
                        <div
                          key={acc.accountId}
                          className="flex items-center justify-between rounded-lg border border-destructive/20 bg-background/80 px-3 py-2 text-xs"
                        >
                          <div>
                            <span className="font-medium">{acc.label}</span>
                            <span className="ml-2 text-muted-foreground">原因: {acc.reason}</span>
                            <span className="ml-2 font-mono text-destructive">
                              剩余: {Math.floor(acc.remainingSeconds / 60)}分{acc.remainingSeconds % 60}秒
                            </span>
                          </div>
                          <Button
                            size="sm"
                            variant="ghost"
                            className="h-7 text-xs"
                            onClick={() => void clearCooldown(acc.accountId)}
                            disabled={protectionBusy}
                          >
                            解除冷却
                          </Button>
                        </div>
                      ))}
                    </div>
                  </div>
                ) : null}

                {protection && protection.recentLogs.length > 0 ? (
                  <div className="border-t border-border/50 px-4 py-3 sm:px-5">
                    <div className="flex items-center justify-between">
                      <span className="text-xs font-medium text-muted-foreground">最近保活巡检日志</span>
                      <span className="text-[11px] text-muted-foreground">
                        {protection.lastKeepaliveAt
                          ? `上次巡检: ${new Date(protection.lastKeepaliveAt).toLocaleTimeString()}`
                          : "尚未执行巡检"}
                      </span>
                    </div>
                    <div className="mt-2 max-h-32 space-y-1.5 overflow-y-auto text-[11px] font-mono text-muted-foreground">
                      {protection.recentLogs.slice(0, 5).map((log, i) => (
                        <div key={i} className="flex items-center gap-2 truncate">
                          <span className="shrink-0 text-foreground/70">
                            [{new Date(log.ts).toLocaleTimeString()}]
                          </span>
                          <span className="shrink-0 font-medium text-foreground/90">
                            {log.label}:
                          </span>
                          <span className="truncate">{log.detail}</span>
                        </div>
                      ))}
                    </div>
                  </div>
                ) : null}
              </CardContent>
            </Section>

            <Section
              title="AutoClaw 账号池"
              description="池内账号按区域隔离参与每日自动签到与 GLM 代理轮询（🌐 国际版 → autoglm-api.autoglm.ai，🇨🇳 国内版 → autoglm-acceleration-api.zhipuai.cn，互不跨区）；本机当前登录账号始终自动包含"
            >
              <CardContent className="p-0">
                <div className="flex flex-wrap items-center justify-between gap-2 px-4 py-3 sm:mx-0 sm:px-5">
                  {/* v1.0+: 显示区域账号统计 */}
                  {(() => {
                    const intlCount = accounts.filter((a: any) => a.region === 'international').length;
                    const domCount = accounts.filter((a: any) => a.region === 'domestic').length;
                    return (
                      <span className="text-xs text-muted-foreground">
                        共 {accounts.length} 个账号（国际版 {intlCount} / 国内版 {domCount}）
                      </span>
                    );
                  })()}
                  
                  {!(() => {
                    const intlCount = accounts.filter((a: any) => a.region === 'international').length;
                    const domCount = accounts.filter((a: any) => a.region === 'domestic').length;
                    if (intlCount === 0 && domCount === 0) return null;
                    return (
                      <div className="flex gap-2 text-xs text-muted-foreground">
                        {intlCount > 0 && <span>🌐 国际版：{intlCount}</span>}
                        {domCount > 0 && <span>🇨🇳 国内版：{domCount}</span>}
                      </div>
                    );
                  })()}
                  
                  <div className="flex flex-wrap gap-2">
                    <Button size="sm" onClick={() => void checkinAll()} disabled={checkinAllRunning || accounts.length === 0}>
                      {checkinAllRunning ? <Loader2 className="animate-spin" /> : <CircleCheck />}全部签到
                    </Button>
                    <Button size="sm" variant="outline" onClick={() => void importLocal()} disabled={accountsBusy === "import"}>
                      {accountsBusy === "import" ? <Loader2 className="animate-spin" /> : <UserPlus />}导入本机账号
                    </Button>
                    <Button size="sm" variant="outline" onClick={() => setShowAddForm((v) => !v)}>
                      <UserPlus />添加账号
                    </Button>
                  </div>
                </div>

                {showAddForm ? (
                  <div className="space-y-2 border-t border-border/50 px-4 py-3 sm:px-5">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="text-xs font-medium text-muted-foreground">账号归属：</span>
                      <div className="flex gap-1.5">
                        {(Object.keys(REGION_LABELS) as Array<keyof typeof REGION_LABELS>).map((r) => (
                          <button
                            key={r}
                            type="button"
                            onClick={() => setNewRegion(r)}
                            className={cn(
                              "rounded-full border px-3 py-1 text-xs transition-colors",
                              newRegion === r
                                ? "border-primary bg-primary/10 font-medium text-primary"
                                : "border-border text-muted-foreground hover:bg-muted/60",
                            )}
                          >
                            {REGION_ICONS[r]} {REGION_LABELS[r]}
                          </button>
                        ))}
                      </div>
                      <span className="text-[11px] text-muted-foreground">
                        国际版（OAuth/邮箱账号）与国内版（本机桌面端/手机号账号）完全隔离，禁止跨区调用
                      </span>
                    </div>
                    <div className="grid gap-2 sm:grid-cols-2">
                      <Input placeholder="备注名（可选，如：小号A）" value={newLabel} onChange={(e) => setNewLabel(e.target.value)} />
                      <Input placeholder="deviceId（可选）" value={newDevice} onChange={(e) => setNewDevice(e.target.value)} />
                      <Input placeholder="access token（必填）" value={newAccess} onChange={(e) => setNewAccess(e.target.value)} />
                      <Input placeholder="refresh token（必填）" value={newRefresh} onChange={(e) => setNewRefresh(e.target.value)} />
                    </div>
                    <div className="flex gap-2">
                      <Button size="sm" onClick={() => void submitAdd()} disabled={accountsBusy === "add"}>
                        {accountsBusy === "add" ? <Loader2 className="animate-spin" /> : <Check />}保存并加入{REGION_LABELS[newRegion]}账号池
                      </Button>
                      <Button size="sm" variant="ghost" onClick={() => setShowAddForm(false)}>取消</Button>
                    </div>
                    <p className="text-[11px] text-muted-foreground">
                      token 可从其他设备 AutoClaw 的 auth.json 获取；保存后立即参与对应区域的签到与代理轮询。
                    </p>
                  </div>
                ) : null}

                {msg ? (
                  <p
                    className={cn(
                      "px-4 pt-3 text-xs sm:px-5",
                      msg.type === "ok" ? "text-primary" : "text-destructive",
                    )}
                  >
                    {msg.text}
                  </p>
                ) : null}
                <div className="divide-y divide-border/50 border-t border-border/50">
                  {accounts.length === 0 ? (
                    <p className="px-4 py-3.5 text-sm text-muted-foreground">账号池为空。可「导入本机账号」或手动添加。</p>
                  ) : (
                    accounts.map((account) => {
                      const result = checkinResults[account.id];
                      // v1.0+: 获取区域信息
                      const region = account.region as 'international' | 'domestic' | undefined;
                      const regionIcon = region ? REGION_ICONS[region] : null;
                      const regionLabel = region ? REGION_LABELS[region] : null;
                      const isManualSpecified = account.is_manual_specified as boolean | undefined;
                      
                      return (
                        <div key={account.id} className="flex min-w-0 flex-wrap items-center justify-between gap-2 border-b border-border/50 px-4 py-3 last:border-0 sm:px-5">
                          <div className="min-w-0">
                            <div className="flex flex-wrap items-center gap-2">
                              <span className="text-sm font-medium">{account.label}</span>
                              {/* v1.0+: 显示区域标签 */}
                              {regionIcon && regionLabel ? (
                                <Badge variant="outline" className="text-[10px]" title={regionLabel}>
                                  {regionIcon} {regionLabel}
                                  {isManualSpecified && ' ✏️'}
                                </Badge>
                              ) : null}
                              <Badge variant={account.status === "ok" ? "secondary" : "destructive"}>
                                {account.status === "ok" ? "正常" : "失效"}
                              </Badge>
                              {account.source === "local-live" ? (
                                <Badge className="text-[10px]">本机登录</Badge>
                              ) : account.source === "local-import" || account.source === "local_snapshot" ? (
                                <Badge variant="outline" className="text-[10px]">本机快照</Badge>
                              ) : null}
                              {result ? (
                                result.result === "success" ? (
                                  <Badge variant="success">签到成功</Badge>
                                ) : result.result === "already" ? (
                                  <Badge variant="secondary">已签到</Badge>
                                ) : (
                                  <Badge variant="destructive">失败</Badge>
                                )
                              ) : null}
                            </div>
                            <p className="mt-0.5 truncate text-[11px] text-muted-foreground" title={result?.error || account.lastError || ""}>
                              {account.id}
                              {result?.error ? ` · ${result.error}` : account.lastError ? ` · ${account.lastError}` : ""}
                            </p>
                          </div>
                          <div className="flex shrink-0 items-center gap-1.5">
                            <Button size="sm" variant="outline" onClick={() => void checkinOne(account.id)} disabled={accountsBusy === account.id || checkinAllRunning}>
                              {accountsBusy === account.id ? <Loader2 className="animate-spin" /> : <CircleCheck />}签到
                            </Button>
                            {region ? (
                              <Button
                                size="sm"
                                variant="ghost"
                                className="text-[11px]"
                                title={`手动把该账号归入${REGION_LABELS[region === 'international' ? 'domestic' : 'international']}（防误判修正）`}
                                onClick={() => void changeRegion(account.id, region === 'international' ? 'domestic' : 'international')}
                                disabled={accountsBusy === account.id}
                              >
                                设为{REGION_LABELS[region === 'international' ? 'domestic' : 'international']}
                              </Button>
                            ) : null}
                            <Button size="sm" variant="ghost" onClick={() => void refreshAccount(account.id)} disabled={accountsBusy === account.id} aria-label="刷新该账号登录态">
                              {accountsBusy === account.id ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                            </Button>
                            <Button size="sm" variant="ghost" className="text-destructive hover:text-destructive" onClick={() => void removeAccount(account.id)} disabled={accountsBusy === account.id} aria-label="移除账号">
                              <Trash2 />
                            </Button>
                          </div>
                        </div>
                      );
                    })
                  )}
                </div>
              </CardContent>
            </Section>


            <Section
              title="扣分账号"
              description="模型请求消耗哪个 AutoClaw 账号的额度；不想被代理消耗的账号，选「只用本机」后可继续留在池里做签到"
            >
              <CardContent className="p-0">
                <Row
                  label="选号策略"
                  hint={
                    cfg.accountMode === "round_robin"
                      ? "池内账号轮流使用：额度均摊到每个账号（默认）"
                      : cfg.accountMode === "local_first"
                        ? "优先本机登录账号，失败才顺延到池内其他账号"
                        : cfg.accountMode === "local_only"
                          ? "只用本机登录账号：池内其他账号不会被代理消耗"
                          : "只用下方指定的账号（失败不会自动换号）"
                  }
                >
                  <Select
                    value={cfg.accountMode}
                    disabled={saving}
                    onValueChange={(v) => {
                      if (v === "pinned" && !cfg.pinnedAccountId) {
                        const first = accounts[0]?.id;
                        if (first) {
                          void save({ accountMode: v, pinnedAccountId: first });
                          return;
                        }
                      }
                      void save({ accountMode: v });
                    }}
                    aria-label="选号策略"
                  >
                    <SelectTrigger className="w-44">
                      <SelectValue placeholder="选择策略" />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="round_robin">轮询均摊（默认）</SelectItem>
                      <SelectItem value="local_first">本机优先</SelectItem>
                      <SelectItem value="local_only">只用本机账号</SelectItem>
                      <SelectItem value="pinned">固定账号</SelectItem>
                    </SelectContent>
                  </Select>
                </Row>
                <Row label="固定账号" hint="仅「固定账号」策略生效：请求始终使用该账号的额度">
                  <Select
                    value={cfg.pinnedAccountId || undefined}
                    disabled={saving || cfg.accountMode !== "pinned"}
                    onValueChange={(v) => void save({ pinnedAccountId: v })}
                    aria-label="固定账号"
                  >
                    <SelectTrigger className="w-56">
                      <SelectValue placeholder="选择账号" />
                    </SelectTrigger>
                    <SelectContent>
                      {accounts.map((a) => (
                        <SelectItem key={a.id} value={a.id}>
                          {a.label}
                          {a.source === "local-live" ? "（本机登录）" : ""}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </Row>
                <div className="mx-4 py-3 sm:mx-5">
                  <p className="text-xs leading-5 text-muted-foreground">
                    池里有几个账号，代理就可能花几个账号的额度；策略切换立即生效，只影响之后的请求。
                    「只用本机账号」时若桌面端未登录，代理会直接报错提示，而不会偷偷消耗池内账号。
                  </p>
                </div>
              </CardContent>
            </Section>

            <Section
              title="速度优先"
              description="把慢模型的纯文本请求自动改投到更快的模型（实测：glm-5.3 平均 21s、glm-5.3-flash 平均 9.5s，auto 约 2.4s）"
            >
              <CardContent className="p-0">
                <Row
                  label="启用速度优先"
                  hint={
                    cfg.speedFirst
                      ? `已开启：${cfg.speedFirstSources.join(" / ")} → ${cfg.speedFirstTarget}（多模态请求自动跳过）`
                      : "关闭时严格按客户端请求的模型转发"
                  }
                >
                  <Switch
                    checked={cfg.speedFirst}
                    disabled={saving}
                    onCheckedChange={(v) => void save({ speedFirst: v })}
                    aria-label="启用速度优先"
                  />
                </Row>
                <Row label="改投目标模型" hint="被改投的请求会记入监控并标注原始模型名；含图片/视频的请求保持原模型">
                  <Select
                    value={cfg.speedFirstTarget}
                    onValueChange={(v) => void save({ speedFirstTarget: v })}
                    disabled={saving || !cfg.speedFirst}
                  >
                    <SelectTrigger className="w-40">
                      <SelectValue placeholder="选择目标模型" />
                    </SelectTrigger>
                    <SelectContent>
                      {cfg.models.map((m) => (
                        <SelectItem key={m.id} value={m.id}>
                          {m.alias}（{m.id}）
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </Row>
                <div className="mx-4 py-3 sm:mx-5">
                  <p className="text-xs leading-5 text-muted-foreground">
                    默认 <span className="text-foreground">auto</span>
                    （实测最快、消耗等级低，仅支持纯文本）；需要看图的任务会自动保持
                    <span className="text-foreground"> glm-5.3-flash</span>，也可把目标改成
                    <span className="text-foreground"> auto-fast</span>（支持图片）来统一提速。
                  </p>
                </div>
              </CardContent>
            </Section>

            <Section
              title="模型选型参考"
              description="消耗等级与模态来自 AutoClaw 模型目录；速度取自本机近 7 天的真实调用记录"
            >
              <CardContent className="p-0">
                <div className="min-w-0 overflow-x-auto">
                  <table className="w-full min-w-[760px] text-left text-xs">
                    <thead>
                      <tr className="border-b border-border/60 text-muted-foreground">
                        <th className="px-4 py-2.5 font-medium sm:px-5">模型</th>
                        <th className="px-3 py-2.5 font-medium">消耗</th>
                        <th className="px-3 py-2.5 font-medium">输入模态</th>
                        <th className="px-3 py-2.5 text-right font-medium">上下文</th>
                        <th className="px-3 py-2.5 text-right font-medium">实测首字</th>
                        <th className="px-3 py-2.5 text-right font-medium">实测总耗时</th>
                        <th className="px-3 py-2.5 text-right font-medium">调用次数</th>
                        <th className="px-4 py-2.5 font-medium sm:px-5">建议</th>
                      </tr>
                    </thead>
                    <tbody>
                      {cfg.models.map((m) => {
                        const stat = speedStats[m.id];
                        const inputs = (m.inputs ?? ["text"]).join(" + ");
                        return (
                          <tr key={m.id} className="border-b border-border/40 last:border-0">
                            <td className="px-4 py-2.5 sm:px-5">
                              <span className="flex flex-wrap items-baseline gap-1.5">
                                <span className="font-medium">{m.alias}</span>
                                <code className="text-[11px] text-muted-foreground">{m.id}</code>
                                {m.region ? (
                                  <Badge variant="outline" className="h-5 px-1.5 text-[10px]" title={`该模型只路由到${REGION_LABELS[m.region]}账号池`}>
                                    {REGION_ICONS[m.region]} {REGION_LABELS[m.region]}
                                  </Badge>
                                ) : null}
                                {cfg.speedFirst && cfg.speedFirstSources.includes(m.id) ? (
                                  <Badge variant="warning" className="h-5 px-1.5 text-[10px]">
                                    已改投 {cfg.speedFirstTarget}
                                  </Badge>
                                ) : null}
                              </span>
                              {m.autoModel ? (
                                <p className="mt-0.5 text-[11px] text-muted-foreground">
                                  目录标注后端：{m.autoModel}
                                </p>
                              ) : null}
                            </td>
                            <td className="px-3 py-2.5">
                              <Badge
                                variant={m.consumption === "高" ? "destructive" : "secondary"}
                                className="h-5 px-1.5 text-[10px]"
                              >
                                {m.consumption ?? "—"}
                              </Badge>
                            </td>
                            <td className="px-3 py-2.5 text-muted-foreground">{inputs}</td>
                            <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                              {m.contextWindow ? formatTokens(m.contextWindow) : "—"}
                            </td>
                            <td className="px-3 py-2.5 text-right tabular-nums">
                              {stat && stat.avgTtfbMs > 0 ? formatLatency(stat.avgTtfbMs) : "—"}
                            </td>
                            <td className="px-3 py-2.5 text-right tabular-nums">
                              {stat ? formatLatency(stat.avgLatencyMs) : "—"}
                            </td>
                            <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                              {stat ? formatNumber(stat.calls) : "—"}
                            </td>
                            <td className="px-4 py-2.5 text-muted-foreground sm:px-5">
                              {MODEL_ADVICE[m.id] ?? "—"}
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
                <div className="mx-4 border-t border-border/50 py-3 sm:mx-5">
                  <p className="text-xs leading-5 text-muted-foreground">
                    提速要点：① 日常轮次用 <span className="text-foreground">Auto / Auto-Fast</span>；
                    ② 客户端上下文里工具 schema 常占数万 token（每轮重算），禁用用不到的插件收益最大；
                    ③ 上游不支持关闭思考与提示缓存，参数层面无法再优化。
                  </p>
                </div>
              </CardContent>
            </Section>

            <Section title="接入信息" description="在任意 OpenAI 兼容客户端中填入以下信息">
              <CardContent className="p-0">
                <Row label="接口地址（Base URL）" hint="OpenAI 兼容端点，支持流式与工具调用">
                  <div className="flex items-center gap-1.5">
                    <code className="max-w-[240px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={cfg.endpoint}>
                      {cfg.endpoint}
                    </code>
                    <CopyButton text={cfg.endpoint} label="接口地址" />
                  </div>
                </Row>
                <Row label="访问密钥" hint="请求头 Authorization: Bearer <密钥>">
                  <div className="flex items-center gap-1.5">
                    <code className="max-w-[200px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={cfg.apiKey}>
                      {cfg.apiKey}
                    </code>
                    <CopyButton text={cfg.apiKey} label="访问密钥" />
                    <Button size="sm" variant="outline" onClick={() => void save({ regenerateKey: true })} disabled={saving}>
                      {saving ? <Loader2 className="animate-spin" /> : <RefreshCw />}重新生成
                    </Button>
                  </div>
                </Row>
                <div className="mx-4 border-b-0 py-3.5 sm:mx-5">
                  <p className="text-sm font-medium leading-5">可用模型</p>
                  <p className="mb-2 mt-0.5 text-xs text-muted-foreground">
                    来自 AutoClaw 模型目录（🌐 国际 / 🇨🇳 国内分别路由到对应区域的账号池与网关），客户端按模型名直接调用
                  </p>
                  <div className="flex flex-wrap gap-1.5">
                    {cfg.models.map((m) => (
                      <span
                        key={m.id}
                        className="rounded bg-muted/70 px-2 py-1 text-xs"
                        title={`${m.alias || m.id} · 路由 ${m.routeId}${m.region ? ` · ${REGION_LABELS[m.region]}` : ''}`}
                      >
                        {m.region ? `${REGION_ICONS[m.region]} ` : ''}{m.id}
                      </span>
                    ))}
                  </div>
                </div>
              </CardContent>
            </Section>

            <Section title="试一试" description="直接通过代理发一条测试对话（服务端转发，等价于外部客户端调用）">
              <CardContent className="p-0">
                <div className="mx-4 space-y-2.5 py-3.5 sm:mx-5">
                  <div className="flex flex-wrap items-center gap-2">
                    <Select value={loadModel} onValueChange={setLoadModel}>
                      <SelectTrigger className="w-48">
                        <SelectValue placeholder="选择模型" />
                      </SelectTrigger>
                      <SelectContent>
                        {cfg.models.map((m) => (
                          <SelectItem key={m.id} value={m.id}>
                            {m.alias ? `${m.alias}（${m.id}${m.region ? ` · ${REGION_ICONS[m.region]}` : ''}）` : m.id}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    <Input
                      className="min-w-0 flex-1"
                      value={testPrompt}
                      onChange={(e) => setTestPrompt(e.target.value)}
                      placeholder="输入测试内容…"
                      onKeyDown={(e) => {
                        if (e.key === "Enter") void runTest();
                      }}
                    />
                    <Button onClick={() => void runTest()} disabled={testing || !cfg.enabled}>
                      {testing ? <Loader2 className="animate-spin" /> : <Send />}发送
                    </Button>
                  </div>
                  {testResult ? (
                    <div
                      className={cn(
                        "min-w-0 whitespace-pre-wrap break-words rounded-lg px-3 py-2.5 text-sm",
                        testResult.ok
                          ? "bg-muted/60 text-foreground"
                          : "bg-destructive/10 text-destructive",
                      )}
                    >
                      {testResult.text}
                    </div>
                  ) : null}
                  {!cfg.enabled ? (
                    <p className="text-xs text-destructive">代理已停用，请打开右上角开关。</p>
                  ) : null}
                </div>
              </CardContent>
            </Section>

            <Section title="Hermes 接入示例" description="hermes-agent 的 cli-config.yaml 模型段写法">
              <CardContent className="p-0">
                <div className="mx-4 flex items-start justify-between gap-3 py-3.5 sm:mx-5">
                  <pre className="min-w-0 overflow-x-auto rounded-lg bg-muted/60 p-3 text-xs leading-5">
                    {HERMES_SNIPPET.replace("<访问密钥>", cfg.apiKey)}
                  </pre>
                  <CopyButton text={HERMES_SNIPPET.replace("<访问密钥>", cfg.apiKey)} label="配置示例" />
                </div>
              </CardContent>
            </Section>
          </>
        ) : (
          <p className="text-sm text-muted-foreground">加载配置中…</p>
        )}
      </div>
    </div>
  );
}

async function callTest(model: string, prompt: string): Promise<{ content: string; model: string; latencyMs: number }> {
  const res = await fetch("/api/glm-proxy/test", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ model, prompt }),
  });
  const data = (await res.json()) as { ok?: boolean; error?: string; content?: string; model?: string; latencyMs?: number };
  if (!res.ok || !data.ok) {
    throw new Error(data.error || `HTTP ${res.status}`);
  }
  return { content: data.content || "", model: data.model || model, latencyMs: data.latencyMs || 0 };
}
