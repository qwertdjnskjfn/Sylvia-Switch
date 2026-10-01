import { useCallback, useEffect, useRef, useState } from "react";
import {
  Bot,
  CircleAlert,
  CircleCheck,
  Database,
  ExternalLink,
  Pause,
  Play,
  RefreshCw,
  RotateCcw,
  Server,
  Settings2,
  Square,
  UserPlus,
} from "lucide-react";

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
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type {
  ChatGptAccount,
  ChatGptChannelStatus,
  ChatGptRegistrarStatus,
} from "@/lib/types";

/** WebUI 默认端口（契约 §2 webui_port 缺省；实际以 channel/status 返回为准）。 */
const DEFAULT_WEBUI_PORT = 8765;

/** 账号表单次拉取条数（工具链 /api/registered 支持 limit/offset）。 */
const ACCOUNTS_LIMIT = 50;

/** 目标成功数默认值（契约 §6：必填 ≥1，工具链 target_count=0 表示不限量、通道页禁止）。 */
const DEFAULT_TARGET_COUNT = 10;

/** 注册机状态文案与配色（state 枚举：stopped/running/paused，auto_loop.py:45-48）。 */
function registrarStateView(state: string | undefined): { text: string; cls: string } {
  switch (state) {
    case "running":
      return { text: "运行中", cls: "font-medium text-emerald-600" };
    case "paused":
      return { text: "已暂停", cls: "font-medium text-amber-600" };
    case "stopped":
      return { text: "已停止", cls: "text-muted-foreground" };
    default:
      return { text: "未知", cls: "text-destructive" };
  }
}

/** created_at 为 unix 秒（SQLite REAL）；兼容毫秒/字符串，解析失败原样展示。 */
function fmtTime(value: number | string | null | undefined): string {
  if (value === null || value === undefined || value === "") return "—";
  const n = typeof value === "number" ? value : Number(value);
  if (Number.isFinite(n) && n > 0) {
    const ms = n < 1e12 ? n * 1000 : n;
    const d = new Date(ms);
    if (!Number.isNaN(d.getTime())) return d.toLocaleString();
  }
  if (typeof value === "string") {
    const d = new Date(value);
    if (!Number.isNaN(d.getTime())) return d.toLocaleString();
  }
  return String(value);
}

