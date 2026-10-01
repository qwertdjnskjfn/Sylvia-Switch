import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Bot,
  CalendarCheck,
  CircleAlert,
  CircleCheck,
  Coins,
  Info,
  Loader2,
  RefreshCw,
  Send,
} from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import { useRefreshAllListener } from "@/lib/refresh-all";
import type {
  DumateCheckinStatus,
  DumateCredits,
  DumatePointsPackage,
  DumateStatus,
} from "@/lib/types";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";

// 静态 4 模型（契约 §0 模型清单；/dumate/v1 网关固定暴露这四个，不做动态目录）
const DUMATE_MODELS = [
  { id: "dumate-lite", desc: "极速档（L0）· 日常对话" },
  { id: "dumate-turbo", desc: "增强档（L1）· 内容 / 代码" },
  { id: "dumate-ultra", desc: "专业档（L2）· 深度推理" },
  { id: "dumate-points", desc: "积分查询 · 直接返回余额，不消耗积分" },
] as const;

/** 积分展示：上游为字符串数值（"16020.00"），可解析则本地化，否则原样。 */
function fmtPoints(v: string | number | null | undefined): string {
  if (v === null || v === undefined || v === "") return "—";
  const n = typeof v === "number" ? v : Number(v);
  if (Number.isFinite(n)) return n.toLocaleString("zh-CN", { maximumFractionDigits: 2 });
  return String(v);
}

/** 到期时间展示：YYYY-MM-DD 前缀直取，其余尝试本地化，失败原样。 */
function fmtExpire(v: string | null | undefined): string {
  if (!v) return "—";
  const m = /^(\d{4}-\d{2}-\d{2})/.exec(v);
  if (m) return m[1];
  const d = new Date(v);
  return Number.isNaN(d.getTime()) ? v : d.toLocaleDateString("zh-CN");
}

/** 包类型人话标签（未知值原样展示）。 */
function packageTypeLabel(p: DumatePointsPackage): string {
  switch (p.packageType) {
    case "grant_point":
      return "加油包";
    case "subscription":
      return "订阅包";
    default:
      return p.packageType || "包";
  }
}

function packageSourceLabel(p: DumatePointsPackage): string {
  switch (p.source) {
    case "login_bonus":
      return "签到奖励";
    default:
      return p.source || "";
  }
}

/** 密钥是否可用（掩码预览串含省略号/星号，不能用于鉴权）。 */
function usableKey(k: string | null | undefined): string {
  if (!k) return "";
  if (k.includes("…") || k.includes("...") || k.includes("*")) return "";
  return k.trim();
}

