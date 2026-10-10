import { useCallback, useEffect, useMemo, useState } from "react";
import { Activity, Loader2, RefreshCw, TriangleAlert } from "lucide-react";
import { Bar, BarChart, CartesianGrid, Cell, Pie, PieChart, XAxis, YAxis } from "recharts";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart";
import * as api from "@/lib/api";
import type { ModelMonitor, ModelMonitorItem } from "@/lib/types";
import { cn } from "@/lib/utils";

/** 与积分统计页共用同一套模型配色，跨页面对比时颜色一致。 */
const MODEL_COLORS = [
  "var(--data-series-emerald)",
  "var(--data-series-teal)",
  "var(--data-series-violet)",
  "var(--data-series-amber)",
  "var(--data-series-rose)",
  "var(--data-series-indigo)",
  "var(--data-series-sky)",
  "var(--data-series-lime)",
];
/** 趋势图最多单独画 6 个模型，其余归入「其他」。 */
const MAX_SERIES = 6;
const OTHERS = "其他";

const WINDOWS = [
  { key: "1h", hours: 1, label: "近 1 小时" },
  { key: "24h", hours: 24, label: "近 24 小时" },
  { key: "7d", hours: 168, label: "近 7 天" },
  { key: "all", hours: 0, label: "全部" },
] as const;
type WindowKey = (typeof WINDOWS)[number]["key"];

const REFRESH_MS = 30_000;

function formatNumber(value: number | undefined | null, digits = 0): string {
  const v = value ?? 0;
  return v.toLocaleString("zh-CN", { minimumFractionDigits: digits, maximumFractionDigits: digits });
}

function formatTokens(value: number): string {
  const v = value ?? 0;
  const abs = Math.abs(v);
  const sign = v < 0 ? "-" : "";
  if (abs >= 100_000_000) return `${sign}${(abs / 100_000_000).toFixed(1)}亿`;
  if (abs >= 10_000) return `${sign}${(abs / 10_000).toFixed(1)}万`;
  return formatNumber(v);
}

function formatPercent(ratio: number, digits = 1): string {
  return `${((ratio ?? 0) * 100).toFixed(digits)}%`;
}

function formatLatency(ms: number): string {
  if (!ms) return "—";
  return ms >= 1000 ? `${(ms / 1000).toFixed(ms >= 10_000 ? 0 : 1)}s` : `${Math.round(ms)}ms`;
}

