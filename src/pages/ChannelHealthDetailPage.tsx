import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import {
  Activity,
  AlertTriangle,
  ArrowLeft,
  CalendarCheck,
  Clock,
  Cpu,
  Gauge,
  Loader2,
  RefreshCw,
  Server,
  Timer,
  Users,
  Zap,
} from "lucide-react";
import { Bar, BarChart, CartesianGrid, Line, LineChart, ReferenceLine, XAxis, YAxis } from "recharts";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
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
import type { ChannelAlert, ChannelDetail, ChannelStatus } from "@/lib/channel-health-types";
import {
  formatDateTimeShort as formatAxisTime,
  formatGroupedNumber as formatNumber,
} from "@/lib/stats-format";

// ---------------------------------------------------------------------------
// 契约常量（docs/channel-health-contract.md §3/§4/§7.5）
// ---------------------------------------------------------------------------

/** 时间范围切换：1/6/24/168 小时。 */
const WINDOW_OPTIONS: { hours: number; label: string }[] = [
  { hours: 1, label: "1 小时" },
  { hours: 6, label: "6 小时" },
  { hours: 24, label: "24 小时" },
  { hours: 168, label: "7 天" },
];

/** 状态色（冻结）：ok=emerald-500、warning=amber-500、critical=rose-500、未知=muted-foreground。 */
const STATUS_LABEL: Record<ChannelStatus, string> = {
  ok: "健康",
  warning: "警告",
  critical: "异常",
};

function statusBadgeClass(status: string | undefined): string {
  switch (status) {
    case "ok":
      return "border-transparent bg-emerald-500/15 text-emerald-700 dark:text-emerald-400";
    case "warning":
      return "border-transparent bg-amber-500/15 text-amber-700 dark:text-amber-400";
    case "critical":
      return "border-transparent bg-rose-500/15 text-rose-700 dark:text-rose-400";
    default:
      return "border-transparent bg-muted text-muted-foreground";
  }
}

function severityBadgeClass(severity: ChannelAlert["severity"]): string {
  switch (severity) {
    case "critical":
      return "border-transparent bg-rose-500/15 text-rose-700 dark:text-rose-400";
    case "warning":
      return "border-transparent bg-amber-500/15 text-amber-700 dark:text-amber-400";
    default:
      return "border-transparent bg-muted text-muted-foreground";
  }
}

const SEVERITY_LABEL: Record<ChannelAlert["severity"], string> = {
  critical: "严重",
  warning: "警告",
  info: "提示",
};

/** 账号池状态徽章色（契约 §2 account_healthy 判定同款映射）。 */
function accountStatusBadgeClass(status: string, enabled: boolean): string {
  if (!enabled) return "border-transparent bg-muted text-muted-foreground";
  const s = status.toLowerCase();
  if (s === "ok" || s === "active" || s === "healthy") {
    return "border-transparent bg-emerald-500/15 text-emerald-700 dark:text-emerald-400";
  }
  if (s === "error" || s === "errored") {
    return "border-transparent bg-rose-500/15 text-rose-700 dark:text-rose-400";
  }
  if (s === "cooling") {
    return "border-transparent bg-amber-500/15 text-amber-700 dark:text-amber-400";
  }
  return "border-transparent bg-muted text-muted-foreground";
}

function accountStatusLabel(status: string, enabled: boolean): string {
  if (!enabled) return "已停用";
  return status || "未知";
}

/** 成功率/异常率百分比（1 位小数；无数据显示 —）。 */
function formatPct(ratio: number | null | undefined): string {
  if (ratio == null || !Number.isFinite(ratio)) return "—";
  return `${(ratio * 100).toFixed(1)}%`;
}

/** 延迟统一 `ms`（千分位；无数据显示 —）。 */
function formatMs(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return "—";
  return `${formatNumber(Math.round(ms))}ms`;
}

