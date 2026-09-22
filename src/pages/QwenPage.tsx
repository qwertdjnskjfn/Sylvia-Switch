import { useCallback, useEffect, useState } from "react";
import {
  Activity,
  CircleCheck,
  Gauge,
  Loader2,
  ScrollText,
  Terminal,
  Wrench,
  XCircle,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
} from "@/components/ui/card";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type { QwenChannel, QwenChannelsStatus, QwenChannelInfo, ToolCallMonitor } from "@/lib/types";

const CHANNEL_ORDER: QwenChannel[] = ["auto", "web", "qoder"];
const CHANNEL_DESC: Record<QwenChannel, string> = {
  auto: "网页版优先，异常自动回落 Qoder（推荐）",
  web: "仅网页版（chat.qwen.ai 直连，失败不回落）",
  qoder: "仅 Qoder 算力池（现状兜底行为）",
};

function StatusDot({ ok }: { ok: boolean }) {
  return (
    <span
      className={cn(
        "inline-block size-2 shrink-0 rounded-full",
        ok ? "bg-emerald-500" : "bg-destructive",
      )}
    />
  );
}

function Section({
  title,
  description,
  action,
  children,
}: {
  title: string;
  description?: string;
  action?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <section className="min-w-0 space-y-2.5">
      <div className="flex items-end justify-between gap-2 px-1">
        <div>
          <h2 className="text-[13px] font-medium leading-5">{title}</h2>
          {description ? <p className="mt-0.5 text-xs text-muted-foreground">{description}</p> : null}
        </div>
        {action}
      </div>
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="p-0">{children}</CardContent>
      </Card>
    </section>
  );
}

function Metric({
  label,
  value,
  sub,
  ok,
}: {
  label: string;
  value: string | number;
  sub?: string;
  ok?: boolean;
}) {
  return (
    <div className="rounded-lg border border-border/60 bg-muted/30 px-2.5 py-2">
      <p className="text-[10px] text-muted-foreground">{label}</p>
      <p
        className={cn(
          "mt-0.5 text-lg font-semibold leading-6 tabular-nums",
          ok === true && "text-emerald-600",
          ok === false && "text-destructive",
        )}
      >
        {value}
      </p>
      {sub ? <p className="text-[10px] text-muted-foreground">{sub}</p> : null}
    </div>
  );
}

