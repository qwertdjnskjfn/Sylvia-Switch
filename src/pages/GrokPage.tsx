import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  BrainCircuit,
  MonitorSmartphone,
  Timer,
  Check,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  Copy,
  Globe,
  KeyRound,
  Loader2,
  Plus,
  RefreshCw,
  ShieldCheck,
  Activity,
  Settings,
  Sparkles,
  Trash2,
  Zap,
  BookOpen,
  Download,
  UserPlus,
  X,
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
import { GatewayAdminSection } from "@/components/gateway/GatewayAdminSection";
import { CartesianGrid, Line, LineChart, XAxis, YAxis } from "recharts";
import { ChartContainer, ChartTooltip, ChartTooltipContent, type ChartConfig } from "@/components/ui/chart";
import { Switch } from "@/components/ui/switch";
import type { GrokAccountInfo, GrokTestResult, GrokPoolOverview, GrokPoolPolicy, GrokPoolSnapshot, GrokRegisterStatus } from "@/lib/types";

const GROK_MODELS = [
  { id: "grok-4.7", name: "Grok 4.7", desc: "最新旗舰 · 支持原生工具调用；免费档无内置联网，实时信息请用 grok-4", badge: "最新" },
  { id: "grok-4.6", name: "Grok 4.6", desc: "已被上游模型目录下线；请求自动回落 web 通道 Grok 4", badge: "已下线" },
  { id: "grok-4", name: "Grok 4", desc: "web 通道自带联网搜索，实时信息问答首选", badge: "推荐" },
  { id: "grok-4-fast", name: "Grok 4 Fast", desc: "Grok 4 高速版，速度与智能平衡", badge: "高速" },
  { id: "grok-4-thinking", name: "Grok 4 Thinking", desc: "Grok 4 深度思考模式，完整思维链推演", badge: "推理" },
  { id: "grok-3", name: "Grok 3", desc: "上一代旗舰基座模型", badge: "经典" },
  { id: "grok-3-mini", name: "Grok 3 Mini", desc: "上一代轻量版，低延迟", badge: "轻量" },
  { id: "grok-2", name: "Grok 2", desc: "旧代基座模型（不建议，智能明显弱于 Grok 4）", badge: "旧代" },
  { id: "grok-2-mini", name: "Grok 2 Mini", desc: "旧代轻量版", badge: "旧代" },
];

// 注册机「零自动触发」说明：与后端 status.autoTriggerNote 保持一致，后端未返回该字段时兜底展示
const REGISTER_AUTO_TRIGGER_NOTE =
  "注册机无任何启动/后台自动触发逻辑；仅在你手动点击「打开注册机」后才会启动专用浏览器";

