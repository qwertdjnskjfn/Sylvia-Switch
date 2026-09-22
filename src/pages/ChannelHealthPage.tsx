import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  Activity,
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  ArrowUpDown,
  Loader2,
  RefreshCw,
  ShieldCheck,
} from "lucide-react";
import {
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  Line,
  LineChart,
  PolarAngleAxis,
  RadialBar,
  RadialBarChart,
  XAxis,
  YAxis,
} from "recharts";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import { useRefreshAllListener } from "@/lib/refresh-all";
import { formatCompactNumber, formatDateTimeShort, formatGroupedNumber } from "@/lib/stats-format";
import type {
  ChannelAlert,
  ChannelHealth,
  ChannelHealthOverview,
  ChannelStatus,
} from "@/lib/channel-health-types";

// ---- 常量（契约 §7.4）-----------------------------------------------------

const RANGE_OPTIONS: Array<{ hours: number; label: string }> = [
  { hours: 1, label: "1 小时" },
  { hours: 6, label: "6 小时" },
  { hours: 24, label: "24 小时" },
  { hours: 168, label: "7 天" },
];

/** 状态色（契约 §7.3：ok=emerald-500 / warning=amber-500 / critical=rose-500）。 */
const STATUS_COLOR: Record<ChannelStatus, string> = {
  ok: "var(--data-series-emerald)",
  warning: "var(--data-series-amber)",
  critical: "var(--data-series-rose)",
};

const STATUS_BADGE_CLASS: Record<ChannelStatus, string> = {
  ok: "border-transparent bg-emerald-500/15 text-emerald-700 dark:text-emerald-400",
  warning: "border-transparent bg-amber-500/15 text-amber-700 dark:text-amber-400",
  critical: "border-transparent bg-rose-500/15 text-rose-700 dark:text-rose-400",
};

const STATUS_TEXT: Record<ChannelStatus, string> = {
  ok: "健康",
  warning: "警告",
  critical: "异常",
};

const SEVERITY_CHIP_CLASS: Record<ChannelAlert["severity"], string> = {
  info: "border-transparent bg-muted text-muted-foreground",
  warning: "border-transparent bg-amber-500/15 text-amber-700 dark:text-amber-400",
  critical: "border-transparent bg-rose-500/15 text-rose-700 dark:text-rose-400",
};

const TRAFFIC_SOURCE_TEXT: Record<ChannelHealth["trafficSource"], string> = {
  logs: "日志",
  usage: "用量",
  none: "无请求级统计",
};

/** 趋势图 / 对比条形的序号色循环（--data-series-*）。 */
const SERIES_COLORS = [
  "--data-series-emerald",
  "--data-series-teal",
  "--data-series-indigo",
  "--data-series-violet",
  "--data-series-amber",
  "--data-series-rose",
  "--data-series-sky",
  "--data-series-lime",
] as const;

type SortKey = "score" | "requests" | "successRate" | "anomalyRate";

const DEFAULT_SORT_DIR: Record<SortKey, "asc" | "desc"> = {
  score: "desc",
  requests: "desc",
  successRate: "desc",
  anomalyRate: "asc",
};

// ---- 展示格式化 -----------------------------------------------------------

function formatPercent(v: number | null): string {
  return v == null ? "—" : `${(v * 100).toFixed(1)}%`;
}

function formatLatencyMs(v: number | null): string {
  return v == null ? "—" : `${Math.round(v)}ms`;
}