export default function QwenPage() {
  const [channelInfo, setChannelInfo] = useState<QwenChannelInfo | null>(null);
  const [status, setStatus] = useState<QwenChannelsStatus | null>(null);
  const [logs, setLogs] = useState<string[]>([]);
  const [monitor, setMonitor] = useState<ToolCallMonitor | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  const loadChannel = useCallback(async () => {
    setChannelInfo(await api.getQwenChannel());
  }, []);

  const loadStatus = useCallback(async (force = false) => {
    try {
      setStatus(await api.getQwenChannels(force));
      setLoadError(null);
    } catch (e) {
      setLoadError(api.asError(e));
    }
  }, []);

  const loadLogs = useCallback(async () => {
    try {
      const res = await api.getQwenWebLog(40);
      setLogs(res.logs ?? []);
    } catch {
      // 静默：日志非关键路径
    }
  }, []);

  const loadMonitor = useCallback(async () => {
    try {
      setMonitor(await api.getQwenToolcallMonitor(40));
    } catch {
      // 静默：监控非关键路径
    }
  }, []);

  useEffect(() => {
    void loadChannel().catch(() => undefined);
    void loadStatus().catch(() => undefined);
    void loadLogs().catch(() => undefined);
    void loadMonitor().catch(() => undefined);
    const t = window.setInterval(() => {
      // 不带 force：真实探活由后端 TTL 5 分钟 + 单飞控制，轮询不会打爆上游
      void loadStatus().catch(() => undefined);
      void loadMonitor().catch(() => undefined);
    }, 15000);
    return () => window.clearInterval(t);
  }, [loadChannel, loadStatus, loadLogs, loadMonitor]);

  async function switchChannel(channel: QwenChannel) {
    setBusy(channel);
    try {
      await api.setQwenChannel(channel, true);
      await loadChannel();
      await loadStatus();
      setMsg({ type: "ok", text: `已切换到「${CHANNEL_DESC[channel].split("（")[0]}」，实时生效` });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function probeNow() {
    setBusy("probe");
    try {
      // force=1：忽略 TTL 立即触发一次真实短调用探活（后端单飞，重复点击安全）
      await loadStatus(true);
      await loadLogs();
    } finally {
      setBusy(null);
    }
  }

  if (!api.isWebui() || api.isDemoMode()) {
    return (
      <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
        <Alert>
          <AlertTitle>Qwen 通道</AlertTitle>
          <AlertDescription>该功能仅在 webui 后端（Sylvia Switch 启动器）下可用。</AlertDescription>
        </Alert>
      </div>
    );
  }

  const web = status?.web;
  const probe = status?.webProbe;
  const qoder = status?.qoder;

  return (
    <div className="mx-auto min-w-0 w-full max-w-3xl space-y-8 px-4 py-6 sm:px-6 sm:py-8">
      <header className="space-y-2">
        <h1 className="text-xl font-semibold tracking-[-0.01em]">Qwen 通道</h1>
        <p className="text-sm text-muted-foreground">
          /qwen/v1 对话的双通道调度：网页版（chat.qwen.ai，模型最新）与 Qoder 算力池（稳定兜底）
          随时切换，实时生效、无需重启；响应头 <code>X-Qwen-Channel</code> 标注实际通道。
        </p>
      </header>

      {msg ? (
        <p className={cn("text-xs", msg.type === "ok" ? "text-primary" : "text-destructive")}>{msg.text}</p>
      ) : null}

      <Section title="通道选择" description="点击即切换；切换后下一次请求立即使用新通道">
        <CardContent className="p-4 sm:p-5">
          <div className="grid gap-2 sm:grid-cols-3">
            {CHANNEL_ORDER.map((ch) => (
              <button
                key={ch}
                type="button"
                disabled={busy === ch || channelInfo?.channel === ch}
                onClick={() => void switchChannel(ch)}
                className={cn(
                  "rounded-xl border p-3 text-left transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
                  channelInfo?.channel === ch
                    ? "border-primary bg-primary/[0.08]"
                    : "border-border hover:bg-muted/50",
                )}
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="text-sm font-medium">
                    {ch === "auto" ? "🌐 网页版优先" : ch === "web" ? "🌐 仅网页版" : "⚡ 仅 Qoder"}
                  </span>
                  {busy === ch ? (
                    <Loader2 className="size-3.5 animate-spin text-muted-foreground" />
                  ) : channelInfo?.channel === ch ? (
                    <Badge variant="secondary" className="text-[10px]">当前</Badge>
                  ) : null}
                </div>
                <p className="mt-1 text-[11px] leading-4 text-muted-foreground">{CHANNEL_DESC[ch]}</p>
              </button>
            ))}
          </div>
        </CardContent>
      </Section>

      <Section
        title="通道健康状态"
        description="表层状态每 15 秒刷新；「检测」会发一次真实短调用验证能否真正对话（TTL 5 分钟，重复点击安全）"
        action={
          <Button size="sm" variant="outline" onClick={() => void probeNow()} disabled={busy === "probe"}>
            {busy === "probe" || probe?.probeInFlight ? <Loader2 className="animate-spin" /> : <Activity />}
            检测
          </Button>
        }
      >
        <CardContent className="p-0">
          {loadError && !status ? (
            <p className="px-4 py-4 text-xs text-destructive sm:px-5">状态加载失败：{loadError}</p>
          ) : (
            <div className="divide-y divide-border/50">
              <div className="px-4 py-3.5 sm:px-5">
                <div className="flex flex-wrap items-center gap-2">
                  <StatusDot ok={web?.available ?? false} />
                  <span className="text-sm font-medium">网页版通道（chat.qwen.ai）</span>
                  {/* 真实探活优先于表层检查：页面打得开≠能对话（惩罚期） */}
                  {probe?.probeOk === true ? (
                    <Badge variant="secondary" className="text-[10px]">可对话</Badge>
                  ) : probe?.probeOk === false ? (
                    <Badge variant="destructive" className="text-[10px]">被风控拦截</Badge>
                  ) : web?.available ? (
                    <Badge variant="outline" className="text-[10px]">页面就绪（未验证对话）</Badge>
                  ) : (
                    <Badge variant="destructive" className="text-[10px]">离线</Badge>
                  )}
                  {probe?.probeInFlight ? <Loader2 className="size-3 animate-spin text-muted-foreground" /> : null}
                  <span className="text-[11px] text-muted-foreground">v{web?.version ?? "—"}</span>
                </div>
                <div className="mt-2 grid gap-1.5 text-[11px] text-muted-foreground sm:grid-cols-2">
                  <span className="flex items-center gap-1.5">
                    <StatusDot ok={web?.tokenReady ?? false} />
                    桌面端登录态（JWT）
                  </span>
                  <span className="flex items-center gap-1.5">
                    <StatusDot ok={web?.browserRunning ?? false} />
                    专用浏览器{web?.browserRunning ? `（端口 ${web.browserPort}）` : "未运行（首次对话自动拉起）"}
                  </span>
                  <span className="flex items-center gap-1.5">
                    <StatusDot ok={web?.pageReady ?? false} />
                    页面就绪{web?.pageLatencyMs != null ? ` · ${web.pageLatencyMs}ms` : ""}（仅表层）
                  </span>
                  <span>可用模型：{web?.modelsCount ?? "—"}</span>
                  <span className="sm:col-span-2">
                    登录账号：{status?.tokenUser ?? "—"}；模型目录动态拉取（最新 qwen3.8 / 3.7 系列）
                  </span>
                  <span className="sm:col-span-2 flex flex-wrap items-center gap-1.5">
                    {probe?.probed ? (
                      <>
                        <StatusDot ok={probe.probeOk ?? false} />
                        真实探活：
                        {probe.probeOk ? `成功（${probe.probeLatencyMs ?? 0}ms）` : "失败"}
                        {probe.probeAtAgeSecs != null ? ` · ${probe.probeAtAgeSecs}s 前` : ""}
                        {probe.probeError ? (
                          <span className="truncate text-destructive" title={probe.probeError}>
                            {probe.probeError.slice(0, 80)}
                          </span>
                        ) : null}
                      </>
                    ) : (
                      <span className="text-muted-foreground">
                        {probe?.probeInFlight ? "真实探活执行中…" : "尚未探活（点「检测」立即验证能否真正对话）"}
                      </span>
                    )}
                  </span>
                </div>
              </div>
              <div className="px-4 py-3.5 sm:px-5">
                <div className="flex flex-wrap items-center gap-2">
                  <StatusDot ok={qoder?.available ?? false} />
                  <span className="text-sm font-medium">Qoder 通道（算力池兜底）</span>
                  {qoder?.available ? (
                    <Badge variant="secondary" className="text-[10px]">可用</Badge>
                  ) : (
                    <Badge variant="destructive" className="text-[10px]">无可用账号</Badge>
                  )}
                </div>
                <div className="mt-2 text-[11px] text-muted-foreground">
                  独立 qwen 池：{qoder?.poolOk ?? 0}/{qoder?.poolTotal ?? 0} 账号可用（与 /qoder 默认池额度互不侵占）
                </div>
              </div>
            </div>
          )}
        </CardContent>
      </Section>

      <Section
        title="工具调用稳定性监控"
        description="统计「这次调了工具、下次却直接回答」的不一致现象；15 秒自动刷新"
        action={
          <div className="flex gap-2">
            <Button size="sm" variant="ghost" onClick={() => void loadMonitor()}>
              <Gauge />
              刷新
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="text-muted-foreground"
              title="开始新一轮测试批次前清空历史"
              onClick={() => {
                void api.clearQwenToolcallMonitor().then(() => void loadMonitor());
              }}
            >
              清空
            </Button>
          </div>
        }
      >
        <CardContent className="p-0">
          {monitor ? (
            <div className="divide-y divide-border/50">
              {/* 诊断结论 */}
              <div className="px-4 py-3.5 sm:px-5">
                <div className="flex flex-wrap items-center gap-2">
                  {monitor.stats.verdictKind === "lite-fluctuation" ? (
                    <Badge variant="secondary" className="text-[10px]">lite 档固有波动</Badge>
                  ) : monitor.stats.verdictKind === "insufficient-data" ? (
                    <Badge variant="outline" className="text-[10px]">样本不足</Badge>
                  ) : (
                    <Badge variant="destructive" className="text-[10px]">需关注</Badge>
                  )}
                  <span className="text-sm font-medium">{monitor.stats.verdict}</span>
                </div>
                <p className="mt-1.5 text-[11px] leading-4 text-muted-foreground">
                  {monitor.stats.verdictDetail}
                </p>
                {monitor.stats.verdictKind !== "insufficient-data" ? (
                  <p className="mt-1.5 flex items-center gap-1.5 text-[11px] text-muted-foreground">
                    <XCircle className="size-3 shrink-0 text-muted-foreground" />
                    是否配置问题：
                    <span className="font-medium text-foreground">否</span>
                    （请求成功、工具可被调用，仅行为不一致）
                  </p>
                ) : null}
              </div>

              {/* 指标 */}
              <div className="grid grid-cols-2 gap-3 px-4 py-3.5 sm:grid-cols-4 sm:px-5">
                <Metric label="带 tools 请求" value={monitor.stats.withTools} sub={`总 ${monitor.stats.total}`} />
                <Metric
                  label="调用了工具"
                  value={monitor.stats.called}
                  sub={`直答 ${monitor.stats.direct}`}
                  ok={monitor.stats.called > 0}
                />
                <Metric label="调用率" value={`${Math.round(monitor.stats.callRate * 100)}%`} />
                <Metric
                  label="相邻不一致率"
                  value={`${Math.round(monitor.stats.fluctuationRate * 100)}%`}
                  sub={`${monitor.stats.flips}/${monitor.stats.transitions} 次翻转`}
                />
              </div>

              {/* 趋势 */}
              {monitor.stats.trend.some((b) => b.total > 0) ? (
                <div className="px-4 py-3.5 sm:px-5">
                  <p className="mb-2 text-xs font-medium text-muted-foreground">调用率趋势（左旧右新）</p>
                  <div className="flex h-12 items-end gap-1">
                    {monitor.stats.trend.map((b) => {
                      const rate = b.callRate;
                      const h = rate == null ? 0 : Math.max(4, rate * 100);
                      return (
                        <div key={b.bucket} className="flex flex-1 flex-col items-center gap-1">
                          <div
                            className={cn(
                              "w-full rounded-t transition-all",
                              rate == null
                                ? "bg-muted"
                                : rate >= 0.99
                                  ? "bg-emerald-500/80"
                                  : rate >= 0.5
                                    ? "bg-amber-500/80"
                                    : "bg-destructive/70",
                            )}
                            style={{ height: `${h}%` }}
                            title={
                              rate == null
                                ? "无样本"
                                : `调用率 ${Math.round(rate * 100)}%（${b.called}/${b.total}）`
                            }
                          />
                          <span className="text-[9px] text-muted-foreground">{b.total || ""}</span>
                        </div>
                      );
                    })}
                  </div>
                </div>
              ) : null}

              {/* temperature 透传 */}
              <div className="px-4 py-3.5 sm:px-5">
                <p className="text-xs font-medium text-muted-foreground">temperature 透传情况</p>
                <div className="mt-1.5 flex flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
                  {monitor.stats.temperature.distribution.length > 0 ? (
                    monitor.stats.temperature.distribution.map((d) => (
                      <Badge key={d.value} variant="outline" className="text-[10px]">
                        temperature={d.value} × {d.count}
                      </Badge>
                    ))
                  ) : (
                    <span>无显式值</span>
                  )}
                  <Badge variant="outline" className="text-[10px]">
                    未携带 × {monitor.stats.temperature.missing}
                  </Badge>
                </div>
                <p className="mt-1.5 text-[11px] leading-4 text-muted-foreground">
                  {monitor.stats.temperature.note}
                  {monitor.stats.temperature.distribution.some((d) => Number(d.value) !== 0)
                    ? " 当前存在非 0 值——客户端在覆盖 temperature，降低随机性需在客户端侧改。"
                    : monitor.stats.temperature.missing > 0
                      ? " 未携带的请求由上游默认决定；若要强制 temperature=0，需在网关侧补默认值。"
                      : " 已全部显式传 0，随机性已尽可能压低。"}
                </p>
              </div>

              {/* 三方向建议 */}
              <div className="px-4 py-3.5 sm:px-5">
                <p className="mb-2 text-xs font-medium text-muted-foreground">三个应对方向</p>
                <div className="space-y-2">
                  {[
                    { icon: CircleCheck, ...monitor.advisories.accept },
                    { icon: Activity, ...monitor.advisories.reduceRandomness },
                    { icon: Terminal, ...monitor.advisories.switchTier },
                  ].map((a) => {
                    const Icon = a.icon;
                    return (
                      <div key={a.title} className="rounded-lg border border-border/60 bg-muted/30 p-2.5">
                        <div className="flex items-center gap-1.5">
                          <Icon className="size-3.5 shrink-0 text-muted-foreground" />
                          <span className="text-xs font-medium">{a.title}</span>
                        </div>
                        <p className="mt-1 text-[11px] leading-4 text-muted-foreground">{a.body}</p>
                      </div>
                    );
                  })}
                </div>
              </div>

              {/* 事件流 */}
              <div className="px-4 py-3.5 sm:px-5">
                <p className="mb-2 text-xs font-medium text-muted-foreground">
                  最近事件（{monitor.events.length} 条）
                </p>
                {monitor.events.length === 0 ? (
                  <p className="text-[11px] text-muted-foreground">
                    暂无数据。在 WorkBuddy 里用该模型发起需要查代码的请求后，这里会实时记录。
                  </p>
                ) : (
                  <div className="max-h-64 space-y-1 overflow-y-auto font-mono text-[11px]">
                    {monitor.events.slice().reverse().map((e, i) => (
                      <div key={i} className="flex flex-wrap items-center gap-1.5 border-b border-border/30 pb-1">
                        <span className="shrink-0 text-muted-foreground">
                          {new Date(e.ts).toLocaleTimeString("zh-CN", { hour12: false })}
                        </span>
                        {!e.hadTools ? (
                          <Badge variant="outline" className="h-4 px-1 text-[9px]">无 tools</Badge>
                        ) : e.called ? (
                          <Badge variant="secondary" className="h-4 px-1 text-[9px]">
                            <Wrench className="mr-0.5 size-2.5" />
                            {e.toolNames.join(",") || "调工具"}
                          </Badge>
                        ) : (
                          <Badge variant="outline" className="h-4 px-1 text-[9px] text-amber-600">
                            直接回答
                          </Badge>
                        )}
                        {!e.ok ? <Badge variant="destructive" className="h-4 px-1 text-[9px]">失败</Badge> : null}
                        <span className="text-muted-foreground">{e.latencyMs}ms</span>
                        {e.temperature != null ? (
                          <span className="text-muted-foreground">t={e.temperature}</span>
                        ) : null}
                        <span className="truncate text-muted-foreground/70">{e.requestedModel}</span>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            </div>
          ) : (
            <p className="px-4 py-4 text-xs text-muted-foreground sm:px-5">监控数据加载中…</p>
          )}
        </CardContent>
      </Section>

      <Section
        title="网页版调试日志"
        description="CDP 桥接、风控（RGV587）与回落事件的实时记录"
        action={
          <Button size="sm" variant="ghost" onClick={() => void loadLogs()}>
            <ScrollText />
            刷新
          </Button>
        }
      >
        <CardContent className="p-0">
          {logs.length === 0 ? (
            <p className="px-4 py-3.5 text-xs text-muted-foreground sm:px-5">
              暂无日志。发起一次 /qwen/v1 对话后这里会显示通道调度细节。
            </p>
          ) : (
            <div className="max-h-64 space-y-1 overflow-y-auto px-4 py-3 font-mono text-[11px] leading-4 text-muted-foreground sm:px-5">
              {logs.map((line, i) => (
                <div key={i} className="break-all">{line}</div>
              ))}
            </div>
          )}
        </CardContent>
      </Section>

      <Section title="接入与原理" description="OpenAI 兼容端点不变，通道由服务端调度">
        <CardContent className="space-y-2 p-4 sm:p-5">
          <div className="flex flex-wrap items-center gap-2 text-xs">
            <code className="rounded bg-muted/70 px-2 py-1">http://127.0.0.1:57890/qwen/v1</code>
            <span className="text-muted-foreground">（/v1/chat/completions + /v1/models，鉴权同网关密钥）</span>
          </div>
          <ul className="list-disc space-y-1.5 pl-4 text-xs leading-5 text-muted-foreground">
            <li>网页版通道通过专用 Chrome（独立 profile，端口 19235）在真实 chat.qwen.ai 页面上下文发请求，浏览器环境自动处理阿里风控令牌（bx-v / bx-ua）；首次调用自动拉起浏览器窗口，请勿关闭。</li>
            <li>登录态取自 Qwen 桌面客户端（深链 JWT，约 30 天有效）：如网页版通道报登录态失效，打开 Qwen 桌面客户端重新登录一次即可自动恢复。</li>
            <li>网页版失败（风控 / 浏览器缺失 / 超时）时，auto 模式自动回落 Qoder 通道，回落原因写入响应体 <code>qwen_fallback_note</code> 与调试日志。</li>
            <li>两个通道账号体系完全隔离：网页版用桌面端登录态，Qoder 用独立 qwen 账号池，互不干扰。</li>
          </ul>
          <div className="flex items-center gap-2 pt-1 text-xs text-muted-foreground">
            <Terminal className="size-3.5" />
            <span>试用：</span>
            <code className="rounded bg-muted/70 px-1.5 py-0.5">curl http://127.0.0.1:57890/qwen/v1/chat/completions -d &apos;&#123;&quot;model&quot;:&quot;qwen3.7-plus&quot;,&quot;messages&quot;:[&#123;&quot;role&quot;:&quot;user&quot;,&quot;content&quot;:&quot;hi&quot;&#125;]&#125;&apos; -H &quot;Authorization: Bearer &lt;网关密钥&gt;&quot;</code>
          </div>
          <div className="flex items-center gap-1.5 pt-1 text-[11px] text-muted-foreground">
            {web?.available ? <CircleCheck className="size-3.5 text-emerald-500" /> : <XCircle className="size-3.5 text-destructive" />}
            当前网页版通道 {web?.available ? "就绪" : "未就绪"}；Qoder 池 {qoder?.available ? "可用" : "无可用账号"}。
          </div>
        </CardContent>
      </Section>
    </div>
  );
}