function scoreTextClass(score: number): string {
  if (score >= 85) return "text-emerald-600 dark:text-emerald-400";
  if (score >= 60) return "text-amber-600 dark:text-amber-400";
  return "text-rose-600 dark:text-rose-400";
}

const CHECKIN_RESULT_LABEL: Record<string, string> = {
  success: "成功",
  already: "已签到",
  error: "失败",
  unsupported: "不支持",
};

function checkinResultBadgeClass(result: string): string {
  switch (result) {
    case "success":
      return "border-transparent bg-emerald-500/15 text-emerald-700 dark:text-emerald-400";
    case "already":
      return "border-transparent bg-sky-500/15 text-sky-700 dark:text-sky-400";
    case "error":
      return "border-transparent bg-rose-500/15 text-rose-700 dark:text-rose-400";
    default:
      return "border-transparent bg-muted text-muted-foreground";
  }
}

// ---------------------------------------------------------------------------
// 本地小组件（页面自包含，不新建共享组件文件）
// ---------------------------------------------------------------------------

function KpiCard({
  icon,
  label,
  value,
  hint,
  valueClass,
}: {
  icon: React.ReactNode;
  label: string;
  value: string;
  hint?: string;
  valueClass?: string;
}) {
  return (
    <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
      <CardContent className="p-4">
        <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <span className="shrink-0 [&>svg]:size-3.5">{icon}</span>
          <span className="truncate">{label}</span>
        </div>
        <p
          className={cn("mt-1 truncate text-lg font-semibold tabular-nums leading-7", valueClass)}
          title={value}
        >
          {value}
        </p>
        {hint ? (
          <p className="truncate text-[11px] text-muted-foreground" title={hint}>
            {hint}
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}

function SectionCard({
  title,
  desc,
  extra,
  children,
}: {
  title: string;
  desc?: string;
  extra?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
      <CardHeader className="gap-1 border-b border-border/60 px-4 py-3 sm:px-5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <CardTitle className="text-[13px] font-semibold leading-5">{title}</CardTitle>
          {extra}
        </div>
        {desc ? <p className="text-[11px] leading-4 text-muted-foreground">{desc}</p> : null}
      </CardHeader>
      <CardContent className="p-0">{children}</CardContent>
    </Card>
  );
}

function EmptyHint({ text }: { text: string }) {
  return (
    <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">{text}</p>
  );
}

function AlertRow({ alert }: { alert: ChannelAlert }) {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-border/40 px-4 py-2.5 last:border-0 sm:px-5">
      <Badge variant="outline" className={cn("h-5 shrink-0 px-1.5 text-[10px]", severityBadgeClass(alert.severity))}>
        {SEVERITY_LABEL[alert.severity]}
      </Badge>
      <span className="min-w-0 flex-1 text-xs text-foreground">{alert.message}</span>
      {alert.value != null || alert.threshold != null ? (
        <span className="shrink-0 font-mono text-[11px] tabular-nums text-muted-foreground">
          {alert.value != null ? `当前 ${alert.value}` : ""}
          {alert.value != null && alert.threshold != null ? " / " : ""}
          {alert.threshold != null ? `阈值 ${alert.threshold}` : ""}
        </span>
      ) : null}
    </div>
  );
}

/** 健康分进度条（账号有 healthScore 时展示）。 */
function HealthScoreBar({ score }: { score: number }) {
  const pct = Math.max(0, Math.min(100, Math.round(score)));
  const barClass =
    pct >= 85 ? "bg-emerald-500" : pct >= 60 ? "bg-amber-500" : "bg-rose-500";
  return (
    <div className="flex items-center gap-2" title={`健康分 ${pct}`}>
      <div className="h-1.5 w-16 shrink-0 overflow-hidden rounded-full bg-muted">
        <div className={cn("h-full rounded-full", barClass)} style={{ width: `${pct}%` }} />
      </div>
      <span className="tabular-nums text-foreground">{pct}</span>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 页面
// ---------------------------------------------------------------------------

export default function ChannelHealthDetailPage() {
  const { gatewayId } = useParams<{ gatewayId: string }>();
  const navigate = useNavigate();

  const [hours, setHours] = useState(24);
  const [detail, setDetail] = useState<ChannelDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!gatewayId || !api.isWebui() || api.isDemoMode()) return;
    setLoading(true);
    try {
      const data = await api.getChannelHealthDetail(gatewayId, hours);
      setDetail(data);
      setLoadError(null);
    } catch (e) {
      setLoadError(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [gatewayId, hours]);

  // 首拉 + 30s 轮询（cleanup clear）+ 订阅全局刷新。
  useEffect(() => {
    void load();
    const timer = window.setInterval(() => void load(), 30_000);
    return () => window.clearInterval(timer);
  }, [load]);

  useRefreshAllListener(() => void load());

  // 环境门禁：仅 WebUI 可用（照抄 ModelUsagePage）。
  if (!api.isWebui() || api.isDemoMode()) {
    return (
      <div className="mx-auto min-w-0 w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
        <Card>
          <CardContent className="p-5 text-sm text-muted-foreground">
            渠道健康度监控仅在 webui 后端（Sylvia Switch 启动器）下可用。
          </CardContent>
        </Card>
      </div>
    );
  }

  const channel = detail?.channel;
  const notFound = !loading && !detail;
  const channelMissing = !!detail && (!channel || !channel.id);

  // gatewayId 不存在 / 首次加载失败 → 「渠道不存在」空态 + 返回按钮。
  if ((notFound || channelMissing) && loadError) {
    return (
      <div className="mx-auto min-w-0 w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
        <Card className="mx-auto max-w-md gap-0 rounded-xl py-0 shadow-none">
          <CardContent className="flex flex-col items-center gap-3 p-8 text-center">
            <Server className="size-8 text-muted-foreground" />
            <h2 className="text-base font-semibold">渠道不存在</h2>
            <p className="text-sm text-muted-foreground">
              未找到 ID 为「{gatewayId ?? "—"}」的渠道，或数据加载失败。
            </p>
            <p className="max-w-full truncate text-[11px] text-muted-foreground" title={loadError}>
              {loadError}
            </p>
            <Button variant="outline" size="sm" onClick={() => navigate("/channel-health")}>
              <ArrowLeft />返回渠道健康
            </Button>
          </CardContent>
        </Card>
      </div>
    );
  }

  if ((notFound || channelMissing) && !loadError) {
    return (
      <div className="mx-auto min-w-0 w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
        <Card className="mx-auto max-w-md gap-0 rounded-xl py-0 shadow-none">
          <CardContent className="flex flex-col items-center gap-3 p-8 text-center">
            <Server className="size-8 text-muted-foreground" />
            <h2 className="text-base font-semibold">渠道不存在</h2>
            <p className="text-sm text-muted-foreground">
              未找到 ID 为「{gatewayId ?? "—"}」的渠道，请从渠道健康总览进入。
            </p>
            <Button variant="outline" size="sm" onClick={() => navigate("/channel-health")}>
              <ArrowLeft />返回渠道健康
            </Button>
          </CardContent>
        </Card>
      </div>
    );
  }

  // 首次加载中 → skeleton。
  if (loading && !detail) {
    return (
      <div className="mx-auto min-w-0 w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
        <Skeleton className="mb-6 h-8 w-48" />
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 xl:grid-cols-6">
          {Array.from({ length: 8 }).map((_, i) => (
            <Skeleton key={i} className="h-20 rounded-xl" />
          ))}
        </div>
        <Skeleton className="mt-3 h-40 rounded-xl" />
        <Skeleton className="mt-3 h-40 rounded-xl" />
      </div>
    );
  }

  if (!detail || !channel) {
    return (
      <div className="mx-auto min-w-0 w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
        <Card>
          <CardContent className="p-5 text-sm text-muted-foreground">暂无数据。</CardContent>
        </Card>
      </div>
    );
  }

  const m = channel.metrics;
  const scoreTrendData = detail.trend.length > 0 ? detail.trend : channel.scoreTrend;
  const trendData = scoreTrendData.map((p) => ({ at: p.at, score: p.score }));
  const bucketData = detail.buckets;
  const hasScoreTrend = trendData.length > 0;
  const hasBuckets = bucketData.length > 0;
  const recentLogs = detail.recentLogs.slice(0, 30);
  const alerts = channel.alerts ?? [];
  const criticalAlerts = alerts.filter((a) => a.severity === "critical").length;
  const warningAlerts = alerts.filter((a) => a.severity === "warning").length;
  const hasPage = !!channel.page && channel.page !== "—";

  const scoreChartConfig: ChartConfig = {
    score: { label: "健康分", color: "var(--data-series-indigo)" },
  };

  const bucketChartConfig: ChartConfig = {
    ok: { label: "成功", color: "var(--data-series-emerald)" },
    failed: { label: "失败", color: "var(--data-series-rose)" },
  };

  return (
    <div className="mx-auto min-w-0 w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-6 flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-2xl font-semibold tracking-tight">
            <Activity className="size-6 text-primary" />
            渠道健康详情
          </h1>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            查看单个渠道的评分趋势、请求分布、账号池与签到情况，数据每 30 秒自动刷新。
          </p>
        </div>
        <Button size="sm" variant="outline" onClick={() => void load()} disabled={loading} className="shrink-0">
          {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新
        </Button>
      </header>

      {/* 返回链接 + 渠道名 + base + 前往管理 + 状态徽章 + 评分 */}
      <div className="mb-4 flex flex-wrap items-center gap-x-3 gap-y-2">
        <Link
          to="/channel-health"
          className="inline-flex shrink-0 items-center gap-1 text-sm text-muted-foreground transition-colors hover:text-foreground"
        >
          <ArrowLeft className="size-4" />
          返回渠道健康
        </Link>
        <span className="min-w-0 truncate text-lg font-semibold" title={channel.name}>
          {channel.name}
        </span>
        <Badge variant="outline" className="shrink-0 font-mono text-[10px]" title={`反代基址 ${channel.base}`}>
          {channel.base}
        </Badge>
        <Button
          variant="outline"
          size="sm"
          className="shrink-0"
          disabled={!hasPage}
          onClick={() => hasPage && navigate(channel.page)}
        >
          前往管理
        </Button>
        {!channel.enabled ? (
          <Badge variant="outline" className="h-5 shrink-0 px-1.5 text-[10px]">
            已停用
          </Badge>
        ) : null}
        <Badge variant="outline" className={cn("h-5 shrink-0 px-1.5 text-[10px]", statusBadgeClass(channel.status))}>
          {STATUS_LABEL[channel.status] ?? channel.status}
        </Badge>
        <span className={cn("shrink-0 text-sm font-semibold tabular-nums", scoreTextClass(channel.score))}>
          {channel.score} 分
        </span>
        {channel.note ? (
          <span className="min-w-0 truncate text-[11px] text-muted-foreground" title={channel.note}>
            {channel.note}
          </span>
        ) : null}
      </div>

      {/* 时间范围切换 */}
      <div className="mb-4 flex flex-wrap items-center gap-1.5">
        {WINDOW_OPTIONS.map((w) => (
          <Button
            key={w.hours}
            size="sm"
            variant={hours === w.hours ? "default" : "outline"}
            onClick={() => setHours(w.hours)}
          >
            {w.label}
          </Button>
        ))}
        <span className="ml-auto text-xs text-muted-foreground">每 30 秒自动刷新</span>
      </div>

      {/* 刷新失败但仍有旧数据时，非致命提示 */}
      {loadError && detail ? (
        <Alert variant="destructive" className="mb-3">
          <AlertTriangle className="size-4" />
          <AlertTitle>刷新失败</AlertTitle>
          <AlertDescription>{loadError}（下方为上一次成功加载的数据）</AlertDescription>
        </Alert>
      ) : null}

      {/* KPI 行 */}
      <div className="mb-3 grid grid-cols-2 gap-3 sm:grid-cols-3 xl:grid-cols-6">
        <KpiCard
          icon={<Gauge />}
          label="请求数"
          value={formatNumber(m.requests)}
          hint={`成功 ${formatNumber(m.okRequests)} · 失败 ${formatNumber(m.errRequests)}`}
        />
        <KpiCard
          icon={<Activity />}
          label="成功率"
          value={formatPct(m.successRate)}
          valueClass={m.successRate != null && m.successRate < 0.9 ? "text-amber-600 dark:text-amber-400" : "text-emerald-600 dark:text-emerald-400"}
        />
        <KpiCard
          icon={<AlertTriangle />}
          label="异常率"
          value={formatPct(m.anomalyRate)}
          valueClass={m.anomalyRate != null && m.anomalyRate > 0.1 ? "text-rose-600 dark:text-rose-400" : undefined}
        />
        <KpiCard icon={<Timer />} label="平均延迟" value={formatMs(m.avgLatencyMs)} />
        <KpiCard
          icon={<Zap />}
          label="总 Token"
          value={formatNumber(m.tokens)}
          hint={`缓存命中 ${formatPct(m.cacheHitRate)}`}
        />
        <KpiCard
          icon={<Users />}
          label="账号"
          value={`${formatNumber(m.healthyAccounts)}/${formatNumber(m.totalAccounts)}`}
          hint={`冷却 ${formatNumber(m.coolingAccounts)} · 异常 ${formatNumber(m.errorAccounts)}`}
        />
        <KpiCard icon={<Cpu />} label="模型数" value={formatNumber(m.modelCount)} />
        <KpiCard
          icon={<CalendarCheck />}
          label="今日签到"
          value={`${formatNumber(m.todayCheckedIn)}/${formatNumber(m.checkinAccounts)}`}
          hint={m.checkinAccounts === 0 ? "暂无签到能力" : undefined}
        />
      </div>

      <div className="space-y-3">
        {/* 本渠道预警列表 */}
        <SectionCard
          title="本渠道预警"
          desc={
            alerts.length > 0
              ? `共 ${alerts.length} 条（严重 ${criticalAlerts} · 警告 ${warningAlerts}）`
              : undefined
          }
        >
          {alerts.length === 0 ? <EmptyHint text="暂无预警" /> : alerts.map((a, i) => <AlertRow key={`${a.metric}-${i}`} alert={a} />)}
        </SectionCard>

        {/* 图表区 */}
        <div className="grid gap-3 md:grid-cols-2">
          <SectionCard title="健康分趋势" desc="历史采样（约 30 点），虚线为 85 健康线与 60 警戒线">
            {hasScoreTrend ? (
              <div className="p-2 sm:p-3">
                <ChartContainer config={scoreChartConfig} className="h-56 w-full">
                  <LineChart data={trendData} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
                    <CartesianGrid vertical={false} strokeDasharray="3 3" />
                    <XAxis
                      dataKey="at"
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                      fontSize={10}
                      minTickGap={32}
                      tickFormatter={(v: number) => formatAxisTime(v)}
                    />
                    <YAxis tickLine={false} axisLine={false} width={36} fontSize={10} domain={[0, 100]} />
                    <ReferenceLine
                      y={85}
                      stroke="var(--data-series-emerald, #10b981)"
                      strokeDasharray="4 3"
                      label={{ value: "85", position: "right", fontSize: 10, fill: "var(--muted-foreground)" }}
                    />
                    <ReferenceLine
                      y={60}
                      stroke="var(--data-series-amber, #f59e0b)"
                      strokeDasharray="4 3"
                      label={{ value: "60", position: "right", fontSize: 10, fill: "var(--muted-foreground)" }}
                    />
                    <ChartTooltip
                      content={
                        <ChartTooltipContent
                          labelFormatter={(v) => formatAxisTime(Number(v))}
                          formatter={(value) => [`${value} 分`, "健康分"]}
                        />
                      }
                    />
                    <Line
                      type="monotone"
                      dataKey="score"
                      stroke="var(--color-score)"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                  </LineChart>
                </ChartContainer>
              </div>
            ) : (
              <EmptyHint text="暂无历史采样数据，趋势将在后续采样后生成。" />
            )}
          </SectionCard>

          <SectionCard title="请求量分布" desc="按时间桶统计的成功 / 失败请求数（堆叠）">
            {hasBuckets ? (
              <div className="p-2 sm:p-3">
                <ChartContainer config={bucketChartConfig} className="h-56 w-full">
                  <BarChart data={bucketData} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
                    <CartesianGrid vertical={false} strokeDasharray="3 3" />
                    <XAxis dataKey="label" tickLine={false} axisLine={false} tickMargin={8} fontSize={10} minTickGap={24} />
                    <YAxis tickLine={false} axisLine={false} width={36} fontSize={10} allowDecimals={false} />
                    <ChartTooltip
                      cursor={{ fill: "var(--muted)", opacity: 0.4 }}
                      content={<ChartTooltipContent labelFormatter={(v) => String(v)} />}
                    />
                    <Bar dataKey="ok" stackId="buckets" fill="var(--color-ok)" maxBarSize={28} isAnimationActive={false} />
                    <Bar dataKey="failed" stackId="buckets" fill="var(--color-failed)" maxBarSize={28} isAnimationActive={false} />
                  </BarChart>
                </ChartContainer>
              </div>
            ) : (
              <EmptyHint text="当前时间范围内暂无请求桶数据。" />
            )}
          </SectionCard>
        </div>

        {/* 模型分布表 */}
        <SectionCard title="模型分布" desc={`共 ${detail.models.length} 个模型`}>
          {detail.models.length === 0 ? (
            <EmptyHint text="当前时间范围内暂无模型调用数据。" />
          ) : (
            <div className="min-w-0 overflow-x-auto">
              <table className="w-full min-w-[560px] text-left text-xs">
                <thead>
                  <tr className="border-b border-border/60 text-muted-foreground">
                    <th className="px-4 py-2.5 font-medium sm:px-5">模型</th>
                    <th className="px-3 py-2.5 text-right font-medium">调用数</th>
                    <th className="px-3 py-2.5 text-right font-medium">成功率</th>
                    <th className="px-3 py-2.5 text-right font-medium">占比</th>
                    <th className="px-4 py-2.5 text-right font-medium sm:px-5">平均延迟</th>
                  </tr>
                </thead>
                <tbody>
                  {detail.models.map((row) => (
                    <tr key={row.model} className="border-b border-border/40 last:border-0">
                      <td className="max-w-[220px] truncate px-4 py-2 font-medium sm:px-5" title={row.model}>
                        {row.model}
                      </td>
                      <td className="px-3 py-2 text-right tabular-nums">{formatNumber(row.calls)}</td>
                      <td
                        className={cn(
                          "px-3 py-2 text-right tabular-nums",
                          row.successRate != null && row.successRate < 0.7 && "text-rose-600 dark:text-rose-400",
                          row.successRate != null && row.successRate >= 0.7 && row.successRate < 0.9 && "text-amber-600 dark:text-amber-400",
                        )}
                      >
                        {formatPct(row.successRate)}
                      </td>
                      <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">{formatPct(row.share)}</td>
                      <td className="px-4 py-2 text-right tabular-nums text-muted-foreground sm:px-5">{formatMs(row.avgLatencyMs)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </SectionCard>

        {/* 账号池表 */}
        <SectionCard
          title="账号池"
          desc={
            m.totalAccounts > 0
              ? `共 ${m.totalAccounts} 个账号，健康 ${m.healthyAccounts} · 冷却 ${m.coolingAccounts} · 异常 ${m.errorAccounts}`
              : undefined
          }
        >
          {detail.accounts.length === 0 ? (
            <EmptyHint text="该渠道暂无账号池数据（可能为凭证模式或未配置账号）。" />
          ) : (
            <div className="min-w-0 overflow-x-auto">
              <table className="w-full min-w-[640px] text-left text-xs">
                <thead>
                  <tr className="border-b border-border/60 text-muted-foreground">
                    <th className="px-4 py-2.5 font-medium sm:px-5">名称</th>
                    <th className="px-3 py-2.5 font-medium">状态</th>
                    <th className="px-3 py-2.5 font-medium">健康分</th>
                    <th className="px-3 py-2.5 font-medium">冷却</th>
                    <th className="px-4 py-2.5 font-medium sm:px-5">最近错误</th>
                  </tr>
                </thead>
                <tbody>
                  {detail.accounts.map((acc, i) => (
                    <tr key={`${acc.name}-${i}`} className="border-b border-border/40 last:border-0">
                      <td className="max-w-[220px] px-4 py-2 sm:px-5">
                        <span className="block truncate font-medium" title={acc.name}>
                          {acc.name}
                        </span>
                        {acc.detail ? (
                          <span className="block truncate text-[10px] text-muted-foreground" title={acc.detail}>
                            {acc.detail}
                          </span>
                        ) : null}
                      </td>
                      <td className="px-3 py-2">
                        <Badge
                          variant="outline"
                          className={cn("h-5 px-1.5 text-[10px]", accountStatusBadgeClass(acc.status, acc.enabled))}
                        >
                          {accountStatusLabel(acc.status, acc.enabled)}
                        </Badge>
                      </td>
                      <td className="px-3 py-2">
                        {acc.healthScore != null ? (
                          <HealthScoreBar score={acc.healthScore} />
                        ) : (
                          <span className="text-muted-foreground">—</span>
                        )}
                      </td>
                      <td className="px-3 py-2">
                        {acc.cooldown ? (
                          <Badge variant="outline" className="h-5 bg-amber-500/15 px-1.5 text-[10px] text-amber-700 dark:text-amber-400">
                            冷却中
                          </Badge>
                        ) : (
                          <span className="text-muted-foreground">—</span>
                        )}
                      </td>
                      <td className="max-w-[240px] px-4 py-2 text-muted-foreground sm:px-5">
                        <span className="block truncate" title={acc.lastError ?? undefined}>
                          {acc.lastError ?? "—"}
                        </span>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </SectionCard>

        {/* 最近请求日志 */}
        <SectionCard title="最近请求日志" desc={`最近 ${recentLogs.length} 条${detail.recentLogs.length > 30 ? "（共 " + detail.recentLogs.length + " 条，仅展示前 30 条）" : ""}`}>
          {recentLogs.length === 0 ? (
            <EmptyHint text="当前时间范围内暂无请求日志。" />
          ) : (
            <div className="min-w-0 overflow-x-auto">
              <table className="w-full min-w-[600px] text-left text-xs">
                <thead>
                  <tr className="border-b border-border/60 text-muted-foreground">
                    <th className="px-4 py-2.5 font-medium sm:px-5">时间</th>
                    <th className="px-3 py-2.5 font-medium">模型</th>
                    <th className="px-3 py-2.5 font-medium">结果</th>
                    <th className="px-3 py-2.5 text-right font-medium">延迟</th>
                    <th className="px-4 py-2.5 font-medium sm:px-5">错误</th>
                  </tr>
                </thead>
                <tbody>
                  {recentLogs.map((log, i) => (
                    <tr key={`${log.ts}-${i}`} className="border-b border-border/40 last:border-0">
                      <td className="whitespace-nowrap px-4 py-2 tabular-nums text-muted-foreground sm:px-5">
                        {formatAxisTime(log.ts)}
                      </td>
                      <td className="max-w-[200px] truncate px-3 py-2" title={log.model}>
                        {log.model || "—"}
                      </td>
                      <td className="px-3 py-2">
                        {log.ok ? (
                          <Badge
                            variant="outline"
                            className="h-5 bg-emerald-500/15 px-1.5 text-[10px] text-emerald-700 dark:text-emerald-400"
                          >
                            成功
                          </Badge>
                        ) : (
                          <Badge
                            variant="outline"
                            className="h-5 bg-rose-500/15 px-1.5 text-[10px] text-rose-700 dark:text-rose-400"
                          >
                            失败
                          </Badge>
                        )}
                      </td>
                      <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">{formatMs(log.latencyMs)}</td>
                      <td className="max-w-[240px] px-4 py-2 text-muted-foreground sm:px-5">
                        <span className="block truncate" title={log.error ?? undefined}>
                          {log.error ?? "—"}
                        </span>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </SectionCard>

        {/* 签到摘要卡 */}
        <SectionCard
          title="签到摘要"
          desc={`今日已签到 ${formatNumber(detail.checkin.todayCheckedIn)} / ${formatNumber(detail.checkin.accounts)}`}
          extra={
            detail.checkin.accounts > 0 ? (
              <span className="inline-flex items-center gap-1 text-[11px] text-muted-foreground">
                <CalendarCheck className="size-3.5 text-emerald-500" />
                {detail.checkin.todayCheckedIn >= detail.checkin.accounts ? "今日全部完成" : "有账号未签到"}
              </span>
            ) : undefined
          }
        >
          {detail.checkin.lastResults.length === 0 ? (
            <EmptyHint text="暂无签到记录。" />
          ) : (
            <div className="min-w-0 overflow-x-auto">
              <table className="w-full min-w-[560px] text-left text-xs">
                <thead>
                  <tr className="border-b border-border/60 text-muted-foreground">
                    <th className="px-4 py-2.5 font-medium sm:px-5">时间</th>
                    <th className="px-3 py-2.5 font-medium">账号</th>
                    <th className="px-3 py-2.5 font-medium">结果</th>
                    <th className="px-4 py-2.5 font-medium sm:px-5">错误</th>
                  </tr>
                </thead>
                <tbody>
                  {detail.checkin.lastResults.map((r, i) => (
                    <tr key={`${r.accountId}-${r.ts}-${i}`} className="border-b border-border/40 last:border-0">
                      <td className="whitespace-nowrap px-4 py-2 tabular-nums text-muted-foreground sm:px-5">
                        {formatAxisTime(r.ts)}
                      </td>
                      <td className="max-w-[200px] truncate px-3 py-2" title={r.accountId}>
                        {r.accountId}
                      </td>
                      <td className="px-3 py-2">
                        <Badge variant="outline" className={cn("h-5 px-1.5 text-[10px]", checkinResultBadgeClass(r.result))}>
                          {CHECKIN_RESULT_LABEL[r.result] ?? r.result}
                        </Badge>
                      </td>
                      <td className="max-w-[240px] px-4 py-2 text-muted-foreground sm:px-5">
                        <span className="block truncate" title={r.error ?? undefined}>
                          {r.error ?? "—"}
                        </span>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </SectionCard>
      </div>

      <p className="mt-6 flex items-center gap-1.5 text-[11px] text-muted-foreground">
        <Clock className="size-3.5" />
        统计窗口 {hours} 小时 · 评分规则与预警阈值见渠道健康契约
      </p>
    </div>
  );
}