function formatMDHM(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

// ---- 本地小组件 -----------------------------------------------------------

function ScoreRing({ score, status }: { score: number; status: ChannelStatus }) {
  const color = STATUS_COLOR[status];
  const config: ChartConfig = { score: { label: "健康分", color } };
  return (
    <div className="relative size-14 shrink-0">
      <ChartContainer config={config} className="size-full">
        <RadialBarChart
          data={[{ name: "score", value: Math.max(0, Math.min(100, Math.round(score))) }]}
          startAngle={90}
          endAngle={-270}
          innerRadius="72%"
          outerRadius="100%"
          barSize={5}
        >
          <PolarAngleAxis type="number" domain={[0, 100]} angleAxisId={0} tick={false} axisLine={false} />
          <RadialBar dataKey="value" background cornerRadius={4} isAnimationActive={false} />
        </RadialBarChart>
      </ChartContainer>
      <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
        <span className="text-sm font-semibold tabular-nums leading-none">{score}</span>
      </div>
    </div>
  );
}

function MiniTrend({ points, color }: { points: Array<{ at: number; score: number }>; color: string }) {
  const data = points.map((p) => ({ label: formatMDHM(p.at), score: p.score }));
  const config: ChartConfig = { score: { label: "健康分", color } };
  return (
    <ChartContainer config={config} className="h-10 w-full">
      <LineChart data={data} margin={{ top: 3, right: 3, bottom: 3, left: 3 }}>
        <Line
          type="monotone"
          dataKey="score"
          stroke={color}
          strokeWidth={1.5}
          dot={false}
          isAnimationActive={false}
        />
      </LineChart>
    </ChartContainer>
  );
}

function MetricCell({ label, value, muted }: { label: string; value: string; muted?: boolean }) {
  return (
    <div className="min-w-0">
      <p className="text-[10px] text-muted-foreground">{label}</p>
      <p
        className={cn(
          "truncate text-sm font-semibold tabular-nums leading-5",
          muted ? "text-muted-foreground" : "text-foreground",
        )}
      >
        {value}
      </p>
    </div>
  );
}

function ChannelCard({ channel, onOpen }: { channel: ChannelHealth; onOpen: () => void }) {
  const m = channel.metrics;
  const color = STATUS_COLOR[channel.status];
  return (
    <Card
      role="button"
      tabIndex={0}
      aria-label={`查看 ${channel.name} 健康详情`}
      onClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onOpen();
        }
      }}
      className={cn(
        "min-w-0 cursor-pointer gap-0 overflow-hidden rounded-xl py-0 shadow-none transition-colors hover:bg-muted/40 focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50",
        channel.status === "critical" && "ring-1 ring-rose-500/70",
        channel.status === "warning" && "ring-1 ring-amber-500/70",
      )}
    >
      <CardContent className="space-y-3 p-4">
        <div className="flex items-center gap-3">
          <ScoreRing score={channel.score} status={channel.status} />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-1.5">
              <h3 className="truncate text-sm font-semibold" title={channel.name}>
                {channel.name}
              </h3>
              {channel.enabled ? null : (
                <Badge variant="outline" className="h-4 shrink-0 px-1 text-[9px]">
                  已停用
                </Badge>
              )}
            </div>
            <div className="mt-1 flex items-center gap-1.5">
              <Badge
                variant="outline"
                className={cn("h-5 shrink-0 px-1.5 text-[10px]", STATUS_BADGE_CLASS[channel.status])}
              >
                {STATUS_TEXT[channel.status]}
              </Badge>
              <span className="truncate text-[10px] text-muted-foreground" title={channel.base || channel.page}>
                {channel.base || "—"}
              </span>
            </div>
          </div>
        </div>

        <div className="grid grid-cols-2 gap-x-3 gap-y-2 sm:grid-cols-4">
          <MetricCell label="请求" value={formatGroupedNumber(m.requests)} />
          <MetricCell label="成功率" value={formatPercent(m.successRate)} muted={m.successRate == null} />
          <MetricCell label="异常率" value={formatPercent(m.anomalyRate)} muted={m.anomalyRate == null} />
          <MetricCell
            label="账号"
            value={m.totalAccounts > 0 ? `${m.healthyAccounts}/${m.totalAccounts}` : "—"}
            muted={m.totalAccounts === 0}
          />
        </div>

        {channel.scoreTrend.length > 0 ? <MiniTrend points={channel.scoreTrend} color={color} /> : null}

        {channel.alerts.length > 0 ? (
          <div className="flex flex-wrap gap-1">
            {channel.alerts.slice(0, 2).map((a, i) => (
              <Badge
                key={`${a.metric}-${i}`}
                variant="outline"
                className={cn("h-5 max-w-full px-1.5 text-[10px]", SEVERITY_CHIP_CLASS[a.severity])}
              >
                <span className="truncate" title={a.message}>
                  {a.message}
                </span>
              </Badge>
            ))}
          </div>
        ) : (
          <p className="truncate text-[10px] text-muted-foreground" title={channel.note ?? undefined}>
            {channel.note ??
              (m.modelCount > 0
                ? `模型 ${m.modelCount} 个 · 流量源 ${TRAFFIC_SOURCE_TEXT[channel.trafficSource]}`
                : "暂无预警")}
          </p>
        )}
      </CardContent>
    </Card>
  );
}

