import { useCallback, useEffect, useState } from "react";
import {
  Activity,
  Brain,
  CircleAlert,
  Copy,
  KeyRound,
  Loader2,
  Plus,
  RefreshCw,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import type {
  AccountProtectionStatusInfo,
  CoolingAccountInfo,
  LoomyAccountInfo,
  LoomyModel,
  LoomyProxyConfigInfo,
  LoomyProxyStatus,
} from "@/lib/types";

/** 从模型名解析积分倍率（"DeepSeek V4 Flash 0731（x3.0）" → 3.0）与免费标记。 */
function parseModelMeta(model: LoomyModel): { multiplier: number | null; free: boolean } {
  const name = model.name || model.id;
  const free = /免费/.test(name);
  const match = name.match(/[x×]\s*([0-9]+(?:\.[0-9]+)?)/i);
  const multiplier = match ? Number(match[1]) : null;
  return { multiplier: Number.isFinite(multiplier) ? multiplier : null, free };
}

/** 模型简要说明（一句话：定位 + 推理 + 模态 + 规模）。 */
function modelSummary(model: LoomyModel): string {
  if (model.type === "image") {
    const modals = model.capabilities?.input_modalities?.filter((m) => m !== "text") ?? [];
    return `图像生成模型${modals.length ? `，支持${modals.join("/")}输入` : ""}，不计推理档位。`;
  }
  const parts: string[] = [];
  const isFlash = /flash|mini/i.test(model.id);
  parts.push(isFlash ? "轻快型对话模型" : "旗舰级对话模型");
  if (model.capabilities?.reasoning) {
    const max = model.reasoning_efforts?.includes("xhigh") ? "xhigh" : model.reasoning_efforts?.slice(-1)[0];
    parts.push(`支持深度思考（档位 none→${model.reasoning_efforts?.join("/") ?? "—"}，最高 ${max ?? "—"}）`);
  } else {
    parts.push("无深度思考");
  }
  if (model.capabilities?.function_calling) parts.push("支持工具调用");
  const modals = model.capabilities?.input_modalities?.filter((m) => m !== "text") ?? [];
  if (modals.length) parts.push(`多模态（${modals.join("/")}）`);
  if (model.context_length) parts.push(`上下文 ${model.context_length}`);
  if (model.max_output_tokens) parts.push(`单次输出上限 ${model.max_output_tokens}`);
  return `${parts.join("；")}。`;
}

function ModelCatalogRow({ model }: { model: LoomyModel }) {
  const { multiplier, free } = parseModelMeta(model);
  const isImage = model.type === "image";
  const reasoning = model.capabilities?.reasoning === true;
  const maxEffort = model.reasoning_efforts?.includes("xhigh")
    ? "xhigh"
    : model.reasoning_efforts?.slice(-1)[0];
  return (
    <div className="flex min-w-0 items-start gap-3 px-4 py-3 sm:px-5">
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          <span className="truncate text-[13px] font-medium">{model.name || model.id}</span>
          {reasoning && maxEffort ? (
            <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">
              <Brain className="mr-0.5 size-2.5" />
              深度思考·{maxEffort}
            </Badge>
          ) : null}
          {free ? (
            <Badge variant="warning" className="shrink-0 px-1.5 py-0 text-[10px]">限时免费</Badge>
          ) : null}
          {isImage ? (
            <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">图像生成</Badge>
          ) : null}
        </div>
        <p className="mt-1 text-[11px] leading-5 text-muted-foreground">{modelSummary(model)}</p>
      </div>
      <div className="flex shrink-0 flex-col items-end gap-1 text-[11px] tabular-nums text-muted-foreground">
        {multiplier !== null ? <span>倍率 x{multiplier}</span> : null}
        <span className="font-mono">{model.id}</span>
      </div>
    </div>
  );
}

function CreditBadge({ account }: { account: LoomyAccountInfo }) {
  const credit = account.credit;
  if (!credit) return null;
  if (!credit.ok) {
    return <Badge variant="destructive" className="shrink-0 px-1.5 py-0 text-[10px]">{credit.error || "积分查询失败"}</Badge>;
  }
  return (
    <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
      余额 {credit.balance ?? 0} · 每日 {credit.dailyBalance ?? 0}
    </span>
  );
}

function AccountRow({
  account,
  cooling,
  onRemove,
  onCheckin,
  onClearCooling,
  busy,
}: {
  account: LoomyAccountInfo;
  cooling?: CoolingAccountInfo;
  onRemove: (id: string) => void;
  onCheckin: (id: string) => void;
  onClearCooling?: (id: string) => void;
  busy: boolean;
}) {
  const isLocal = account.source === "local-live";
  const invalid = account.status === "invalid";
  return (
    <div className="flex min-w-0 items-center gap-3 px-4 py-3 sm:px-5">
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          <span className="truncate text-[13px] font-medium">{account.label}</span>
          {isLocal ? (
            <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">本机登录</Badge>
          ) : null}
          {invalid ? (
            <Badge variant="destructive" className="shrink-0 px-1.5 py-0 text-[10px]">失效</Badge>
          ) : null}
          {cooling ? (
            <Badge variant="destructive" className="shrink-0 px-1.5 py-0 text-[10px]">
              <ShieldAlert className="mr-1 size-3" />
              熔断冷却 (剩 {Math.max(1, Math.round(cooling.remainingSeconds / 60))} 分钟)
            </Badge>
          ) : null}
          {typeof account.expiresDays === "number" ? (
            <Badge variant={account.expiresDays <= 3 ? "warning" : "outline"} className="shrink-0 px-1.5 py-0 text-[10px]">
              session 剩 {account.expiresDays} 天
            </Badge>
          ) : null}
        </div>
        <div className="mt-0.5 flex min-w-0 flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
          <span className="truncate">{account.id}</span>
          {cooling ? <span className="truncate text-destructive">原因: {cooling.reason}</span> : null}
          {account.lastError ? <span className="truncate text-destructive">{account.lastError}</span> : null}
        </div>
      </div>
      <CreditBadge account={account} />
      <div className="flex shrink-0 items-center gap-1.5">
        {cooling && onClearCooling ? (
          <Button
            size="sm"
            variant="outline"
            className="text-destructive hover:bg-destructive/10"
            disabled={busy}
            onClick={() => onClearCooling(account.id)}
          >
            解除冷却
          </Button>
        ) : null}
        <Button size="sm" variant="outline" disabled={busy} onClick={() => onCheckin(account.id)}>
          签到
        </Button>
        {!isLocal ? (
          <Button size="sm" variant="ghost" disabled={busy} onClick={() => onRemove(account.id)}>
            移除
          </Button>
        ) : null}
      </div>
    </div>
  );
}

export default function LoomyPage() {
  const [accounts, setAccounts] = useState<LoomyAccountInfo[]>([]);
  const [status, setStatus] = useState<{ localLogin: boolean; dataDir?: string | null } | null>(null);
  const [proxyConfig, setProxyConfig] = useState<LoomyProxyConfigInfo | null>(null);
  const [proxyStatus, setProxyStatus] = useState<LoomyProxyStatus | null>(null);
  const [models, setModels] = useState<LoomyModel[]>([]);
  const [modelsError, setModelsError] = useState<string | null>(null);
  const [protection, setProtection] = useState<AccountProtectionStatusInfo | null>(null);
  const [triggeringKeepalive, setTriggeringKeepalive] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [sessionInput, setSessionInput] = useState("");
  const [importing, setImporting] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [accountsRes, statusRes, configRes, proxyRes, protectionRes] = await Promise.all([
        api.getLoomyAccounts(),
        api.getLoomyStatus(),
        api.getLoomyProxyConfig(),
        api.getLoomyProxyStatus(),
        api.getLoomyProtectionStatus().catch(() => null),
      ]);
      setAccounts(accountsRes.accounts);
      setStatus(statusRes);
      setProxyConfig(configRes);
      setProxyStatus(proxyRes);
      if (protectionRes) {
        setProtection(protectionRes);
      }
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, []);

  const reloadProtection = useCallback(async () => {
    try {
      const res = await api.getLoomyProtectionStatus();
      setProtection(res);
    } catch {
      // 忽略静默刷新错误
    }
  }, []);

  const triggerKeepaliveNow = async () => {
    setTriggeringKeepalive(true);
    try {
      await api.triggerLoomyProtectionKeepalive();
      toast.success("已触发 Loomy 主动保活巡检");
      await reloadProtection();
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setTriggeringKeepalive(false);
    }
  };

  const clearCooldown = async (id: string) => {
    try {
      await api.clearLoomyProtectionCooldown(id);
      toast.success("已解除账号熔断冷却限制");
      await reloadProtection();
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  const copyUserAgent = async () => {
    if (!protection?.simulatedUserAgent) return;
    try {
      await navigator.clipboard.writeText(protection.simulatedUserAgent);
      toast.success("已复制拟真客户端 UA");
    } catch {
      toast.error("复制失败，请手动选择复制");
    }
  };

  useEffect(() => {
    void load();
    const interval = setInterval(() => {
      void reloadProtection();
    }, 15000);
    return () => clearInterval(interval);
  }, [load, reloadProtection]);

  // 模型目录：独立加载（失败不阻塞页面），深度思考模型优先。
  useEffect(() => {
    let disposed = false;
    api.getLoomyModelsCatalog()
      .then((res) => {
        if (disposed) return;
        const list = res.data ?? [];
        const sorted = [...list].sort((a, b) => {
          const imageA = a.type === "image" ? 1 : 0;
          const imageB = b.type === "image" ? 1 : 0;
          if (imageA !== imageB) return imageA - imageB; // 对话模型在前
          const effA = a.reasoning_efforts?.includes("xhigh") ? 1 : 0;
          const effB = b.reasoning_efforts?.includes("xhigh") ? 1 : 0;
          if (effA !== effB) return effB - effA; // xhigh 档优先
          const multA = parseModelMeta(a).multiplier ?? Number.MAX_VALUE;
          const multB = parseModelMeta(b).multiplier ?? Number.MAX_VALUE;
          return multA - multB; // 同档按倍率从低到高
        });
        setModels(sorted);
        setModelsError(null);
      })
      .catch((e) => {
        if (!disposed) setModelsError(api.asError(e));
      });
    return () => {
      disposed = true;
    };
  }, []);

  const importLocal = async () => {
    setImporting(true);
    try {
      await api.importLoomyLocal();
      toast.success("已导入本机 Loomy 登录账号");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setImporting(false);
    }
  };

  const addBySession = async () => {
    if (!sessionInput.trim()) return;
    setImporting(true);
    try {
      await api.addLoomyAccount({ session: sessionInput.trim() });
      setSessionInput("");
      toast.success("session 导入成功");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setImporting(false);
    }
  };

  const removeAccount = async (id: string) => {
    try {
      await api.removeLoomyAccount(id);
      toast.success("已移除");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  const checkinOne = async (id: string) => {
    setBusy(true);
    try {
      const res = await api.loomyCheckinOne(id);
      if (res.result === "error") {
        toast.error(res.error || "签到失败");
      } else {
        toast.success("登录态有效（每日积分随登录自动发放）");
      }
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const checkinAll = async () => {
    setBusy(true);
    try {
      const res = await api.loomyCheckinAll();
      const failed = res.accounts.filter((a) => a.result === "error");
      if (failed.length === 0) {
        toast.success(`全部 ${res.accounts.length} 个账号登录态有效`);
      } else {
        toast.warning(`${res.accounts.length - failed.length} 成功，${failed.length} 失败`);
      }
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const toggleProxy = async (enabled: boolean) => {
    try {
      await api.saveLoomyProxyConfig({ enabled });
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  const copyKey = async () => {
    if (!proxyConfig?.apiKey) return;
    try {
      await navigator.clipboard.writeText(proxyConfig.apiKey);
      toast.success("API Key 已复制");
    } catch {
      toast.error("复制失败，请手动选择复制");
    }
  };

  const loading_ = loading && accounts.length === 0;

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-8 flex min-w-0 flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">Loomy</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            讯飞 Loomy 桌面客户端账号池：集中管理登录态、积分余额与每日签到，
            并把 Loomy 订阅内的模型反代为 OpenAI 兼容接口（<code className="rounded bg-muted px-1">/loomy/v1/chat/completions</code>）。
          </p>
        </div>
        <Button className="shrink-0" variant="outline" size="sm" onClick={() => void load()} disabled={loading}>
          {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
          刷新
        </Button>
      </header>

      {status && !status.localLogin && (
        <Alert variant="warning" className="mb-5">
          <CircleAlert />
          <AlertTitle>未检测到本机 Loomy 登录态</AlertTitle>
          <AlertDescription>
            请安装并登录 Loomy 桌面客户端（Windows 数据目录 C:\Users\Public\Loomy\&lt;用户哈希&gt;\userData），
            或在下方粘贴 auth-session.json 中的 session 手动导入。
          </AlertDescription>
        </Alert>
      )}

      {loading_ ? (
        <div className="flex items-center gap-2 py-16 text-sm text-muted-foreground">
          <Loader2 className="animate-spin" />
          正在读取 Loomy 账号池…
        </div>
      ) : (
        <div className="min-w-0 space-y-8">
          {/* 账号池 */}
          <section className="min-w-0 space-y-2.5" aria-label="Loomy 账号池">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
              <h2 className="text-[13px] font-medium leading-5">账号池 · {accounts.length} 个</h2>
              <div className="flex items-center gap-2">
                <Button size="sm" variant="outline" disabled={busy} onClick={() => void checkinAll()}>
                  全部签到
                </Button>
                <Button size="sm" variant="outline" disabled={importing} onClick={() => void importLocal()}>
                  <Plus />
                  导入本机账号
                </Button>
              </div>
            </div>
            <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
              <div className="divide-y divide-border/50">
                {accounts.length === 0 ? (
                  <p className="px-4 py-8 text-center text-sm text-muted-foreground">
                    暂无账号。登录 Loomy 客户端后点「导入本机账号」，或在下方粘贴 session。
                  </p>
                ) : (
                  accounts.map((account) => {
                    const cooling = protection?.coolingAccounts.find(
                      (c) => c.accountId === account.id || c.accountId === `loomy:${account.id}`
                    );
                    return (
                      <AccountRow
                        key={account.id}
                        account={account}
                        cooling={cooling}
                        onRemove={(id) => void removeAccount(id)}
                        onCheckin={(id) => void checkinOne(id)}
                        onClearCooling={(id) => void clearCooldown(id)}
                        busy={busy}
                      />
                    );
                  })
                )}
              </div>
            </Card>
            <div className="flex min-w-0 flex-wrap items-end gap-2 px-1 pt-1">
              <div className="min-w-0 flex-1">
                <Label htmlFor="loomy-session" className="text-xs text-muted-foreground">
                  手动导入 session（另一台机器 Loomy 的 auth-session.json）
                </Label>
                <Input
                  id="loomy-session"
                  className="mt-1.5 font-mono text-xs"
                  placeholder="粘贴 32 位 session token"
                  value={sessionInput}
                  onChange={(e) => setSessionInput(e.target.value)}
                />
              </div>
              <Button size="sm" disabled={importing || !sessionInput.trim()} onClick={() => void addBySession()}>
                导入
              </Button>
            </div>
            <p className="px-1 text-[11px] leading-5 text-muted-foreground">
              Loomy 的每日 5000 积分随客户端登录自动发放（无独立签到接口），「签到」操作实际校验登录态有效性并刷新余额。
              session 由登录时申请、固定 14 天有效期，无自动续期；失效后请重新登录客户端并再次导入。
            </p>
          </section>

          {/* 账号保护与主动保活 */}
          <section className="min-w-0 space-y-2.5" aria-label="Loomy 账号保护与主动保活">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
              <div>
                <h2 className="flex items-center gap-1.5 text-[13px] font-medium leading-5">
                  <ShieldCheck className="size-4 text-emerald-500" />
                  账号保护与主动保活
                </h2>
                <p className="text-[11px] text-muted-foreground">
                  官方客户端拟真 UA · 零积分后台保活巡检 · 单账号并发削峰 · 429/403/额度耗尽自适应熔断冷却 · 本地 session 静默自愈
                </p>
              </div>
              <div className="flex items-center gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  disabled={triggeringKeepalive}
                  onClick={() => void triggerKeepaliveNow()}
                >
                  {triggeringKeepalive ? <Loader2 className="animate-spin" /> : <Activity className="size-3.5" />}
                  立即保活巡检
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => void reloadProtection()}
                  title="刷新防护状态"
                >
                  <RefreshCw className="size-3.5" />
                </Button>
              </div>
            </div>

            <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
              <div className="grid divide-y border-b border-border/50 sm:grid-cols-2 sm:divide-x sm:divide-y-0">
                {/* 客户端 UA 拟真 */}
                <div className="p-4 sm:p-5">
                  <div className="flex items-center justify-between">
                    <span className="text-xs font-medium">官方客户端 UA 拟真</span>
                    <Badge variant="success" className="text-[10px]">已激活</Badge>
                  </div>
                  <p className="mt-1 text-[11px] text-muted-foreground">
                    探测本机安装版本并构造真实 Electron/Chromium 请求指纹，规避服务端特征检测
                  </p>
                  <div className="mt-2.5 flex items-center gap-2">
                    <code className="min-w-0 flex-1 truncate rounded bg-muted px-2 py-1 font-mono text-[11px] text-muted-foreground" title={protection?.simulatedUserAgent || "Loomy/0.9.37 Chrome/144.0.7559.236 Electron/40.8.2"}>
                      {protection?.simulatedUserAgent || "Loomy/0.9.37 Chrome/144.0.7559.236 Electron/40.8.2"}
                    </code>
                    <Button size="sm" variant="outline" className="h-7 px-2 text-xs" onClick={() => void copyUserAgent()}>
                      <Copy className="size-3" />
                      复制
                    </Button>
                  </div>
                </div>

                {/* 并发与熔断参数 */}
                <div className="p-4 sm:p-5">
                  <div className="flex items-center justify-between">
                    <span className="text-xs font-medium">防封控流策略</span>
                    <Badge variant="outline" className="text-[10px]">自适应</Badge>
                  </div>
                  <div className="mt-2 space-y-1.5 text-xs text-muted-foreground">
                    <p className="flex items-center justify-between">
                      <span>单账号并发硬限制：</span>
                      <span className="font-mono font-medium text-foreground">{protection?.config.maxConcurrencyPerAccount ?? 2} 并发（超额平滑排队）</span>
                    </p>
                    <p className="flex items-center justify-between">
                      <span>巡检保活心跳周期：</span>
                      <span className="font-mono font-medium text-foreground">每 {protection?.config.keepaliveIntervalMinutes ?? 60} 分钟（零积分消耗）</span>
                    </p>
                    <p className="flex items-center justify-between">
                      <span>异常熔断触发机制：</span>
                      <span className="text-foreground">遇 429/403/积分耗尽 自动冷却 15~30 分钟</span>
                    </p>
                  </div>
                </div>
              </div>

              {/* 熔断告警提示 */}
              {protection && protection.coolingAccounts.length > 0 ? (
                <div className="border-b border-destructive/20 bg-destructive/5 p-4 sm:px-5">
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
                        <div className="min-w-0 flex-1 truncate">
                          <span className="font-medium">{acc.label}</span>
                          <span className="ml-2 text-muted-foreground">原因: {acc.reason}</span>
                          <span className="ml-2 font-mono text-destructive">
                            剩余: {Math.floor(acc.remainingSeconds / 60)}分{acc.remainingSeconds % 60}秒
                          </span>
                        </div>
                        <Button
                          size="sm"
                          variant="ghost"
                          className="h-7 text-xs text-destructive hover:bg-destructive/10"
                          onClick={() => void clearCooldown(acc.accountId.replace(/^loomy:/, ""))}
                        >
                          解除冷却
                        </Button>
                      </div>
                    ))}
                  </div>
                </div>
              ) : null}

              {/* 最近巡检日志 */}
              {protection && protection.recentLogs.length > 0 ? (
                <div className="px-4 py-3 sm:px-5">
                  <div className="flex items-center justify-between">
                    <span className="text-xs font-medium text-muted-foreground">最近巡检防护日志</span>
                    <span className="text-[11px] text-muted-foreground">
                      {protection.lastKeepaliveAt
                        ? `上次巡检: ${new Date(protection.lastKeepaliveAt).toLocaleTimeString()}`
                        : "尚未执行巡检"}
                    </span>
                  </div>
                  <div className="mt-2 max-h-32 space-y-1.5 overflow-y-auto font-mono text-[11px] text-muted-foreground">
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
            </Card>
          </section>

          {/* 模型目录 */}
          <section className="min-w-0 space-y-2.5" aria-label="Loomy 模型目录">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
              <h2 className="text-[13px] font-medium leading-5">
                可用模型 · {models.length} 个
                <span className="ml-2 text-xs font-normal text-muted-foreground">
                  深度思考模型优先，同档按积分倍率从低到高
                </span>
              </h2>
            </div>
            {modelsError ? (
              <Alert variant="warning" className="mb-2">
                <CircleAlert />
                <AlertTitle>模型目录获取失败</AlertTitle>
                <AlertDescription>{modelsError}</AlertDescription>
              </Alert>
            ) : (
              <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
                <div className="divide-y divide-border/50">
                  {models.length === 0 ? (
                    <p className="px-4 py-8 text-center text-sm text-muted-foreground">
                      正在拉取模型目录…
                    </p>
                  ) : (
                    models.map((model) => <ModelCatalogRow key={model.id} model={model} />)
                  )}
                </div>
              </Card>
            )}
            <p className="px-1 text-[11px] leading-5 text-muted-foreground">
              全部对话模型均支持 5 档深度思考（none / low / medium / high / xhigh），通过请求体
              <code className="mx-1 rounded bg-muted px-1">reasoning_effort</code>字段指定（默认 low）；
              倍率即积分消耗系数（如 x3.0 = 每次调用按基础消耗的 3 倍计费），
              上 / 下文与输出上限均为原始整数。
            </p>
          </section>

          {/* 模型反代 */}
          <section className="min-w-0 space-y-2.5" aria-label="Loomy 模型代理">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
              <h2 className="text-[13px] font-medium leading-5">模型代理</h2>
              {proxyConfig ? (
                <div className="flex items-center gap-2">
                  <span className="text-xs text-muted-foreground">{proxyConfig.enabled ? "已启用" : "已停用"}</span>
                  <Switch checked={proxyConfig.enabled} onCheckedChange={(v) => void toggleProxy(v)} />
                </div>
              ) : null}
            </div>
            <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
              <CardContent className="grid min-w-0 gap-4 p-4 sm:grid-cols-2 sm:p-5">
                <div className="min-w-0">
                  <div className="flex items-center gap-1.5 text-[13px] font-medium">
                    <ShieldCheck className="size-4 text-muted-foreground" />
                    代理状态
                  </div>
                  <div className="mt-2 space-y-1 text-xs text-muted-foreground">
                    <p>
                      账号池规模：<span className="tabular-nums">{proxyStatus?.poolSize ?? 0}</span>
                      {" · "}模型目录：
                      {proxyStatus?.models.ok
                        ? <span className="text-foreground">{proxyStatus.models.count} 个可用</span>
                        : <span className="text-destructive">{proxyStatus?.models.error || "不可达"}</span>}
                    </p>
                    <p>
                      OpenAI 端点：<code className="rounded bg-muted px-1">POST {proxyConfig?.endpoint || "/loomy/v1/chat/completions"}</code>
                      {" · "}
                      <code className="rounded bg-muted px-1">GET {proxyConfig?.modelsEndpoint || "/loomy/v1/models"}</code>
                    </p>
                    <p>
                      Claude 端点：<code className="rounded bg-muted px-1">POST /loomy/v1/messages</code>
                      <span className="ml-1 text-[11px] text-muted-foreground">（Anthropic 协议兼容 Claude Code / Cursor / Windsurf）</span>
                    </p>
                    {proxyStatus?.lastError ? (
                      <p className="text-destructive">最近错误：{proxyStatus.lastError.error}</p>
                    ) : null}
                  </div>
                </div>
                <div className="min-w-0">
                  <div className="flex items-center gap-1.5 text-[13px] font-medium">
                    <KeyRound className="size-4 text-muted-foreground" />
                    接入密钥
                  </div>
                  <div className="mt-2 flex min-w-0 items-center gap-2">
                    <code className="min-w-0 flex-1 truncate rounded bg-muted px-2 py-1.5 font-mono text-xs">
                      {proxyConfig?.apiKey || "—"}
                    </code>
                    <Button size="sm" variant="outline" onClick={() => void copyKey()}>
                      <Copy />
                      复制
                    </Button>
                  </div>
                  <p className="mt-2 text-[11px] leading-5 text-muted-foreground">
                    请求头带 <code className="rounded bg-muted px-1">Authorization: Bearer &lt;密钥&gt;</code> 或
                    <code className="mx-1 rounded bg-muted px-1">x-api-key</code>；密钥仅存本机配置文件。
                  </p>
                </div>
              </CardContent>
            </Card>
            <p className="px-1 text-[11px] leading-5 text-muted-foreground">
              上游为讯飞 Loomy 积分网关 loomyad.xunfei.cn（OpenAI 兼容）。模型调用按 Loomy 积分倍率计费
              （如 GLM-5.3-Flash ×0.8、Spark X 限时免费），真实扣分记录在「模型用量」页。
            </p>
          </section>
        </div>
      )}
    </div>
  );
}
