/**
 * WorkBuddy 网关（独立网关 · 2026-09-22 从原「TRAE / WorkBuddy 模型」合并页拆出）。
 *
 * 职责边界：只管 WorkBuddy —— 桌面端模型代理、通道状态、上游可达性与保活、接入信息。
 * **不涉及 TRAE 账号池**（那是 TRAE Work CN 网关的事）；
 * 借道 OpenCode 的 provider 目录归属见「OpenCode 网关」页。
 *
 * 六项能力落点：
 *   网关状态     → 通道态（direct / opencode-fallback）+ OpenCode server 健康与端口
 *   账号保护/保活 → 保活对象是 OpenCode server 进程：看门狗拉起 + 端口探测 + 恢复中瞬态
 *   扣分账号     → 负缓存/连续失败计数（is_neg_cache_error / last_discovery_ok_ms）
 *   速度优先策略 → 免费模型与 flash 类模型优先（直连接入后替换为真实首字延迟排序）
 *   模型选型参考 → provider × 模型清单（含 reasoning / 上下文）
 *   接入信息     → Base URL / 密钥 / 可用模型 / Hermes 配置样例
 *
 * ⚠️ 通道现状（用户 2026-09-22 第 2 项要求）：**以直连优先 + OpenCode 兜底**。
 *    直连协议尚在探测中，故当前通道态标为 `opencode-fallback`；探测通过后置为 `direct`，
 *    回落链路由后端负责，前端仅如实呈现 `channel` 字段。
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import {
  BarChart3,
  Brain,
  CircleAlert,
  Cpu,
  Gauge,
  Loader2,
  RefreshCw,
  Route,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import type { WorkbuddyProxyStatus } from "@/lib/api";
import {
  TASK_META,
  TASK_ORDER,
  classifyTask,
  groupByChannel,
  isFreeModelId,
  speedScore,
  stripChannel,
  type TaskKind,
} from "@/lib/model-taxonomy";
import { cn } from "@/lib/utils";

const WB_HERMES_SNIPPET = `model:
  default: "opencode/mimo-v2.6-flash-free"
  provider: "custom"
  base_url: "http://127.0.0.1:57890/workbuddy/v1"
  api_key: "<访问密钥>"`;

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

export default function WorkbuddyPage() {
  const [wbStatus, setWbStatus] = useState<WorkbuddyProxyStatus | null>(null);
  const [wbModels, setWbModels] = useState<WorkbuddyProxyStatus["providers"] | null>(null);
  const [wbError, setWbError] = useState<string | null>(null);
  const [wbRecovering, setWbRecovering] = useState(false);
  const [wbCfg, setWbCfg] = useState<api.WorkbuddyProxyConfigInfo | null>(null);
  const [nativeModels, setNativeModels] = useState<{ id: string; owned_by: string }[]>([]);
  const [savingKey, setSavingKey] = useState(false);
  const [savingEnabled, setSavingEnabled] = useState(false);
  const [wbMsg, setWbMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    const [statusRes, modelsRes, cfgRes, nativeRes] = await Promise.allSettled([
      api.getWorkbuddyProxyStatus(),
      api.getWorkbuddyProxyModels(),
      api.getWorkbuddyProxyConfig(),
      api.getWorkbuddyNativeModels(),
    ]);
    if (statusRes.status === "fulfilled") setWbStatus(statusRes.value);
    if (modelsRes.status === "fulfilled") {
      setWbModels(modelsRes.value.providers ?? []);
      setWbError(null);
      setWbRecovering(false);
    } else {
      const msg = api.asError(modelsRes.reason);
      setWbError(msg);
      // 上游恢复中（看门狗拉起窗口）与稳态「读取失败」区分展示。
      setWbRecovering(/恢复中|未发现|连接|超时|refused|ECONN/i.test(msg));
    }
    if (cfgRes.status === "fulfilled") setWbCfg(cfgRes.value);
    if (nativeRes.status === "fulfilled") setNativeModels(nativeRes.value.data ?? []);
    setLoading(false);
  }, []);

  useEffect(() => {
    void load();
    const timer = window.setTimeout(() => void load(), 20000);
    return () => window.clearTimeout(timer);
  }, [load]);

  const regenerateWbKey = useCallback(async () => {
    setSavingKey(true);
    try {
      const next = await api.saveWorkbuddyProxyConfig({ regenerateKey: true });
      setWbCfg(next);
      setWbMsg({ type: "ok", text: "已重新生成 WorkBuddy 访问密钥" });
    } catch (e) {
      setWbMsg({ type: "err", text: api.asError(e) });
    }
    setSavingKey(false);
  }, []);

  const toggleWbEnabled = useCallback(async (enabled: boolean) => {
    setSavingEnabled(true);
    try {
      const next = await api.saveWorkbuddyProxyConfig({ enabled });
      setWbCfg(next);
      setWbMsg({ type: "ok", text: enabled ? "已启用 WorkBuddy 代理" : "已停用 WorkBuddy 代理" });
    } catch (e) {
      setWbMsg({ type: "err", text: api.asError(e) });
    }
    setSavingEnabled(false);
  }, []);

  const wbCount = useMemo(
    () => wbModels?.reduce((acc, p) => acc + (p.models?.length ?? 0), 0) ?? 0,
    [wbModels],
  );

  /** 全部模型（provider/model 形态）。 */
  const allModels = useMemo(
    () => (wbModels ?? []).flatMap((p) => (p.models ?? []).map((m) => `${p.id}/${m.id}`)),
    [wbModels],
  );

  /**
   * 速度优先推荐（共享口径）：**每个渠道各取首选**，而不是把 591 条平铺。
   * 排序依据：① 免费优先 ② 快速档（flash/lightning/mini/nano/…）③ 场景序 → 名称。
   * 直连接入后此处替换为按真实首字延迟（TTFB）排序。
   */
  const channelPicks = useMemo(() => {
    return groupByChannel(allModels).map(({ channel, ids }) => {
      const sorted = ids
        .slice()
        .sort((a, b) => speedScore(a) - speedScore(b) || a.localeCompare(b));
      return {
        channel,
        total: ids.length,
        freeCount: ids.filter(isFreeModelId).length,
        best: sorted[0],
        alt: sorted[1],
      };
    });
  }, [allModels]);

  /** 全部模型的场景分布（让「591 条」可读化）。 */
  const taskCounts = useMemo(() => {
    const c = new Map<TaskKind, number>();
    for (const id of allModels) {
      const k = classifyTask(id);
      c.set(k, (c.get(k) ?? 0) + 1);
    }
    return TASK_ORDER.filter((k) => (c.get(k) ?? 0) > 0).map((k) => ({ kind: k, n: c.get(k) ?? 0 }));
  }, [allModels]);

  // 通道态由后端下发（direct = 直连；缺省视为 opencode 兜底）。
  // 后端未定义该字段时做安全读取，避免类型耦合。
  const channel = (wbStatus as { channel?: string } | null)?.channel ?? "opencode-fallback";

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-8 flex min-w-0 flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">WorkBuddy 网关</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            独立网关：WorkBuddy 桌面端模型代理。与 OpenCode 网关同级、互不共享账号池与密钥。
            通道策略为 <strong>直连优先 + OpenCode 兜底</strong>（直连协议探测中，当前实际走兜底）。
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

      {/* ① 通道状态 + 网关状态 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="WorkBuddy 通道状态">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            <Route className="mr-1 inline size-3.5" />
            通道状态 · {wbModels?.length ?? 0} 个渠道 / {wbCount} 个模型条目
          </h2>
          <div className="flex shrink-0 flex-wrap items-center gap-3">
            {channel === "direct" ? (
              <Badge variant="success" className="shrink-0">直连</Badge>
            ) : (
              <Badge variant="outline" className="shrink-0">OpenCode 兜底（直连待接）</Badge>
            )}
            <span className="text-xs text-muted-foreground">
              OpenCode server：
              {wbStatus?.server.ok ? (
                <span className="text-foreground">已连接（端口 {wbStatus.server.port}）</span>
              ) : (
                <span className="text-destructive">{wbStatus?.server.error || "未发现"}</span>
              )}
            </span>
            {wbCfg ? (
              <div className="flex items-center gap-2">
                <span className="text-xs text-muted-foreground">{wbCfg.enabled ? "已启用" : "已停用"}</span>
                <Switch
                  checked={wbCfg.enabled}
                  disabled={savingEnabled}
                  onCheckedChange={(v) => void toggleWbEnabled(v)}
                  aria-label="启用 WorkBuddy 模型代理"
                />
              </div>
            ) : null}
          </div>
        </div>
        {wbMsg ? (
          <p className={cn("px-1 text-xs", wbMsg.type === "ok" ? "text-primary" : "text-destructive")}>
            {wbMsg.text}
          </p>
        ) : null}
        {wbCfg && !wbCfg.enabled ? (
          <Alert variant="warning">
            <CircleAlert />
            <AlertTitle>WorkBuddy 代理已停用</AlertTitle>
            <AlertDescription>
              开关已关：<code className="mx-1 rounded bg-muted px-1">/workbuddy/v1/*</code>
              全部请求（含对话与模型列表）会直接返回 503，不再转发上游。重新打开即可恢复。
            </AlertDescription>
          </Alert>
        ) : null}
        <p className="px-1 text-[11px] leading-5 text-muted-foreground">
          对话端点 <code className="rounded bg-muted px-1">POST /workbuddy/v1/chat/completions</code>，
          模型 ID 形如 <code className="rounded bg-muted px-1">deepseek/deepseek-v4-flash</code>（provider/model）。
          当前兜底链路：本机 OpenCode server（独立实例，端口自动发现，未运行时由看门狗经
          start-serve-4097.bat 拉起）——<strong>保活对象即该 server 进程</strong>；
          直连链路上线后优先走直连，失败自动回落。
        </p>
      </section>

      {/* ② 速度优先（每渠道各取首选）+ 场景分布 */}
      {channelPicks.length > 0 ? (
        <section className="mb-10 min-w-0 space-y-2.5" aria-label="WorkBuddy 速度优先">
          <div className="px-1">
            <h2 className="text-[13px] font-medium leading-5">
              <Gauge className="mr-1 inline size-3.5" />
              速度优先推荐 · {channelPicks.length} 个渠道各取首选
            </h2>
            <p className="mt-0.5 text-xs text-muted-foreground">
              排序依据：① 免费优先 ② 快速档（flash·lightning·mini·nano）③ 场景序 → 名称。
              直连接入后此处将替换为按真实首字延迟（TTFB）排序。悬停场景标签可看适用任务。
            </p>
          </div>

          {/* 场景分布 */}
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <CardContent className="flex min-w-0 flex-wrap items-center gap-1.5 p-3">
              <span className="mr-1 text-[11px] text-muted-foreground">全部 {allModels.length} 个模型按任务场景：</span>
              {taskCounts.map(({ kind, n }) => (
                <Badge key={kind} variant="secondary" className="shrink-0" title={TASK_META[kind].hint}>
                  {TASK_META[kind].label} · {n}
                </Badge>
              ))}
            </CardContent>
          </Card>

          {/* 每渠道首选 */}
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <div className="divide-y divide-border/50">
              {channelPicks.map(({ channel, total, freeCount, best, alt }) => (
                <div
                  key={channel}
                  className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-2.5 sm:px-5"
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                      <span className="truncate text-[13px] font-medium">{channel}</span>
                      {channel === "opencode" ? (
                        <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">自有 · 直连本机</Badge>
                      ) : null}
                      <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">
                        免费 {freeCount}/{total}
                      </Badge>
                    </div>
                    <div className="mt-0.5 flex min-w-0 flex-wrap items-center gap-1.5">
                      <Badge
                        variant={classifyTask(best) === "safety" ? "destructive" : "outline"}
                        className="shrink-0 px-1.5 py-0 text-[10px]"
                        title={TASK_META[classifyTask(best)].hint}
                      >
                        {TASK_META[classifyTask(best)].label}
                      </Badge>
                      <span className="min-w-0 truncate font-mono text-[11px]" title={best}>
                        {stripChannel(best)}
                      </span>
                      {isFreeModelId(best) ? (
                        <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">免费</Badge>
                      ) : null}
                      {alt ? (
                        <span
                          className="min-w-0 truncate text-[10px] text-muted-foreground"
                          title={`备选：${alt}`}
                        >
                          备选 {stripChannel(alt)}
                        </span>
                      ) : null}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          </Card>
        </section>
      ) : null}

      {/* ③ 模型目录（provider × model） */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="WorkBuddy 模型目录">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            <Cpu className="mr-1 inline size-3.5" />
            模型目录 · {wbModels?.length ?? 0} 个渠道 / {wbCount} 个模型条目
          </h2>
          <span className="text-xs text-muted-foreground">
            来源：上游 provider 表（当前经 OpenCode server）；条目数 = Σ(各渠道模型数)，同一模型被多渠道提供会各计一次
          </span>
        </div>
        {wbRecovering ? (
          <Alert variant="default">
            <Loader2 className="animate-spin" />
            <AlertTitle>上游恢复中</AlertTitle>
            <AlertDescription>
              看门狗正在自动拉起（通常 ≤60 秒），页面每 20 秒自动重试；
              急用可手动执行 <code className="mx-1 rounded bg-muted px-1">E:\opencode\start-serve-4097.bat</code>。
            </AlertDescription>
          </Alert>
        ) : wbError ? (
          <Alert variant="warning">
            <CircleAlert />
            <AlertTitle>模型目录读取失败</AlertTitle>
            <AlertDescription>{wbError}</AlertDescription>
          </Alert>
        ) : (
          <div className="grid min-w-0 gap-3 sm:grid-cols-2">
            {(wbModels ?? []).map((provider) => (
              <Card key={provider.id} className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
                <CardContent className="p-0">
                  <div className="flex items-center justify-between border-b border-border/50 px-4 py-2.5">
                    <span className="truncate text-[13px] font-medium">{provider.id}</span>
                    <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">
                      {provider.models?.length ?? 0} 模型
                    </Badge>
                  </div>
                  <div className="max-h-52 overflow-y-auto">
                    {(provider.models ?? []).map((model) => (
                      <div key={model.id} className="flex min-w-0 items-center gap-2 px-4 py-1.5 text-[12px]">
                        <span className="min-w-0 flex-1 truncate font-mono">{model.id}</span>
                        {model.reasoning ? <Brain className="size-3 shrink-0 text-muted-foreground" /> : null}
                        {typeof model.context === "number" ? (
                          <span className="shrink-0 text-[10px] tabular-nums text-muted-foreground">
                            {model.context}
                          </span>
                        ) : null}
                      </div>
                    ))}
                  </div>
                </CardContent>
              </Card>
            ))}
          </div>
        )}
      </section>

      {/* ④ 接入信息 · WorkBuddy 内置模型（桌面端会话自带，走 WorkBuddy 订阅） */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="WorkBuddy 内置模型接入信息">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">接入信息 · WorkBuddy 内置模型</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            WorkBuddy 桌面端内嵌会话自带的模型（走 WorkBuddy 订阅），经 ACP 通道转发——与「OpenCode 兜底」是两套模型体系
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            <InfoRow label="接口地址（Base URL）" hint="OpenAI 兼容端点；ACP 真流式（逐块增量下发）">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[260px] truncate rounded bg-muted/70 px-2 py-1 text-xs">
                  http://127.0.0.1:57890/workbuddy-native/v1
                </code>
                <CopyButton text="http://127.0.0.1:57890/workbuddy-native/v1" label="接口地址" />
              </div>
            </InfoRow>
            <InfoRow label="内置模型" hint="实时探测桌面端内嵌会话的 --model；桌面端未运行时为空">
              <span className="font-mono text-xs">
                {nativeModels.length > 0
                  ? nativeModels.map((m) => m.id).join("、")
                  : "未探测到（请先在 WorkBuddy 打开一个 AI 会话）"}
              </span>
            </InfoRow>
            <InfoRow label="访问密钥" hint="仅回环监听，无需密钥；fullAccess 门禁默认开启（本机白名单目录内的会话）">
              <span className="text-xs text-muted-foreground">不需要</span>
            </InfoRow>
            <div className="mx-4 flex items-start justify-between gap-3 py-3.5 sm:mx-5">
              <p className="mb-0 text-xs text-muted-foreground">调用示例（curl）：</p>
              <pre className="min-w-0 overflow-x-auto rounded-lg bg-muted/60 p-3 text-xs leading-5">
                {`curl -X POST http://127.0.0.1:57890/workbuddy-native/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -d '{"model":"${nativeModels[0]?.id ?? "deepseek-v4.1-flash"}","messages":[{"role":"user","content":"你好"}]}'`}
              </pre>
              <CopyButton
                text={`curl -X POST http://127.0.0.1:57890/workbuddy-native/v1/chat/completions -H "Content-Type: application/json" -d '{"model":"${nativeModels[0]?.id ?? "deepseek-v4.1-flash"}","messages":[{"role":"user","content":"你好"}]}'`}
                label="调用示例"
              />
            </div>
          </CardContent>
        </Card>
      </section>

      {/* ⑤ 接入信息 · WorkBuddy 代理 */}
      <section className="min-w-0 space-y-2.5" aria-label="WorkBuddy 接入信息">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">接入信息 · WorkBuddy</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">在任意 OpenAI 兼容客户端中填入以下信息</p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            <InfoRow label="接口地址（Base URL）" hint="OpenAI 兼容端点；流式为合成 SSE（整段完成后一次下发）">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[240px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={wbCfg?.endpoint}>
                  {wbCfg?.endpoint ?? "加载中…"}
                </code>
                {wbCfg ? <CopyButton text={wbCfg.endpoint} label="接口地址" /> : null}
              </div>
            </InfoRow>
            <InfoRow label="访问密钥" hint="请求头 Authorization: Bearer <密钥>（或 x-api-key）">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[200px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={wbCfg?.apiKey}>
                  {wbCfg?.apiKey ?? "加载中…"}
                </code>
                {wbCfg ? <CopyButton text={wbCfg.apiKey} label="访问密钥" /> : null}
                <Button size="sm" variant="outline" onClick={() => void regenerateWbKey()} disabled={savingKey || !wbCfg}>
                  {savingKey ? <Loader2 className="animate-spin" /> : <RefreshCw />}重新生成
                </Button>
              </div>
            </InfoRow>
            <InfoRow label="可用模型" hint="provider/model 形态；请求缺省模型为 opencode/mimo-v2.6-flash-free">
              <span className="text-xs text-muted-foreground">上方列表共 {wbCount} 个，客户端按模型名直接调用</span>
            </InfoRow>
            <div className="mx-4 flex items-start justify-between gap-3 py-3.5 sm:mx-5">
              <p className="mb-0 text-xs text-muted-foreground">Hermes（cli-config.yaml 模型段）：</p>
              <pre className="min-w-0 overflow-x-auto rounded-lg bg-muted/60 p-3 text-xs leading-5">
                {WB_HERMES_SNIPPET.replace("<访问密钥>", wbCfg?.apiKey ?? "<访问密钥>")}
              </pre>
              {wbCfg ? (
                <CopyButton text={WB_HERMES_SNIPPET.replace("<访问密钥>", wbCfg.apiKey)} label="配置示例" />
              ) : null}
            </div>
          </CardContent>
        </Card>
      </section>
    </div>
  );
}