export default function GrokPage() {
  const [accounts, setAccounts] = useState<GrokAccountInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  // SSO 自动同步状态
  const [ssoStatus, setSsoStatus] = useState<Record<string, unknown> | null>(null);
  const [ssoBusy, setSsoBusy] = useState<string | null>(null);

  // 注册机状态（功能默认关闭、零自动触发：面板默认不加载，仅手动点击入口后才请求接口）
  const [registerOpen, setRegisterOpen] = useState(false);
  const [registerStatus, setRegisterStatus] = useState<GrokRegisterStatus | null>(null);
  const [registerBusy, setRegisterBusy] = useState<string | null>(null);
  const [showRegisterPrinciple, setShowRegisterPrinciple] = useState(false);
  // 自动拉取（2026-09-26 用户要求）：开启后前端定时探测注册机实时登录态，
  // 新登录自动入池（后端按 sso 去重）。仅前端定时驱动，后端零自动触发纪律不变。
  // 开关持久化到 localStorage（2026-09-26 用户反馈：刷新后开关被重置导致"自动拉取没生效"）。
  const [autoCapture, setAutoCapture] = useState(() => {
    try {
      return localStorage.getItem("grok_register_auto_capture") === "1";
    } catch {
      return false;
    }
  });
  const autoSeenRef = useRef<Set<string>>(new Set());
  const registerBusyRef = useRef<string | null>(null);
  const autoWaitingMsgRef = useRef(false);

  // 添加账号弹窗状态
  const [showAdd, setShowAdd] = useState(false);
  const [addProvider, setAddProvider] = useState<"web" | "build" | "console">("web");
  const [addName, setAddName] = useState("");
  const [addToken, setAddToken] = useState("");
  const [addTier, setAddTier] = useState("free");

  // 测试对话状态
  const [testModel, setTestModel] = useState("grok-4.7");
  const [testPrompt, setTestPrompt] = useState("请用一句话介绍 Grok 的核心特色与优势");
  const [enableThinking, setEnableThinking] = useState(false);
  const [selectedAccountId, setSelectedAccountId] = useState<string>("");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<GrokTestResult | null>(null);
  const [testError, setTestError] = useState<string | null>(null);
  const [showThinkDetails, setShowThinkDetails] = useState(true);

  // 复制反馈
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  // 账号池管理（v1.5.0）
  const [pool, setPool] = useState<GrokPoolOverview | null>(null);
  const [poolPolicy, setPoolPolicy] = useState<GrokPoolPolicy | null>(null);
  const [poolBusy, setPoolBusy] = useState<string | null>(null);
  const [showPolicy, setShowPolicy] = useState(false);
  const [history, setHistory] = useState<GrokPoolSnapshot[]>([]);

  const loadPool = useCallback(async () => {
    try {
      const [o, p, h] = await Promise.all([
        api.getGrokPoolOverview(),
        api.getGrokPoolPolicy(),
        api.getGrokPoolHistory(),
      ]);
      setPool(o);
      setPoolPolicy(p);
      setHistory(h.snapshots ?? []);
    } catch {
      // 容错
    }
  }, []);

  useEffect(() => {
    void loadPool();
  }, [loadPool]);

  async function handlePoolCheck() {
    setPoolBusy("check");
    try {
      const res = await api.grokPoolCheck();
      await loadPool();
      await loadAccounts();
      setMsg({ type: "ok", text: `体检完成：检查了 ${res.checked?.length ?? 0} 个账号` });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setPoolBusy(null);
    }
  }

  async function handleSavePolicy(patch: Partial<GrokPoolPolicy>) {
    setPoolBusy("policy");
    try {
      await api.setGrokPoolPolicy(patch);
      await loadPool();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setPoolBusy(null);
    }
  }

  // 趋势图数据（按时间升序，格式化时间标签）
  const chartData = useMemo(
    () =>
      [...history]
        .sort((a, b) => a.at - b.at)
        .map((s) => ({
          time: new Date(s.at).toLocaleString("zh-CN", {
            month: "numeric",
            day: "numeric",
            hour: "2-digit",
            minute: "2-digit",
          }),
          可用: s.ok,
          异常: s.errored,
          健康分: s.avgHealth,
        })),
    [history],
  );

  const poolChartConfig: ChartConfig = {
    可用: { label: "可用", color: "var(--color-emerald-500, #10b981)" },
    异常: { label: "异常", color: "var(--color-rose-500, #f43f5e)" },
    健康分: { label: "平均健康分", color: "var(--color-blue-500, #3b82f6)" },
  };

  const loadAccounts = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.getGrokAccounts();
      setAccounts(res.accounts ?? []);
    } catch {
      // 容错处理
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadAccounts();
  }, [loadAccounts]);

  function copyText(key: string, text: string) {
    void navigator.clipboard.writeText(text);
    setCopiedKey(key);
    setTimeout(() => setCopiedKey(null), 1500);
  }

  async function handleAddAccount() {
    if (!addToken.trim()) {
      setMsg({ type: "err", text: "凭证内容（Cookie 或 API Key）不能为空" });
      return;
    }
    setBusy("add");
    setMsg(null);
    try {
      await api.grokAddAccount({
        name: addName.trim(),
        provider: addProvider,
        token: addToken.trim(),
        tier: addTier,
      });
      setAddName("");
      setAddToken("");
      setShowAdd(false);
      await loadAccounts();
      setMsg({ type: "ok", text: "Grok 账号已成功接入账号池！" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function handleRemoveAccount(id: string) {
    if (!confirm("确定要移除该 Grok 账号吗？")) return;
    setBusy(id);
    try {
      await api.grokRemoveAccount(id);
      await loadAccounts();
      setMsg({ type: "ok", text: "账号已移除" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function handleToggleAccount(account: GrokAccountInfo) {
    setBusy(account.id);
    try {
      await api.grokEnableAccount(account.id, !account.enabled);
      await loadAccounts();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function handleRunTest() {
    if (!testPrompt.trim()) return;
    setTesting(true);
    setTestResult(null);
    setTestError(null);
    try {
      const res = await api.grokTest({
        model: testModel,
        prompt: testPrompt.trim(),
        enableThinking,
        accountId: selectedAccountId || undefined,
      });
      setTestResult(res);
    } catch (e) {
      setTestError(api.asError(e));
    } finally {
      setTesting(false);
    }
  }

  // 拆解思维链标签与正文
  function parseContent(raw: string) {
    const thinkMatch = raw.match(/<think>([\s\S]*?)<\/think>/i);
    if (thinkMatch) {
      const reasoning = thinkMatch[1].trim();
      const content = raw.replace(/<think>[\s\S]*?<\/think>/i, "").trim();
      return { reasoning, content };
    }
    return { reasoning: null, content: raw };
  }

  const loadSso = useCallback(async () => {
    try {
      setSsoStatus(await api.getGrokSsoStatus());
    } catch {
      setSsoStatus(null);
    }
  }, []);

  useEffect(() => {
    void loadSso();
  }, [loadSso]);

  const openLogin = async () => {
    setSsoBusy("login");
    try {
      const res = await api.openGrokSsoLogin();
      if (res.ok) setMsg({ type: "ok", text: "专用浏览器已启动：请在弹出的窗口里登录 grok.com，登录后点「立即同步」。" });
      else setMsg({ type: "err", text: res.error || "启动失败" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSsoBusy(null);
    }
  };

  const syncNow = async () => {
    setSsoBusy("sync");
    try {
      const res = await api.syncGrokSso();
      const status = res.status as string;
      if (status === "ok") {
        // v1.1.0：同步成功即刷新账号池列表，让 grok_web_autosync 立即可见。
        await loadAccounts();
        const poolEntry = (await api.getGrokAccounts()).accounts.find(
          (a) => a.id === "grok_web_autosync",
        );
        setMsg({
          type: "ok",
          text: `同步完成：sso${res.ssoPresent ? "✓" : "✗"} / sso-rw${res.ssoRwPresent ? "✓" : "✗"}，${res.cookieCount} 个 cookie，已写入 Grok 账号池（grok_web_autosync · ${poolEntry?.status === "ok" ? "状态正常" : poolEntry?.status || "?"}），已写盘`,
        });
      } else if (status === "not_logged_in") {
        setMsg({ type: "err", text: "专用浏览器里还没有 grok.com 登录态：请先点「打开登录窗口」登录。" });
      } else if (status === "no_cookies") {
        setMsg({ type: "err", text: "未抓到任何 grok.com cookie，请确认已在该浏览器窗口打开过 grok.com。" });
      } else {
        setMsg({ type: "err", text: `同步失败：${(res as Record<string, unknown>).error || status}（旧 token 未被改动）` });
      }
      await loadSso();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSsoBusy(null);
    }
  };

  // ---- 注册机（专用 Chrome 实例；仅手动触发，零自动）----

  // 只读刷新状态：仅在用户点击后调用；该接口不触发注册、不启动浏览器
  const refreshRegisterStatus = useCallback(async () => {
    try {
      setRegisterStatus(await api.getGrokRegisterStatus());
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    }
  }, []);

  // 入口：手动打开注册机（启动专用 Chrome 并打开 grok.com；未登录时即注册页）
  const openRegister = async () => {
    setRegisterBusy("open");
    try {
      const res = await api.openGrokRegister();
      // 后端可能以 200 + {ok:false} 体返回失败；失败只走 msg 报错、不展开面板
      const raw = res as unknown as Record<string, unknown>;
      if (raw.ok === false) {
        setMsg({ type: "err", text: String(raw.error || raw.message || "打开注册机失败") });
        return;
      }
      setRegisterOpen(true);
      await refreshRegisterStatus();
      setMsg({ type: "ok", text: "注册机已启动：请在专用 Chrome 窗口里按 grok.com 流程完成新账号注册，完成后点「抓取注册成果」。" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setRegisterBusy(null);
    }
  };

  // 抓取注册成果：读取 sso/sso-rw cookie，按 sso 去重入池（同号原位刷新、新登录新建账号）
  const captureRegister = async () => {
    setRegisterBusy("capture");
    try {
      const res = await api.captureGrokRegister();
      if (res.ok) {
        // 抓取成功即刷新账号池列表，让新入池账号立即可见
        await loadAccounts();
        await refreshRegisterStatus();
        setMsg({
          type: "ok",
          text: `注册成果已抓取：${res.cookieCount ?? "?"} 个 cookie，入池账号 ${res.accountId || "grok_register"}（同 sso 重复抓取原位刷新；新登录将新建独立账号）`,
        });
      } else if (res.status === "not_registered") {
        setMsg({
          type: "err",
          text:
            res.message ||
            "注册机窗口的实时会话里没有登录态（未登录或已失效）：请在窗口内完成注册/重新登录后再抓取。",
        });
      } else {
        setMsg({ type: "err", text: `抓取失败：${res.error || res.status}` });
      }
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setRegisterBusy(null);
    }
  };

  // 关闭注册机：只结束带该 profile 目录的 Chrome 进程，不影响日常浏览器
  const closeRegister = async () => {
    setRegisterBusy("close");
    try {
      const res = await api.closeGrokRegister();
      // 同 openRegister：兼容 200 + {ok:false} 的失败体
      const raw = res as unknown as Record<string, unknown>;
      if (raw.ok === false) {
        setMsg({ type: "err", text: String(raw.error || raw.message || "关闭注册机失败") });
        return;
      }
      setRegisterStatus(res);
      setMsg({
        type: raw.ok ? "ok" : "err",
        text: String(raw.message || (raw.ok ? "注册机已关闭：专用 Chrome 实例已结束，日常浏览器不受影响。" : "关闭注册机失败")),
      });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setRegisterBusy(null);
    }
  };

  // busy 态镜像（自动拉取循环里读取，避免闭包读到过期值）
  useEffect(() => {
    registerBusyRef.current = registerBusy;
  }, [registerBusy]);

  // 自动拉取循环（2026-09-26 用户要求）：开关开启且面板展开时，每 8 秒探测一次
  // 注册机实时登录态；新登录（后端按 sso 去重判新）自动入池并提示，同号静默刷新。
  // 仅前端定时驱动：关开关 / 收起面板 / 离开页面即停；后端零自动触发纪律不变。
  // 注册机未运行时给一次性待机提示（不刷屏），重新打开后自动恢复拉取。
  useEffect(() => {
    if (!autoCapture || !registerOpen) return;
    let stopped = false;
    const tick = async () => {
      if (stopped || registerBusyRef.current !== null) return;
      try {
        const res = await api.captureGrokRegister();
        if (res.ok && res.accountId) {
          autoWaitingMsgRef.current = false;
          if (!autoSeenRef.current.has(res.accountId)) {
            autoSeenRef.current.add(res.accountId);
            await loadAccounts();
            setMsg({
              type: "ok",
              text: `自动拉取：新账号已入池 ${res.accountId}（${res.cookieCount ?? "?"} 个 cookie）。可继续在注册机窗口注册下一个账号。`,
            });
          }
          await refreshRegisterStatus();
        } else if (res.status === "error" && !autoWaitingMsgRef.current) {
          // 注册机未启动/CDP 无响应：一次性待机提示，不刷屏
          autoWaitingMsgRef.current = true;
          setMsg({ type: "err", text: "自动拉取待机中：注册机未运行。点「打开注册机」后自动恢复拉取。" });
        } else if (res.status === "not_registered") {
          autoWaitingMsgRef.current = false; // 注册机在跑、只是还没登录态——正常注册中
        }
      } catch {
        // 静默（网络抖动），下一轮重试
      }
    };
    void tick();
    const id = window.setInterval(() => void tick(), 8000);
    return () => {
      stopped = true;
      window.clearInterval(id);
    };
  }, [autoCapture, registerOpen]);

  /** 自动拉取开关（含 localStorage 持久化：刷新页面不丢状态）。 */
  const toggleAutoCapture = useCallback((on: boolean) => {
    setAutoCapture(on);
    try {
      localStorage.setItem("grok_register_auto_capture", on ? "1" : "0");
    } catch {
      // localStorage 不可用时仅内存态
    }
  }, []);

  return (
    <div className="mx-auto max-w-5xl space-y-6 pb-12">
      {/* 顶部标题栏 */}
      <div className="flex flex-wrap items-center justify-between gap-4 border-b pb-4">
        <div>
          <div className="flex items-center gap-2.5">
            <div className="flex size-9 items-center justify-center rounded-xl bg-primary/10 text-primary">
              <Sparkles className="size-5" />
            </div>
            <div>
              <h1 className="text-xl font-bold tracking-tight">Grok 融合网关</h1>
              <p className="text-xs text-muted-foreground">
                移植自 chenyme/grok2api 开源项目，支持 Grok Web SSO 与 Build 开发者接口，提供完整 OpenAI / Anthropic 兼容能力
              </p>
            </div>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <Badge variant="outline" className="border-emerald-500/30 bg-emerald-500/10 text-emerald-600 text-xs">
            网关在线 · 端口 57890
          </Badge>
          <Button
            variant="outline"
            size="sm"
            onClick={() => void loadAccounts()}
            disabled={loading}
            className="h-8 gap-1 text-xs"
          >
            <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
            刷新
          </Button>
          <Button
            size="sm"
            onClick={() => setShowAdd(!showAdd)}
            className="h-8 gap-1 text-xs"
          >
            <Plus className="size-3.5" />
            添加账号
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

      {/* SSO 自动同步 */}
      <Card className="border-primary/20 bg-primary/[0.02] shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-sm font-semibold">
            <MonitorSmartphone className="size-4" />
            SSO Cookie 自动同步
            <Badge variant="outline" className="px-1.5 py-0 text-[10px]">
              v{(ssoStatus?.toolVersion as string) || "1.0.0"} · 每 30 分钟
            </Badge>
          </CardTitle>
          <CardDescription className="text-xs">
            专用浏览器实例（独立 profile，不影响日常 Chrome）里登录 grok.com 一次，工具定时经 CDP
            抓取最新 sso / sso-rw 并自动更新到融合网关账号池，登录态长期有效。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <Button size="sm" variant="outline" className="h-8 gap-1 text-xs" disabled={ssoBusy !== null} onClick={() => void openLogin()}>
              <MonitorSmartphone className={cn("size-3.5", ssoBusy === "login" && "animate-pulse")} />
              打开登录窗口
            </Button>
            <Button size="sm" variant="outline" className="h-8 gap-1 text-xs" disabled={ssoBusy !== null} onClick={() => void syncNow()}>
              <Timer className={cn("size-3.5", ssoBusy === "sync" && "animate-spin")} />
              立即同步
            </Button>
          </div>
          <div className="grid grid-cols-2 gap-2 text-xs sm:grid-cols-4">
            <div>
              <span className="text-muted-foreground">登录态：</span>
              {ssoStatus?.status === "ok" ? (
                <span className="font-medium text-emerald-600">已登录</span>
              ) : ssoStatus?.status === "empty" ? (
                <span className="text-amber-600">尚未同步</span>
              ) : (
                <span className="text-destructive">{String(ssoStatus?.status || "未知")}</span>
              )}
            </div>
            <div>
              <span className="text-muted-foreground">账号池：</span>
              {(() => {
                const entry = accounts.find((a) => a.id === "grok_web_autosync");
                if (!entry) return <span className="text-muted-foreground">未接入</span>;
                return (
                  <span className={cn("font-medium", entry.status === "ok" ? "text-emerald-600" : "text-amber-600")}>
                    已接入 · {entry.status === "ok" ? "正常" : entry.status}
                  </span>
                );
              })()}
            </div>
            <div>
              <span className="text-muted-foreground">有效期：</span>
              {typeof ssoStatus?.expiresInDays === "number" ? (
                <span className={cn("font-medium", (ssoStatus.expiresInDays as number) <= 2 ? "text-amber-600" : "text-foreground")}>
                  剩 {(ssoStatus.expiresInDays as number)} 天
                </span>
              ) : (
                <span>—</span>
              )}
            </div>
            <div>
              <span className="text-muted-foreground">sso 预览：</span>
              <span className="font-mono">{String(ssoStatus?.sso || "—")}</span>
            </div>
            <div>
              <span className="text-muted-foreground">cookie 数：</span>
              <span className="tabular-nums">{String(ssoStatus?.cookieCount ?? "—")}</span>
            </div>
          </div>
        </CardContent>
      </Card>

      {/* 注册机（专用 Chrome 实例；默认关闭，仅手动打开，零自动触发） */}
      <Card className="border-primary/20 bg-primary/[0.02] shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-sm font-semibold">
            <UserPlus className="size-4" />
            注册机 · Grok 账号注册专用浏览器
            <Badge variant="outline" className="border-emerald-500/30 bg-emerald-500/10 px-1.5 py-0 text-[10px] text-emerald-600">
              仅手动 · 零自动触发
            </Badge>
          </CardTitle>
          <CardDescription className="text-xs">
            注册机是网关专用的一个 Chrome 实例：独立 profile（不影响日常 Chrome）、固定 CDP 调试端口 19233；
            未登录时打开 grok.com 即是注册页，人工完成注册后可一键抓取成果写入账号池。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          {!registerOpen ? (
            /* 未展开态（默认）：只渲染入口按钮，此态不请求任何接口 */
            <div className="space-y-2">
              <Button
                size="sm"
                className="h-8 gap-1 text-xs"
                disabled={registerBusy !== null}
                onClick={() => void openRegister()}
              >
                {registerBusy === "open" ? <Loader2 className="size-3.5 animate-spin" /> : <UserPlus className="size-3.5" />}
                {registerBusy === "open" ? "正在打开注册机…" : "打开注册机（手动）"}
              </Button>
              <p className="text-xs text-muted-foreground">
                注册功能默认关闭：只有点击上面的按钮才会启动专用浏览器，网关不会在开机、后台循环或任何计划任务中自动拉起它。
              </p>
            </div>
          ) : (
            <div className="space-y-3">
              {/* 操作按钮行（打开注册机重复点击幂等：复用同一注册机实例） */}
              <div className="flex flex-wrap items-center gap-2">
                <Button
                  size="sm"
                  className="h-8 gap-1 text-xs"
                  disabled={registerBusy !== null}
                  onClick={() => void openRegister()}
                >
                  {registerBusy === "open" ? <Loader2 className="size-3.5 animate-spin" /> : <UserPlus className="size-3.5" />}
                  打开注册机
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  className="h-8 gap-1 text-xs"
                  disabled={registerBusy !== null}
                  onClick={() => void captureRegister()}
                >
                  {registerBusy === "capture" ? <Loader2 className="size-3.5 animate-spin" /> : <Download className="size-3.5" />}
                  抓取注册成果
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  className="h-8 gap-1 text-xs"
                  disabled={registerBusy !== null}
                  onClick={() => void closeRegister()}
                >
                  {registerBusy === "close" ? <Loader2 className="size-3.5 animate-spin" /> : <X className="size-3.5" />}
                  关闭注册机
                </Button>
                {/* 自动拉取开关（2026-09-26 用户要求）：开启后前端定时探测实时登录态，
                    新登录自动入池——批量注册时无需逐个手动点「抓取注册成果」。 */}
                <div className="ml-auto flex items-center gap-2 rounded-lg border bg-muted/20 px-2.5 py-1.5">
                  <Switch
                    checked={autoCapture}
                    onCheckedChange={toggleAutoCapture}
                    disabled={registerBusy !== null}
                    aria-label="自动拉取开关"
                  />
                  <div className="text-xs leading-4">
                    <span className="font-medium">自动拉取</span>
                    <div className="text-[10px] text-muted-foreground">
                      {autoCapture
                        ? "开启中 · 每 8 秒探测实时登录态，新账号自动入池"
                        : "注册完成自动入池，无需手动点抓取"}
                    </div>
                  </div>
                </div>
              </div>

              {/* 状态网格（样式对齐 SSO 卡片） */}
              <div className="grid grid-cols-2 gap-2 text-xs sm:grid-cols-4">
                <div>
                  <span className="text-muted-foreground">注册机状态：</span>
                  {registerStatus?.phase === "captured" ? (
                    <Badge variant="outline" className="border-emerald-500/30 bg-emerald-500/10 px-1.5 py-0 text-[10px] text-emerald-600">成果已入池</Badge>
                  ) : registerStatus?.phase === "open" ? (
                    <Badge variant="outline" className="border-emerald-500/30 bg-emerald-500/10 px-1.5 py-0 text-[10px] text-emerald-600">运行中</Badge>
                  ) : (
                    <Badge variant="outline" className="px-1.5 py-0 text-[10px] text-muted-foreground">待启动</Badge>
                  )}
                </div>
                <div>
                  <span className="text-muted-foreground">专用浏览器：</span>
                  {registerStatus?.browserRunning ? (
                    <span className="font-medium text-emerald-600">运行中</span>
                  ) : (
                    <span className="text-muted-foreground">未启动</span>
                  )}
                </div>
                <div>
                  <span className="text-muted-foreground">CDP 端口：</span>
                  <span className="font-mono tabular-nums">{registerStatus?.cdpPort ?? 19233}</span>
                </div>
                <div>
                  <span className="text-muted-foreground">当前登录态：</span>
                  {registerStatus?.loginState === "logged_in" ? (
                    <span className="font-medium text-emerald-600">已登录</span>
                  ) : registerStatus?.loginState === "not_logged_in" ? (
                    <span className="font-medium text-amber-600">未登录 · 可注册</span>
                  ) : (
                    <span className="text-muted-foreground">未知</span>
                  )}
                </div>
                <div>
                  <span className="text-muted-foreground">sso 预览：</span>
                  <span className="font-mono">{registerStatus?.ssoPreview || "—"}</span>
                </div>
                <div>
                  <span className="text-muted-foreground">cookie 数：</span>
                  <span className="tabular-nums">{registerStatus?.cookieCount ?? "—"}</span>
                </div>
                <div>
                  <span className="text-muted-foreground">手动打开次数：</span>
                  <span className="tabular-nums">{registerStatus?.manualOpenCount ?? 0}</span>
                </div>
                <div>
                  <span className="text-muted-foreground">最近打开：</span>
                  <span>{registerStatus?.lastOpenedAt ? new Date(registerStatus.lastOpenedAt).toLocaleString("zh-CN") : "—"}</span>
                </div>
              </div>

              {/* 自动触发说明行：后端原文 + 醒目的零自动标记 + 自动拉取边界说明 */}
              <div className="rounded-lg border bg-muted/20 p-2.5 text-xs text-muted-foreground">
                <span className="font-medium text-emerald-600">✓ 无启动/后台自动触发</span>
                <span className="mx-1.5">·</span>
                {registerStatus?.autoTriggerNote || REGISTER_AUTO_TRIGGER_NOTE}
                <div className="mt-1">
                  「自动拉取」是前端可选开关（默认关，由你控制）：只自动读取注册机窗口里
                  <strong>已完成</strong>的登录态入池，不代填、不自动提交任何注册动作；关闭页面或关掉开关即停止。
                </div>
                {registerStatus?.message && <div className="mt-1">{registerStatus.message}</div>}
              </div>

              {/* 「工作原理」折叠面板：内置原理说明全文（与 docs/grok-register-machine.md 一致） */}
              <div className="rounded-lg border border-purple-500/20 bg-purple-500/[0.03] p-3">
                <button
                  type="button"
                  onClick={() => setShowRegisterPrinciple(!showRegisterPrinciple)}
                  className="flex w-full items-center justify-between text-[11px] font-semibold text-purple-600"
                >
                  <div className="flex items-center gap-1.5">
                    <BookOpen className="size-3.5" />
                    <span>注册机工作原理</span>
                  </div>
                  {showRegisterPrinciple ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
                </button>
                {showRegisterPrinciple && (
                  <div className="mt-2.5 space-y-2 text-[11px] leading-relaxed text-muted-foreground">
                    <p>注册机是 Grok 融合网关专用的一个 Chrome 实例：独立 profile（<code className="rounded bg-muted px-1 py-0.5 font-mono">~/.wb-switch/grok-chrome-profile</code>，与你日常用的 Chrome 完全隔离）、固定调试端口 19233、非 headless 常驻窗口。</p>
                    <p className="font-medium text-foreground">执行流程：</p>
                    <ol className="list-inside list-decimal space-y-1">
                      <li><strong>手动启动</strong>：你点击「打开注册机」——这是注册机唯一的启动方式，网关不会在开机、后台循环或任何计划任务中自动拉起它。</li>
                      <li><strong>打开注册页</strong>：注册机以调试模式启动 Chrome 并打开 <code className="rounded bg-muted px-1 py-0.5 font-mono">https://grok.com</code>；未登录时该页面就是注册页。</li>
                      <li><strong>人工注册</strong>：在注册机窗口里按 grok.com 的流程完成新账号注册（填邮箱、验证码等由你本人操作；注册机不代填、不自动提交）。</li>
                      <li><strong>抓取成果</strong>：注册完成后点「抓取注册成果」，网关经 CDP 的 <code className="rounded bg-muted px-1 py-0.5 font-mono">Network.getCookies</code> 读取该实例的 sso / sso-rw 等 cookie（含 httpOnly），拼成账号凭证写入 Grok 账号池（账号 ID <code className="rounded bg-muted px-1 py-0.5 font-mono">grok_register</code>）。</li>
                      <li><strong>入池即用</strong>：该账号随即参与 web 通道的轮询调度；之后每次同步/保活都会经同一实例续期，无需重复注册。</li>
                    </ol>
                    <p className="font-medium text-foreground">设计纪律：</p>
                    <ul className="list-inside list-disc space-y-1">
                      <li><strong>零自动触发</strong>：模块不注册任何后台循环（无 30 分钟同步、无保活、无体检会启动它）；<code className="rounded bg-muted px-1 py-0.5 font-mono">status</code> 接口只读探测，不会启动浏览器。</li>
                      <li><strong>会话隔离</strong>：专用 profile 不影响日常 Chrome；多个功能共用同一实例，手动「立即同步」会把你当前的登录态写回 autosync 账号。</li>
                      <li><strong>进程精准</strong>：「关闭注册机」只结束带该 profile 目录命令行参数的 Chrome 进程，绝不误杀日常浏览器。</li>
                    </ul>
                  </div>
                )}
              </div>
            </div>
          )}
        </CardContent>
      </Card>

      {/* 添加账号折叠区 */}
      {showAdd && (
        <Card className="border-primary/20 bg-primary/[0.02] shadow-sm">
          <CardHeader className="pb-3">
            <CardTitle className="text-sm font-semibold">接入新的 Grok 账号凭证</CardTitle>
            <CardDescription className="text-xs">
              支持网页端 SSO Cookie 反代或开发者 API Key，添加后自动纳入轮询调度池
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
              <div>
                <label className="text-xs font-medium text-muted-foreground">渠道类型</label>
                <select
                  value={addProvider}
                  onChange={(e) => setAddProvider(e.target.value as "web" | "build" | "console")}
                  className="mt-1 block w-full rounded-md border border-input bg-background px-3 py-1.5 text-xs shadow-sm focus:border-primary focus:outline-none focus:ring-1 focus:ring-primary"
                >
                  <option value="web">Grok Web (网页端 SSO Cookie)</option>
                  <option value="build">Grok Build (开发者 API Key)</option>
                  <option value="console">Grok Console (控制台凭据)</option>
                </select>
              </div>
              <div>
                <label className="text-xs font-medium text-muted-foreground">账号备注（选填）</label>
                <Input
                  placeholder="例如：主号、网页版备用号"
                  value={addName}
                  onChange={(e) => setAddName(e.target.value)}
                  className="mt-1 h-8 text-xs"
                />
              </div>
              <div>
                <label className="text-xs font-medium text-muted-foreground">订阅级别</label>
                <select
                  value={addTier}
                  onChange={(e) => setAddTier(e.target.value)}
                  className="mt-1 block w-full rounded-md border border-input bg-background px-3 py-1.5 text-xs shadow-sm focus:border-primary focus:outline-none focus:ring-1 focus:ring-primary"
                >
                  <option value="free">免费用户 (Free Tier)</option>
                  <option value="super">Premium / Super (付费会员)</option>
                  <option value="enterprise">Enterprise (企业级)</option>
                </select>
              </div>
            </div>

            <div>
              <label className="text-xs font-medium text-muted-foreground">
                {addProvider === "web"
                  ? "SSO Cookie（登录 grok.com 后在 DevTools 中提取的 sso / sso-rw 值，或整个 Cookie 字符串）"
                  : "API Key / Access Token（xai-... 或 Bearer Token）"}
              </label>
              <textarea
                rows={3}
                placeholder={
                  addProvider === "web"
                    ? "例如：sso=eyJhbGciOi...; 或直接粘贴完整 sso Cookie 字符串"
                    : "例如：xai-xxxxxxxxxxxx"
                }
                value={addToken}
                onChange={(e) => setAddToken(e.target.value)}
                className="mt-1 block w-full rounded-md border border-input bg-background p-2.5 font-mono text-xs shadow-sm focus:border-primary focus:outline-none focus:ring-1 focus:ring-primary"
              />
            </div>

            <div className="flex items-center justify-end gap-2 pt-1">
              <Button
                variant="ghost"
                size="sm"
                onClick={() => setShowAdd(false)}
                className="h-8 text-xs"
              >
                取消
              </Button>
              <Button
                size="sm"
                onClick={() => void handleAddAccount()}
                disabled={busy === "add"}
                className="h-8 gap-1.5 text-xs"
              >
                {busy === "add" ? <Loader2 className="size-3.5 animate-spin" /> : <Check className="size-3.5" />}
                保存凭证并入池
              </Button>
            </div>
          </CardContent>
        </Card>
      )}

      {/* 账号池卡片列表 */}
      <Card>
        <CardHeader className="pb-3">
          <div className="flex items-center justify-between">
            <div>
              <CardTitle className="text-base font-semibold">Grok 账号池</CardTitle>
              <CardDescription className="text-xs">
                系统对请求进行智能负载轮询与自动故障降级；遇到 429 会自动将账号冷却 60 秒
              </CardDescription>
            </div>
            <span className="text-xs text-muted-foreground">
              共 <strong className="text-foreground">{accounts.length}</strong> 个账号
            </span>
          </div>
        </CardHeader>
        <CardContent>
          {accounts.length === 0 ? (
            <div className="flex flex-col items-center justify-center rounded-xl border border-dashed py-10 text-center">
              <KeyRound className="size-8 text-muted-foreground/50" />
              <p className="mt-2 text-sm font-medium">账号池暂无已配置账号</p>
              <p className="mt-1 text-xs text-muted-foreground">
                请点击上方“添加账号”，粘贴 Grok Web SSO Cookie 或 Build API Key 开通网关调用
              </p>
              <Button
                size="sm"
                variant="outline"
                onClick={() => setShowAdd(true)}
                className="mt-4 h-8 gap-1 text-xs"
              >
                <Plus className="size-3.5" />
                立即添加第一个账号
              </Button>
            </div>
          ) : (
            <div className="divide-y rounded-xl border">
              {accounts.map((acc) => (
                <div
                  key={acc.id}
                  className="flex flex-wrap items-center justify-between gap-3 p-3.5 transition-colors hover:bg-muted/40"
                >
                  <div className="flex items-center gap-3">
                    <div className="flex size-8 items-center justify-center rounded-lg bg-primary/10 text-primary">
                      {acc.provider === "build" ? <Zap className="size-4" /> : <Globe className="size-4" />}
                    </div>
                    <div>
                      <div className="flex items-center gap-2">
                        <span className="text-sm font-medium">{acc.name}</span>
                        <Badge
                          variant="secondary"
                          className={cn(
                            "h-5 text-[10px]",
                            acc.provider === "build"
                              ? "bg-emerald-500/10 text-emerald-600"
                              : "bg-blue-500/10 text-blue-600",
                          )}
                        >
                          {acc.provider === "build" ? "Build 开发者" : "Web 网页逆向"}
                        </Badge>
                        {acc.isCooling && (
                          <Badge variant="outline" className="h-5 border-amber-500/30 bg-amber-500/10 text-[10px] text-amber-600">
                            冷却降级中
                          </Badge>
                        )}
                        {!acc.enabled && (
                          <Badge variant="outline" className="h-5 text-[10px] text-muted-foreground">
                            已禁用
                          </Badge>
                        )}
                      </div>
                      <div className="mt-1 flex items-center gap-3 text-xs text-muted-foreground">
                        <span>凭证：<code className="rounded bg-muted px-1 py-0.5 font-mono">{acc.tokenMasked || "******"}</code></span>
                        {acc.tier && <span>级别：{acc.tier}</span>}
                        {acc.lastUsedAt && (
                          <span>最近使用：{new Date(acc.lastUsedAt).toLocaleTimeString()}</span>
                        )}
                      </div>
                      {acc.error && (
                        <p className="mt-1 text-xs text-rose-500">最近错误：{acc.error}</p>
                      )}
                    </div>
                  </div>

                  <div className="flex items-center gap-2">
                    <Button
                      variant={acc.enabled ? "outline" : "secondary"}
                      size="sm"
                      onClick={() => void handleToggleAccount(acc)}
                      disabled={busy === acc.id}
                      className="h-8 text-xs"
                    >
                      {acc.enabled ? "暂停" : "启用"}
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => void handleRemoveAccount(acc.id)}
                      disabled={busy === acc.id}
                      className="h-8 text-xs text-muted-foreground hover:text-destructive"
                    >
                      <Trash2 className="size-3.5" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>

      {/* 账号池健康管理（v1.5.0） */}
      <Card className="border-emerald-500/20 bg-emerald-500/[0.02]">
        <CardHeader className="pb-3">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <ShieldCheck className="size-4 text-emerald-600" />
              <CardTitle className="text-base font-semibold">账号池健康管理</CardTitle>
            </div>
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                variant="outline"
                className="h-8 gap-1 text-xs"
                disabled={poolBusy !== null}
                onClick={() => void handlePoolCheck()}
              >
                <Activity className={cn("size-3.5", poolBusy === "check" && "animate-pulse")} />
                立即体检
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-8 text-xs"
                onClick={() => setShowPolicy(!showPolicy)}
              >
                <Settings className="size-3.5" />
                策略
              </Button>
            </div>
          </div>
          <CardDescription className="text-xs">
            低频轮转体检 + 健康分 + 风险分级，异常账号自动降权/退避，维护账号池长期可用
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {/* 总览 */}
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
            <div className="rounded-lg border p-2.5">
              <div className="text-xs text-muted-foreground">账号总数</div>
              <div className="mt-0.5 text-lg font-semibold tabular-nums">{pool?.total ?? "—"}</div>
            </div>
            <div className="rounded-lg border p-2.5">
              <div className="text-xs text-muted-foreground">可用 / 异常</div>
              <div className="mt-0.5 text-lg font-semibold tabular-nums">
                <span className="text-emerald-600">{pool?.ok ?? "—"}</span>
                <span className="text-muted-foreground"> / </span>
                <span className="text-rose-500">{pool?.errored ?? "—"}</span>
              </div>
            </div>
            <div className="rounded-lg border p-2.5">
              <div className="text-xs text-muted-foreground">冷却中</div>
              <div className="mt-0.5 text-lg font-semibold tabular-nums text-amber-600">{pool?.cooling ?? "—"}</div>
            </div>
            <div className="rounded-lg border p-2.5">
              <div className="text-xs text-muted-foreground">风险 低/中/高</div>
              <div className="mt-0.5 text-sm font-semibold tabular-nums">
                <span className="text-emerald-600">{pool?.risk.low ?? 0}</span>
                <span className="text-muted-foreground"> / </span>
                <span className="text-amber-600">{pool?.risk.medium ?? 0}</span>
                <span className="text-muted-foreground"> / </span>
                <span className="text-rose-500">{pool?.risk.high ?? 0}</span>
              </div>
            </div>
          </div>

          {/* 容量守卫告警：可用账号低于策略下限时提醒（暂停调度时给出明确原因） */}
          {pool?.capacity && pool.capacity.available < pool.capacity.minPoolSize && (
            <Alert className={cn("py-2.5", pool.capacity.belowMin ? "border-amber-500/30 bg-amber-500/[0.06]" : "border-border")}>
              <CircleAlert className={cn("size-4", pool.capacity.belowMin ? "text-amber-600" : "text-muted-foreground")} />
              <AlertTitle className="text-xs font-medium">
                {pool.capacity.belowMin ? "账号池低于容量下限，调度已暂停" : "账号池低于容量下限（仅提醒）"}
              </AlertTitle>
              <AlertDescription className="text-xs">
                当前可用账号 <strong>{pool.capacity.available}</strong> 个，低于容量下限 <strong>{pool.capacity.minPoolSize}</strong> 个。
                {pool.capacity.belowMin
                  ? "新的 chat / 测试请求会直接失败并提示补充账号；请添加账号或在「策略」中调整下限。"
                  : "「低于下限暂停调度」已关闭，请求仍会继续调度；建议尽快补充账号。"}
              </AlertDescription>
            </Alert>
          )}

          {/* 账号健康明细 */}
          {pool && pool.accounts.length > 0 && (
            <div className="overflow-hidden rounded-xl border">
              <table className="w-full text-xs">
                <thead className="bg-muted/40 text-muted-foreground">
                  <tr>
                    <th className="px-3 py-2 text-left font-medium">账号</th>
                    <th className="px-3 py-2 text-left font-medium">状态</th>
                    <th className="px-3 py-2 text-left font-medium">健康分</th>
                    <th className="px-3 py-2 text-left font-medium">风险</th>
                    <th className="px-3 py-2 text-left font-medium">连续失败</th>
                    <th className="px-3 py-2 text-left font-medium">限流次数</th>
                    <th className="px-3 py-2 text-left font-medium">最近体检</th>
                  </tr>
                </thead>
                <tbody className="divide-y">
                  {pool.accounts.map((a) => (
                    <tr key={a.id} className="hover:bg-muted/30">
                      <td className="px-3 py-2 font-medium">{a.name}</td>
                      <td className="px-3 py-2">
                        <span className={cn(
                          "rounded px-1.5 py-0.5 text-[10px]",
                          a.status === "ok" ? "bg-emerald-500/10 text-emerald-600"
                            : a.status === "cooldown" ? "bg-amber-500/10 text-amber-600"
                            : "bg-rose-500/10 text-rose-600",
                        )}>
                          {a.status === "ok" ? "正常" : a.status === "cooldown" ? "冷却" : a.status === "error" ? "异常" : a.status}
                        </span>
                      </td>
                      <td className="px-3 py-2">
                        <div className="flex items-center gap-1.5">
                          <div className="h-1.5 w-12 overflow-hidden rounded-full bg-muted">
                            <div
                              className={cn("h-full rounded-full",
                                a.healthScore >= 80 ? "bg-emerald-500" : a.healthScore >= 50 ? "bg-amber-500" : "bg-rose-500")}
                              style={{ width: `${a.healthScore}%` }}
                            />
                          </div>
                          <span className="tabular-nums">{a.healthScore}</span>
                        </div>
                      </td>
                      <td className="px-3 py-2">
                        <span className={cn(
                          "rounded px-1.5 py-0.5 text-[10px]",
                          a.riskLevel === "low" ? "bg-emerald-500/10 text-emerald-600"
                            : a.riskLevel === "medium" ? "bg-amber-500/10 text-amber-600"
                            : "bg-rose-500/10 text-rose-600",
                        )}>
                          {a.riskLevel === "low" ? "低" : a.riskLevel === "medium" ? "中" : a.riskLevel === "high" ? "高" : a.riskLevel}
                        </span>
                      </td>
                      <td className="px-3 py-2 tabular-nums">{a.consecutiveFailures}</td>
                      <td className="px-3 py-2 tabular-nums">{a.rateLimitCount}</td>
                      <td className="px-3 py-2 text-muted-foreground">
                        {a.lastCheckAt ? new Date(a.lastCheckAt).toLocaleTimeString() : "—"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}

          {/* 趋势图（C3） */}
          {chartData.length > 0 && (
            <div className="rounded-xl border p-3">
              <div className="mb-2 flex items-center justify-between">
                <span className="text-xs font-medium">健康趋势（近 7 天）</span>
                <span className="text-[11px] text-muted-foreground">共 {chartData.length} 个采样点</span>
              </div>
              <ChartContainer config={poolChartConfig} className="h-48 w-full">
                <LineChart data={chartData} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
                  <CartesianGrid vertical={false} strokeDasharray="3 3" />
                  <XAxis dataKey="time" tickLine={false} axisLine={false} tickMargin={8} fontSize={10} />
                  <YAxis tickLine={false} axisLine={false} width={36} fontSize={10} />
                  <ChartTooltip content={<ChartTooltipContent />} />
                  <Line type="monotone" dataKey="可用" stroke="var(--color-emerald-500, #10b981)" strokeWidth={2} dot={false} isAnimationActive={false} />
                  <Line type="monotone" dataKey="异常" stroke="var(--color-rose-500, #f43f5e)" strokeWidth={2} dot={false} isAnimationActive={false} />
                  <Line type="monotone" dataKey="健康分" stroke="var(--color-blue-500, #3b82f6)" strokeWidth={1.5} strokeDasharray="4 2" dot={false} isAnimationActive={false} />
                </LineChart>
              </ChartContainer>
            </div>
          )}

          {/* 策略配置 */}
          {showPolicy && poolPolicy && (
            <div className="space-y-3 rounded-xl border bg-muted/20 p-3">
              <div className="text-xs font-medium">监控策略</div>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
                <label className="flex items-center gap-2 text-xs">
                  <input
                    type="checkbox"
                    checked={poolPolicy.autoCheckEnabled}
                    onChange={(e) => void handleSavePolicy({ autoCheckEnabled: e.target.checked })}
                    className="rounded border-input text-primary focus:ring-primary"
                  />
                  启用自动体检
                </label>
                <div>
                  <label className="text-xs text-muted-foreground">体检间隔（分钟）</label>
                  <Input
                    type="number"
                    defaultValue={poolPolicy.checkIntervalMinutes}
                    onBlur={(e) => void handleSavePolicy({ checkIntervalMinutes: Number(e.target.value) })}
                    className="mt-1 h-8 text-xs"
                  />
                </div>
                <div>
                  <label className="text-xs text-muted-foreground">账号驻留时长（分钟）</label>
                  <Input
                    type="number"
                    min={5}
                    max={120}
                    step={1}
                    defaultValue={poolPolicy.resident_ttl_min ?? 30}
                    onBlur={(e) => void handleSavePolicy({ resident_ttl_min: Number(e.target.value) })}
                    className="mt-1 h-8 text-xs"
                  />
                </div>
                <div>
                  <label className="text-xs text-muted-foreground">每轮体检账号数</label>
                  <Input
                    type="number"
                    defaultValue={poolPolicy.batchSize}
                    onBlur={(e) => void handleSavePolicy({ batchSize: Number(e.target.value) })}
                    className="mt-1 h-8 text-xs"
                  />
                </div>
                <div>
                  <label className="text-xs text-muted-foreground">低风险阈值</label>
                  <Input
                    type="number"
                    defaultValue={poolPolicy.riskLowThreshold}
                    onBlur={(e) => void handleSavePolicy({ riskLowThreshold: Number(e.target.value) })}
                    className="mt-1 h-8 text-xs"
                  />
                </div>
                <div>
                  <label className="text-xs text-muted-foreground">高风险阈值</label>
                  <Input
                    type="number"
                    defaultValue={poolPolicy.riskHighThreshold}
                    onBlur={(e) => void handleSavePolicy({ riskHighThreshold: Number(e.target.value) })}
                    className="mt-1 h-8 text-xs"
                  />
                </div>
                <label className="flex items-center gap-2 text-xs">
                  <input
                    type="checkbox"
                    checked={poolPolicy.pauseHighRisk}
                    onChange={(e) => void handleSavePolicy({ pauseHighRisk: e.target.checked })}
                    className="rounded border-input text-primary focus:ring-primary"
                  />
                  高风险账号自动熔断（暂停）
                </label>
                <div>
                  <label className="text-xs text-muted-foreground">容量下限（可用账号数）</label>
                  <Input
                    type="number"
                    min={1}
                    defaultValue={poolPolicy.minPoolSize}
                    onBlur={(e) => void handleSavePolicy({ minPoolSize: Number(e.target.value) })}
                    className="mt-1 h-8 text-xs"
                  />
                </div>
                <label className="flex items-center gap-2 text-xs">
                  <input
                    type="checkbox"
                    checked={poolPolicy.pauseWhenBelowMin}
                    onChange={(e) => void handleSavePolicy({ pauseWhenBelowMin: e.target.checked })}
                    className="rounded border-input text-primary focus:ring-primary"
                  />
                  低于容量下限时暂停调度
                </label>
              </div>
              <p className="text-[11px] text-muted-foreground">
                体检为低频轮转（默认每 2 小时 1 个账号），行为接近真人使用；熔断宁可少用也不硬刚，降低风控风险。
                容量守卫：可用账号低于下限时桌面告警，开启「暂停调度」后新的 chat / 测试请求直接失败并提示补充账号（体检循环不受影响——池越浅越需要体检）。
              </p>
            </div>
          )}
        </CardContent>
      </Card>

      {/* 在线全功能调试终端 */}
      <Card className="border-blue-500/20 bg-blue-500/[0.01]">
        <CardHeader className="pb-3">
          <div className="flex items-center gap-2">
            <BrainCircuit className="size-4 text-blue-500" />
            <CardTitle className="text-base font-semibold">在线全功能测试终端</CardTitle>
          </div>
          <CardDescription className="text-xs">
            无需启动第三方客户端，在此处直接测试 Grok 的多模型生成、深度思考思维链与联网响应
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            <div>
              <label className="text-xs font-medium text-muted-foreground">目标模型</label>
              <select
                value={testModel}
                onChange={(e) => setTestModel(e.target.value)}
                className="mt-1 block w-full rounded-md border border-input bg-background px-3 py-1.5 text-xs shadow-sm focus:border-primary focus:outline-none focus:ring-1 focus:ring-primary"
              >
                {GROK_MODELS.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.name} ({m.badge}) - {m.desc}
                  </option>
                ))}
              </select>
            </div>

            <div>
              <label className="text-xs font-medium text-muted-foreground">指定账号（选填）</label>
              <select
                value={selectedAccountId}
                onChange={(e) => setSelectedAccountId(e.target.value)}
                className="mt-1 block w-full rounded-md border border-input bg-background px-3 py-1.5 text-xs shadow-sm focus:border-primary focus:outline-none focus:ring-1 focus:ring-primary"
              >
                <option value="">自动轮询（系统最优调度）</option>
                {accounts.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name} ({a.provider})
                  </option>
                ))}
              </select>
            </div>

            <div className="flex items-end pb-1.5">
              <label className="flex cursor-pointer items-center gap-2 text-xs font-medium">
                <input
                  type="checkbox"
                  checked={enableThinking}
                  onChange={(e) => setEnableThinking(e.target.checked)}
                  className="rounded border-input text-primary focus:ring-primary"
                />
                <span>开启深度思考 (Thinking 模式)</span>
              </label>
            </div>
          </div>

          <div>
            <label className="text-xs font-medium text-muted-foreground">提示词 (Prompt)</label>
            <div className="mt-1 flex gap-2">
              <Input
                value={testPrompt}
                onChange={(e) => setTestPrompt(e.target.value)}
                placeholder="输入想要向 Grok 提问的内容..."
                className="h-9 text-xs"
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    void handleRunTest();
                  }
                }}
              />
              <Button
                onClick={() => void handleRunTest()}
                disabled={testing || !testPrompt.trim() || accounts.length === 0}
                className="h-9 shrink-0 gap-1.5 px-4 text-xs"
              >
                {testing ? <Loader2 className="size-3.5 animate-spin" /> : <Zap className="size-3.5" />}
                发送测试
              </Button>
            </div>
          </div>

          {/* 测试错误展示 */}
          {testError && (
            <Alert variant="destructive" className="py-2.5">
              <CircleAlert className="size-4" />
              <AlertTitle className="text-xs font-medium">测试失败</AlertTitle>
              <AlertDescription className="text-xs">{testError}</AlertDescription>
            </Alert>
          )}

          {/* 测试结果展示 */}
          {testResult && (
            <div className="space-y-3 rounded-xl border bg-background p-4 shadow-sm">
              <div className="flex flex-wrap items-center justify-between gap-2 border-b pb-2 text-xs">
                <div className="flex items-center gap-2">
                  <Badge variant="outline" className="border-emerald-500/30 bg-emerald-500/10 text-emerald-600">
                    调用成功
                  </Badge>
                  <span className="font-semibold text-foreground">{testResult.model}</span>
                  <span className="text-muted-foreground">· 来自账号：{testResult.accountName}</span>
                </div>
                <div className="flex items-center gap-3 text-muted-foreground">
                  <span>首字耗时：<strong className="text-foreground">{testResult.durationMs}ms</strong></span>
                  {testResult.tokens ? <span>Token 消耗：~{testResult.tokens}</span> : null}
                </div>
              </div>

              {/* 思维链展示 */}
              {(() => {
                const { reasoning, content } = parseContent(testResult.reply);
                return (
                  <div className="space-y-3 text-xs leading-relaxed">
                    {reasoning && (
                      <div className="rounded-lg border border-purple-500/20 bg-purple-500/[0.03] p-3">
                        <button
                          type="button"
                          onClick={() => setShowThinkDetails(!showThinkDetails)}
                          className="flex w-full items-center justify-between text-[11px] font-semibold text-purple-600"
                        >
                          <div className="flex items-center gap-1.5">
                            <BrainCircuit className="size-3.5" />
                            <span>深度思考推演过程 (Thinking Process)</span>
                          </div>
                          {showThinkDetails ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
                        </button>
                        {showThinkDetails && (
                          <div className="mt-2.5 whitespace-pre-wrap font-mono text-[11px] text-muted-foreground/90">
                            {reasoning}
                          </div>
                        )}
                      </div>
                    )}
                    <div className="whitespace-pre-wrap text-sm text-foreground">
                      {content}
                    </div>
                  </div>
                );
              })()}
            </div>
          )}
        </CardContent>
      </Card>

      {/* 接入说明与客户端配置卡片 */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base font-semibold">第三方客户端接入指南</CardTitle>
          <CardDescription className="text-xs">
            网关提供完全标准的 OpenAI API 接口规范，可直接用于任何支持自定义 API 代理的应用
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">接口 Base URL (OpenAI 兼容)</label>
              <div className="flex items-center gap-1.5">
                <code className="flex-1 rounded-md border bg-muted/50 px-3 py-1.5 font-mono text-xs text-foreground">
                  http://127.0.0.1:57890/grok/v1
                </code>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => copyText("base_url", "http://127.0.0.1:57890/grok/v1")}
                  className="h-8 px-2.5 text-xs"
                >
                  {copiedKey === "base_url" ? <Check className="size-3.5 text-emerald-500" /> : <Copy className="size-3.5" />}
                </Button>
              </div>
            </div>

            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">推荐模型名称</label>
              <div className="flex items-center gap-1.5">
                <code className="flex-1 rounded-md border bg-muted/50 px-3 py-1.5 font-mono text-xs text-foreground">
                  grok-2, grok-3-thinking, grok-2-mini
                </code>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => copyText("models", "grok-2,grok-3-thinking,grok-2-mini")}
                  className="h-8 px-2.5 text-xs"
                >
                  {copiedKey === "models" ? <Check className="size-3.5 text-emerald-500" /> : <Copy className="size-3.5" />}
                </Button>
              </div>
            </div>
          </div>

          <div className="rounded-lg border bg-muted/20 p-3 text-xs text-muted-foreground">
            <p className="font-semibold text-foreground">常用客户端配置步骤：</p>
            <ol className="mt-1.5 list-inside list-decimal space-y-1">
              <li>在 <strong>Chatbox / NextChat / VS Code Cline / Claude Code</strong> 等工具中将模型提供商选择为 <code>OpenAI 兼容</code> 或 <code>自定义</code>；</li>
              <li>将 <strong>API 代理地址 (Base URL)</strong> 填入 <code>http://127.0.0.1:57890/grok/v1</code>；</li>
              <li>在下方管理区创建或复制一个网关 API Key（以 <code>gw_live_...</code> 开头），填入客户端的 API Key 输入框；</li>
              <li>模型名输入 <code>grok-2</code> 或 <code>grok-3-thinking</code> 即可畅享高质量对话！</li>
            </ol>
          </div>
        </CardContent>
      </Card>

      {/* 统一密钥网关管理区域 */}
      <GatewayAdminSection
        title="Grok 网关 API Key 与调用审计"
        description="所有对 /grok/v1/* 的请求均通过此密钥进行鉴权和频率监控，用量与延迟会自动汇聚到全局监控中"
      />
    </div>
  );
}
