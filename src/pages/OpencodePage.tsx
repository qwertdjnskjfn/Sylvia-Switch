/**
 * OpenCode 网关（新建 · 2026-09-22 第 3 项要求）。
 *
 * 职责边界：只管 OpenCode —— 本机 OpenCode server 的 provider / 模型清单，
 * **只暴露免费模型**（免费判定 = 元数据自动筛 `free`/`cost=0` + 手动白名单覆盖），
 * 以及该 server 进程的健康与保活。
 *
 * 与 WorkBuddy 网关的关系（重要）：
 *   - 原先 WorkBuddy 代理**借道**本机 OpenCode server 复用其已鉴权 provider；
 *   - 拆分后本网关与 WorkBuddy 网关**同级、互不共享密钥**；
 *   - WorkBuddy 改为「直连优先 + OpenCode 兜底」，即 OpenCode server 从
 *     WorkBuddy 的传输层退居为**兜底链路**，其清单归本网关呈现与管理。
 *
 * ⚠️ 本页当前为「清单 + 状态」阶段：
 *   - provider / 模型清单复用已有 `get_workbuddyProxyStatus()`（它读的就是 OpenCode server 的 provider 表）；
 *   - 独立的 `/opencode/v1/*` 对话端点与免费白名单配置属增量 3（后端模块待建），
 *     故本页在「接入信息」里明确标注「待建」，不对外宣称可用。
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { BarChart3, CircleAlert, Gauge, Loader2, RefreshCw, Server, Terminal } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import type { WorkbuddyProxyStatus } from "@/lib/api";
import { TASK_META, TASK_ORDER, classifyTask, groupByChannel, isFreeModelId, stripChannel, type TaskKind } from "@/lib/model-taxonomy";
import { cn } from "@/lib/utils";

/** 从 api 层推导类型，避免重复声明。 */
type OcStatus = Awaited<ReturnType<typeof api.getOpencodeProxyStatus>>;
type OcCfg = Awaited<ReturnType<typeof api.getOpencodeProxyConfig>>;

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