function AlertBanner({ alerts }: { alerts: ChannelAlert[] }) {
  const criticalCount = alerts.filter((a) => a.severity === "critical").length;
  const warningCount = alerts.filter((a) => a.severity === "warning").length;
  const worst: "critical" | "warning" | null = criticalCount > 0 ? "critical" : warningCount > 0 ? "warning" : null;

  if (!worst) {
    return (
      <Card className="mb-5 border-emerald-500/30 bg-emerald-500/10">
        <CardContent className="flex items-center gap-2 p-4 text-sm text-emerald-700 dark:text-emerald-400">
          <ShieldCheck className="size-4 shrink-0" />
          全部渠道运行正常（当前时间范围内未触发任何预警）。
        </CardContent>
      </Card>
    );
  }

  return (
    <Card
      className={cn(
        "mb-5",
        worst === "critical" ? "border-rose-500/40 bg-rose-500/10" : "border-amber-500/40 bg-amber-500/10",
      )}
    >
      <CardContent className="space-y-2 p-4">
        <div
          className={cn(
            "flex items-center gap-2 text-sm font-semibold",
            worst === "critical" ? "text-rose-700 dark:text-rose-400" : "text-amber-700 dark:text-amber-400",
          )}
        >
          <AlertTriangle className="size-4 shrink-0" />
          活跃预警 {alerts.length} 条（严重 {criticalCount} · 警告 {warningCount}）
        </div>
        <ul className="grid gap-1 text-xs text-foreground/90">
          {alerts.slice(0, 6).map((a, i) => (
            <li key={`${a.channelId}-${a.metric}-${i}`} className="truncate" title={a.message}>
              <span className="font-medium">[{a.channelName}]</span> {a.message}
            </li>
          ))}
        </ul>
        {alerts.length > 6 ? (
          <p className="text-[11px] text-muted-foreground">另有 {alerts.length - 6} 条预警未展示…</p>
        ) : null}
      </CardContent>
    </Card>
  );
}

function KpiCell({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className="min-w-0">
      <p className="text-[11px] text-muted-foreground">{label}</p>
      <p className={cn("truncate text-lg font-semibold tabular-nums leading-7", className ?? "text-foreground")}>
        {value}
      </p>
    </div>
  );
}

function KpiRow({ data }: { data: ChannelHealthOverview }) {
  const s = data.summary;
  return (
    <Card className="mb-5 min-w-0 gap-0 rounded-xl py-0 shadow-none">
      <CardContent className="grid grid-cols-2 gap-3 p-4 sm:grid-cols-3 xl:grid-cols-6">
        <KpiCell label="监控渠道" value={String(s.channels)} />
        <KpiCell label="健康" value={String(s.ok)} className="text-emerald-600 dark:text-emerald-400" />
        <KpiCell label="警告" value={String(s.warning)} className="text-amber-600 dark:text-amber-400" />
        <KpiCell label="异常" value={String(s.critical)} className="text-rose-600 dark:text-rose-400" />
        <KpiCell label="总请求" value={formatGroupedNumber(s.totalRequests)} />
        <KpiCell label="平均成功率" value={formatPercent(s.avgSuccessRate)} />
        <KpiCell label="活跃账号" value={`${s.healthyAccounts}/${s.totalAccounts}`} />
      </CardContent>
    </Card>
  );
}

function DashedHint({ text }: { text: string }) {
  return (
    <div className="rounded-lg border border-dashed px-4 py-10 text-center text-sm text-muted-foreground">
      {text}
    </div>
  );
}

// ---- 页面 ----------------------------------------------------------------

