import { useCallback, useEffect, useMemo, useState } from "react";
import {
  ArrowLeftRight,
  Bot,
  CircleAlert,
  CircleCheck,
  Gift,
  Globe,
  Coins,
  Info,
  Loader2,
  Network,
  RefreshCw,
  Trash2,
  UserPlus,
  Users,
} from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import { useRefreshAllListener } from "@/lib/refresh-all";
import type {
  ZcodeAccount,
  ZcodeIdentity,
  ZcodeQuota,
  ZcodeStatus,
} from "@/lib/types";

/** 额度展示：上游可为字符串数值，可解析则本地化，否则原样。 */
function fmtQuota(v: string | number | null | undefined): string {
  if (v === null || v === undefined || v === "") return "—";
  const n = typeof v === "number" ? v : Number(v);
  if (Number.isFinite(n)) return n.toLocaleString("zh-CN", { maximumFractionDigits: 2 });
  return String(v);
}

/** 字符串/数值 → f64（容错：去千分位逗号）。 */
function numOf(v: string | number | null | undefined): number | null {
  if (v === null || v === undefined || v === "") return null;
  const n = typeof v === "number" ? v : Number(String(v).replace(/,/g, ""));
  return Number.isFinite(n) ? n : null;
}

/** 剩余百分比（remaining/total），total 缺失/为 0 → 100%（视为满额）。 */
function remainPct(item: { total?: unknown; remaining?: unknown; percentUsed?: unknown }): number {
  const pu = numOf(item.percentUsed as string | number | null);
  const rem = numOf(item.remaining as string | number | null);
  const tot = numOf(item.total as string | number | null);
  if (pu !== null) return Math.max(0, Math.min(100, pu));
  if (rem !== null && tot !== null && tot > 0) return Math.max(0, Math.min(100, (rem / tot) * 100));
  if (rem !== null && tot !== null && tot === 0) return 100;
  return 100;
}