export default function ChatGptPage() {
  const [status, setStatus] = useState<ChatGptChannelStatus | null>(null);
  const [statusError, setStatusError] = useState(false);
  const [registrar, setRegistrar] = useState<ChatGptRegistrarStatus | null>(null);
  const [accounts, setAccounts] = useState<ChatGptAccount[]>([]);
  const [accountsTotal, setAccountsTotal] = useState<number | null>(null);
  const [poolTotal, setPoolTotal] = useState<number | null>(null);
  const [targetCount, setTargetCount] = useState(String(DEFAULT_TARGET_COUNT));
  const [busy, setBusy] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const mountedRef = useRef(true);

  const webuiPort = status?.webuiPort || DEFAULT_WEBUI_PORT;
  const webuiBase = `http://127.0.0.1:${webuiPort}`;
  const regState = registrar?.state ?? status?.registrar?.state;
  const registrarActive = regState === "running" || regState === "paused";

  /** 通道聚合状态（WebUI 存活/端口/计数）；轮询失败静默置空，页面显示「未知」。 */
  const loadStatus = useCallback(async () => {
    try {
      const s = await api.getChatGptChannelStatus();
      if (!mountedRef.current) return;
      setStatus(s);
      setStatusError(false);
      if (s.registrar) setRegistrar(s.registrar);
    } catch {
      if (!mountedRef.current) return;
      setStatus(null);
      setStatusError(true);
    }
  }, []);

  /** 注册机快照（工具链 /api/auto/status 透传；5s 轮询，只读不触发）。 */
  const loadRegistrar = useCallback(async () => {
    try {
      const r = await api.chatGptRegistrarStatus();
      if (mountedRef.current) setRegistrar(r);
    } catch {
      // WebUI 未运行时后端透传失败：保留上次快照即可，徽标由 status 呈现
    }
  }, []);

  /** 账号表（GET /api/chatgpt/registered 主源）+ 号池计数（GET /api/chatgpt/accounts）。 */
  const loadAccounts = useCallback(async () => {
    setLoading(true);
    try {
      const [reg, pool] = await Promise.all([
        api.chatGptRegistered({ limit: ACCOUNTS_LIMIT }),
        api.chatGptAccounts({ limit: 1 }),
      ]);
      if (!mountedRef.current) return;
      setAccounts(Array.isArray(reg.items) ? reg.items : []);
      setAccountsTotal(typeof reg.total === "number" ? reg.total : null);
      setPoolTotal(typeof pool.total === "number" ? pool.total : null);
    } catch {
      // WebUI 未运行/后端未部署：清空展示，由服务托管卡的状态指引处理
      if (!mountedRef.current) return;
      setAccounts([]);
      setAccountsTotal(null);
      setPoolTotal(null);
    } finally {
      if (mountedRef.current) setLoading(false);
    }
  }, []);

  const loadAll = useCallback(async () => {
    await Promise.all([loadStatus(), loadRegistrar(), loadAccounts()]);
  }, [loadStatus, loadRegistrar, loadAccounts]);

  useEffect(() => {
    mountedRef.current = true;
    void loadAll();
    // 服务托管徽标 10s 轮询；注册机快照 5s 轮询（均为只读 GET，零自动触发纪律不受影响）
    const t1 = window.setInterval(() => void loadStatus(), 10_000);
    const t2 = window.setInterval(() => void loadRegistrar(), 5_000);
    return () => {
      mountedRef.current = false;
      window.clearInterval(t1);
      window.clearInterval(t2);
    };
  }, [loadAll, loadStatus, loadRegistrar]);

  const run = useCallback(
    async (key: string, action: () => Promise<void>, okText: string) => {
      setBusy(key);
      setMsg(null);
      try {
        await action();
        setMsg({ type: "ok", text: okText });
      } catch (e) {
        setMsg({ type: "err", text: api.asError(e) });
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  const startWebui = () =>
    run(
      "webui-start",
      async () => {
        await api.chatGptWebuiStart();
        await loadStatus();
        await loadRegistrar();
      },
      "WebUI 已启动（或本就在运行）",
    );

  const stopWebui = () =>
    run(
      "webui-stop",
      async () => {
        await api.chatGptWebuiStop();
        await loadStatus();
        await loadRegistrar();
      },
      "WebUI 已停止",
    );

  /** 目标成功数校验：必须为 ≥1 的整数（0=不限量，禁止）。 */
  const parseTarget = (): number | null => {
    const n = Number(targetCount.trim());
    if (!Number.isInteger(n) || n < 1) return null;
    return n;
  };

  const startRegistrar = () => {
    const n = parseTarget();
    if (n === null) {
      setMsg({ type: "err", text: "目标成功数必须为 ≥1 的整数（0 表示不限量，通道页不允许）" });
      return;
    }
    void run(
      "reg-start",
      async () => {
        await api.chatGptRegistrarStart({ target_count: n });
        await loadRegistrar();
        await loadStatus();
      },
      `批量注册已启动（目标成功 ${n} 个）`,
    );
  };

  const pauseRegistrar = () =>
    run(
      "reg-pause",
      async () => {
        await api.chatGptRegistrarPause();
        await loadRegistrar();
      },
      "批量注册已暂停",
    );

  const resumeRegistrar = () =>
    run(
      "reg-resume",
      async () => {
        await api.chatGptRegistrarResume();
        await loadRegistrar();
      },
      "批量注册已恢复",
    );

  const stopRegistrar = () =>
    run(
      "reg-stop",
      async () => {
        await api.chatGptRegistrarStop();
        await loadRegistrar();
        await loadStatus();
      },
      "批量注册已停止",
    );

  const syncPool = () =>
    run(
      "sync",
      async () => {
        await api.chatGptAccountsSync();
        await loadStatus();
      },
      "已同步到本地池（~/.wb-switch/chatgpt_accounts.json）",
    );

  return (
    <div className="mx-auto max-w-5xl space-y-6 pb-12">
      {/* 顶部标题栏 */}
      <div className="flex flex-wrap items-center justify-between gap-4 border-b pb-4">
        <div>
          <div className="flex items-center gap-2.5">
            <div className="flex size-9 items-center justify-center rounded-xl bg-primary/10 text-primary">
              <Bot className="size-5" />
            </div>
            <div>
              <h1 className="text-xl font-bold tracking-tight">ChatGPT 通道</h1>
              <p className="text-xs text-muted-foreground">
                托管 gpt-reg-review 批量注册工具链：服务托管 + 注册机控制 + 账号表；对话反代为 v2 范围
              </p>
            </div>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <Badge
            variant="outline"
            className={cn(
              "text-xs",
              status?.webuiRunning
                ? "border-emerald-500/30 bg-emerald-500/10 text-emerald-600"
                : "border-amber-500/30 bg-amber-500/10 text-amber-600",
            )}
          >
            WebUI {status?.webuiRunning ? `在线 · 端口 ${webuiPort}` : "未运行"}
          </Badge>
          <Button
            variant="outline"
            size="sm"
            onClick={() => void loadAll()}
            disabled={loading || busy !== null}
            className="h-8 gap-1 text-xs"
          >
            <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
            刷新
          </Button>
        </div>
      </div>

      {/* 消息提示 */}
      {msg && (
        <Alert
          variant={msg.type === "ok" ? "default" : "destructive"}
          className={cn(msg.type === "ok" && "border-emerald-500/30 bg-emerald-500/10 text-emerald-700")}
        >
          {msg.type === "ok" ? <CircleCheck className="size-4" /> : <CircleAlert className="size-4" />}
          <AlertTitle className="text-sm font-medium">{msg.type === "ok" ? "成功" : "提示"}</AlertTitle>
          <AlertDescription className="text-xs">{msg.text}</AlertDescription>
        </Alert>
      )}

      {/* ① 服务托管卡 */}
      <Card className="border-primary/20 bg-primary/[0.02] shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-sm font-semibold">
            <Server className="size-4" />
            服务托管
            <Badge
              variant="outline"
              className={cn(
                "px-1.5 py-0 text-[10px]",
                status?.webuiRunning
                  ? "border-emerald-500/30 bg-emerald-500/10 text-emerald-600"
                  : statusError
                    ? "border-amber-500/30 bg-amber-500/10 text-amber-600"
                    : "border-rose-500/30 bg-rose-500/10 text-rose-600",
              )}
            >
              {status?.webuiRunning ? "运行中" : statusError ? "状态未知" : "已停止"}
            </Badge>
          </CardTitle>
          <CardDescription className="text-xs">
            由 Sylvia Switch 托管拉起 gpt-reg-review WebUI（python start_webui.py --no-browser）；
            「停止」会先停注册机再结束进程——批量注册进行中禁止停止，以免中断在跑任务。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              className="h-8 gap-1 text-xs"
              disabled={busy !== null || status?.webuiRunning === true}
              onClick={startWebui}
            >
              <Play className={cn("size-3.5", busy === "webui-start" && "animate-pulse")} />
              启动服务
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-8 gap-1 text-xs"
              disabled={busy !== null || !status?.webuiRunning || registrarActive}
              onClick={stopWebui}
            >
              <Square className={cn("size-3.5", busy === "webui-stop" && "animate-pulse")} />
              停止服务
            </Button>
            <Button size="sm" variant="outline" className="h-8 gap-1 text-xs" asChild>
              <a href={webuiBase} target="_blank" rel="noreferrer">
                <ExternalLink className="size-3.5" />
                打开 WebUI（{webuiBase}）
              </a>
            </Button>
          </div>
          <div className="grid grid-cols-2 gap-2 text-xs sm:grid-cols-4">
            <div>
              <span className="text-muted-foreground">WebUI：</span>
              {status?.webuiRunning ? (
                <span className="font-medium text-emerald-600">运行中</span>
              ) : statusError ? (
                <span className="text-amber-600">状态未知</span>
              ) : (
                <span className="text-muted-foreground">已停止</span>
              )}
            </div>
            <div>
              <span className="text-muted-foreground">端口：</span>
              <span className="tabular-nums">{status?.webuiPort ?? webuiPort}</span>
            </div>
            <div>
              <span className="text-muted-foreground">已注册账号：</span>
              <span className="tabular-nums">{status ? status.accountCount : "—"}</span>
            </div>
            <div>
              <span className="text-muted-foreground">号池可用：</span>
              <span className="tabular-nums">{status ? status.poolCount : "—"}</span>
            </div>
          </div>
          <p className="text-xs text-muted-foreground">
            v1 范围＝托管 + 控制 + 账号表；ChatGPT 反代（对话转发）为 v2。可调模型由 WebUI
            内注册的账号池决定，本页不透出模型调用入口。
          </p>
        </CardContent>
      </Card>

      {/* ② 注册机控制卡（零自动触发：仅手动启动，无任何自动/定时拉起） */}
      <Card className="border-primary/20 bg-primary/[0.02] shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-sm font-semibold">
            <UserPlus className="size-4" />
            注册机控制
            <Badge variant="outline" className="px-1.5 py-0 text-[10px]">
              零自动触发 · 仅手动启动
            </Badge>
          </CardTitle>
          <CardDescription className="text-xs">
            批量注册循环由 gpt-reg-review 全自动完成（OTP 自动收码）；邮箱池等前置配置需先在 WebUI
            内就绪。目标成功数必填 ≥1（工具链 0＝不限量，本页禁止，避免无限跑）。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-1.5 text-xs">
              <span className="text-muted-foreground">目标成功数</span>
              <Input
                type="number"
                min={1}
                step={1}
                value={targetCount}
                onChange={(e) => setTargetCount(e.target.value)}
                disabled={busy !== null || regState === "running"}
                className="h-8 w-24 text-xs"
              />
            </label>
            <Button
              size="sm"
              className="h-8 gap-1 text-xs"
              disabled={busy !== null || regState === "running"}
              onClick={startRegistrar}
            >
              <Play className={cn("size-3.5", busy === "reg-start" && "animate-pulse")} />
              启动
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-8 gap-1 text-xs"
              disabled={busy !== null || regState !== "running"}
              onClick={pauseRegistrar}
            >
              <Pause className={cn("size-3.5", busy === "reg-pause" && "animate-pulse")} />
              暂停
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-8 gap-1 text-xs"
              disabled={busy !== null || regState !== "paused"}
              onClick={resumeRegistrar}
            >
              <RotateCcw className={cn("size-3.5", busy === "reg-resume" && "animate-pulse")} />
              恢复
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-8 gap-1 text-xs"
              disabled={busy !== null || !regState || regState === "stopped"}
              onClick={stopRegistrar}
            >
              <Square className={cn("size-3.5", busy === "reg-stop" && "animate-pulse")} />
              停止
            </Button>
          </div>
          <div className="grid grid-cols-2 gap-2 text-xs sm:grid-cols-4">
            <div>
              <span className="text-muted-foreground">状态：</span>
              <span className={registrarStateView(regState ?? undefined).cls}>
                {registrarStateView(regState ?? undefined).text}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">成功：</span>
              <span className="font-medium tabular-nums text-emerald-600">
                {registrar ? registrar.registered_ok : "—"}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">失败：</span>
              <span className="font-medium tabular-nums text-destructive">
                {registrar ? registrar.registered_fail : "—"}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">目标：</span>
              <span className="tabular-nums">
                {registrar && typeof registrar.target_count === "number" ? registrar.target_count : "—"}
              </span>
            </div>
          </div>
          {registrar && (registrar.remaining !== null || registrar.last_message) && (
            <div className="grid grid-cols-1 gap-2 text-xs sm:grid-cols-2">
              <div>
                <span className="text-muted-foreground">剩余：</span>
                <span className="tabular-nums">{registrar.remaining ?? "不限"}</span>
              </div>
              <div className="truncate">
                <span className="text-muted-foreground">最近消息：</span>
                <span className="font-mono">{registrar.last_message || "—"}</span>
              </div>
            </div>
          )}
        </CardContent>
      </Card>

      {/* ③ 账号表卡（主源 GET /api/chatgpt/registered；号池计数取 GET /api/chatgpt/accounts） */}
      <Card className="border-primary/20 bg-primary/[0.02] shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-sm font-semibold">
            <Database className="size-4" />
            已注册账号
            <Badge variant="outline" className="px-1.5 py-0 text-[10px]">
              {accountsTotal !== null ? `共 ${accountsTotal} 个` : "未加载"}
              {poolTotal !== null ? ` · 邮箱号池 ${poolTotal}` : ""}
            </Badge>
          </CardTitle>
          <CardDescription className="text-xs">
            透传 gpt-reg-review 已注册账号表（token 只回长度不回原文）；「同步到本地池」按 email
            去重镜像到 ~/.wb-switch/chatgpt_accounts.json。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              className="h-8 gap-1 text-xs"
              disabled={loading || busy !== null}
              onClick={() => void loadAccounts()}
            >
              <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
              刷新
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-8 gap-1 text-xs"
              disabled={busy !== null}
              onClick={syncPool}
            >
              <Database className={cn("size-3.5", busy === "sync" && "animate-pulse")} />
              同步到本地池
            </Button>
          </div>
          {accounts.length === 0 ? (
            <p className="rounded-lg border border-dashed px-3 py-6 text-center text-xs text-muted-foreground">
              {status?.webuiRunning
                ? "暂无已注册账号——先在 WebUI 配置邮箱池，再启动批量注册"
                : "WebUI 未运行或后端路由未就绪——先「启动服务」，并确认 Sylvia Switch 后端已含 ChatGPT 通道路由"}
            </p>
          ) : (
            <div className="overflow-hidden rounded-xl border">
              <table className="w-full text-xs">
                <thead className="bg-muted/40 text-muted-foreground">
                  <tr>
                    <th className="px-3 py-2 text-left font-medium">邮箱</th>
                    <th className="px-3 py-2 text-left font-medium">状态</th>
                    <th className="px-3 py-2 text-left font-medium">token</th>
                    <th className="px-3 py-2 text-left font-medium">类型</th>
                    <th className="px-3 py-2 text-left font-medium">添加时间</th>
                  </tr>
                </thead>
                <tbody className="divide-y">
                  {accounts.map((a) => {
                    const hasToken = (a.at_len ?? 0) > 0;
                    return (
                      <tr key={a.email} className="hover:bg-muted/30">
                        <td className="px-3 py-2 font-medium">{a.email}</td>
                        <td className="px-3 py-2">
                          <span
                            className={cn(
                              "rounded px-1.5 py-0.5 text-[10px]",
                              hasToken
                                ? "bg-emerald-500/10 text-emerald-600"
                                : "bg-amber-500/10 text-amber-600",
                            )}
                          >
                            {hasToken ? "可用" : "无 token"}
                          </span>
                        </td>
                        <td className="px-3 py-2 font-mono">
                          {hasToken ? `AT ${a.at_len}` : "—"}
                          {(a.st_len ?? 0) > 0 ? ` · ST ${a.st_len}` : ""}
                          {(a.rt_len ?? 0) > 0 ? ` · RT ${a.rt_len}` : ""}
                        </td>
                        <td className="px-3 py-2">{a.mail_kind || "—"}</td>
                        <td className="px-3 py-2">{fmtTime(a.created_at)}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
          {accountsTotal !== null && accountsTotal > accounts.length && (
            <p className="text-xs text-muted-foreground">
              仅展示最近 {accounts.length} 条（总数 {accountsTotal}），完整列表见 WebUI。
            </p>
          )}
        </CardContent>
      </Card>

      {/* ④ 设置指引卡（v1 不透传设置，指引用户到 WebUI 配置） */}
      <Card className="border-primary/20 bg-primary/[0.02] shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-sm font-semibold">
            <Settings2 className="size-4" />
            设置指引
            <Badge variant="outline" className="px-1.5 py-0 text-[10px]">
              v1 不透传设置
            </Badge>
          </CardTitle>
          <CardDescription className="text-xs">
            批量注册的前置配置（邮箱池 / SMS 接码 / 代理池）均在 gpt-reg-review WebUI
            内完成；通道页 v1 不透传这些设置项。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <ul className="list-inside list-disc space-y-1 text-xs text-muted-foreground">
            <li>邮箱池：配置 cf_temp / icloud_relay / mail_com / outlook 邮箱来源（批量注册的硬前置）</li>
            <li>SMS：接码平台密钥与号码池（2FA / 验证码场景）</li>
            <li>代理池：出口代理列表与指纹画像国家（风控前置）</li>
          </ul>
          <Button size="sm" variant="outline" className="h-8 gap-1 text-xs" asChild>
            <a href={webuiBase} target="_blank" rel="noreferrer">
              <ExternalLink className="size-3.5" />
              打开 WebUI 配置（{webuiBase}，新窗口）
            </a>
          </Button>
        </CardContent>
      </Card>
    </div>
  );
}
