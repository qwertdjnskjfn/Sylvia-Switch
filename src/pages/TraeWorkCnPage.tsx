/**
 * TRAE Work CN 网关（独立网关 · 2026-09-22 从原「TRAE / WorkBuddy 模型」合并页拆出）。
 *
 * 职责边界：只管 TRAE Work CN —— 模型目录（本地客户端缓存 + 积分倍率）、账号池、
 * 账号保护与续期、倍率驱动的选型参考、接入信息。**不涉及 WorkBuddy / OpenCode**。
 *
 * 六项能力落点：
 *   网关状态     → 凭证可用性 + 目录规模 + 对话转发开关（当前 V1 未开放）
 *   账号保护/保活 → 账号池 + 续期 + 到期/失效标记（refresh 与设备密钥绑定）
 *   扣分账号     → 上游限流标记（rate-limit）与失效态在账号行内呈现
 *   速度优先策略 → 「速度优先推荐」小节：按 consumption_rate 升序（倍率越低越快/越省）
 *   模型选型参考 → 场景矩阵 + 推理/多模态标记
 *   接入信息     → BASE URL / 密钥 / 可用模型 / 调用示例
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import {
  BarChart3,
  Brain,
  CircleAlert,
  Gauge,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
  UserPlus,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import * as api from "@/lib/api";
import type { TraeProxyModel } from "@/lib/api";
import type { TraeAccountInfo } from "@/lib/types";
import { cn } from "@/lib/utils";

function InfoRow({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
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

function RateTag({ rate }: { rate?: number | null }) {
  if (typeof rate !== "number") return null;
  return (
    <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px] tabular-nums">
      倍率 x{rate}
    </Badge>
  );
}

function TraeModelRow({ model }: { model: TraeProxyModel }) {
  return (
    <div className="flex min-w-0 items-start gap-3 px-4 py-2.5 sm:px-5">
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          <span className="truncate font-mono text-[12px] font-medium">{model.id}</span>
          {model.display_name && model.display_name !== model.id ? (
            <span className="truncate text-[11px] text-muted-foreground">{model.display_name}</span>
          ) : null}
          {model.reasoning ? (
            <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">
              <Brain className="mr-0.5 size-2.5" />
              推理
            </Badge>
          ) : null}
          {model.multimodal ? (
            <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">多模态</Badge>
          ) : null}
        </div>
        <p className="mt-0.5 text-[11px] text-muted-foreground">
          场景 {model.scene || "—"}
          {typeof model.context_max === "number" ? ` · 上下文 ${model.context_max}` : ""}
        </p>
      </div>
      <RateTag rate={model.rate} />
    </div>
  );
}

export default function TraeWorkCnPage() {
  const [traeStatus, setTraeStatus] = useState<api.TraeProxyStatus | null>(null);
  const [traeModels, setTraeModels] = useState<TraeProxyModel[]>([]);
  const [traeError, setTraeError] = useState<string | null>(null);
  const [traeCfg, setTraeCfg] = useState<api.TraeProxyConfigInfo | null>(null);
  const [traeAccounts, setTraeAccounts] = useState<TraeAccountInfo[]>([]);
  const [traeBusy, setTraeBusy] = useState<string | null>(null);
  const [traeMsg, setTraeMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [newLabel, setNewLabel] = useState("");
  const [newAccess, setNewAccess] = useState("");
  const [newRefresh, setNewRefresh] = useState("");
  const [newDeviceId, setNewDeviceId] = useState("");
  const [loading, setLoading] = useState(true);

  const loadTraeAccounts = useCallback(async () => {
    try {
      const res = await api.getTraeAccounts();
      setTraeAccounts(res.accounts ?? []);
    } catch (e) {
      setTraeMsg({ type: "err", text: api.asError(e) });
    }
  }, []);

  const load = useCallback(async () => {
    setLoading(true);
    const [statusRes, modelsRes, cfgRes] = await Promise.allSettled([
      api.getTraeProxyStatus(),
      api.getTraeProxyModels(),
      api.getTraeProxyConfig(),
    ]);
    if (statusRes.status === "fulfilled") setTraeStatus(statusRes.value);
    if (modelsRes.status === "fulfilled") {
      setTraeModels(modelsRes.value.models ?? []);
      setTraeError(null);
    } else {
      setTraeError(api.asError(modelsRes.reason));
      setTraeModels([]);
    }
    if (cfgRes.status === "fulfilled") setTraeCfg(cfgRes.value);
    await loadTraeAccounts();
    setLoading(false);
  }, [loadTraeAccounts]);

  useEffect(() => {
    void load();
    const timer = window.setTimeout(() => void load(), 20000);
    return () => window.clearTimeout(timer);
  }, [load]);

  const importTraeLocal = useCallback(async () => {
    setTraeBusy("import");
    try {
      await api.importTraeLocal();
      await loadTraeAccounts();
      setTraeMsg({ type: "ok", text: "已导入客户端当前登录的 TRAE 账号快照" });
    } catch (e) {
      setTraeMsg({ type: "err", text: api.asError(e) });
    }
    setTraeBusy(null);
  }, [loadTraeAccounts]);

  const submitAddTraeAccount = useCallback(async () => {
    setTraeBusy("add");
    try {
      await api.addTraeAccount({
        label: newLabel.trim() || undefined,
        accessToken: newAccess.trim(),
        refreshToken: newRefresh.trim() || undefined,
        deviceId: newDeviceId.trim() || undefined,
      });
      setAddOpen(false);
      setNewLabel("");
      setNewAccess("");
      setNewRefresh("");
      setNewDeviceId("");
      await loadTraeAccounts();
      setTraeMsg({ type: "ok", text: "账号已加入 TRAE 池" });
    } catch (e) {
      setTraeMsg({ type: "err", text: api.asError(e) });
    }
    setTraeBusy(null);
  }, [loadTraeAccounts, newAccess, newDeviceId, newLabel, newRefresh]);

  const refreshTraeAccount = useCallback(async (id: string) => {
    setTraeBusy(id);
    try {
      await api.refreshTraeAccount(id);
      await loadTraeAccounts();
      setTraeMsg({ type: "ok", text: "已尝试续期该账号登录态" });
    } catch (e) {
      setTraeMsg({ type: "err", text: api.asError(e) });
    }
    setTraeBusy(null);
  }, [loadTraeAccounts]);

  const removeTraeAccount = useCallback(async (id: string) => {
    setTraeBusy(id);
    try {
      await api.removeTraeAccount(id);
      await loadTraeAccounts();
    } catch (e) {
      setTraeMsg({ type: "err", text: api.asError(e) });
    }
    setTraeBusy(null);
  }, [loadTraeAccounts]);

  const traeCount = traeModels.length;

  /** 速度优先：倍率升序（倍率越低 = 越快/越省，TRAE 按 consumption_rate 计费）。 */
  const fastest = useMemo(() => {
    return traeModels
      .filter((m) => typeof m.rate === "number")
      .slice()
      .sort((a, b) => (a.rate ?? 0) - (b.rate ?? 0))
      .slice(0, 6);
  }, [traeModels]);

  /** 选型参考：按场景聚合，便于「哪类任务用哪个模型」。 */
  const sceneMatrix = useMemo(() => {
    const map = new Map<string, number>();
    for (const m of traeModels) {
      const key = m.scene || "—";
      map.set(key, (map.get(key) ?? 0) + 1);
    }
    return [...map.entries()].sort((a, b) => b[1] - a[1]);
  }, [traeModels]);

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-8 flex min-w-0 flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">TRAE Work CN 网关</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            独立网关：模型目录（本地客户端缓存 + 积分倍率）、账号池与续期、倍率驱动的速度优先与选型参考。
            与 WorkBuddy、OpenCode 三个网关**互不共享账号池与密钥**。
          </p>
        </div>
        <div className="flex shrink-0 flex-wrap items-center gap-2">
          <Button className="shrink-0" variant="outline" size="sm" asChild>
            <Link to="/stats?tab=model">
              <BarChart3 />
              模型用量
            </Link>
          </Button>
          <Button className="shrink-0" variant="outline" size="sm" onClick={() => void load()} disabled={loading}>
            {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
            刷新
          </Button>
        </div>
      </header>

      {/* ① 网关状态 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="TRAE 网关状态">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">网关状态 · {traeCount} 个模型</h2>
          <span className="text-xs text-muted-foreground">
            凭证：
            {traeStatus?.credential.ok ? (
              <span className="text-foreground">已登录（{traeStatus.credential.user}）</span>
            ) : (
              <span className="text-destructive">{traeStatus?.credential.error || "未登录"}</span>
            )}
          </span>
        </div>
        <Alert variant="default">
          <CircleAlert />
          <AlertTitle>对话转发暂未开放（V1：目录 + 权益）</AlertTitle>
          <AlertDescription>
            TRAE 的对话接口要求先建立客户端会话（CreateChatSession），单发请求会返回
            「all models failed」。当前提供模型目录、倍率与账号权益；会话栈打通后开放转发。
          </AlertDescription>
        </Alert>
      </section>

      {/* ② 速度优先策略 */}
      {fastest.length > 0 ? (
        <section className="mb-10 min-w-0 space-y-2.5" aria-label="TRAE 速度优先">
          <div className="px-1">
            <h2 className="text-[13px] font-medium leading-5">
              <Gauge className="mr-1 inline size-3.5" />
              速度优先推荐（按倍率升序）
            </h2>
            <p className="mt-0.5 text-xs text-muted-foreground">
              TRAE 按 consumption_rate 倍率计费与调度，倍率越低越快越省；下列为当前最低的 6 个。
            </p>
          </div>
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <div className="divide-y divide-border/50">
              {fastest.map((m) => (
                <TraeModelRow key={`fast-${m.id}`} model={m} />
              ))}
            </div>
          </Card>
        </section>
      ) : null}

      {/* ③ 模型选型参考 */}
      {sceneMatrix.length > 0 ? (
        <section className="mb-10 min-w-0 space-y-2.5" aria-label="TRAE 模型选型参考">
          <div className="px-1">
            <h2 className="text-[13px] font-medium leading-5">模型选型参考（按场景）</h2>
            <p className="mt-0.5 text-xs text-muted-foreground">
              TRAE 同一模型在不同场景（builder / chat / refactor / solo_agent …）下可被分别调度；
              下列为各场景的模型条目数，用于判断某场景可选的模型面。
            </p>
          </div>
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <CardContent className="flex min-w-0 flex-wrap gap-2 p-4">
              {sceneMatrix.map(([scene, n]) => (
                <Badge key={scene} variant="secondary" className="shrink-0">
                  {scene} · {n}
                </Badge>
              ))}
            </CardContent>
          </Card>
        </section>
      ) : null}

      {/* ④ 模型目录 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="TRAE Work CN 模型目录">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">模型目录 · {traeCount} 个（去重后）</h2>
          <span className="text-xs text-muted-foreground">来源：TRAE 客户端本地缓存（服务端下发）</span>
        </div>
        {traeError ? (
          <Alert variant="warning">
            <CircleAlert />
            <AlertTitle>模型目录读取失败</AlertTitle>
            <AlertDescription>{traeError}</AlertDescription>
          </Alert>
        ) : (
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <div className="divide-y divide-border/50">
              {traeModels.length === 0 ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground">
                  暂无模型数据（打开 TRAE 客户端一次后会同步模型目录缓存）。
                </p>
              ) : (
                traeModels.map((model) => <TraeModelRow key={model.id} model={model} />)
              )}
            </div>
          </Card>
        )}
      </section>

      {/* ⑤ 账号保护与主动保活 + 扣分账号 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="TRAE Work CN 账号池">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            TRAE Work CN 账号池 · {traeAccounts.length} 个账号
          </h2>
          <span className="text-xs text-muted-foreground">
            {traeStatus?.credential.ok ? "本机客户端已登录，可一键快照" : "本机客户端未登录，可手动添加账号"}
          </span>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
              <span className="text-xs text-muted-foreground">
                池内账号用于签到与登录态自动续期；快照的 refresh 令牌与设备密钥绑定，
                客户端切换登录后旧快照无法续期（到期日约导入后 2 周）。上游限流会被记为扣分态。
              </span>
              <div className="flex shrink-0 flex-wrap gap-2">
                <Button size="sm" variant="outline" onClick={() => void importTraeLocal()} disabled={traeBusy === "import"}>
                  {traeBusy === "import" ? <Loader2 className="animate-spin" /> : <UserPlus />}
                  导入客户端当前账号
                </Button>
                <Button size="sm" variant="outline" onClick={() => setAddOpen(true)} disabled={traeBusy !== null}>
                  <Plus />
                  手动添加
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void loadTraeAccounts()}
                  disabled={traeBusy !== null || loading}
                >
                  {traeBusy !== null ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                  刷新列表
                </Button>
              </div>
            </div>

            {traeMsg ? (
              <p
                className={cn(
                  "px-4 pb-2 text-xs sm:px-5",
                  traeMsg.type === "ok" ? "text-primary" : "text-destructive",
                )}
              >
                {traeMsg.text}
              </p>
            ) : null}

            <div className="divide-y divide-border/50 border-t border-border/50">
              {traeAccounts.length === 0 ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground">
                  账号池为空。登录 TRAE Work CN 客户端后点「导入客户端当前账号」，或点「手动添加」粘贴已抓包导出的凭证。
                </p>
              ) : (
                traeAccounts.map((account) => {
                  const busy = traeBusy === account.id;
                  return (
                    <div
                      key={account.id}
                      className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 last:border-0 sm:px-5"
                    >
                      <div className="min-w-0">
                        <div className="flex min-w-0 flex-wrap items-center gap-2">
                          <span className="truncate text-sm font-medium">{account.label}</span>
                          {account.expired ? (
                            <Badge variant="destructive" className="shrink-0">已到期</Badge>
                          ) : account.status === "ok" ? (
                            <Badge variant="secondary" className="shrink-0">正常</Badge>
                          ) : (
                            <Badge variant="destructive" className="shrink-0">失效</Badge>
                          )}
                          {account.localLogin ? (
                            <Badge className="shrink-0 text-[10px]">客户端登录中</Badge>
                          ) : null}
                        </div>
                        <p
                          className="mt-0.5 truncate text-[11px] text-muted-foreground"
                          title={account.error || ""}
                        >
                          {account.expiredAt ? `登录态至 ${String(account.expiredAt).slice(0, 10)}` : "—"}
                          {account.error ? ` · ${account.error}` : ""}
                        </p>
                      </div>
                      <div className="flex shrink-0 items-center gap-1.5">
                        <Button
                          size="sm"
                          variant="ghost"
                          className="text-[11px]"
                          onClick={() => void refreshTraeAccount(account.id)}
                          disabled={busy}
                          aria-label="刷新该账号登录态"
                        >
                          {busy ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                          续期
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          className="text-destructive hover:text-destructive"
                          onClick={() => void removeTraeAccount(account.id)}
                          disabled={busy}
                          aria-label="移除账号"
                        >
                          <Trash2 />
                        </Button>
                      </div>
                    </div>
                  );
                })
              )}
            </div>
          </CardContent>
        </Card>
      </section>

      {/* 手动添加 TRAE 账号 */}
      <Dialog open={addOpen} onOpenChange={setAddOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>手动添加 TRAE 账号</DialogTitle>
            <DialogDescription>
              粘贴从 TRAE 客户端 / 抓包导出的凭证快照（access token 必填，refresh token 与 deviceId 决定到期后能否自动续期）。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2">
            <Input className="h-8 text-xs" value={newLabel} onChange={(e) => setNewLabel(e.target.value)} placeholder="备注名（选填，如「小号」）" aria-label="备注名" />
            <Input className="h-8 text-xs" value={newAccess} onChange={(e) => setNewAccess(e.target.value)} placeholder="access token（必填）" aria-label="access token" />
            <Input className="h-8 text-xs" value={newRefresh} onChange={(e) => setNewRefresh(e.target.value)} placeholder="refresh token（选填，用于 401 自动续期）" aria-label="refresh token" />
            <Input className="h-8 text-xs" value={newDeviceId} onChange={(e) => setNewDeviceId(e.target.value)} placeholder="deviceId（选填，缺省用本机设备标识）" aria-label="deviceId" />
          </div>
          {traeMsg?.type === "err" ? <p className="text-xs text-destructive">{traeMsg.text}</p> : null}
          <div className="flex justify-end gap-2">
            <Button size="sm" variant="outline" onClick={() => setAddOpen(false)}>取消</Button>
            <Button size="sm" onClick={() => void submitAddTraeAccount()} disabled={traeBusy === "add"}>
              {traeBusy === "add" ? <Loader2 className="animate-spin" /> : null}
              添加
            </Button>
          </div>
        </DialogContent>
      </Dialog>

      {/* ⑥ 接入信息 */}
      <section className="min-w-0 space-y-2.5" aria-label="TRAE 接入信息">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">接入信息 · TRAE Work CN</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">当前开放模型目录查询；对话转发待会话栈打通</p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            <InfoRow label="接口地址（Base URL）" hint="OpenAI 兼容目录端点 GET /trae/v1/models">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[240px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={traeCfg?.endpoint}>
                  {traeCfg?.endpoint ?? "加载中…"}
                </code>
                {traeCfg ? <CopyButton text={traeCfg.endpoint} label="接口地址" /> : null}
              </div>
            </InfoRow>
            <InfoRow
              label="访问密钥"
              hint={
                traeCfg?.keySource === "trae"
                  ? "TRAE 自有密钥（与其它网关密钥相互独立）"
                  : "迁移窗口内仍兼容旧密钥；窗口结束后切换为 TRAE 自有密钥"
              }
            >
              <div className="flex items-center gap-1.5">
                <code className="max-w-[200px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={traeCfg?.apiKey}>
                  {traeCfg?.apiKey ?? "加载中…"}
                </code>
                {traeCfg ? <CopyButton text={traeCfg.apiKey} label="访问密钥" /> : null}
                {traeCfg?.keySource === "trae" ? (
                  <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">TRAE 独立密钥</Badge>
                ) : null}
              </div>
            </InfoRow>
            <InfoRow label="对话转发" hint="上游要求先建立客户端会话（CreateChatSession），单发请求不可用">
              <Badge variant="outline" className="shrink-0">暂未开放</Badge>
            </InfoRow>
            <InfoRow label="模型目录" hint="来自 TRAE 客户端本地缓存；打开过一次客户端后即可查">
              <span className="text-xs text-muted-foreground">
                共 {traeCfg?.modelCount?.count ?? traeCount} 个（免鉴权管理端点 /api/trae-proxy/models 可查倍率）
              </span>
            </InfoRow>
          </CardContent>
        </Card>
      </section>
    </div>
  );
}