/** 到期/周期时间展示：毫秒/秒级 epoch → 「10月7日 23:59」；字符串原样。 */
function fmtPeriod(v: unknown): string {
  if (v === null || v === undefined || v === "") return "";
  const n = typeof v === "number" ? v : Number(v);
  if (Number.isFinite(n) && n > 0) {
    const ms = n < 1e12 ? n * 1000 : n;
    const d = new Date(ms);
    if (!Number.isNaN(d.getTime())) {
      return `${d.getMonth() + 1}月${d.getDate()}日 ${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
    }
  }
  return String(v);
}

/** 时间展示：毫秒/秒级 epoch 兜底秒 ×1000，解析失败原样「—」。 */
function fmtDay(v: number | string | null | undefined): string {
  if (v === null || v === undefined || v === "") return "—";
  const n = typeof v === "number" ? v : Number(v);
  if (Number.isFinite(n) && n > 0) {
    const d = new Date(n < 1e12 ? n * 1000 : n);
    return Number.isNaN(d.getTime()) ? "—" : d.toLocaleDateString("zh-CN");
  }
  const d = new Date(String(v));
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleDateString("zh-CN");
}

/** 身份展示名（优先级 displayName > username > email > userId，契约 §0 身份解析）。 */
function identityLabel(id: ZcodeIdentity | null | undefined): string {
  if (!id) return "—";
  return id.displayName || id.username || id.email || id.userId || "未知身份";
}

/** 身份一行文本：展示名 + 邮箱（池列表行内用）。 */
function identityText(id: ZcodeIdentity | null | undefined): string {
  if (!id) return "身份未知";
  const name = identityLabel(id);
  return id.email && id.email !== name ? `${name} · ${id.email}` : name;
}

/** 池账号登录态：identity 缺失 = 未知；JWT 过期 = 已过期；否则正常。 */
function accountLoginState(acc: ZcodeAccount): "ok" | "expired" | "unknown" {
  if (!acc.identity) return "unknown";
  return acc.identity.jwtExpired ? "expired" : "ok";
}

/** 额度摘要：取第一个有 available 值的通道（契约 §3.1「available 大数字」）。 */
function quotaAvailable(q: ZcodeQuota | null | undefined): string | number | null {
  for (const ch of q?.channels ?? []) {
    if (ch.available !== null && ch.available !== undefined && ch.available !== "") {
      return ch.available;
    }
  }
  return null;
}

/** 登录态徽标（对齐 DuMatePage 的 PoolStatusBadge 三态）。 */
function LoginStateBadge({ state }: { state: "ok" | "expired" | "unknown" }) {
  if (state === "ok") {
    return (
      <Badge variant="success" className="text-[10px]">
        <CircleCheck />正常
      </Badge>
    );
  }
  if (state === "expired") {
    return (
      <Badge variant="destructive" className="text-[10px]">
        已过期
      </Badge>
    );
  }
  return (
    <Badge variant="outline" className="text-[10px] text-muted-foreground">
      未知
    </Badge>
  );
}

export default function ZcodePage() {
  const [status, setStatus] = useState<ZcodeStatus | null>(null);
  const [liveQuota, setLiveQuota] = useState<ZcodeQuota | null>(null);
  const [quotaLoading, setQuotaLoading] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  // 捕获当前账号：名称可填（缺省后端用身份名）
  const [captureName, setCaptureName] = useState("");

  // 切换确认框（Tauri WebView 不支持 window.confirm，用 Dialog，先例 AccountsPage/ModelTestPage）
  const [switchTarget, setSwitchTarget] = useState<ZcodeAccount | null>(null);

  // 行内「额度刷新」结果覆盖池缓存摘要（key = 账号 id）
  const [quotaBulkBusy, setQuotaBulkBusy] = useState(false);
  const [claimScan, setClaimScan] = useState<Record<string, unknown> | null>(null);
  const [claimBusy, setClaimBusy] = useState(false);
  const [claimAutoBusyId, setClaimAutoBusyId] = useState<string | null>(null);
  const [autoClaim, setAutoClaim] = useState(false);
  const [zcodeProxyEdit, setZcodeProxyEdit] = useState<{ id: string; value: string } | null>(null);
  // 1.5.2 额度耗尽自动切换
  const [asEnabled, setAsEnabled] = useState<boolean | null>(null);
  const [asThreshold, setAsThreshold] = useState("0");
  const [asBusy, setAsBusy] = useState(false);
  // 2026-10-14 反代网关（融合 Z-Accounts Gateway Edition 池算法）
  const [gw, setGw] = useState<Record<string, unknown> | null>(null);
  const [gwBusy, setGwBusy] = useState(false);
  const [gwMaxInFlight, setGwMaxInFlight] = useState("2");
  const [gwModels, setGwModels] = useState("");
  const [gwApiKey, setGwApiKey] = useState("");

  const accounts = useMemo(() => status?.accounts ?? [], [status]);
  const desktopRunning = status?.desktopRunning ?? false;

  const loadStatus = useCallback(async () => {
    const st = await api.getZcodeStatus();
    setStatus(st);
    if (st.autoSwitch?.config) {
      setAsEnabled(st.autoSwitch.config.enabled);
      setAsThreshold(String(st.autoSwitch.config.thresholdTokens ?? 0));
    }
  }, []);

  /** live 实时额度（独立于状态加载：上游网络慢，失败不阻断页面）。 */
  const loadLiveQuota = useCallback(async () => {
    setQuotaLoading(true);
    try {
      setLiveQuota(await api.zcodeQuota());
    } catch (e) {
      setLiveQuota({ ok: false, error: api.asError(e) });
    } finally {
      setQuotaLoading(false);
    }
  }, []);

  const loadAll = useCallback(async () => {
    await loadStatus();
  }, [loadStatus]);

  /** 活动领取：全量扫描可领套餐（live+池账号，只读）。 */
  const loadClaimScan = useCallback(async (showBusy = false) => {
    if (showBusy) setClaimBusy(true);
    try {
      setClaimScan(await api.zcodeClaimScan());
    } catch (e) {
      toast.error("活动扫描失败", { description: api.asError(e) });
    } finally {
      if (showBusy) setClaimBusy(false);
    }
  }, []);

  /** 保存池账号出站代理（一号一 IP，覆盖 IP 维度限领/风控）。 */
  async function saveZcodeProxy(acc: ZcodeAccount, proxy: string) {
    setBusy(`zproxy:${acc.id}`);
    try {
      await api.updateZcodeAccount({ id: acc.id, proxy });
      toast.success(proxy ? `「${acc.name || acc.id}」代理已更新` : `「${acc.name || acc.id}」已改为直连`);
      setZcodeProxyEdit(null);
      await loadStatus();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  /** 读取自动领取开关。 */
  useEffect(() => {
    api.getZcodeClaimState().then((s) => setAutoClaim(Boolean(s.autoClaim))).catch(() => {});
  }, []);

  /** 切换自动领取开关（持久化，定时循环读取）。 */
  async function saveAutoClaim(v: boolean) {
    setAutoClaim(v);
    try {
      await api.saveZcodeClaimState({ autoClaim: v });
      toast.success(v ? "已开启自动领取（每小时扫描并自动领取）" : "已关闭自动领取（仅扫描+通知）");
    } catch (e) {
      setAutoClaim(!v);
      toast.error("保存失败", { description: api.asError(e) });
    }
  }

  /** 单账号全自动领取（CDP 无痕验证码；ZCode 没开会自动拉起）。 */
  async function runClaimAuto(id: string, name: string) {
    setClaimAutoBusyId(id);
    try {
      const res = await api.zcodeClaimAuto(id);
      const claimed = Number(res.claimed ?? 0);
      const details = Array.isArray(res.details) ? (res.details as string[]).join("；") : "";
      if (claimed > 0) toast.success(`「${name}」领取成功`, { description: details || `${claimed} 个套餐已到账` });
      else toast.error(`「${name}」未领取`, { description: String(res.lastError || res.message || details || "未知原因") });
      await Promise.all([loadClaimScan(), loadAll()]);
    } catch (e) {
      toast.error(`「${name}」自动领取失败`, { description: api.asError(e) });
    } finally {
      setClaimAutoBusyId(null);
    }
  }

  useEffect(() => {
    // 初次加载静默失败（后端未就绪呈现空态，不打扰）；额度单独拉（失败在卡内展示）
    void loadAll().catch((e) => setMsg({ type: "err", text: api.asError(e) }));
    void loadClaimScan();
    void loadLiveQuota();
    // 全局刷新按钮广播：重拉状态 + live 额度
    return useRefreshAllListener(() => {
      void loadAll().catch(() => undefined);
      void loadLiveQuota();
    });
  }, [loadAll, loadLiveQuota]);

  // 定时轮询：只轮本地状态（登录态/进程/池 + 额度缓存回读）。
  // 额度的上游查询不轮询——由手动/批量/领取后触发并落缓存。
  useEffect(() => {
    const t = window.setInterval(() => {
      void loadStatus().catch(() => undefined);
    }, 30000);
    return () => window.clearInterval(t);
  }, [loadStatus]);

  /** 反代网关状态（本地读取，随页面加载与操作后刷新）。 */
  const loadGateway = useCallback(async () => {
    try {
      const st = (await api.getZcodeGatewayStatus()) as Record<string, unknown>;
      setGw(st);
      setGwMaxInFlight(String(st.maxInFlight ?? 2));
      setGwModels(Array.isArray(st.models) ? (st.models as string[]).join(", ") : "");
    } catch {
      // 网关状态读取失败不阻断页面（旧后端无此端点时保持卡片空态）
    }
  }, []);

  useEffect(() => {
    void loadGateway();
  }, [loadGateway]);

  /** 保存网关配置（开关 / 并发上限 / 放行模型 / 访问密钥）。 */
  const saveGateway = useCallback(
    async (patch: Record<string, unknown>, label: string) => {
      setGwBusy(true);
      try {
        const res = (await api.saveZcodeGatewayConfig(patch)) as Record<string, unknown>;
        setGw((res.config as Record<string, unknown>) ?? null);
        toast.success(`网关${label}已保存`);
      } catch (e) {
        toast.error(api.asError(e));
      } finally {
        setGwBusy(false);
      }
    },
    [],
  );

  // 页面聚焦/切回本页：重拉状态（含额度缓存）——跨路由返回即见最新缓存。
  useEffect(() => {
    const onFocus = () => {
      if (document.visibilityState === "visible") {
        void loadStatus().catch(() => undefined);
      }
    };
    document.addEventListener("visibilitychange", onFocus);
    window.addEventListener("focus", onFocus);
    return () => {
      document.removeEventListener("visibilitychange", onFocus);
      window.removeEventListener("focus", onFocus);
    };
  }, [loadStatus]);

  async function refresh() {
    setBusy("refresh");
    setMsg(null);
    try {
      await loadAll();
      await loadLiveQuota();
    } catch (e) {
      setMsg({ type: "err", text: `刷新失败：${api.asError(e)}` });
    } finally {
      setBusy(null);
    }
  }

  /** 捕获当前登录态入池（hash 去重：同指纹后端只更新名字，重复捕获幂等）。 */
  async function runCapture() {
    setBusy("capture");
    try {
      const res = await api.captureZcode(captureName.trim() || undefined);
      toast.success(
        res.message ||
          `已捕获当前登录账号${res.account?.name ? `「${res.account.name}」` : ""}入池`,
      );
      setCaptureName("");
      await loadStatus();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  /** 确认切换：ZCode 运行中携 force=true（确认框已说明会关闭 ZCode，契约 §3.2）。 */
  async function runSwitch() {
    const target = switchTarget;
    if (!target) return;
    setBusy(`switch:${target.id}`);
    try {
      let res: {
        ok?: boolean;
        message?: string;
        error?: string;
        hot?: boolean;
        launched?: boolean;
        launchError?: string;
      };
      if (desktopRunning) {
        // 热切换优先（不关 ZCode，无感换号）；失败回退冷切换需用户再确认，这里先报错给选择。
        res = await api.switchZcode({ id: target.id, hot: true, restart: false });
      } else {
        // 冷切换：后端写完凭据后自动拉起 ZCode（全链路径解析），拉起失败时返回 launchError。
        res = await api.switchZcode({ id: target.id, restart: true });
      }
      setSwitchTarget(null);
      const label = target.name || target.id;
      if (res.hot) {
        toast.success(
          res.message || `已无感热切换到「${label}」（ZCode 未重启，下一次鉴权生效）`,
        );
      } else if (res.launched) {
        toast.success(
          res.message || `已切换到「${label}」并自动拉起 ZCode（以该账号登录）`,
        );
      } else if (res.launchError) {
        // 凭据已写入但拉起失败：给出具体原因 + 降级指引（手动启动即以该账号登录）。
        toast.error(
          `凭据已切换到「${label}」，但自动拉起 ZCode 失败：${res.launchError}`,
        );
      } else {
        toast.success(res.message || `已切换到「${label}」；打开 ZCode 即以新账号登录`);
      }
      // 切换后 live 身份/额度全变：状态与 live 额度都重拉
      await loadStatus();
      await loadLiveQuota();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  /** 行内实时额度（结果覆盖池缓存摘要；失败 toast，不动缓存）。 */
  async function runRowQuota(acc: ZcodeAccount) {
    setBusy(`quota:${acc.id}`);
    try {
      const q = await api.zcodeQuota(acc.id);
      if (!q.ok) toast.error(q.error || `「${acc.name || acc.id}」额度查询失败`);
      // 端点已落后端缓存：重拉状态让全表（含其他挂载点）同步最新额度。
      await loadStatus();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  /** 批量刷新池内全部账号额度（串行逐个查询并落缓存，避免并发打爆上游）。 */
  async function runQuotaBulk() {
    if (quotaBulkBusy) return;
    setQuotaBulkBusy(true);
    try {
      for (const acc of accounts) {
        if (accountLoginState(acc) === "expired") continue;
        try {
          await api.zcodeQuota(acc.id);
        } catch {
          // 单账号失败不阻断其他账号
        }
      }
      await loadStatus();
      toast.success("全部账号额度已刷新");
    } finally {
      setQuotaBulkBusy(false);
    }
  }

  /** 缓存时效：>10 分钟视为过期（视觉提示），>60 分钟自动视为失效需重查。 */
  function quotaStale(fetchedAt: number | string | null | undefined): boolean {
    if (fetchedAt == null) return false;
    const ts = typeof fetchedAt === "number" ? fetchedAt : Number(fetchedAt);
    if (!Number.isFinite(ts)) return false;
    return Date.now() - ts > 10 * 60 * 1000;
  }

  function fmtAgo(fetchedAt: number | string | null | undefined): string {
    if (fetchedAt == null) return "";
    const ts = typeof fetchedAt === "number" ? fetchedAt : Number(fetchedAt);
    if (!Number.isFinite(ts)) return "";
    const diff = Math.max(0, Date.now() - ts);
    if (diff < 60_000) return "刚刚";
    if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
    if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
    return `${Math.floor(diff / 86_400_000)} 天前`;
  }

  /** 删除池账号（只移出池，不动 live 登录态文件）。 */
  async function runRemove(acc: ZcodeAccount) {
    if (!window.confirm(`确定删除账号「${acc.name || acc.id}」？仅从账号池移除，删除后不可恢复。`)) {
      return;
    }
    setBusy(`remove:${acc.id}`);
    try {
      await api.removeZcode(acc.id);
      toast.success(`已删除「${acc.name || acc.id}」`);
      await loadStatus();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  const identity = status?.identity ?? null;
  const loggedIn = status?.loggedIn ?? false;
  const liveChannels = liveQuota?.channels ?? [];

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-10 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-12">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">ZCode 账号</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            ZCode（智谱编程客户端）的登录身份管理：捕获当前登录态入池、一键冷切换账号、
            每账号独立设备遥测身份与实时额度查询。
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

      {/* 卡 1 + 卡 3：当前登录 / 说明文案 */}
      <section className="grid min-w-0 gap-4 lg:grid-cols-2" aria-label="当前登录与说明">
        <Card className="flex min-w-0 flex-col gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardTitle className="flex items-center gap-2 text-sm font-medium">
                <Bot className="size-4 text-muted-foreground" />
                当前登录
              </CardTitle>
              {status ? (
                loggedIn ? (
                  <Badge variant="success" className="text-[10px]">
                    <CircleCheck />已登录
                  </Badge>
                ) : status.liveExists ? (
                  <Badge variant="destructive" className="text-[10px]">
                    未登录
                  </Badge>
                ) : (
                  <Badge variant="outline" className="text-[10px] text-muted-foreground">
                    无登录态
                  </Badge>
                )
              ) : null}
            </div>
            <CardDescription className="mt-1 text-xs">
              ~/.zcode/v2/credentials.json 的实时登录身份与额度
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
                      label: "登录态文件",
                      on: status.liveExists,
                      okText: "已找到",
                      badText: "未找到",
                    },
                    {
                      label: "ZCode 桌面端",
                      on: desktopRunning,
                      okText: "运行中",
                      badText: "未运行",
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
                  {/* 登录身份：provider 徽标 + 展示名 + 邮箱 + JWT 过期徽标（契约 §3.1） */}
                  <div className="flex min-w-0 items-center justify-between gap-2 px-4 py-2.5 sm:px-5">
                    <span className="shrink-0 text-xs text-muted-foreground">登录身份</span>
                    {loggedIn && identity ? (
                      <span className="flex min-w-0 flex-wrap items-center justify-end gap-1.5">
                        {identity.provider ? (
                          <Badge variant="outline" className="text-[10px]">
                            {identity.provider}
                          </Badge>
                        ) : null}
                        <span className="min-w-0 truncate text-xs font-medium">
                          {identityLabel(identity)}
                        </span>
                        {identity.email ? (
                          <span className="min-w-0 truncate text-[11px] text-muted-foreground">
                            {identity.email}
                          </span>
                        ) : null}
                        {identity.jwtExpired === true ? (
                          <Badge variant="destructive" className="text-[10px]">
                            JWT 已过期
                          </Badge>
                        ) : (
                          <Badge variant="success" className="text-[10px]">
                            JWT 有效
                          </Badge>
                        )}
                      </span>
                    ) : (
                      <span className="text-xs text-muted-foreground">—</span>
                    )}
                  </div>
                </>
              )}
            </div>
          </CardContent>
          {/* 额度：available 大数字 + 通道明细（仅已登录时查询展示，契约 §3.1） */}
          {status !== null && loggedIn ? (
            <div className="border-t border-border/50 px-4 py-3 sm:px-5">
              <div className="flex items-center justify-between gap-2">
                <p className="text-[11px] text-muted-foreground">可用额度</p>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-7 gap-1 px-2 text-xs"
                  onClick={() => void loadLiveQuota()}
                  disabled={quotaLoading}
                  title="实时查询当前登录账号额度（Monitor / ZaiBilling 通道）"
                >
                  <RefreshCw className={cn("size-3.5", quotaLoading && "animate-spin")} />
                  刷新额度
                </Button>
              </div>
              <p className="mt-0.5 text-3xl font-semibold tracking-tight tabular-nums">
                {quotaLoading && liveQuota === null ? (
                  <span className="flex items-center gap-2 text-base font-normal text-muted-foreground">
                    <Loader2 className="size-4 animate-spin" />
                    正在查询额度…
                  </span>
                ) : (
                  fmtQuota(quotaAvailable(liveQuota))
                )}
              </p>
              {liveChannels.length > 0 ? (
                <div className="mt-2 flex min-w-0 flex-wrap gap-x-4 gap-y-1">
                  {liveChannels.map((ch, i) => (
                    <span key={`${ch.name}-${i}`} className="flex items-center gap-1.5 text-[11px]">
                      <Coins className="size-3 shrink-0 text-muted-foreground" />
                      <span className="text-muted-foreground">{ch.name}</span>
                      {ch.error ? (
                        <span className="text-destructive">{ch.error}</span>
                      ) : (
                        <span className="font-medium tabular-nums text-foreground">
                          {fmtQuota(ch.available)}
                        </span>
                      )}
                    </span>
                  ))}
                </div>
              ) : liveQuota && !liveQuota.ok ? (
                <p className="mt-2 flex items-start gap-1.5 text-[11px] leading-5 text-destructive">
                  <CircleAlert className="mt-0.5 size-3.5 shrink-0" />
                  {liveQuota.error || "额度查询失败，请稍后重试。"}
                </p>
              ) : null}
            </div>
          ) : null}
          {/* 捕获当前账号（v1 加号方式：ZCode 里登录新号 → 回本页捕获，契约 §6） */}
          <div className="mt-auto border-t border-border/50 px-4 py-3 sm:px-5">
            <div className="flex items-center gap-2">
              <Input
                className="h-8 min-w-0 flex-1 text-xs"
                placeholder="账号名称（可选，默认用登录身份命名）"
                value={captureName}
                onChange={(e) => setCaptureName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.nativeEvent.isComposing) void runCapture();
                }}
              />
              {loggedIn ? (
                <Button
                  size="sm"
                  className="h-8 shrink-0"
                  onClick={() => void runCapture()}
                  disabled={busy === "capture"}
                  title="把当前 ZCode 登录态捕获入账号池（同一登录态重复捕获只会更新名字）"
                >
                  {busy === "capture" ? <Loader2 className="animate-spin" /> : <UserPlus />}
                  捕获当前账号
                </Button>
              ) : (
                <span className="inline-flex" title="未检测到登录态：请先打开 ZCode 登录账号">
                  <Button size="sm" className="h-8 shrink-0" disabled>
                    <UserPlus />
                    捕获当前账号
                  </Button>
                </span>
              )}
            </div>
            {!status ? null : !status.liveExists ? (
              <p className="mt-2 flex items-start gap-1.5 text-[11px] leading-5 text-amber-600 dark:text-amber-500">
                <Info className="mt-0.5 size-3.5 shrink-0" />
                未找到 ZCode 登录态文件（~/.zcode/v2/credentials.json）。请先打开 ZCode 并登录账号。
              </p>
            ) : !loggedIn ? (
              <p className="mt-2 flex items-start gap-1.5 text-[11px] leading-5 text-amber-600 dark:text-amber-500">
                <Info className="mt-0.5 size-3.5 shrink-0" />
                登录态文件存在但解析不出有效身份。请打开 ZCode 重新登录账号。
              </p>
            ) : null}
          </div>
        </Card>

        {/* 卡 2.5：额度耗尽自动切换（1.5.2 多账号自动管理） */}
        <Card className="flex min-w-0 flex-col gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <CardTitle className="flex items-center gap-2 text-sm font-medium">
              <RefreshCw className="size-4 text-muted-foreground" />
              额度耗尽自动切换
            </CardTitle>
            <CardDescription className="mt-1 text-xs">
              后台每 5 分钟检查当前账号剩余额度；低于阈值时自动热切换到还有余额的池账号（复用已存凭据，无需重新登录，会话不中断）；全部耗尽时桌面通知提示补充
            </CardDescription>
          </CardHeader>
          <CardContent className="p-0">
            <div className="flex flex-wrap items-center gap-3 px-4 py-3 sm:px-5">
              <Switch
                checked={asEnabled ?? false}
                onCheckedChange={(checked) => {
                  setAsBusy(true);
                  api
                    .saveZcodeAutoSwitchConfig({ enabled: checked, thresholdTokens: Number(asThreshold) || 0 })
                    .then(() => {
                      setAsEnabled(checked);
                      setMsg({ type: "ok", text: checked ? "自动切换已启用（后台每 5 分钟检查）" : "自动切换已停用" });
                    })
                    .catch((e) => setMsg({ type: "err", text: String(e) }))
                    .finally(() => setAsBusy(false));
                }}
                disabled={asBusy}
              />
              <span className="text-xs text-muted-foreground">{asEnabled ? "已启用" : "已停用"}</span>
              <span className="text-xs text-muted-foreground">耗尽阈值（tokens，剩余低于该值即切换）</span>
              <Input
                className="w-36"
                value={asThreshold}
                onChange={(e) => setAsThreshold(e.target.value.replace(/[^0-9]/g, ""))}
                placeholder="0"
              />
              <Button
                variant="outline"
                size="sm"
                className="h-8"
                disabled={asBusy}
                onClick={() => {
                  setAsBusy(true);
                  api
                    .saveZcodeAutoSwitchConfig({ enabled: asEnabled ?? false, thresholdTokens: Number(asThreshold) || 0 })
                    .then(() => setMsg({ type: "ok", text: "阈值已保存" }))
                    .catch((e) => setMsg({ type: "err", text: String(e) }))
                    .finally(() => setAsBusy(false));
                }}
              >
                保存阈值
              </Button>
              <Button
                variant="outline"
                size="sm"
                className="h-8"
                disabled={asBusy}
                onClick={() => {
                  setAsBusy(true);
                  api
                    .tickZcodeAutoSwitch()
                    .then((r) => {
                      const res = r as { action?: string; message?: string };
                      setMsg({ type: "ok", text: res.message || `本轮结果: ${res.action ?? "none"}` });
                      void loadStatus();
                    })
                    .catch((e) => setMsg({ type: "err", text: String(e) }))
                    .finally(() => setAsBusy(false));
                }}
              >
                立即检查
              </Button>
            </div>
            {status?.autoSwitch?.lastMessage ? (
              <div className="mx-4 mb-3 rounded-lg bg-muted/60 px-3 py-2 text-xs leading-5 text-muted-foreground sm:mx-5">
                最近状态：<span className="text-foreground">{status.autoSwitch.lastState ?? "—"}</span>
                {status.autoSwitch.lastSwitchTo ? ` · 已切到「${status.autoSwitch.lastSwitchTo}」` : ""}
                <br />
                {status.autoSwitch.lastMessage}
              </div>
            ) : null}
          </CardContent>
        </Card>

        {/* 卡 3：说明文案（契约 §3.3） */}
        <Card className="flex min-w-0 flex-col gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <CardTitle className="flex items-center gap-2 text-sm font-medium">
              <Info className="size-4 text-muted-foreground" />
              说明
            </CardTitle>
            <CardDescription className="mt-1 text-xs">
              切换语义 · 设备隔离 · 冷切换边界
            </CardDescription>
          </CardHeader>
          <CardContent className="p-0">
            <div className="divide-y divide-border/50 border-t border-border/50">
              {[
                {
                  title: "切换 = 换登录身份",
                  body: "只改写 ZCode 的登录凭据（credentials.json）并重置套餐缓存；项目 / 会话 / 设置不动。",
                },
                {
                  title: "每账号独立设备遥测身份",
                  body: "每个账号有独立 deviceMid（UUID v4，写入 telemetry-state.json），多账号设备指纹互相隔离，降低关联风控。",
                },
                {
                  title: "ZCode 运行中切换会先关闭 ZCode",
                  body: "默认热切换：ZCode 运行中也可直接换登录身份（无感、不重启、项目会话不动）；若热切换校验失败（进程回写），再改用冷切换（关闭 ZCode 约 8 秒）。",
                },
                {
                  title: "如何添加账号",
                  body: "在 ZCode 里登录新账号后，回本页点「捕获当前账号」入池。同一登录态重复捕获只更新名字，不会产生重复条目。",
                },
              ].map((item) => (
                <div key={item.title} className="px-4 py-3 sm:px-5">
                  <p className="text-xs font-medium">{item.title}</p>
                  <p className="mt-1 text-[11px] leading-5 text-muted-foreground">{item.body}</p>
                </div>
              ))}
            </div>
          </CardContent>
        </Card>
      </section>

      {/* 卡 2：账号池列表（契约 §3.2：name | identity | 登录态 | 额度摘要 | 操作 + is_active 徽标） */}
      <section className="mb-8 min-w-0" aria-label="活动领取">
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
            <div className="flex min-w-0 items-center gap-2">
              <Gift className="size-4 shrink-0 text-muted-foreground" />
              <h2 className="text-sm font-medium">活动领取</h2>
              {claimScan ? (
                <Badge
                  variant={(Number(claimScan.claimableCount ?? 0)) > 0 ? "warning" : "secondary"}
                  className="text-[10px] tabular-nums"
                >
                  可领 {Number(claimScan.claimableCount ?? 0)}
                </Badge>
              ) : null}
            </div>
            <div className="flex shrink-0 items-center gap-3">
              <label className="flex items-center gap-1.5 text-[11px] text-muted-foreground" title="开启后定时循环自动扫描并领取（需 CDP 可访问 ZCode）">
                <Switch checked={autoClaim} onCheckedChange={(v) => void saveAutoClaim(v)} />
                自动领取
              </label>
              <Button
                size="sm"
                variant="ghost"
                className="h-7 gap-1 px-2 text-xs"
                onClick={() => void loadClaimScan(true)}
                disabled={claimBusy}
              >
                <RefreshCw className={cn("size-3.5", claimBusy && "animate-spin")} />
                扫描可领套餐
              </Button>
            </div>
          </div>
          <div className="border-t border-border/50 px-4 py-3 sm:px-5">
            {claimScan === null ? (
              <p className="text-[11px] text-muted-foreground">
                点「扫描可领套餐」检查当前登录与池账号的每日免费额度（如 ZCode Trust Build · GLM-5.3-Flash 1 亿 tokens/日）。
              </p>
            ) : (Number(claimScan.claimableCount ?? 0)) === 0 ? (
              <div className="space-y-1.5">
                <p className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
                  <CircleCheck className="size-3.5 text-primary" />
                  当前没有可领名额——每日福利为限量抢购制（名额放出后很快被领完），扫描只反映「此刻」状态。
                </p>
                <p className="text-[10px] leading-4 text-muted-foreground">
                  自动领取循环每 30 分钟巡检一次：名额一出现即按各账号独立设备身份自动领取，无需盯守。
                </p>
                {(Array.isArray(claimScan.accounts) ? claimScan.accounts : []).map((a) => {
                  const acc = a as Record<string, unknown>;
                  return (
                    <div key={String(acc.id ?? "")} className="flex min-w-0 items-center gap-2 text-[10px] text-muted-foreground">
                      <span className="max-w-[160px] truncate">{String(acc.name ?? acc.id)}</span>
                      {acc.error ? (
                        <span className="text-destructive">{String(acc.error).slice(0, 60)}</span>
                      ) : (
                        <span>此刻无可领名额（已领 / 未开放 / 已抢完）</span>
                      )}
                    </div>
                  );
                })}
              </div>
            ) : (
              <div className="space-y-2">
                {(Array.isArray(claimScan.accounts) ? claimScan.accounts : [])
                  .filter((a): a is Record<string, unknown> => Boolean(a) && (a as Record<string, unknown>).claimable === true)
                  .map((a) => {
                    const id = String(a.id ?? "");
                    const name = String(a.name ?? id);
                    const plans = Array.isArray(a.plans) ? (a.plans as Record<string, unknown>[]) : [];
                    return (
                      <div key={id} className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1.5">
                        <span className="min-w-0 max-w-[180px] truncate text-xs font-medium">{name}</span>
                        <span className="min-w-0 flex-1 truncate text-[11px] text-muted-foreground">
                          {plans
                            .map((pl) => {
                              const pname = String(pl.name ?? pl.planId ?? "套餐");
                              const grants = Array.isArray(pl.grants) ? (pl.grants as Record<string, unknown>[]) : [];
                              const g = grants
                                .map((gr) => `${gr.name ?? ""} ${Number(gr.units ?? 0) >= 1e8 ? `${(Number(gr.units ?? 0) / 1e8).toFixed(1)} 亿` : Number(gr.units ?? 0)} tokens`)
                                .join(" + ");
                              return g ? `${pname}（${g}）` : pname;
                            })
                            .join("；")}
                        </span>
                        <Button
                          size="sm"
                          className="h-7 px-2.5 text-xs"
                          onClick={() => void runClaimAuto(id, name)}
                          disabled={claimAutoBusyId !== null}
                        >
                          {claimAutoBusyId === id ? <Loader2 className="animate-spin" /> : <Gift />}
                          一键领取
                        </Button>
                      </div>
                    );
                  })}
                <p className="text-[10px] leading-4 text-muted-foreground">
                  一键领取走 CDP 无痕验证（ZCode 未运行时会自动拉起；运行中未开调试端口时请先重启 ZCode——右上角说明或关闭后重试）。
                </p>
              </div>
            )}
          </div>
        </Card>
      </section>
      {/* 反代网关（2026-10-14 融合 Z-Accounts Gateway Edition 池算法） */}
      <section className="mt-10 min-w-0 space-y-2.5" aria-label="反代网关">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">反代网关</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            把账号池包装成 OpenAI / Anthropic 兼容接口：每账号并发上限 + 分级冷却 + 会话亲和
            （同一对话固定账号，保温上游 prompt 缓存）
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
            <div className="flex min-w-0 items-center gap-2">
              <Network className="size-4 shrink-0 text-muted-foreground" />
              <span className="text-sm font-medium">网关</span>
              {gw?.enabled ? (
                <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">已启用</Badge>
              ) : (
                <Badge variant="outline" className="h-5 px-1.5 text-[10px]">未启用</Badge>
              )}
              {typeof gw?.apiKeySet === "boolean" && !gw.apiKeySet ? (
                <Badge variant="warning" className="h-5 px-1.5 text-[10px]">未设密钥</Badge>
              ) : null}
            </div>
            <Switch
              checked={gw?.enabled === true}
              disabled={gwBusy}
              onCheckedChange={(checked) => void saveGateway({ enabled: checked }, checked ? "启用" : "停用")}
            />
          </div>
          <div className="border-t px-4 py-3 text-xs sm:px-5">
            <div className="grid gap-2 sm:grid-cols-2">
              <label className="flex items-center gap-2">
                <span className="w-28 shrink-0 text-muted-foreground">每账号并发</span>
                <Input
                  className="h-7 w-20 text-xs"
                  value={gwMaxInFlight}
                  onChange={(e) => setGwMaxInFlight(e.target.value)}
                  onBlur={() => {
                    const n = Number(gwMaxInFlight);
                    if (Number.isFinite(n) && n >= 1) void saveGateway({ maxInFlight: n }, "并发上限");
                  }}
                />
                <span className="text-muted-foreground">（在飞请求）</span>
              </label>
              <label className="flex items-center gap-2">
                <span className="w-28 shrink-0 text-muted-foreground">会话亲和</span>
                <Switch
                  checked={gw?.sessionAffinity === true}
                  disabled={gwBusy}
                  onCheckedChange={(checked) =>
                    void saveGateway({ sessionAffinity: checked }, checked ? "开启亲和" : "关闭亲和")
                  }
                />
                <span className="text-muted-foreground">
                  会话数 {String(gw?.sessionCount ?? 0)}/{String(gw?.sessionMax ?? 0)}
                </span>
              </label>
              <label className="flex items-center gap-2 sm:col-span-2">
                <span className="w-28 shrink-0 text-muted-foreground">放行模型</span>
                <Input
                  className="h-7 flex-1 text-xs"
                  placeholder="glm-5.3-flash, glm-5.2（逗号分隔；留空 = 不放行任何模型）"
                  value={gwModels}
                  onChange={(e) => setGwModels(e.target.value)}
                  onBlur={() => {
                    const list = gwModels
                      .split(",")
                      .map((s) => s.trim())
                      .filter(Boolean);
                    void saveGateway({ models: list }, "模型清单");
                  }}
                />
              </label>
              <label className="flex items-center gap-2 sm:col-span-2">
                <span className="w-28 shrink-0 text-muted-foreground">访问密钥</span>
                <Input
                  className="h-7 flex-1 text-xs"
                  type="password"
                  placeholder="留空 = 不鉴权（仅本机回环可用）"
                  value={gwApiKey}
                  onChange={(e) => setGwApiKey(e.target.value)}
                />
                <Button
                  size="sm"
                  variant="outline"
                  className="h-7 shrink-0 px-2 text-xs"
                  disabled={gwBusy}
                  onClick={() => {
                    void saveGateway({ apiKey: gwApiKey }, "访问密钥");
                    setGwApiKey("");
                  }}
                >
                  保存
                </Button>
              </label>
            </div>
            <div className="mt-2 flex flex-wrap items-center gap-2 text-muted-foreground">
              <span>调用地址：</span>
              <code className="rounded bg-muted px-1.5 py-0.5">/zcode/v1/chat/completions</code>
              <code className="rounded bg-muted px-1.5 py-0.5">/zcode/v1/messages</code>
              <Button
                size="sm"
                variant="ghost"
                className="h-6 px-1.5 text-[11px]"
                disabled={gwBusy}
                onClick={() => void api.zcodeGatewayClearCooldown().then((r) => {
                  setGw((r.status as Record<string, unknown>) ?? gw);
                  toast.success("已解除全部冷却");
                }).catch((e) => toast.error(api.asError(e)))}
              >
                解除全部冷却
              </Button>
            </div>
            {Array.isArray(gw?.members) && (gw.members as Record<string, unknown>[]).length > 0 ? (
              <div className="mt-3 space-y-1">
                {(gw.members as Record<string, unknown>[]).map((m, i) => {
                  const key = String(m.key ?? i);
                  const cooling = m.cooldown === true;
                  const disabled = m.disabled === true;
                  const excluded = m.excluded === true;
                  const quota = m.quotaBlocked === true;
                  const inFlight = Number(m.inFlight ?? 0);
                  const maxInFlight = Number(m.maxInFlight ?? 0);
                  return (
                    <div key={key} className="flex flex-wrap items-center gap-2 rounded-md border px-2 py-1.5">
                      <span className="min-w-0 flex-1 truncate">{String(m.name ?? key)}</span>
                      {excluded ? (
                        <Badge variant="outline" className="h-5 px-1.5 text-[10px]" title="桌面端当前登录账号，网关默认排除以免与客户端争抢额度">
                          已排除
                        </Badge>
                      ) : null}
                      {disabled ? <Badge variant="warning" className="h-5 px-1.5 text-[10px]">已禁用</Badge> : null}
                      {cooling ? (
                        <Badge variant="warning" className="h-5 px-1.5 text-[10px]">
                          冷却 {String(m.cooldownRemainSecs ?? 0)}s
                        </Badge>
                      ) : null}
                      {quota ? <Badge variant="warning" className="h-5 px-1.5 text-[10px]">额度耗尽</Badge> : null}
                      <span className="text-muted-foreground">
                        在飞 {inFlight}/{maxInFlight} · 成功 {String(m.successes ?? 0)} · 失败 {String(m.failures ?? 0)}
                      </span>
                      {cooling || disabled ? (
                        <Button
                          size="sm"
                          variant="ghost"
                          className="h-6 px-1.5 text-[11px]"
                          onClick={() =>
                            void api.zcodeGatewayClearCooldown(key).then((r) => {
                              setGw((r.status as Record<string, unknown>) ?? gw);
                              toast.success("已解除冷却");
                            }).catch((e) => toast.error(api.asError(e)))
                          }
                        >
                          解除
                        </Button>
                      ) : null}
                      {typeof m.lastError === "string" && m.lastError ? (
                        <span className="w-full truncate text-[11px] text-destructive/80" title={m.lastError}>
                          {m.lastError}
                        </span>
                      ) : null}
                    </div>
                  );
                })}
              </div>
            ) : (
              <p className="mt-2 text-muted-foreground">
                池内暂无可用账号：需至少一个含 Coding Plan api-key 的 ZCode 账号（在下方账号池导入）
              </p>
            )}
          </div>
        </Card>
      </section>
      <section className="mt-10 min-w-0 space-y-2.5" aria-label="账号池">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">账号池</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            池内账号存于 ~/.wb-switch/zcode_accounts.json；「使用中」徽标 = 当前 live 登录态
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
            <div className="flex min-w-0 items-center gap-2">
              <Users className="size-4 shrink-0 text-muted-foreground" />
              <span className="text-sm font-medium">账号（{accounts.length}）</span>
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <Button
                size="sm"
                variant="ghost"
                className="h-7 gap-1 px-2 text-xs"
                onClick={() => void runQuotaBulk()}
                disabled={quotaBulkBusy || accounts.length === 0}
                title="逐个查询池内全部账号额度并落缓存（串行，避免打爆上游）"
              >
                {quotaBulkBusy ? <Loader2 className="size-3.5 animate-spin" /> : <Coins />}
                刷新全部额度
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-7 gap-1 px-2 text-xs"
                onClick={() => void loadStatus().catch(() => setMsg({ type: "err", text: "刷新账号池失败" }))}
                disabled={busy === "refresh"}
              >
                <RefreshCw className={cn("size-3.5", busy === "refresh" && "animate-spin")} />
                刷新列表
              </Button>
            </div>
          </div>

          {/* 额度对比条：全部账号可用额度一眼对比（降序，失效账号置灰） */}
          {accounts.length > 0 && (
            <div className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1.5 border-t border-border/50 bg-muted/20 px-4 py-2 sm:px-5">
              <span className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
                额度对比
              </span>
              {[...accounts]
                .map((acc) => ({ acc, val: Number(quotaAvailable(acc.quota ?? null)) || 0 }))
                .sort((a, b) => b.val - a.val)
                .map(({ acc, val }) => (
                  <span key={acc.id} className="flex items-center gap-1 text-[11px]">
                    <span className="max-w-[120px] truncate text-muted-foreground">{acc.name || acc.id}</span>
                    <span
                      className={cn(
                        "font-medium tabular-nums",
                        accountLoginState(acc) === "expired" ? "text-muted-foreground/50" : "text-foreground",
                      )}
                    >
                      {fmtQuota(val)}
                    </span>
                  </span>
                ))}
            </div>
          )}

          <div className="divide-y divide-border/50 border-t border-border/50">
            {accounts.length === 0 ? (
              <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                {status === null
                  ? "暂无数据。请确认 Sylvia Switch 服务已启动后点「刷新」。"
                  : "账号池为空。在 ZCode 登录账号后点「捕获当前账号」加入池。"}
              </p>
            ) : (
              accounts.map((acc) => {
                const switching = busy === `switch:${acc.id}`;
                const refreshingQuota = busy === `quota:${acc.id}`;
                const removing = busy === `remove:${acc.id}`;
                const rowQuota = acc.quota ?? null;
                const rowFetchedAt = acc.quotaFetchedAt ?? null;
                const isActive = acc.isActive === true;
                const name = acc.name || acc.id;
                const channels = (rowQuota?.channels ?? []).filter(
                  (c) => Array.isArray(c.items) && (c.items as unknown[]).length > 0,
                );
                return (
                  <div key={acc.id} className="min-w-0">
                  <div
                    className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-2 px-4 py-2.5 sm:px-5"
                  >
                    <div className="flex min-w-0 items-center gap-2">
                      <span className="max-w-[180px] truncate text-xs font-medium" title={name}>
                        {name}
                      </span>
                      {isActive ? (
                        <Badge className="text-[10px]">使用中</Badge>
                      ) : null}
                      <LoginStateBadge state={accountLoginState(acc)} />
                    </div>
                    <div className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1 text-[11px] text-muted-foreground">
                      <span className="max-w-[240px] truncate" title={identityText(acc.identity)}>
                        {identityText(acc.identity)}
                      </span>
                      <span className="shrink-0 tabular-nums">
                        额度{" "}
                        <span
                          className={cn(
                            "font-medium",
                            quotaStale(rowFetchedAt) ? "text-warning" : "text-foreground",
                          )}
                        >
                          {fmtQuota(quotaAvailable(rowQuota))}
                        </span>
                        {rowFetchedAt ? (
                          <span
                            className={cn(
                              "ml-1 text-[10px]",
                              quotaStale(rowFetchedAt) ? "text-warning" : "text-muted-foreground",
                            )}
                            title={quotaStale(rowFetchedAt) ? "缓存已过期，点「额度」重查" : "缓存时间"}
                          >
                            {fmtAgo(rowFetchedAt)}
                          </span>
                        ) : (
                          <span className="ml-1 text-[10px] text-muted-foreground">未查询</span>
                        )}
                      </span>
                      <span className="shrink-0">添加于 {fmtDay(acc.addedAt)}</span>
                      <span className="max-w-[200px] truncate">
                        {acc.proxy ? `代理 ${acc.proxy}` : "直连"}
                      </span>
                      {rowQuota && !rowQuota.ok && rowQuota.error ? (
                        <span className="text-destructive">{rowQuota.error}</span>
                      ) : null}
                    </div>
                    <div className="ml-auto flex shrink-0 items-center gap-1.5">
                      {zcodeProxyEdit?.id === acc.id ? (
                        <>
                          <Input
                            className="h-7 w-44 text-xs"
                            placeholder="http:// 或 socks5://，留空直连"
                            value={zcodeProxyEdit.value}
                            onChange={(e) => setZcodeProxyEdit({ id: acc.id, value: e.target.value })}
                            onKeyDown={(e) => {
                              if (e.key === "Enter" && !e.nativeEvent.isComposing)
                                void saveZcodeProxy(acc, zcodeProxyEdit.value);
                            }}
                          />
                          <Button
                            size="sm"
                            className="h-7 px-2 text-xs"
                            onClick={() => void saveZcodeProxy(acc, zcodeProxyEdit.value)}
                            disabled={busy === `zproxy:${acc.id}`}
                          >
                            {busy === `zproxy:${acc.id}` ? <Loader2 className="animate-spin" /> : null}保存
                          </Button>
                          <Button size="sm" variant="ghost" className="h-7 px-2 text-xs" onClick={() => setZcodeProxyEdit(null)}>
                            取消
                          </Button>
                        </>
                      ) : (
                        <Button
                          size="sm"
                          variant="ghost"
                          className="h-7 px-1.5 text-xs"
                          onClick={() => setZcodeProxyEdit({ id: acc.id, value: acc.proxy ?? "" })}
                          title="编辑出站代理（一号一 IP：领取/额度请求走该代理，覆盖 IP 维度限领与风控）"
                        >
                          <Globe />
                        </Button>
                      )}
                      {isActive ? (
                        <span className="inline-flex" title="当前已是该账号">
                          <Button size="sm" variant="outline" className="h-7 px-2 text-xs" disabled>
                            <ArrowLeftRight />
                            切换
                          </Button>
                        </span>
                      ) : (
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-7 px-2 text-xs"
                          onClick={() => setSwitchTarget(acc)}
                          disabled={switching}
                          title="切换到此账号（默认热切换：不关闭 ZCode 无感换号；失败时按提示改用冷切换）"
                        >
                          {switching ? <Loader2 className="animate-spin" /> : <ArrowLeftRight />}
                          切换
                        </Button>
                      )}
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 px-2 text-xs"
                        onClick={() => void runRowQuota(acc)}
                        disabled={refreshingQuota}
                        title="实时查询该账号额度（Monitor / ZaiBilling 通道）"
                      >
                        {refreshingQuota ? <Loader2 className="animate-spin" /> : <Coins />}
                        额度
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 px-2 text-xs text-destructive hover:text-destructive"
                        onClick={() => void runRemove(acc)}
                        disabled={removing}
                        title="从账号池移除（不影响当前 live 登录态文件）"
                      >
                        {removing ? <Loader2 className="animate-spin" /> : <Trash2 />}
                        删除
                      </Button>
                    </div>
                  </div>
                  {/* 1.5.3 分模型用量明细（仿 ZCode 官方 UI：模型名 + 百分比 + 进度条 + 剩余/总量 + 到期） */}
                  {channels.map((ch) => (
                    <div key={String(ch.name)} className="border-t border-border/40 px-4 py-2.5 sm:px-5">
                      <div className="flex items-center gap-2">
                        <span className="text-[11px] font-medium text-foreground">{String(ch.name)}</span>
                        {ch.error ? <span className="text-[10px] text-destructive">{String(ch.error)}</span> : null}
                      </div>
                      {(ch.items as Array<Record<string, unknown>>).map((it, idx) => {
                        const name = String(it.name ?? "套餐");
                        const pct = remainPct(it);
                        const rem = numOf(it.remaining as string | number | null);
                        const tot = numOf(it.total as string | number | null);
                        const period = fmtPeriod(it.periodEnd ?? it.expireAtMs);
                        return (
                          <div key={`${name}-${idx}`} className="mt-2 rounded-lg border border-border/40 p-2.5">
                            <div className="text-[12px] font-medium">{name}</div>
                            <div className="mt-0.5 flex items-end justify-between">
                              <span className="text-base font-semibold leading-none">
                                {pct.toFixed(0)}%
                              </span>
                              {period ? (
                                <span className="text-[10px] text-muted-foreground">{period}</span>
                              ) : null}
                            </div>
                            <div className="mt-1.5 h-1.5 w-full overflow-hidden rounded-full bg-muted">
                              <div
                                className={cn(
                                  "h-full rounded-full",
                                  pct > 30 ? "bg-emerald-500" : pct > 10 ? "bg-amber-500" : "bg-destructive",
                                )}
                                style={{ width: `${pct}%` }}
                              />
                            </div>
                            <div className="mt-1 text-[10px] tabular-nums text-muted-foreground">
                              {rem !== null ? fmtQuota(rem) : "—"} / {tot !== null ? fmtQuota(tot) : "—"}
                            </div>
                          </div>
                        );
                      })}
                    </div>
                  ))}
                  </div>
                );
              })
            )}
          </div>

          <div className="border-t border-border/50 px-4 py-3 sm:px-5">
            <p className="flex items-start gap-1.5 text-[11px] leading-5 text-muted-foreground">
              <Info className="mt-0.5 size-3.5 shrink-0" />
              切换只替换登录身份（项目 / 会话 / 设置不动），每账号独立设备遥测身份（deviceMid 隔离）；
              ZCode 运行中切换会先强制关闭 ZCode，完成后重新打开即可。
            </p>
          </div>
        </Card>
      </section>

      {/* 切换确认框（契约 §3.2：确认框说明会关闭 ZCode；运行中需强制） */}
      <Dialog open={switchTarget !== null} onOpenChange={(open) => { if (!open) setSwitchTarget(null); }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>切换 ZCode 账号</DialogTitle>
            <DialogDescription>
              即将把 ZCode 登录身份切换为「{switchTarget?.name || switchTarget?.id || ""}」。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 text-sm">
            {desktopRunning ? (
              <p className="flex items-start gap-1.5 rounded-md bg-amber-500/10 p-2.5 text-[13px] leading-5 text-amber-700 dark:text-amber-500">
                <CircleAlert className="mt-0.5 size-4 shrink-0" />
                检测到 ZCode 正在运行：确认后将先强制关闭 ZCode（约 8 秒），再写入新登录态；
                完成后请重新打开 ZCode。
              </p>
            ) : (
              <p className="text-[13px] leading-5 text-muted-foreground">
                当前未检测到 ZCode 运行：写入新登录态后，打开 ZCode 即以新账号身份登录。
              </p>
            )}
            <p className="text-xs leading-5 text-muted-foreground">
              仅切换登录身份与设备遥测标识（项目 / 会话 / 设置不动）；已在池中的原账号可随时切回。
            </p>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setSwitchTarget(null)}>
              取消
            </Button>
            <Button
              onClick={() => void runSwitch()}
              disabled={switchTarget !== null && busy === `switch:${switchTarget.id}`}
            >
              {switchTarget !== null && busy === `switch:${switchTarget.id}` ? (
                <Loader2 className="animate-spin" />
              ) : (
                <ArrowLeftRight />
              )}
              确认切换
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