/** 本地日期 → YYYY-MM-DD（签到打点用）。 */
function dayKey(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

export default function DuMatePage() {
  const [status, setStatus] = useState<DumateStatus | null>(null);
  const [credits, setCredits] = useState<DumateCredits | null>(null);
  const [checkin, setCheckin] = useState<DumateCheckinStatus | null>(null);
  const [endpoint, setEndpoint] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  // 模型测试
  const [testModel, setTestModel] = useState<string>("dumate-lite");
  const [testPrompt, setTestPrompt] = useState("只回复四个字：测试成功");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<api.DumateTestChatResult | null>(null);

  const loadCore = useCallback(async () => {
    const [st, cr, ck] = await Promise.allSettled([
      api.getDumateStatus(),
      api.getDumateCredits(),
      api.getDumateCheckinStatus(),
    ]);
    if (st.status === "fulfilled") {
      setStatus(st.value);
      setCredits(st.value.credits ?? null);
    }
    if (cr.status === "fulfilled") setCredits(cr.value);
    if (ck.status === "fulfilled") setCheckin(ck.value);
    // 整体失败（服务未启动）时给出一条错误，单接口失败静默（对应卡片展示空态）
    if (st.status === "rejected" && cr.status === "rejected" && ck.status === "rejected") {
      throw st.reason instanceof Error ? st.reason : new Error(api.asError(st.reason));
    }
  }, []);

  const loadConfig = useCallback(async () => {
    try {
      const cfg = await api.getDumateConfig();
      setEndpoint(cfg?.endpoint ?? "");
    } catch {
      // 配置读取失败不阻断页面（状态/积分仍可用）
    }
  }, []);

  const loadAll = useCallback(async () => {
    await Promise.all([loadCore(), loadConfig()]);
  }, [loadCore, loadConfig]);

  useEffect(() => {
    // 初次加载静默失败（后端未就绪时呈现空态，不打扰）
    void loadAll().catch((e) => setMsg({ type: "err", text: api.asError(e) }));
    // 全局刷新按钮广播：重拉状态/积分/签到/配置
    return useRefreshAllListener(() => {
      void loadAll().catch(() => undefined);
    });
  }, [loadAll]);

  // 定时轮询：桌面端/引擎/积分可能在本页打开后变化，30s 自动同步一次（对齐妙手页轮询风格）。
  useEffect(() => {
    const t = window.setInterval(() => {
      void loadCore().catch(() => undefined);
    }, 30000);
    return () => window.clearInterval(t);
  }, [loadCore]);

  async function refresh() {
    setBusy("refresh");
    setMsg(null);
    try {
      await loadAll();
    } catch (e) {
      setMsg({ type: "err", text: `刷新失败：${api.asError(e)}` });
    } finally {
      setBusy(null);
    }
  }

  async function toggleGateway(enabled: boolean) {
    setBusy("gateway");
    try {
      await api.saveDumateConfig({ enabled });
      await loadConfig();
      toast.success(enabled ? "已启用 DuMate 网关" : "已停用 DuMate 网关");
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  async function runCheckin() {
    setBusy("checkin");
    try {
      const res = await api.dumateCheckin();
      if (res.result === "already") {
        toast.info(res.message || "今日已签到过，+500 积分已入账");
      } else if (res.ok && res.result === "success") {
        toast.success(res.message || "签到成功，+500 积分");
      } else {
        toast.error(res.message || res.error || "签到失败");
      }
      // 签到后重拉签到状态与积分（+500 到账）
      await loadCore().catch(() => undefined);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  /** 测试鉴权密钥：优先 DuMate 网关自身密钥；掩码/缺失时回退主网关密钥。 */
  async function resolveTestApiKey(): Promise<string> {
    try {
      const cfg = await api.getDumateConfig();
      const own = usableKey(cfg?.apiKey);
      if (own) return own;
    } catch {
      // 忽略，走主网关密钥兜底
    }
    try {
      const glm = await api.getGlmProxyConfig();
      return usableKey(glm?.apiKey);
    } catch {
      return "";
    }
  }

  async function runTest() {
    if (!testPrompt.trim()) return;
    setTesting(true);
    setTestResult(null);
    try {
      const apiKey = await resolveTestApiKey();
      const res = await api.dumateTestChat({
        model: testModel,
        prompt: testPrompt.trim(),
        apiKey,
      });
      setTestResult(res);
    } catch (e) {
      setTestResult({ ok: false, content: "", model: testModel, latencyMs: 0, error: api.asError(e) });
    } finally {
      setTesting(false);
    }
  }

  // 状态卡的人话指引：按故障链给第一条可操作建议（对齐契约 §4.2 卡 1）
  const statusHint = useMemo(() => {
    if (!status) return null;
    if (!status.desktopRunning)
      return "未检测到 DuMate 桌面客户端进程。请先启动 DuMate（百度搭子）并保持登录，再点「刷新」。";
    if (!status.credentialsPresent)
      return "未找到百度登录凭据（auth.json / .cookie-key）。请打开 DuMate 客户端重新登录百度账号。";
    if (!status.engineDiscovered)
      return "未发现内嵌 OpenCode 引擎（动态端口）。请确认 DuMate 客户端已完全启动（含对话功能可用）。";
    if (status.engineDiscovered && !status.engineAlive)
      return "OpenCode 引擎无响应（/global/health 探活失败）。请重启 DuMate 客户端后重试。";
    if (!status.gatewayEnabled)
      return "DuMate 网关未启用。打开右上角「启用网关」开关即可暴露 /dumate/v1 接口。";
    return null;
  }, [status]);

  // 最近 7 天签到打点（今天在最右）
  const last7Days = useMemo(() => {
    const labels = ["日", "一", "二", "三", "四", "五", "六"];
    const days: { key: string; label: string; isToday: boolean }[] = [];
    const now = new Date();
    for (let i = 6; i >= 0; i--) {
      const d = new Date(now.getFullYear(), now.getMonth(), now.getDate() - i);
      days.push({ key: dayKey(d), label: labels[d.getDay()], isToday: i === 0 });
    }
    return days;
  }, []);
  const signedDays = useMemo(
    () => new Set(checkin?.signInDays ?? checkin?.raw?.signInDays ?? []),
    [checkin],
  );

  const packages = useMemo(
    () => [
      ...(credits?.subscription ?? []).map((p) => ({ ...p, _kind: "订阅包" as const })),
      ...(credits?.incremental ?? []).map((p) => ({ ...p, _kind: "加油包" as const })),
    ],
    [credits],
  );

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-10 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-12">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">DuMate（百度搭子）</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            百度桌面 AI 办公智能体渠道：每日签到（+500 积分）、积分余额查询，以及内嵌
            OpenCode 引擎的 <code className="rounded bg-muted px-1">/dumate/v1</code> OpenAI 兼容模型网关。
            引擎由 DuMate 客户端自行启动，本页只读报告其状态。
          </p>
        </div>
        <Button
          size="sm"
          variant="outline"
          onClick={() => void refresh()}
          disabled={busy === "refresh"}
        >
          {busy === "refresh" ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新
        </Button>
      </header>

      {msg ? (
        <p
          className={cn(
            "mb-4 text-xs",
            msg.type === "ok" ? "text-primary" : "text-destructive",
          )}
        >
          {msg.text}
        </p>
      ) : null}

      {/* 卡 1 + 卡 2：通道状态 / 每日签到 */}
      <section className="grid min-w-0 gap-4 lg:grid-cols-2" aria-label="通道状态与每日签到">
        <Card className="flex min-w-0 flex-col gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardTitle className="flex items-center gap-2 text-sm font-medium">
                <Bot className="size-4 text-muted-foreground" />
                通道状态
              </CardTitle>
              {status ? (
                <Badge
                  variant={status.ok && status.gatewayEnabled ? "default" : "secondary"}
                  className="text-[10px]"
                >
                  {status.ok && status.gatewayEnabled ? "可用" : "不可用"}
                </Badge>
              ) : null}
            </div>
            <CardDescription className="mt-1 text-xs">
              桌面端进程 · 内嵌引擎 · 登录凭据 · 网关开关
            </CardDescription>
          </CardHeader>
          <CardContent className="p-0">
            <div className="divide-y divide-border/50 border-t border-border/50">
              {status === null ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                  暂无状态数据。请确认 Sylvia Switch 服务已启动，点「刷新」重试。
                </p>
              ) : (
                <>
                  {[
                    {
                      label: "桌面客户端",
                      on: status.desktopRunning,
                      okText: "运行中",
                      badText: "未运行",
                    },
                    {
                      label: "OpenCode 引擎",
                      on: status.engineDiscovered,
                      okText: status.engineUrl ?? "已发现",
                      badText: "未发现",
                    },
                    {
                      label: "引擎健康",
                      on: status.engineAlive,
                      okText: "正常",
                      badText: "无响应",
                    },
                    {
                      label: "百度登录凭据",
                      on: status.credentialsPresent,
                      okText: "已就绪",
                      badText: "缺失",
                    },
                    {
                      label: "模型网关",
                      on: status.gatewayEnabled,
                      okText: "已启用",
                      badText: "未启用",
                    },
                  ].map((row) => (
                    <div
                      key={row.label}
                      className="flex min-w-0 items-center justify-between gap-2 px-4 py-2.5 sm:px-5"
                    >
                      <span className="text-xs text-muted-foreground">{row.label}</span>
                      <span
                        className={cn(
                          "flex items-center gap-1.5 text-xs font-medium",
                          row.on ? "text-foreground" : "text-destructive",
                        )}
                      >
                        {row.on ? (
                          <CircleCheck className="size-3.5 text-emerald-600" />
                        ) : (
                          <CircleAlert className="size-3.5" />
                        )}
                        {row.on ? row.okText : row.badText}
                      </span>
                    </div>
                  ))}
                  <div className="flex min-w-0 items-center justify-between gap-2 px-4 py-2.5 sm:px-5">
                    <span className="text-xs text-muted-foreground">启用网关</span>
                    <Switch
                      checked={status.gatewayEnabled}
                      disabled={busy === "gateway"}
                      onCheckedChange={(v) => void toggleGateway(v)}
                      aria-label="启用 DuMate 网关"
                    />
                  </div>
                  <div className="flex min-w-0 flex-wrap gap-x-4 gap-y-1 px-4 py-2.5 text-[11px] text-muted-foreground sm:px-5">
                    {status.inappKeyPreview ? <span>引擎 Key {status.inappKeyPreview}</span> : null}
                    {status.apiKeyPreview ? <span>网关 Key {status.apiKeyPreview}</span> : null}
                    {endpoint ? <span className="min-w-0 truncate">{endpoint}</span> : null}
                  </div>
                </>
              )}
            </div>
          </CardContent>
          {statusHint ? (
            <div className="border-t border-border/50 px-4 py-3 sm:px-5">
              <p className="flex items-start gap-1.5 text-[11px] leading-5 text-amber-600 dark:text-amber-500">
                <Info className="mt-0.5 size-3.5 shrink-0" />
                {statusHint}
              </p>
            </div>
          ) : status?.message ? (
            <div className="border-t border-border/50 px-4 py-3 sm:px-5">
              <p className="flex items-start gap-1.5 text-[11px] leading-5 text-muted-foreground">
                <Info className="mt-0.5 size-3.5 shrink-0" />
                {status.message}
              </p>
            </div>
          ) : null}
        </Card>

        <Card className="flex min-w-0 flex-col gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardTitle className="flex items-center gap-2 text-sm font-medium">
                <CalendarCheck className="size-4 text-muted-foreground" />
                每日签到
              </CardTitle>
              {checkin ? (
                checkin.todayCheckedIn ? (
                  <Badge className="text-[10px]">
                    <CircleCheck />已领
                  </Badge>
                ) : (
                  <Badge variant="outline" className="text-[10px]">
                    未领
                  </Badge>
                )
              ) : null}
            </div>
            <CardDescription className="mt-1 text-xs">
              每日 +500 积分 · 幂等，不重复领
            </CardDescription>
          </CardHeader>
          <CardContent className="p-0">
            <div className="space-y-4 px-4 py-4 sm:px-5">
              {checkin === null ? (
                <p className="text-center text-sm text-muted-foreground">
                  暂无签到数据。请确认已登录百度账号后点「刷新」。
                </p>
              ) : !checkin.ok ? (
                <p className="text-xs text-destructive">
                  {checkin.message ||
                    (checkin.reason === "login_expired"
                      ? "百度登录态已失效，请打开 DuMate 客户端重新登录。"
                      : checkin.error || "签到状态查询失败。")}
                </p>
              ) : (
                <>
                  <div className="flex flex-wrap items-end justify-between gap-x-6 gap-y-4">
                    <div className="flex items-end gap-7">
                      <div>
                        <p className="text-[11px] leading-4 text-muted-foreground">连续天数</p>
                        <p className="text-2xl font-semibold leading-7 tabular-nums">
                          {checkin.totalTimes ?? checkin.raw?.totalTimes ?? "—"}
                        </p>
                      </div>
                      <div>
                        <p className="text-[11px] leading-4 text-muted-foreground">累计签到积分</p>
                        <p className="text-2xl font-semibold leading-7 tabular-nums">
                          {fmtPoints(checkin.totalPoints ?? checkin.raw?.totalPoints)}
                        </p>
                      </div>
                    </div>
                    <div className="min-w-0">
                      <p className="mb-1.5 text-[11px] leading-4 text-muted-foreground">最近 7 天</p>
                      <div className="flex items-center gap-1.5">
                      {last7Days.map((d) => {
                        const signed = signedDays.has(d.key);
                        return (
                          <div key={d.key} className="flex flex-col items-center gap-1">
                            <span
                              className={cn(
                                "flex size-6 items-center justify-center rounded-full text-[10px]",
                                signed
                                  ? "bg-primary font-medium text-primary-foreground"
                                  : "border border-border text-muted-foreground/70",
                                d.isToday && !signed && "border-primary/60 text-foreground",
                              )}
                              title={`${d.key}${signed ? " · 已签" : ""}`}
                            >
                              {signed ? <CircleCheck className="size-3.5" /> : ""}
                            </span>
                            <span
                              className={cn(
                                "text-[10px]",
                                d.isToday ? "font-medium text-foreground" : "text-muted-foreground",
                              )}
                            >
                              {d.label}
                            </span>
                          </div>
                        );
                      })}
                      </div>
                    </div>
                  </div>
                </>
              )}
            </div>
          </CardContent>
          <div className="mt-auto border-t border-border/50 px-4 py-3 sm:px-5">
            <Button
              size="sm"
              className="w-full"
              variant={checkin?.todayCheckedIn ? "outline" : "default"}
              onClick={() => void runCheckin()}
              disabled={busy === "checkin"}
              title={
                checkin?.todayCheckedIn ? "今日已签到（重复点击不会重复领积分）" : "立即签到（+500 积分）"
              }
            >
              {busy === "checkin" ? <Loader2 className="animate-spin" /> : <CalendarCheck />}
              {checkin?.todayCheckedIn ? "今日已签到 · 点击刷新状态" : "立即签到 +500"}
            </Button>
          </div>
        </Card>
      </section>

      {/* 卡 3：积分余额 */}
      <section className="mt-10 min-w-0 space-y-2.5" aria-label="积分余额">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">积分余额</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            来自百度 console 积分接口（quota_overview）：总额 / 已用 / 订阅包与加油包明细
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            {credits === null || credits.ok === false ? (
              <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                {credits?.error || "暂无积分数据。请确认百度登录态有效后点「刷新」。"}
              </p>
            ) : (
              <>
                <div className="flex min-w-0 flex-wrap items-end justify-between gap-4 px-4 py-5 sm:px-5">
                  <div>
                    <p className="text-[11px] text-muted-foreground">可用积分</p>
                    <p className="mt-1 text-3xl font-semibold tracking-tight tabular-nums">
                      {fmtPoints(credits.availablePoints)}
                    </p>
                    {credits.isSubscribed ? (
                      <Badge variant="secondary" className="mt-2 text-[10px]">
                        订阅生效中
                      </Badge>
                    ) : null}
                  </div>
                  <div className="flex gap-8 text-xs">
                    <div>
                      <p className="text-muted-foreground">总额</p>
                      <p className="mt-1 font-medium tabular-nums">
                        {fmtPoints(credits.totalPoints)}
                      </p>
                    </div>
                    <div>
                      <p className="text-muted-foreground">已用</p>
                      <p className="mt-1 font-medium tabular-nums">
                        {fmtPoints(credits.usedPoints)}
                      </p>
                    </div>
                  </div>
                </div>
                {credits.throttled ? (
                  <div className="border-t border-border/50 px-4 py-3 sm:px-5">
                    <p className="flex items-start gap-1.5 text-[11px] leading-5 text-amber-600 dark:text-amber-500">
                      <CircleAlert className="mt-0.5 size-3.5 shrink-0" />
                      模型限流中
                      {credits.throttleReason ? `：${credits.throttleReason}` : "。请稍后再试或降低调用频率。"}
                    </p>
                  </div>
                ) : null}
                {packages.length > 0 ? (
                  <div className="divide-y divide-border/50 border-t border-border/50">
                    <p className="px-4 pt-3 text-[11px] text-muted-foreground sm:px-5">
                      订阅包 / 加油包明细（{packages.length}）
                    </p>
                    {packages.map((p, i) => (
                      <div
                        key={`${p.packageId ?? "pkg"}-${i}`}
                        className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-2.5 sm:px-5"
                      >
                        <div className="flex min-w-0 flex-wrap items-center gap-2">
                          <Coins className="size-3.5 shrink-0 text-muted-foreground" />
                          <span className="text-xs font-medium">
                            {packageTypeLabel(p)}
                          </span>
                          {packageSourceLabel(p) ? (
                            <Badge variant="outline" className="text-[10px]">
                              {packageSourceLabel(p)}
                            </Badge>
                          ) : null}
                          {p.autoRenewStatus === "auto" ? (
                            <Badge variant="secondary" className="text-[10px]">
                              自动续费
                            </Badge>
                          ) : null}
                          {p.status ? (
                            <Badge variant="outline" className="text-[10px] text-muted-foreground">
                              {p.status}
                            </Badge>
                          ) : null}
                        </div>
                        <div className="flex shrink-0 items-center gap-4 text-[11px] text-muted-foreground">
                          <span>
                            {fmtPoints(p.usedPoints)} / {fmtPoints(p.totalPoints)}
                          </span>
                          <span>到期 {fmtExpire(p.expireDate)}</span>
                        </div>
                      </div>
                    ))}
                  </div>
                ) : null}
              </>
            )}
          </CardContent>
        </Card>
      </section>

      {/* 卡 4：模型测试 */}
      <section className="mt-10 min-w-0 space-y-2.5" aria-label="模型测试">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">模型测试</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            通过 /dumate/v1 网关直调内嵌引擎（鉴权 = 网关密钥或主网关密钥；同步返回最终回复）
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="space-y-3 p-4 sm:p-5">
            <div className="flex flex-wrap items-center gap-2">
              <Select value={testModel} onValueChange={setTestModel}>
                <SelectTrigger className="w-64">
                  <SelectValue placeholder="选择模型" />
                </SelectTrigger>
                <SelectContent>
                  {DUMATE_MODELS.map((m) => (
                    <SelectItem key={m.id} value={m.id}>
                      {m.id} · {m.desc}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <Button
                size="sm"
                onClick={() => void runTest()}
                disabled={testing || !testPrompt.trim()}
              >
                {testing ? <Loader2 className="animate-spin" /> : <Send />}发送
              </Button>
            </div>
            <Input
              placeholder="输入提示词，如：只回复四个字：测试成功"
              value={testPrompt}
              onChange={(e) => setTestPrompt(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.nativeEvent.isComposing) void runTest();
              }}
              disabled={testing}
            />
            {testResult ? (
              testResult.ok ? (
                <div className="space-y-2 rounded-lg border border-border/60 bg-muted/40 p-3">
                  <div className="flex flex-wrap items-center justify-between gap-2 text-[11px] text-muted-foreground">
                    <div className="flex items-center gap-2">
                      <Badge
                        variant="outline"
                        className="border-emerald-500/30 bg-emerald-500/10 text-[10px] text-emerald-600"
                      >
                        调用成功
                      </Badge>
                      <span className="font-medium text-foreground">{testResult.model}</span>
                    </div>
                    <div className="flex items-center gap-3">
                      <span>耗时 {testResult.latencyMs}ms</span>
                      {testResult.usage ? (
                        <span>
                          tokens {testResult.usage.prompt_tokens ?? "?"}+{testResult.usage.completion_tokens ?? "?"}={testResult.usage.total_tokens ?? "?"}
                        </span>
                      ) : null}
                    </div>
                  </div>
                  <pre className="max-h-64 overflow-auto whitespace-pre-wrap text-xs leading-5 text-foreground">
                    {testResult.content}
                  </pre>
                </div>
              ) : (
                <Alert variant="destructive" className="py-2.5">
                  <CircleAlert className="size-4" />
                  <AlertTitle className="text-xs font-medium">测试失败</AlertTitle>
                  <AlertDescription className="text-xs">
                    {testResult.error}
                    {testResult.error?.includes("401")
                      ? "（鉴权失败：请检查 DuMate 网关密钥或主网关密钥配置）"
                      : ""}
                  </AlertDescription>
                </Alert>
              )
            ) : null}
          </CardContent>
        </Card>
      </section>
    </div>
  );
}
