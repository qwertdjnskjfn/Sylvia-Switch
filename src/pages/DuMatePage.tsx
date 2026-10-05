import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Bot,
  CalendarCheck,
  CircleAlert,
  CircleCheck,
  Coins,
  Download,
  Info,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  Send,
  Trash2,
  Users,
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
  DumatePoolAccount,
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

/** 池账号登录态徽标（契约 §1.1 status：ok | expired | unknown）。 */
function PoolStatusBadge({ status }: { status?: string }) {
  if (status === "ok") {
    return (
      <Badge variant="success" className="text-[10px]">
        <CircleCheck />正常
      </Badge>
    );
  }
  if (status === "expired") {
    return (
      <Badge variant="destructive" className="text-[10px]">
        已失效
      </Badge>
    );
  }
  return (
    <Badge variant="outline" className="text-[10px] text-muted-foreground">
      未知
    </Badge>
  );
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

  // 多账号池（契约 §1：多账号仅用于签到/积分，一号一 IP；模型调用仍走本机主账号）
  const [poolAccounts, setPoolAccounts] = useState<DumatePoolAccount[]>([]);
  const [poolError, setPoolError] = useState<string | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [addName, setAddName] = useState("");
  const [addCookies, setAddCookies] = useState("");
  const [addProxy, setAddProxy] = useState("");
  const [adding, setAdding] = useState(false);
  /** 行内编辑代理：{ 账号 id, 输入框当前值 }；null = 无编辑中的行 */
  const [proxyEdit, setProxyEdit] = useState<{ id: string; value: string } | null>(null);

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

  /** 多账号池列表（失败落 poolError，行内展示，不打断其他卡片）。 */
  const loadPool = useCallback(async () => {
    try {
      const res = await api.getDumateAccounts();
      setPoolAccounts(res.accounts);
      setPoolError(null);
    } catch (e) {
      setPoolError(api.asError(e));
    }
  }, []);

  const loadAll = useCallback(async () => {
    await Promise.all([loadCore(), loadConfig(), loadPool()]);
  }, [loadCore, loadConfig, loadPool]);

  useEffect(() => {
    // 初次加载静默失败（后端未就绪时呈现空态，不打扰）
    void loadAll().catch((e) => setMsg({ type: "err", text: api.asError(e) }));
    // 全局刷新按钮广播：重拉状态/积分/签到/配置
    return useRefreshAllListener(() => {
      void loadAll().catch(() => undefined);
    });
  }, [loadAll]);

  // 定时轮询：桌面端/引擎/积分/账号池可能在本页打开后变化，30s 自动同步一次（对齐妙手页轮询风格）。
  useEffect(() => {
    const t = window.setInterval(() => {
      void Promise.all([loadCore(), loadPool()]).catch(() => undefined);
    }, 30000);
    return () => window.clearInterval(t);
  }, [loadCore, loadPool]);

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

  /** 导入本机客户端登录态为池账号（后端按 bce-user-info 去重，重复直接报错）。 */
  async function runImportClient() {
    setBusy("pool-import");
    try {
      const res = await api.importDumateClient();
      toast.success(res.message || "已导入本机登录态");
      await Promise.all([loadPool(), loadCore()]).catch(() => undefined);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  /** 添加池账号：客户端先校验 cookies JSON（数组或 {cookies:[...]}，逐条 name/value 非空）。 */
  async function runAddAccount() {
    const name = addName.trim();
    const text = addCookies.trim();
    if (!name) {
      toast.error("请填写账号名称");
      return;
    }
    if (!text) {
      toast.error("请粘贴 cookies JSON");
      return;
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      toast.error("cookies JSON 解析失败：请粘贴完整的 JSON 数组或 {\"cookies\":[...]}");
      return;
    }
    const list = Array.isArray(parsed)
      ? parsed
      : parsed && typeof parsed === "object" && Array.isArray((parsed as { cookies?: unknown }).cookies)
        ? (parsed as { cookies: unknown[] }).cookies
        : null;
    if (!list || list.length === 0) {
      toast.error('未识别到 cookie 列表：支持 JSON 数组或 {"cookies":[...]}');
      return;
    }
    const bad = list.findIndex((c) => {
      if (!c || typeof c !== "object") return true;
      const o = c as { name?: unknown; value?: unknown };
      return typeof o.name !== "string" || !o.name || typeof o.value !== "string" || !o.value;
    });
    if (bad >= 0) {
      toast.error(`第 ${bad + 1} 条 cookie 缺少非空的 name / value 字段`);
      return;
    }
    setAdding(true);
    try {
      await api.addDumateAccount({
        name,
        cookies: parsed,
        proxy: addProxy.trim(),
        note: "",
      });
      toast.success(`已添加账号「${name}」`);
      setAddName("");
      setAddCookies("");
      setAddProxy("");
      setAddOpen(false);
      await loadPool();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setAdding(false);
    }
  }

  /** 池账号/本机账号单账号签到（dumate:local 与 dumate:pool_* 都走 /api/dumate/account/checkin）。 */
  async function runPoolCheckin(acc: DumatePoolAccount) {
    setBusy(`pool-checkin:${acc.id}`);
    try {
      const res = await api.dumateAccountCheckin(acc.id);
      if (res.result === "already") {
        toast.info(res.message || `「${acc.name}」今日已签到`);
      } else if (res.result === "success") {
        toast.success(
          res.message || `「${acc.name}」签到成功${res.points != null ? `，+${res.points} 积分` : ""}`,
        );
      } else {
        toast.error(res.message || res.error || `「${acc.name}」签到失败`);
      }
      // 签到后刷新账号列表（lastCheckinDay/状态/积分缓存）与本机积分卡
      await Promise.all([loadPool(), loadCore()]).catch(() => undefined);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  /** 保存行内编辑的出站代理（空串 = 直连；仅池账号）。 */
  async function runSaveProxy(acc: DumatePoolAccount) {
    const proxy = (proxyEdit?.id === acc.id ? proxyEdit.value : acc.proxy ?? "").trim();
    setBusy(`pool-proxy:${acc.id}`);
    try {
      await api.updateDumateAccount({ id: acc.id, proxy });
      toast.success(proxy ? `「${acc.name}」代理已更新` : `「${acc.name}」已清除代理（直连）`);
      setProxyEdit(null);
      await loadPool();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  /** 删除池账号（本机账号不可删，按钮已隐藏）。 */
  async function runRemoveAccount(acc: DumatePoolAccount) {
    if (!window.confirm(`确定删除账号「${acc.name}」？删除后不可恢复。`)) return;
    setBusy(`pool-remove:${acc.id}`);
    try {
      await api.removeDumateAccount(acc.id);
      toast.success(`已删除「${acc.name}」`);
      if (proxyEdit?.id === acc.id) setProxyEdit(null);
      await loadPool();
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
            百度桌面 AI 办公智能体渠道：每日签到（+500 积分）、多账号签到（一号一 IP）、积分余额查询，
            以及内嵌
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

      {/* 卡 3.5：多账号池（契约 unified-accounts-contract §1/§4.3；多账号仅用于签到/积分） */}
      <section className="mt-10 min-w-0 space-y-2.5" aria-label="多账号">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">多账号</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            多账号用于每日签到与积分查询（一号一独立出站 IP，降低同 IP 关联风控）；
            模型调用引擎绑定 DuMate 桌面端当前登录用户，仍走本机主账号
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          {/* 工具条：导入本机登录态 / 添加账号 */}
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
            <div className="flex min-w-0 items-center gap-2">
              <Users className="size-4 shrink-0 text-muted-foreground" />
              <span className="text-sm font-medium">账号池（{poolAccounts.length}）</span>
              {poolError ? (
                <Badge variant="destructive" className="text-[10px]">
                  加载失败
                </Badge>
              ) : null}
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <Button
                size="sm"
                variant="outline"
                onClick={() => void runImportClient()}
                disabled={busy === "pool-import"}
                title="把本机 DuMate 客户端当前登录的百度账号导入账号池（与既有账号同一登录态时会提示）"
              >
                {busy === "pool-import" ? <Loader2 className="animate-spin" /> : <Download />}
                导入本机登录态
              </Button>
              <Button
                size="sm"
                variant={addOpen ? "secondary" : "outline"}
                onClick={() => setAddOpen((v) => !v)}
              >
                <Plus />
                {addOpen ? "收起" : "添加账号"}
              </Button>
            </div>
          </div>

          {/* 添加账号（折叠表单：名称 / cookies JSON / 代理 URL） */}
          {addOpen ? (
            <div className="space-y-3 border-t border-border/50 bg-muted/30 px-4 py-4 sm:px-5">
              <div className="grid gap-3 sm:grid-cols-2">
                <div>
                  <label className="text-xs font-medium text-muted-foreground">账号名称</label>
                  <Input
                    className="mt-1"
                    placeholder="如：二号机"
                    value={addName}
                    onChange={(e) => setAddName(e.target.value)}
                  />
                </div>
                <div>
                  <label className="text-xs font-medium text-muted-foreground">
                    出站代理 URL（一号一 IP，可选）
                  </label>
                  <Input
                    className="mt-1"
                    placeholder="http:// 或 socks5://，留空 = 直连"
                    value={addProxy}
                    onChange={(e) => setAddProxy(e.target.value)}
                  />
                </div>
              </div>
              <div>
                <label className="text-xs font-medium text-muted-foreground">
                  Cookies JSON（浏览器 DevTools 导出的百度登录 cookie）
                </label>
                <textarea
                  rows={4}
                  placeholder='[{"name":"...","value":"...","domain":"..."},...] 或 {"cookies":[...]}'
                  value={addCookies}
                  onChange={(e) => setAddCookies(e.target.value)}
                  className="mt-1 block w-full rounded-md border border-input bg-background p-2.5 font-mono text-xs shadow-sm focus:border-primary focus:outline-none focus:ring-1 focus:ring-primary"
                />
              </div>
              <div className="flex items-center gap-2">
                <Button size="sm" onClick={() => void runAddAccount()} disabled={adding}>
                  {adding ? <Loader2 className="animate-spin" /> : <Plus />}保存账号
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setAddOpen(false)}>
                  取消
                </Button>
              </div>
            </div>
          ) : null}

          {/* 账号列表：name / 状态 / 最近签到日 / cookie 数 / 代理 + 行内操作 */}
          <div className="divide-y divide-border/50 border-t border-border/50">
            {poolAccounts.length === 0 ? (
              <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                {poolError
                  ? `账号池加载失败：${poolError}`
                  : "暂无账号。点「导入本机登录态」或「添加账号」开始（多账号仅用于签到/积分）。"}
              </p>
            ) : (
              poolAccounts.map((acc) => {
                const checkingIn = busy === `pool-checkin:${acc.id}`;
                const savingProxy = busy === `pool-proxy:${acc.id}`;
                const removing = busy === `pool-remove:${acc.id}`;
                return (
                  <div
                    key={acc.id}
                    className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-2 px-4 py-2.5 sm:px-5"
                  >
                    <div className="flex min-w-0 items-center gap-2">
                      <span className="max-w-[180px] truncate text-xs font-medium">
                        {acc.name || acc.id}
                      </span>
                      {acc.isLocal ? (
                        <Badge variant="secondary" className="text-[10px]">
                          本机
                        </Badge>
                      ) : null}
                      <PoolStatusBadge status={acc.status} />
                    </div>
                    <div className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1 text-[11px] text-muted-foreground">
                      <span>最近签到 {acc.lastCheckinDay || "—"}</span>
                      <span>cookies {acc.cookieCount ?? 0}</span>
                      <span className="max-w-[240px] truncate">
                        {acc.proxy ? `代理 ${acc.proxy}` : "直连"}
                      </span>
                      {acc.credits?.availablePoints != null ? (
                        <span>可用 {fmtPoints(acc.credits.availablePoints)}</span>
                      ) : null}
                    </div>
                    <div className="ml-auto flex shrink-0 items-center gap-1.5">
                      {proxyEdit?.id === acc.id ? (
                        <>
                          <Input
                            className="h-7 w-48 text-xs"
                            placeholder="http:// 或 socks5://，留空直连"
                            value={proxyEdit.value}
                            onChange={(e) => setProxyEdit({ id: acc.id, value: e.target.value })}
                            onKeyDown={(e) => {
                              if (e.key === "Enter" && !e.nativeEvent.isComposing)
                                void runSaveProxy(acc);
                            }}
                          />
                          <Button
                            size="sm"
                            className="h-7 px-2 text-xs"
                            onClick={() => void runSaveProxy(acc)}
                            disabled={savingProxy}
                          >
                            {savingProxy ? <Loader2 className="animate-spin" /> : null}保存
                          </Button>
                          <Button
                            size="sm"
                            variant="ghost"
                            className="h-7 px-2 text-xs"
                            onClick={() => setProxyEdit(null)}
                          >
                            取消
                          </Button>
                        </>
                      ) : (
                        <>
                          <Button
                            size="sm"
                            variant="outline"
                            className="h-7 px-2 text-xs"
                            onClick={() => void runPoolCheckin(acc)}
                            disabled={checkingIn}
                            title={
                              acc.isLocal
                                ? "本机账号签到（与「每日签到」卡同款幂等逻辑）"
                                : "以该账号的 cookies 与代理签到（幂等，今日已领不重复加积分）"
                            }
                          >
                            {checkingIn ? <Loader2 className="animate-spin" /> : <CalendarCheck />}
                            签到
                          </Button>
                          {!acc.isLocal ? (
                            <>
                              <Button
                                size="sm"
                                variant="ghost"
                                className="h-7 px-2 text-xs"
                                onClick={() => setProxyEdit({ id: acc.id, value: acc.proxy ?? "" })}
                              >
                                <Pencil />
                                编辑代理
                              </Button>
                              <Button
                                size="sm"
                                variant="ghost"
                                className="h-7 px-2 text-xs text-destructive hover:text-destructive"
                                onClick={() => void runRemoveAccount(acc)}
                                disabled={removing}
                              >
                                {removing ? <Loader2 className="animate-spin" /> : <Trash2 />}
                                删除
                              </Button>
                            </>
                          ) : null}
                        </>
                      )}
                    </div>
                  </div>
                );
              })
            )}
          </div>

          <div className="border-t border-border/50 px-4 py-3 sm:px-5">
            <p className="flex items-start gap-1.5 text-[11px] leading-5 text-muted-foreground">
              <Info className="mt-0.5 size-3.5 shrink-0" />
              多账号仅用于签到与积分查询（每个账号可配独立出站代理，实现一号一 IP）；模型调用不支持多账号——
              内嵌引擎绑定 DuMate 桌面端当前登录用户，仍走本机主账号。
            </p>
          </div>
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