export default function OpencodePage() {
  const [status, setStatus] = useState<WorkbuddyProxyStatus | null>(null);
  const [providers, setProviders] = useState<WorkbuddyProxyStatus["providers"] | null>(null);
  const [ocStatus, setOcStatus] = useState<OcStatus | null>(null);
  const [ocCfg, setOcCfg] = useState<OcCfg | null>(null);
  const [savingKey, setSavingKey] = useState(false);
  const [savingEnabled, setSavingEnabled] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    const [statusRes, modelsRes, ocStatusRes, ocCfgRes] = await Promise.allSettled([
      api.getWorkbuddyProxyStatus(),
      api.getWorkbuddyProxyModels(),
      api.getOpencodeProxyStatus(),
      api.getOpencodeProxyConfig(),
    ]);
    if (statusRes.status === "fulfilled") setStatus(statusRes.value);
    if (modelsRes.status === "fulfilled") {
      setProviders(modelsRes.value.providers ?? []);
      setError(null);
    } else {
      setError(api.asError(modelsRes.reason));
      setProviders([]);
    }
    if (ocStatusRes.status === "fulfilled") setOcStatus(ocStatusRes.value);
    if (ocCfgRes.status === "fulfilled") setOcCfg(ocCfgRes.value);
    setLoading(false);
  }, []);

  useEffect(() => {
    void load();
    const timer = window.setTimeout(() => void load(), 20000);
    return () => window.clearTimeout(timer);
  }, [load]);

  const regenerateKey = useCallback(async () => {
    setSavingKey(true);
    try {
      const next = await api.saveOpencodeProxyConfig({ regenerateKey: true });
      setOcCfg(next);
      setMsg({ type: "ok", text: "已重新生成 OpenCode 网关密钥（旧密钥立即失效）" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    }
    setSavingKey(false);
  }, []);

  const toggleEnabled = useCallback(async (enabled: boolean) => {
    setSavingEnabled(true);
    try {
      const next = await api.saveOpencodeProxyConfig({ enabled });
      setOcCfg(next);
      setMsg({ type: "ok", text: enabled ? "已启用 OpenCode 网关" : "已停用 OpenCode 网关" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    }
    setSavingEnabled(false);
  }, []);

  const all = useMemo(
    () => (providers ?? []).flatMap((p) => (p.models ?? []).map((m) => `${p.id}/${m.id}`)),
    [providers],
  );
  const freeModels = useMemo(() => all.filter(isFreeModelId), [all]);
  const paidCount = all.length - freeModels.length;

  /** 按用途聚合计数（给顶部的分类导航用）。 */
  const taskCounts = useMemo(() => {
    const c = new Map<TaskKind, number>();
    for (const id of freeModels) {
      const k = classifyTask(id);
      c.set(k, (c.get(k) ?? 0) + 1);
    }
    return TASK_ORDER.filter((k) => (c.get(k) ?? 0) > 0).map((k) => ({ kind: k, n: c.get(k) ?? 0 }));
  }, [freeModels]);

  /** 按渠道分组（共享口径：自有 opencode 优先，其余按免费数降序；组内按场景序）。 */
  const freeByChannel = useMemo(() => groupByChannel(freeModels), [freeModels]);

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-8 flex min-w-0 flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">OpenCode 网关</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            独立网关：本机 OpenCode server 的 provider / 模型清单，<strong>只暴露免费模型</strong>
            （元数据自动筛 + 白名单覆盖）。与 WorkBuddy 网关同级、互不共享账号池与密钥。
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

      {/* ① 网关状态 + 保活 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="OpenCode 网关状态">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            <Server className="mr-1 inline size-3.5" />
            网关状态 · {ocStatus?.channelCount ?? providers?.length ?? 0} 个渠道 / 对外暴露{" "}
            {ocStatus?.exposedModelCount ?? freeModels.length} 个免费模型
          </h2>
          <div className="flex shrink-0 flex-wrap items-center gap-3">
            <span className="text-xs text-muted-foreground">
              OpenCode server：
              {(ocStatus?.server ?? status?.server)?.ok ? (
                <span className="text-foreground">
                  已连接（端口 {(ocStatus?.server ?? status?.server)?.port}）
                </span>
              ) : (
                <span className="text-destructive">
                  {(ocStatus?.server ?? status?.server)?.error || "未发现"}
                </span>
              )}
            </span>
            {ocCfg ? (
              <div className="flex items-center gap-2">
                <span className="text-xs text-muted-foreground">{ocCfg.enabled ? "已启用" : "已停用"}</span>
                <Switch
                  checked={ocCfg.enabled}
                  disabled={savingEnabled}
                  onCheckedChange={(v) => void toggleEnabled(v)}
                  aria-label="启用 OpenCode 网关"
                />
              </div>
            ) : null}
          </div>
        </div>
        {msg ? (
          <p
            className={cn(
              "px-1 text-xs",
              msg.type === "ok" ? "text-primary" : "text-destructive",
            )}
          >
            {msg.text}
          </p>
        ) : null}
        <Alert variant="default">
          <CircleAlert />
          <AlertTitle>端点已上线：{ocCfg?.endpoint ?? "http://127.0.0.1:57890/opencode/v1"}</AlertTitle>
          <AlertDescription>
            本网关**只放行免费模型**：请求非免费模型会被拒绝（HTTP 400）并提示可用清单；
            未指定 `model` 时自动选第一个免费模型。密钥与 WorkBuddy / TRAE 网关**互不共用**。
          </AlertDescription>
        </Alert>
        <p className="px-1 text-[11px] leading-5 text-muted-foreground">
          账号保护与主动保活：保活对象是**本机 OpenCode server 进程**（独立实例，端口自动发现；
          未运行时由看门狗经 <code className="rounded bg-muted px-1">E:\opencode\start-serve-4097.bat</code> 拉起）。
          扣分/风控态由连续失败计数与负缓存驱动，健康检查走 <code className="rounded bg-muted px-1">GET /global/health</code>。
        </p>
      </section>

      {/* ② 免费模型（仅保留免费）· 已按「渠道 → 任务场景」分类排序 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="OpenCode 免费模型">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">
            <Gauge className="mr-1 inline size-3.5" />
            免费模型 · {freeModels.length} 个 / {freeByChannel.length} 个渠道
          </h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            判定口径：id 命中 <code className="rounded bg-muted px-1">free / :0</code> 自动筛入；
            另有 {paidCount} 个非免费模型不在本网关注册范围内（保留在 WorkBuddy 网关的目录里）。
            <br />
            排列口径：<strong>先按渠道分组</strong>（自有 opencode 渠道优先，其余按免费数降序），
            <strong>组内按任务场景排序</strong>（快速 → 代码 → 视觉 → 推理 → 通用 → 安全），同类按名称。
            鼠标悬停标签可看该场景的适用任务。
          </p>
        </div>

        {/* 用途分类导航 */}
        {freeModels.length > 0 ? (
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <CardContent className="flex min-w-0 flex-wrap items-center gap-1.5 p-3">
              <span className="mr-1 text-[11px] text-muted-foreground">按任务场景：</span>
              {taskCounts.map(({ kind, n }) => (
                <Badge key={kind} variant="secondary" className="shrink-0" title={TASK_META[kind].hint}>
                  {TASK_META[kind].label} · {n}
                </Badge>
              ))}
            </CardContent>
          </Card>
        ) : null}

        {error ? (
          <Alert variant="warning">
            <CircleAlert />
            <AlertTitle>清单读取失败</AlertTitle>
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        ) : freeModels.length === 0 ? (
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <CardContent className="p-4">
              <p className="py-4 text-center text-sm text-muted-foreground">
                未发现免费模型（OpenCode server 未运行或该实例未配置免费 provider）。
              </p>
            </CardContent>
          </Card>
        ) : (
          <div className="grid min-w-0 gap-3 sm:grid-cols-2">
            {freeByChannel.map(({ channel, ids }) => (
              <Card key={channel} className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
                <CardContent className="p-0">
                  <div className="flex items-center justify-between border-b border-border/50 px-4 py-2.5">
                    <span className="flex min-w-0 items-center gap-1.5">
                      <span className="truncate text-[13px] font-medium">{channel}</span>
                      {channel === "opencode" ? (
                        <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">自有 · 直连本机</Badge>
                      ) : null}
                    </span>
                    <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">
                      免费 {ids.length}
                    </Badge>
                  </div>
                  <div className="divide-y divide-border/50">
                    {ids.map((id) => {
                      const kind = classifyTask(id);
                      return (
                        <div key={id} className="flex min-w-0 items-center gap-2 px-4 py-1.5 text-[12px]">
                          <Badge
                            variant={kind === "safety" ? "destructive" : "outline"}
                            className="shrink-0 px-1.5 py-0 text-[10px]"
                            title={TASK_META[kind].hint}
                          >
                            {TASK_META[kind].label}
                          </Badge>
                          <span className="min-w-0 flex-1 truncate font-mono" title={id}>
                            {stripChannel(id)}
                          </span>
                        </div>
                      );
                    })}
                  </div>
                </CardContent>
              </Card>
            ))}
          </div>
        )}
      </section>

      {/* ③ 选型参考：provider × 模型（仅展示，标注免费与否） */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="OpenCode 模型选型参考">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            <Terminal className="mr-1 inline size-3.5" />
            provider × 模型（选型参考）
          </h2>
          <span className="text-xs text-muted-foreground">带「免费」标的为其可承担的任务优先候选</span>
        </div>
        <div className="grid min-w-0 gap-3 sm:grid-cols-2">
          {(providers ?? []).map((provider) => (
            <Card key={provider.id} className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
              <CardContent className="p-0">
                <div className="flex items-center justify-between border-b border-border/50 px-4 py-2.5">
                  <span className="truncate text-[13px] font-medium">{provider.id}</span>
                  <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">
                    {(provider.models ?? []).filter((m) => isFreeModelId(`${provider.id}/${m.id}`)).length} 免费
                    / {provider.models?.length ?? 0}
                  </Badge>
                </div>
                <div className="max-h-52 overflow-y-auto">
                  {(provider.models ?? []).map((model) => {
                    const full = `${provider.id}/${model.id}`;
                    const free = isFreeModelId(full);
                    return (
                      <div key={model.id} className="flex min-w-0 items-center gap-2 px-4 py-1.5 text-[12px]">
                        <span className="min-w-0 flex-1 truncate font-mono">{model.id}</span>
                        {free ? (
                          <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">免费</Badge>
                        ) : null}
                        {model.reasoning ? <span className="shrink-0 text-[10px] text-muted-foreground">推理</span> : null}
                      </div>
                    );
                  })}
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      </section>

      {/* ④ 接入信息（真实可用） */}
      <section className="min-w-0 space-y-2.5" aria-label="OpenCode 接入信息">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">接入信息 · OpenCode 网关</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            在任意 OpenAI 兼容客户端中填入以下信息即可用；流式为合成 SSE（聚合后一次下发）
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            <InfoRow
              label="接口地址（Base URL）"
              hint="OpenAI 兼容端点；对话 POST /opencode/v1/chat/completions，目录 GET /opencode/v1/models"
            >
              <div className="flex items-center gap-1.5">
                <code
                  className="max-w-[260px] truncate rounded bg-muted/70 px-2 py-1 text-xs"
                  title={ocCfg?.endpoint}
                >
                  {ocCfg?.endpoint ?? "加载中…"}
                </code>
                {ocCfg ? <CopyButton text={ocCfg.endpoint} label="接口地址" /> : null}
              </div>
            </InfoRow>
            <InfoRow
              label="访问密钥"
              hint="请求头 Authorization: Bearer <密钥>（或 x-api-key）；与 WorkBuddy / TRAE 网关独立"
            >
              <div className="flex items-center gap-1.5">
                <code
                  className="max-w-[200px] truncate rounded bg-muted/70 px-2 py-1 text-xs"
                  title={ocCfg?.apiKey}
                >
                  {ocCfg?.apiKey ?? "加载中…"}
                </code>
                {ocCfg ? <CopyButton text={ocCfg.apiKey} label="访问密钥" /> : null}
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void regenerateKey()}
                  disabled={savingKey || !ocCfg}
                >
                  {savingKey ? <Loader2 className="animate-spin" /> : <RefreshCw />}重新生成
                </Button>
              </div>
            </InfoRow>
            <InfoRow
              label="可用模型"
              hint={`仅免费模型；总数 ${ocStatus?.modelCount ?? all.length} 中筛出 ${
                ocStatus?.exposedModelCount ?? freeModels.length
              } 个，非免费模型请求会被拒绝（HTTP 400）`}
            >
              <span className="text-xs text-muted-foreground">
                {ocStatus?.exposedModelCount ?? freeModels.length} 个（见上方清单）
              </span>
            </InfoRow>
            <InfoRow label="上游 server 健康检查" hint="端口自动发现；命中该端点即认定为 OpenCode server">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[220px] truncate rounded bg-muted/70 px-2 py-1 text-xs">
                  GET /global/health
                </code>
                {(ocStatus?.server ?? status?.server)?.ok ? (
                  <CopyButton
                    text={`http://127.0.0.1:${(ocStatus?.server ?? status?.server)?.port}/global/health`}
                    label="健康检查"
                  />
                ) : null}
              </div>
            </InfoRow>
            <InfoRow label="兜底关系" hint="WorkBuddy 网关直连失败时回落至本网关的同一个上游 server">
              <span className="text-xs text-muted-foreground">
                OpenCode server = WorkBuddy 的兜底链路（两网关共用该进程，密钥各自独立）
              </span>
            </InfoRow>
            <div className="mx-4 flex items-start justify-between gap-3 py-3.5 sm:mx-5">
              <p className="mb-0 text-xs text-muted-foreground">调用示例（curl）：</p>
              <pre className="min-w-0 overflow-x-auto rounded-lg bg-muted/60 p-3 text-xs leading-5">
                {`curl -X POST ${ocCfg?.endpoint ?? "http://127.0.0.1:57890/opencode/v1"}/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer ${ocCfg?.apiKey ?? "<访问密钥>"}" \\
  -d '{"model":"${freeModels[0] ?? "opencode/mimo-v2.6-flash-free"}","messages":[{"role":"user","content":"你好"}]}'`}
              </pre>
              <CopyButton
                text={`curl -X POST ${ocCfg?.endpoint ?? "http://127.0.0.1:57890/opencode/v1"}/chat/completions -H "Content-Type: application/json" -H "Authorization: Bearer ${ocCfg?.apiKey ?? ""}" -d '{"model":"${freeModels[0] ?? "opencode/mimo-v2.6-flash-free"}","messages":[{"role":"user","content":"你好"}]}'`}
                label="调用示例"
              />
            </div>
          </CardContent>
        </Card>
      </section>
    </div>
  );
}