function formatClock(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 时间桶标签：桶越粗，标签越粗（分钟 → 小时 → 日期）。 */
function formatBucketLabel(ts: number, bucketMs: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  if (bucketMs >= 24 * 3600_000) return `${pad(d.getMonth() + 1)}/${pad(d.getDate())}`;
  if (bucketMs >= 6 * 3600_000)
    return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}`;
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function modelColor(index: number): string {
  return MODEL_COLORS[index % MODEL_COLORS.length];
}

function KpiCard({
  label,
  value,
  hint,
  tone,
}: {
  label: string;
  value: string;
  hint?: string;
  tone?: "default" | "success" | "danger";
}) {
  return (
    <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
      <CardContent className="p-3.5">
        <p className="text-[11px] text-muted-foreground">{label}</p>
        <p
          className={cn(
            "mt-1 truncate text-xl font-semibold tabular-nums leading-7",
            tone === "success" && "text-emerald-600",
            tone === "danger" && "text-destructive",
          )}
          title={value}
        >
          {value}
        </p>
        {hint ? <p className="mt-0.5 truncate text-[11px] text-muted-foreground" title={hint}>{hint}</p> : null}
      </CardContent>
    </Card>
  );
}

/** 调用率进度条：与图表同色，便于横向对比。 */
function ShareBar({ ratio, color }: { ratio: number; color: string }) {
  return (
    <div className="flex items-center gap-2">
      <div className="h-1.5 w-full min-w-[60px] overflow-hidden rounded-full bg-muted">
        <div
          className="h-full rounded-full transition-[width]"
          style={{ width: `${Math.min(100, Math.max(0, ratio * 100))}%`, backgroundColor: color }}
        />
      </div>
      <span className="w-12 shrink-0 text-right tabular-nums">{formatPercent(ratio)}</span>
    </div>
  );
}

export const MONITOR_PROVIDER_OPTIONS = [
  { key: "all", label: "全部账号" },
  { key: "autoclaw", label: "AutoClaw" },
  { key: "loomy", label: "Loomy" },
  { key: "trae_work_cn", label: "TRAE Work CN" },
  { key: "minimax", label: "MiniMax" },
] as const;

export function ModelMonitorPanel({
  provider: controlledProvider,
  onProviderChange,
}: {
  provider?: string;
  onProviderChange?: (provider: string) => void;
} = {}) {
  const [internalProvider, setInternalProvider] = useState<string>("all");
  const activeProvider = controlledProvider ?? internalProvider;
  const setProvider = (p: string) => {
    if (onProviderChange) onProviderChange(p);
    setInternalProvider(p);
  };

  const [monitor, setMonitor] = useState<ModelMonitor | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [windowKey, setWindowKey] = useState<WindowKey>("24h");
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);

  const hours = WINDOWS.find((w) => w.key === windowKey)?.hours ?? 24;

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const data = await api.getGlmUsageMonitor(hours, undefined, activeProvider);
      setMonitor(data);
      setUpdatedAt(Date.now());
      setError(null);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [hours, activeProvider]);

  useEffect(() => {
    if (!api.isWebui() || api.isDemoMode()) return;
    void load();
    const timer = window.setInterval(() => void load(), REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [load]);

  const models = monitor?.models ?? [];
  const summary = monitor?.summary;

  /** 趋势图序列：调用量前 6 的模型 + 「其他」。 */
  const series = useMemo(() => {
    const names = (monitor?.seriesModels ?? []).slice(0, MAX_SERIES);
    return (monitor?.seriesModels ?? []).length > MAX_SERIES ? [...names, OTHERS] : names;
  }, [monitor]);

  const trendData = useMemo(() => {
    if (!monitor) return [];
    const top = monitor.seriesModels.slice(0, MAX_SERIES);
    return monitor.series.map((point) => {
      const row: Record<string, number | string> = {
        label: formatBucketLabel(point.ts, monitor.bucketMs),
        ts: point.ts,
        failed: point.failed,
      };
      let others = 0;
      for (const [model, calls] of Object.entries(point.byModel)) {
        if (top.includes(model)) {
          row[model] = ((row[model] as number) ?? 0) + calls;
        } else {
          others += calls;
        }
      }
      if (monitor.seriesModels.length > MAX_SERIES) row[OTHERS] = others;
      return row;
    });
  }, [monitor]);

  const chartConfig = useMemo(() => {
    const config: ChartConfig = {};
    series.forEach((name, index) => {
      config[name] = { label: name, color: modelColor(index) };
    });
    return config;
  }, [series]);

  const donutData = useMemo(
    () =>
      models.map((item, index) => ({
        name: item.model,
        value: item.calls,
        fill: modelColor(index),
      })),
    [models],
  );

  if (!api.isWebui() || api.isDemoMode()) return null;

  const hasData = models.length > 0;
  const failedTotal = summary?.failedCalls ?? 0;

  return (
    <section className="min-w-0 space-y-3" aria-labelledby="model-monitor-title">
      <div className="flex flex-wrap items-end justify-between gap-3 px-1">
        <div className="min-w-0">
          <h2 id="model-monitor-title" className="flex items-center gap-2 text-[13px] font-medium leading-5">
            <Activity className="size-4 text-primary" />
            模型监控
          </h2>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            实时统计各模型的调用率、调用次数与成功/失败情况，并按时间维度观察趋势。
            {updatedAt ? ` 最后更新 ${formatClock(updatedAt)}，每 30 秒自动刷新。` : ""}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <div className="flex items-center rounded-lg border p-0.5" role="tablist" aria-label="账号平台">
            {MONITOR_PROVIDER_OPTIONS.map((p) => (
              <button
                key={p.key}
                type="button"
                role="tab"
                aria-selected={activeProvider === p.key}
                className={cn(
                  "rounded-md px-2 py-1 text-xs transition-colors",
                  activeProvider === p.key
                    ? "bg-primary/10 font-medium text-primary shadow-sm"
                    : "text-muted-foreground hover:text-foreground",
                )}
                onClick={() => setProvider(p.key)}
              >
                {p.label}
              </button>
            ))}
          </div>
          <div className="flex items-center rounded-lg border p-0.5" role="tablist" aria-label="监控窗口">
            {WINDOWS.map((w) => (
              <button
                key={w.key}
                type="button"
                role="tab"
                aria-selected={windowKey === w.key}
                className={cn(
                  "rounded-md px-2.5 py-1 text-xs transition-colors",
                  windowKey === w.key
                    ? "bg-background font-medium text-foreground shadow-sm"
                    : "text-muted-foreground hover:text-foreground",
                )}
                onClick={() => setWindowKey(w.key)}
              >
                {w.label}
              </button>
            ))}
          </div>
          <Button size="sm" variant="outline" onClick={() => void load()} disabled={loading}>
            {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
            刷新
          </Button>
        </div>
      </div>

      {error ? (
        <Card>
          <CardContent className="flex items-center gap-2 p-4 text-sm text-destructive">
            <TriangleAlert className="size-4 shrink-0" />
            监控数据加载失败：{error}
          </CardContent>
        </Card>
      ) : null}

      <div className="grid min-w-0 grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
        <KpiCard
          label="调用总次数"
          value={formatNumber(summary?.calls)}
          hint={`${summary?.modelCount ?? 0} 个模型 · Token ${formatTokens(summary?.totalTokens ?? 0)}`}
        />
        <KpiCard label="成功" value={formatNumber(summary?.okCalls)} tone="success" hint="正常返回的调用" />
        <KpiCard
          label="失败"
          value={formatNumber(failedTotal)}
          tone={failedTotal > 0 ? "danger" : "default"}
          hint={failedTotal > 0 ? "失败调用不产生计费" : "暂无失败调用"}
        />
        <KpiCard
          label="成功率"
          value={formatPercent(summary?.successRate ?? 0)}
          hint={`${formatNumber(summary?.calls)} 次调用`}
        />
        <KpiCard
          label="平均耗时"
          value={formatLatency(summary?.avgLatencyMs ?? 0)}
          hint={`首字 ${formatLatency(summary?.avgTtfbMs ?? 0)} · 出字 ${formatLatency(summary?.avgStreamMs ?? 0)}`}
        />
      </div>

      {hasData ? (
        <p className="px-1 text-[11px] leading-5 text-muted-foreground">
          耗时拆分：<span className="text-foreground">首字延迟</span> 是上游预填充（含排队）耗时，取决于上下文长度与工具数量；
          <span className="text-foreground">出字阶段</span> 是首字之后到结束的耗时，主要受思考（reasoning）与输出长度影响。
          两者都慢时优先精简上下文，仅出字慢时优先降低推理档位或换更快的模型。
        </p>
      ) : null}

      {!hasData ? (
        <Card>
          <CardContent className="p-5 text-sm text-muted-foreground">
            该时间范围内暂无调用记录。通过对应渠道发起对话后，监控数据会实时出现在这里。
          </CardContent>
        </Card>
      ) : (
        <>
          <div className="grid min-w-0 gap-3 lg:grid-cols-[minmax(0,320px)_minmax(0,1fr)]">
            {/* 调用率占比 */}
            <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
              <CardContent className="p-4">
                <h3 className="text-[13px] font-medium">调用率占比</h3>
                <p className="mt-0.5 text-[11px] text-muted-foreground">各模型调用次数占窗口内总调用次数的比例</p>
                <ChartContainer config={chartConfig} className="mx-auto mt-2 h-[190px] w-full max-w-[320px]">
                  <PieChart>
                    <ChartTooltip
                      content={
                        <ChartTooltipContent
                          nameKey="name"
                          hideLabel
                          formatter={(value, name) => (
                            <div className="flex w-full items-center justify-between gap-3">
                              <span className="text-muted-foreground">{String(name)}</span>
                              <span className="font-medium tabular-nums">
                                {formatNumber(Number(value))} 次 ·{" "}
                                {formatPercent(Number(value) / (summary?.calls || 1))}
                              </span>
                            </div>
                          )}
                        />
                      }
                    />
                    <Pie
                      data={donutData}
                      dataKey="value"
                      nameKey="name"
                      innerRadius={44}
                      outerRadius={72}
                      paddingAngle={2}
                      stroke="var(--background)"
                      strokeWidth={2}
                      isAnimationActive={false}
                    >
                      {donutData.map((entry) => (
                        <Cell key={entry.name} fill={entry.fill} />
                      ))}
                    </Pie>
                  </PieChart>
                </ChartContainer>
                <ul className="mt-1 space-y-1.5">
                  {models.slice(0, MAX_SERIES).map((item, index) => (
                    <li key={item.model} className="flex items-center gap-2 text-xs">
                      <span
                        className="size-2 shrink-0 rounded-[2px]"
                        style={{ backgroundColor: modelColor(index) }}
                        aria-hidden="true"
                      />
                      <span className="min-w-0 flex-1 truncate" title={item.model}>
                        {item.model}
                      </span>
                      <span className="shrink-0 tabular-nums text-muted-foreground">
                        {formatNumber(item.calls)} 次
                      </span>
                      <span className="w-12 shrink-0 text-right tabular-nums font-medium">
                        {formatPercent(item.share)}
                      </span>
                    </li>
                  ))}
                </ul>
              </CardContent>
            </Card>

            {/* 时间趋势 */}
            <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
              <CardContent className="p-4">
                <div className="flex flex-wrap items-baseline justify-between gap-2">
                  <h3 className="text-[13px] font-medium">调用量趋势（按模型堆叠）</h3>
                  <span className="text-[11px] text-muted-foreground">
                    每格 {" "}
                    {monitor ? Math.round(monitor.bucketMs / 60_000) : 0} 分钟 · 单位：次
                  </span>
                </div>
                <ChartContainer config={chartConfig} className="mt-3 h-[240px] w-full">
                  <BarChart data={trendData} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
                    <CartesianGrid vertical={false} strokeDasharray="3 3" />
                    <XAxis
                      dataKey="label"
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                      minTickGap={28}
                    />
                    <YAxis
                      tickLine={false}
                      axisLine={false}
                      width={34}
                      allowDecimals={false}
                      tickFormatter={(value) => formatNumber(Number(value))}
                    />
                    <ChartTooltip
                      cursor={{ fill: "var(--muted)", opacity: 0.4 }}
                      content={
                        <ChartTooltipContent
                          indicator="dot"
                          labelFormatter={(_value, payload) => {
                            const item = Array.isArray(payload) ? payload[0] : payload;
                            const ts = item?.payload?.ts as number | undefined;
                            const failed = (item?.payload?.failed as number | undefined) ?? 0;
                            const label = ts != null ? formatClock(ts) : "该时间桶";
                            return failed > 0 ? `${label}（${failed} 次失败）` : label;
                          }}
                          formatter={(value, name) => (
                            <div className="flex w-full items-center justify-between gap-3">
                              <span className="text-muted-foreground">{String(name)}</span>
                              <span className="font-medium tabular-nums">{formatNumber(Number(value))} 次</span>
                            </div>
                          )}
                        />
                      }
                    />
                    {series.map((name, index) => (
                      <Bar
                        key={name}
                        dataKey={name}
                        stackId="calls"
                        fill={modelColor(index)}
                        stroke="var(--background)"
                        strokeWidth={1}
                        maxBarSize={30}
                        isAnimationActive={false}
                      />
                    ))}
                  </BarChart>
                </ChartContainer>
              </CardContent>
            </Card>
          </div>

          {/* 模型明细表 */}
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <CardContent className="p-0">
              <div className="flex flex-wrap items-center justify-between gap-2 border-b border-border/60 px-4 py-3 sm:px-5">
                <h3 className="text-[13px] font-medium">各模型明细</h3>
                <span className="text-[11px] text-muted-foreground">
                  按调用次数降序 · 数据窗口：{WINDOWS.find((w) => w.key === windowKey)?.label}
                </span>
              </div>
              <div className="min-w-0 overflow-x-auto">
                <table className="w-full min-w-[960px] text-left text-xs">
                  <thead>
                    <tr className="border-b border-border/60 text-muted-foreground">
                      <th className="px-4 py-2.5 font-medium sm:px-5">模型</th>
                      <th className="px-3 py-2.5 text-right font-medium">调用次数</th>
                      <th className="px-3 py-2.5 font-medium">调用率</th>
                      <th className="px-3 py-2.5 text-right font-medium">成功</th>
                      <th className="px-3 py-2.5 text-right font-medium">失败</th>
                      <th className="px-3 py-2.5 text-right font-medium">成功率</th>
                      <th className="px-3 py-2.5 text-right font-medium">首字延迟</th>
                      <th className="px-3 py-2.5 text-right font-medium">平均耗时</th>
                      <th className="px-3 py-2.5 text-right font-medium">Token</th>
                      <th className="px-3 py-2.5 text-right font-medium">积分</th>
                      <th className="px-4 py-2.5 text-right font-medium sm:px-5">最近调用</th>
                    </tr>
                  </thead>
                  <tbody>
                    {models.map((item: ModelMonitorItem, index: number) => (
                      <tr key={item.model} className="border-b border-border/40 last:border-0">
                        <td className="px-4 py-2.5 sm:px-5">
                          <span className="flex items-center gap-2">
                            <span
                              className="size-2 shrink-0 rounded-[2px]"
                              style={{ backgroundColor: modelColor(index) }}
                              aria-hidden="true"
                            />
                            <span className="truncate font-medium" title={item.model}>
                              {item.model}
                            </span>
                          </span>
                        </td>
                        <td className="px-3 py-2.5 text-right tabular-nums font-medium">
                          {formatNumber(item.calls)}
                        </td>
                        <td className="px-3 py-2.5">
                          <ShareBar ratio={item.share} color={modelColor(index)} />
                        </td>
                        <td className="px-3 py-2.5 text-right tabular-nums text-emerald-600">
                          {formatNumber(item.okCalls)}
                        </td>
                        <td
                          className={cn(
                            "px-3 py-2.5 text-right tabular-nums",
                            item.failedCalls > 0 ? "font-medium text-destructive" : "text-muted-foreground",
                          )}
                        >
                          {formatNumber(item.failedCalls)}
                        </td>
                        <td className="px-3 py-2.5 text-right tabular-nums">
                          {item.failedCalls > 0 ? (
                            <Badge
                              variant={item.successRate >= 0.9 ? "secondary" : "destructive"}
                              className="h-5 px-1.5 text-[10px]"
                            >
                              {formatPercent(item.successRate)}
                            </Badge>
                          ) : (
                            <span className="text-emerald-600">{formatPercent(item.successRate)}</span>
                          )}
                        </td>
                        <td
                          className={cn(
                            "px-3 py-2.5 text-right tabular-nums",
                            item.avgTtfbMs > 8000 ? "font-medium text-amber-600" : "text-muted-foreground",
                          )}
                          title={`出字阶段 ${formatLatency(item.avgStreamMs)}`}
                        >
                          {formatLatency(item.avgTtfbMs)}
                        </td>
                        <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                          {formatLatency(item.avgLatencyMs)}
                        </td>
                        <td className="px-3 py-2.5 text-right tabular-nums">{formatTokens(item.totalTokens)}</td>
                        <td className="px-3 py-2.5 text-right tabular-nums text-primary">
                          {formatNumber(item.credits)}
                        </td>
                        <td className="whitespace-nowrap px-4 py-2.5 text-right tabular-nums text-muted-foreground sm:px-5">
                          {item.lastTs ? formatClock(item.lastTs) : "—"}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                  <tfoot>
                    <tr className="border-t border-border/60 text-xs">
                      <td className="px-4 py-2.5 font-medium sm:px-5">合计</td>
                      <td className="px-3 py-2.5 text-right tabular-nums font-medium">
                        {formatNumber(summary?.calls)}
                      </td>
                      <td className="px-3 py-2.5 text-muted-foreground">
                        {hasData ? "100%" : "—"}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums font-medium text-emerald-600">
                        {formatNumber(summary?.okCalls)}
                      </td>
                      <td
                        className={cn(
                          "px-3 py-2.5 text-right tabular-nums font-medium",
                          failedTotal > 0 ? "text-destructive" : "text-muted-foreground",
                        )}
                      >
                        {formatNumber(failedTotal)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums font-medium">
                        {formatPercent(summary?.successRate ?? 0)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                        {formatLatency(summary?.avgTtfbMs ?? 0)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                        {formatLatency(summary?.avgLatencyMs ?? 0)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        {formatTokens(summary?.totalTokens ?? 0)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums text-primary">
                        {formatNumber(summary?.credits)}
                      </td>
                      <td className="px-4 py-2.5 sm:px-5" />
                    </tr>
                  </tfoot>
                </table>
              </div>
            </CardContent>
          </Card>
        </>
      )}
    </section>
  );
}