export default function ChannelHealthPage() {
  const navigate = useNavigate();
  const [hours, setHours] = useState(24);
  const [data, setData] = useState<ChannelHealthOverview | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sort, setSort] = useState<{ key: SortKey; dir: "asc" | "desc" }>({ key: "score", dir: "desc" });
  const [hiddenSeries, setHiddenSeries] = useState<ReadonlySet<string>>(() => new Set());
  const loadSeq = useRef(0);

  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    setLoading(true);
    try {
      const res = await api.getChannelHealth(hours);
      if (seq !== loadSeq.current) return;
      setData(res);
      setError(null);
    } catch (e) {
      if (seq !== loadSeq.current) return;
      setError(api.asError(e));
    } finally {
      if (seq === loadSeq.current) setLoading(false);
    }
  }, [hours]);

  useEffect(() => {
    if (!api.isWebui() || api.isDemoMode()) return;
    void load();
    const timer = window.setInterval(() => void load(), 30_000);
    return () => window.clearInterval(timer);
  }, [load]);

  useEffect(() => useRefreshAllListener(() => void load()), [load]);

  const channels = data?.channels ?? [];

  const openChannel = useCallback((id: string) => navigate(`/channel-health/${id}`), [navigate]);

  const pinned = useMemo(
    () => channels.filter((c) => c.status !== "ok").sort((a, b) => a.score - b.score),
    [channels],
  );

  const byScore = useMemo(() => [...channels].sort((a, b) => b.score - a.score), [channels]);

  const sorted = useMemo(() => {
    const dir = sort.dir === "asc" ? 1 : -1;
    const value = (c: ChannelHealth): number => {
      switch (sort.key) {
        case "score":
          return c.score;
        case "requests":
          return c.metrics.requests;
        case "successRate":
          return c.metrics.successRate ?? -1;
        case "anomalyRate":
          return c.metrics.anomalyRate ?? -1;
      }
    };
    return [...channels].sort((a, b) => (value(a) - value(b)) * dir || a.name.localeCompare(b.name));
  }, [channels, sort]);

  const onSort = useCallback((key: SortKey) => {
    setSort((prev) =>
      prev.key === key
        ? { key, dir: prev.dir === "asc" ? "desc" : "asc" }
        : { key, dir: DEFAULT_SORT_DIR[key] },
    );
  }, []);

  // 多渠道健康分趋势：按时间点合并各渠道 scoreTrend。
  const trendChannels = useMemo(() => channels.filter((c) => c.scoreTrend.length > 0), [channels]);
  const trendData = useMemo(() => {
    const rows = new Map<string, { label: string; at: number; [key: string]: number | string }>();
    for (const c of trendChannels) {
      for (const p of c.scoreTrend) {
        const label = formatMDHM(p.at);
        const row = rows.get(label) ?? { label, at: p.at };
        row[c.id] = p.score;
        rows.set(label, row);
      }
    }
    return [...rows.values()].sort((a, b) => a.at - b.at);
  }, [trendChannels]);

  const trendConfig = useMemo(() => {
    const cfg: ChartConfig = {};
    trendChannels.forEach((c, i) => {
      cfg[c.id] = { label: c.name, color: `var(${SERIES_COLORS[i % SERIES_COLORS.length]})` };
    });
    return cfg;
  }, [trendChannels]);

  const toggleSeries = useCallback((id: string) => {
    setHiddenSeries((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  // 请求量横向对比（降序）。
  const barData = useMemo(
    () =>
      [...channels]
        .sort((a, b) => b.metrics.requests - a.metrics.requests)
        .map((c) => ({ id: c.id, name: c.name, requests: c.metrics.requests, status: c.status })),
    [channels],
  );

  if (!api.isWebui() || api.isDemoMode()) {
    return (
      <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
        <Card>
          <CardContent className="p-5 text-sm text-muted-foreground">
            渠道健康度监控仅在 webui 后端（Sylvia Switch 启动器）下可用。
          </CardContent>
        </Card>
      </div>
    );
  }

  const successRateCellClass = (v: number | null): string => {
    if (v == null) return "text-muted-foreground";
    if (v < 0.7) return "bg-rose-500/15 text-rose-700 dark:text-rose-400";
    if (v < 0.9) return "bg-amber-500/15 text-amber-700 dark:text-amber-400";
    return "";
  };

  const anomalyRateCellClass = (v: number | null): string => {
    if (v == null) return "text-muted-foreground";
    if (v > 0.25) return "bg-rose-500/15 text-rose-700 dark:text-rose-400";
    if (v > 0.1) return "bg-amber-500/15 text-amber-700 dark:text-amber-400";
    return "";
  };

  const accountsCellClass = (c: ChannelHealth): string =>
    c.metrics.totalAccounts > 0 && c.metrics.healthyAccounts === 0
      ? "bg-rose-500/15 text-rose-700 dark:text-rose-400"
      : "";

  function SortHeader({ label, sortKey, align = "right" }: { label: string; sortKey: SortKey; align?: "left" | "right" }) {
    const active = sort.key === sortKey;
    return (
      <th
        className={cn(
          "px-3 py-2.5 font-medium",
          align === "right" ? "text-right" : "text-left",
        )}
      >
        <button
          type="button"
          onClick={() => onSort(sortKey)}
          aria-label={`按${label}排序`}
          className={cn(
            "inline-flex items-center gap-1 transition-colors hover:text-foreground",
            active && "text-foreground",
          )}
        >
          {label}
          {active ? (
            sort.dir === "asc" ? (
              <ArrowUp className="size-3" />
            ) : (
              <ArrowDown className="size-3" />
            )
          ) : (
            <ArrowUpDown className="size-3 opacity-40" />
          )}
        </button>
      </th>
    );
  }

  return (
    <div className="mx-auto min-w-0 w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-5 flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-2xl font-semibold tracking-tight">
            <Activity className="size-6 text-primary" />
            渠道健康
          </h1>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            监控全部反代渠道的可用性、账号健康、流量异常与签到情况，评分与预警规则见渠道健康契约。
            {data ? ` 数据更新于 ${formatDateTimeShort(data.generatedAt)}。` : ""}
          </p>
        </div>
        <Button size="sm" variant="outline" onClick={() => void load()} disabled={loading} className="shrink-0">
          {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新
        </Button>
      </header>

      <div className="mb-5 flex flex-wrap items-center gap-3">
        <div className="flex gap-1 rounded-lg bg-muted p-1" role="tablist" aria-label="时间范围">
          {RANGE_OPTIONS.map((opt) => (
            <button
              key={opt.hours}
              type="button"
              role="tab"
              aria-selected={hours === opt.hours}
              className={cn(
                "cursor-pointer rounded-md px-2.5 py-1.5 text-xs transition-colors",
                hours === opt.hours
                  ? "bg-background font-medium text-foreground shadow-sm"
                  : "text-muted-foreground hover:text-foreground",
              )}
              onClick={() => setHours(opt.hours)}
            >
              {opt.label}
            </button>
          ))}
        </div>
        <span className="text-xs text-muted-foreground">每 30 秒自动刷新</span>
      </div>

      {error ? (
        <Card className="mb-5">
          <CardContent className="p-4 text-sm text-destructive">加载失败：{error}</CardContent>
        </Card>
      ) : null}

      {loading && !data ? (
        <div className="space-y-5">
          <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
            <CardContent className="space-y-2 p-4">
              <Skeleton className="h-5 w-48" />
              <Skeleton className="h-4 w-full max-w-md" />
            </CardContent>
          </Card>
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 xl:grid-cols-6">
            {Array.from({ length: 6 }).map((_, i) => (
              <Card key={i} className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
                <CardContent className="space-y-2 p-4">
                  <Skeleton className="h-3 w-12" />
                  <Skeleton className="h-6 w-16" />
                </CardContent>
              </Card>
            ))}
          </div>
          <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
            {Array.from({ length: 6 }).map((_, i) => (
              <Card key={i} className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
                <CardContent className="space-y-3 p-4">
                  <div className="flex items-center gap-3">
                    <Skeleton className="size-14 rounded-full" />
                    <div className="flex-1 space-y-2">
                      <Skeleton className="h-4 w-24" />
                      <Skeleton className="h-4 w-16" />
                    </div>
                  </div>
                  <Skeleton className="h-10 w-full" />
                </CardContent>
              </Card>
            ))}
          </div>
        </div>
      ) : !data ? null : channels.length === 0 ? (
        <Card className="border-dashed">
          <CardContent className="flex flex-col items-center gap-2 p-10 text-center text-sm text-muted-foreground">
            <Activity className="size-8 text-muted-foreground/50" />
            当前时间范围内暂无渠道健康数据，稍后会自动重试。
          </CardContent>
        </Card>
      ) : (
        <div className="min-w-0">
          <AlertBanner alerts={data.alerts} />
          <KpiRow data={data} />

          {pinned.length > 0 ? (
            <section className="mb-6 min-w-0 space-y-2.5">
              <div className="px-1">
                <h2 className="text-[13px] font-medium leading-5">预警渠道（{pinned.length}）</h2>
              </div>
              <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
                {pinned.map((c) => (
                  <ChannelCard key={c.id} channel={c} onOpen={() => openChannel(c.id)} />
                ))}
              </div>
            </section>
          ) : null}

          <section className="mb-6 min-w-0 space-y-2.5">
            <div className="px-1">
              <h2 className="text-[13px] font-medium leading-5">全部渠道（按健康分排序）</h2>
            </div>
            <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
              {byScore.map((c) => (
                <ChannelCard key={c.id} channel={c} onOpen={() => openChannel(c.id)} />
              ))}
            </div>
          </section>

          <section className="mb-6 min-w-0 space-y-2.5">
            <div className="px-1">
              <h2 className="text-[13px] font-medium leading-5">横向对比</h2>
            </div>
            <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
              <CardContent className="p-0">
                <div className="min-w-0 overflow-x-auto">
                  <table className="w-full min-w-[880px] text-left text-xs">
                    <thead>
                      <tr className="border-b border-border/60 text-muted-foreground">
                        <th className="px-4 py-2.5 font-medium sm:px-5">渠道</th>
                        <th className="px-3 py-2.5 font-medium">状态</th>
                        <SortHeader label="健康分" sortKey="score" />
                        <SortHeader label="请求数" sortKey="requests" />
                        <SortHeader label="成功率" sortKey="successRate" />
                        <SortHeader label="异常率" sortKey="anomalyRate" />
                        <th className="px-3 py-2.5 text-right font-medium">平均延迟</th>
                        <th className="px-3 py-2.5 text-right font-medium">账号（健康/总数）</th>
                        <th className="px-4 py-2.5 text-right font-medium sm:px-5">模型数</th>
                      </tr>
                    </thead>
                    <tbody>
                      {sorted.map((c) => (
                        <tr
                          key={c.id}
                          onClick={() => openChannel(c.id)}
                          className="cursor-pointer border-b border-border/40 transition-colors last:border-0 hover:bg-muted/40"
                        >
                          <td className="px-4 py-2.5 sm:px-5">
                            <div className="flex items-center gap-2">
                              <span
                                className="size-2 shrink-0 rounded-[2px]"
                                style={{ backgroundColor: STATUS_COLOR[c.status] }}
                                aria-hidden="true"
                              />
                              <span className="font-medium text-foreground">{c.name}</span>
                            </div>
                            <div className="mt-0.5 truncate text-[10px] text-muted-foreground" title={c.page}>
                              {c.page}
                            </div>
                          </td>
                          <td className="px-3 py-2.5">
                            <Badge
                              variant="outline"
                              className={cn("h-5 px-1.5 text-[10px]", STATUS_BADGE_CLASS[c.status])}
                            >
                              {STATUS_TEXT[c.status]}
                            </Badge>
                          </td>
                          <td className="px-3 py-2.5 text-right font-semibold tabular-nums text-foreground">
                            {c.score}
                          </td>
                          <td className="px-3 py-2.5 text-right tabular-nums">{formatGroupedNumber(c.metrics.requests)}</td>
                          <td className="px-3 py-2.5 text-right">
                            <span
                              className={cn(
                                "rounded px-1.5 py-0.5 tabular-nums",
                                successRateCellClass(c.metrics.successRate),
                              )}
                            >
                              {formatPercent(c.metrics.successRate)}
                            </span>
                          </td>
                          <td className="px-3 py-2.5 text-right">
                            <span
                              className={cn(
                                "rounded px-1.5 py-0.5 tabular-nums",
                                anomalyRateCellClass(c.metrics.anomalyRate),
                              )}
                            >
                              {formatPercent(c.metrics.anomalyRate)}
                            </span>
                          </td>
                          <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                            {formatLatencyMs(c.metrics.avgLatencyMs)}
                          </td>
                          <td className="px-3 py-2.5 text-right">
                            <span
                              className={cn(
                                "rounded px-1.5 py-0.5 tabular-nums",
                                accountsCellClass(c),
                                c.metrics.totalAccounts === 0 && "text-muted-foreground",
                              )}
                            >
                              {c.metrics.totalAccounts > 0
                                ? `${c.metrics.healthyAccounts}/${c.metrics.totalAccounts}`
                                : "—"}
                            </span>
                          </td>
                          <td className="px-4 py-2.5 text-right tabular-nums sm:px-5">{c.metrics.modelCount}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </CardContent>
            </Card>
          </section>

          <section className="mb-6 min-w-0 space-y-2.5">
            <div className="px-1">
              <h2 className="text-[13px] font-medium leading-5">健康分趋势</h2>
            </div>
            <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
              <CardContent className="p-4 sm:p-5">
                {trendChannels.length === 0 ? (
                  <DashedHint text="暂无可展示的健康分历史（首次采样后逐步累积）。" />
                ) : (
                  <>
                    <div className="mb-2 flex flex-wrap gap-1.5">
                      {trendChannels.map((c, i) => {
                        const hidden = hiddenSeries.has(c.id);
                        return (
                          <button
                            key={c.id}
                            type="button"
                            onClick={() => toggleSeries(c.id)}
                            className={cn(
                              "inline-flex cursor-pointer items-center gap-1.5 rounded-md border px-2 py-0.5 text-[11px] transition-colors",
                              hidden
                                ? "border-border/60 text-muted-foreground line-through opacity-60"
                                : "border-border bg-background text-foreground hover:bg-muted/50",
                            )}
                            title={hidden ? "点击显示" : "点击隐藏"}
                          >
                            <span
                              className="size-2 shrink-0 rounded-[2px]"
                              style={{ backgroundColor: `var(${SERIES_COLORS[i % SERIES_COLORS.length]})` }}
                              aria-hidden="true"
                            />
                            {c.name}
                          </button>
                        );
                      })}
                    </div>
                    <ChartContainer config={trendConfig} className="h-72 w-full">
                      <LineChart data={trendData} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
                        <CartesianGrid vertical={false} strokeDasharray="3 3" />
                        <XAxis
                          dataKey="label"
                          tickLine={false}
                          axisLine={false}
                          tickMargin={8}
                          minTickGap={28}
                        />
                        <YAxis domain={[0, 100]} tickLine={false} axisLine={false} width={28} />
                        <ChartTooltip content={<ChartTooltipContent />} />
                        {trendChannels.map((c, i) => (
                          <Line
                            key={c.id}
                            dataKey={c.id}
                            stroke={`var(${SERIES_COLORS[i % SERIES_COLORS.length]})`}
                            strokeWidth={1.8}
                            dot={false}
                            isAnimationActive={false}
                            connectNulls
                            hide={hiddenSeries.has(c.id)}
                          />
                        ))}
                      </LineChart>
                    </ChartContainer>
                  </>
                )}
              </CardContent>
            </Card>
          </section>

          <section className="min-w-0 space-y-2.5">
            <div className="px-1">
              <h2 className="text-[13px] font-medium leading-5">请求量对比</h2>
            </div>
            <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
              <CardContent className="p-4 sm:p-5">
                {barData.every((d) => d.requests === 0) ? (
                  <DashedHint text="当前时间范围内暂无请求量数据。" />
                ) : (
                  <>
                    <ChartContainer config={{ requests: { label: "请求数" } }} className="h-[380px] w-full">
                      <BarChart
                        data={barData}
                        layout="vertical"
                        margin={{ top: 4, right: 16, left: 0, bottom: 0 }}
                      >
                        <CartesianGrid horizontal={false} strokeDasharray="3 3" />
                        <XAxis
                          type="number"
                          tickLine={false}
                          axisLine={false}
                          tickMargin={8}
                          tickFormatter={(v) => formatCompactNumber(Number(v))}
                        />
                        <YAxis
                          type="category"
                          dataKey="name"
                          width={110}
                          tickLine={false}
                          axisLine={false}
                        />
                        <ChartTooltip cursor={{ fill: "var(--muted)", opacity: 0.4 }} content={<ChartTooltipContent />} />
                        <Bar dataKey="requests" radius={[0, 4, 4, 0]} maxBarSize={18} isAnimationActive={false}>
                          {barData.map((d) => (
                            <Cell key={d.id} fill={STATUS_COLOR[d.status]} />
                          ))}
                        </Bar>
                      </BarChart>
                    </ChartContainer>
                    <p className="mt-2 text-[11px] text-muted-foreground">
                      条形颜色即渠道状态：绿 健康 · 黄 警告 · 红 异常；按请求数降序排列。
                    </p>
                  </>
                )}
              </CardContent>
            </Card>
          </section>
        </div>
      )}
    </div>
  );
}
