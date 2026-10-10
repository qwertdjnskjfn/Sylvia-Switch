import { useCallback, useEffect, useMemo, useState } from "react";
import {
  CalendarOff,
  Coins,
  Cpu,
  Info,
  Loader2,
  Monitor,
  RefreshCw,
  Send,
  Trash2,
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
} from "@/components/ui/card";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import { useRefreshAllListener } from "@/lib/refresh-all";
import { CopyButton } from "@/components/ui/copy-button";
import type { MiaoshouAccessInfo, MiaoshouAccountInfo, MiaoshouModelInfo, MiaoshouStatusItem } from "@/lib/types";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Input } from "@/components/ui/input";

function accountName(account: MiaoshouAccountInfo): string {
  return account.nickname || account.email || account.miaoshouUid || account.uid || account.id;
}

/** 移除账号使用的 uid：优先账号库内部 uid，缺省退回 id。 */
function accountUid(account: MiaoshouAccountInfo): string {
  return account.uid ?? account.id;
}

function formatBalance(balance: number | null | undefined): string {
  if (balance === null || balance === undefined) return "—";
  return balance.toLocaleString("zh-CN");
}

function formatRefreshTime(ts: number | null | undefined): string {
  if (!ts) return "—";
  return new Date(ts).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export default function MiaoshouPage() {
  const [accounts, setAccounts] = useState<MiaoshouAccountInfo[]>([]);
  const [status, setStatus] = useState<MiaoshouStatusItem[]>([]);
  // 接入信息（Base URL / API Key）：来自 /api/miaoshou/status 顶层（2026-10-10 新增）
  const [access, setAccess] = useState<MiaoshouAccessInfo | null>(null);
  const [models, setModels] = useState<MiaoshouModelInfo[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  // 试一试
  const [testModel, setTestModel] = useState("LongCat-2.0");
  const [testPrompt, setTestPrompt] = useState("只回复四个字：测试成功");
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);
  const [testing, setTesting] = useState(false);

  const loadAccounts = useCallback(async () => {
    const res = await api.getMiaoshouAccounts();
    setAccounts(res.accounts ?? []);
  }, []);

  const loadStatus = useCallback(async () => {
    const res = await api.getMiaoshouStatus();
    setStatus(res.accounts ?? []);
    // 接入信息：后端顶层字段（缺失时保持 null，区块显示占位）
    if (res.apiKey && res.basePath) {
      setAccess({ apiKey: res.apiKey, basePath: res.basePath });
    }
  }, []);

  const loadModels = useCallback(async (force = false) => {
    try {
      const res = await api.getMiaoshouModels(force);
      const list = res.data ?? [];
      setModels(list);
      // 默认选中第一个免费/常用模型
      setTestModel((prev) =>
        list.some((m) => m.id === prev) ? prev : (list[0]?.id ?? prev),
      );
    } catch {
      // 模型目录拉取失败不阻断页面（账号/积分仍可用）
    }
  }, []);

  const loadAll = useCallback(
    async (forceModels = false) => {
      await Promise.all([loadAccounts(), loadStatus(), loadModels(forceModels)]);
    },
    [loadAccounts, loadStatus, loadModels],
  );

  useEffect(() => {
    // 初次加载静默失败（后端未就绪时呈现空态，不打扰）
    void loadAll().catch(() => undefined);
    // 全局刷新按钮广播：重拉账号/积分/模型目录（force 绕过服务端缓存）
    return useRefreshAllListener(() => {
      void loadAll(true).catch(() => undefined);
    });
  }, [loadAll]);

  // 定时轮询：桌面端登录态/积分可能在本页打开后变化，30s 自动同步一次。
  // 模型目录不随轮询 force（走服务端 10 分钟缓存，避免频繁打上游）。
  useEffect(() => {
    const t = window.setInterval(() => {
      void Promise.all([loadAccounts(), loadStatus()]).catch(() => undefined);
    }, 30000);
    return () => window.clearInterval(t);
  }, [loadAccounts, loadStatus]);

  const accountById = useMemo(() => new Map(accounts.map((a) => [a.id, a])), [accounts]);
  const statusById = useMemo(() => new Map(status.map((s) => [s.id, s])), [status]);

  /** 重新拉取账号池与积分；scope 作为 busy 键（区分顶栏与行内按钮）。 */
  async function refresh(scope = "refresh") {
    setBusy(scope);
    setMsg(null);
    try {
      await loadAll(true);
    } catch (e) {
      setMsg({ type: "err", text: `刷新失败：${api.asError(e)}` });
    } finally {
      setBusy(null);
    }
  }

  async function refreshModels() {
    setBusy("models");
    setMsg(null);
    try {
      await loadModels(true);
    } catch (e) {
      setMsg({ type: "err", text: `刷新目录失败：${api.asError(e)}` });
    } finally {
      setBusy(null);
    }
  }

  async function importLocal() {
    setBusy("import");
    setMsg(null);
    try {
      const res = await api.miaoshouImportLocal();
      await loadAll(true);
      const imported = res.account?.nickname || res.account?.email || "";
      setMsg({
        type: "ok",
        text: `已导入本机妙手登录态${imported ? `（${imported}）` : ""}`,
      });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function removeAccount(uid: string) {
    setBusy(uid);
    setMsg(null);
    try {
      await api.miaoshouRemoveAccount(uid);
      await loadAll();
      setMsg({ type: "ok", text: "账号已移除" });
    } catch (e) {
      setMsg({ type: "err", text: `移除失败：${api.asError(e)}` });
    } finally {
      setBusy(null);
    }
  }

  async function runTest() {
    if (!testPrompt.trim()) return;
    setTesting(true);
    setTestResult(null);
    try {
      const res = await api.miaoshouChat({
        model: testModel,
        messages: [{ role: "user", content: testPrompt.trim() }],
        stream: false,
      });
      const text = res.choices?.[0]?.message?.content ?? "";
      setTestResult({ ok: true, text: text || "(空回复)" });
    } catch (e) {
      setTestResult({ ok: false, text: api.asError(e) });
    } finally {
      setTesting(false);
    }
  }


  // 接入信息：Base URL 用运行时 origin 拼接 —— 网关端口可能被 launcher 调整，
  // 故后端只给相对 basePath，前端绝不硬编码 127.0.0.1:57890。
  const origin =
    typeof window !== "undefined" && window.location?.origin
      ? window.location.origin
      : "http://127.0.0.1:57890";
  const baseUrl = `${origin}${(access?.basePath ?? "/miaoshou/v1/chat/completions").replace(
    /\/chat\/completions$/,
    "",
  )}`;
  const sampleModel = models[0]?.id ?? "glm-5.3-flash";
  const curlExample = access
    ? [
        `curl -X POST '${origin}${access.basePath}' \\`,
        `  -H 'Authorization: Bearer ${access.apiKey}' \\`,
        `  -H 'Content-Type: application/json' \\`,
        `  -d '{"model":"${sampleModel}","messages":[{"role":"user","content":"你好"}]}'`,
      ].join("\n")
    : "";

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-10 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-12">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">妙手</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            妙手（美团智能体桌面应用）账号池：导入本机登录态后，统一查看各账号的积分余额、套餐与刷新时间。
            该平台<strong className="mx-1 text-foreground">无每日签到功能</strong>
            （积分按月刷新），无需手动签到。
          </p>
        </div>
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

      <section className="min-w-0 space-y-2.5" aria-label="妙手账号池">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">账号池</h2>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardDescription className="min-w-0 text-xs">
                共 {accounts.length} 个账号 · 本机桌面端登录态可一键导入
              </CardDescription>
              <div className="flex flex-wrap gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void refresh()}
                  disabled={busy === "refresh"}
                >
                  {busy === "refresh" ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void importLocal()}
                  disabled={busy === "import"}
                >
                  {busy === "import" ? <Loader2 className="animate-spin" /> : <UserPlus />}
                  导入本机登录态
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  disabled
                  title="该平台无每日签到功能（积分按月刷新）"
                >
                  <CalendarOff />每日签到
                </Button>
              </div>
            </div>
            <p className="mt-2 flex items-center gap-1.5 text-[11px] text-muted-foreground">
              <Info className="size-3.5 shrink-0" />
              该平台无每日签到功能（积分按月刷新），「每日签到」按钮置灰不可用。
            </p>
          </CardHeader>
          <CardContent className="p-0">
            <div className="divide-y divide-border/50 border-t border-border/50">
              {accounts.length === 0 ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                  账号池为空。在电脑上安装并登录妙手（美团智能体桌面应用）后，点「导入本机登录态」即可纳入账号。
                </p>
              ) : (
                accounts.map((account) => {
                  const uid = accountUid(account);
                  const rowBusy = `refresh-${account.id}`;
                  const st = statusById.get(account.id);
                  const name = accountName(account);
                  const balance = account.credit?.balance ?? st?.balance ?? null;
                  const planName = account.credit?.planName ?? st?.planName ?? null;
                  const nextRefreshTime =
                    account.credit?.nextRefreshTime ?? st?.nextRefreshTime ?? null;
                  return (
                    <div
                      key={account.id}
                      className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5"
                    >
                      <div className="min-w-0">
                        <div className="flex min-w-0 flex-wrap items-center gap-2">
                          <span className="truncate text-sm font-medium">{name}</span>
                          {account.email && account.email !== name ? (
                            <Badge variant="outline" className="text-[10px]" title={account.email}>
                              {account.email}
                            </Badge>
                          ) : null}
                          {account.isDesktop ? (
                            <Badge className="text-[10px]">
                              <Monitor />桌面端
                            </Badge>
                          ) : (
                            <Badge variant="outline" className="text-[10px] text-muted-foreground">
                              云端
                            </Badge>
                          )}
                          {balance !== null ? (
                            <Badge variant="secondary" className="text-[10px]">
                              <Coins />
                              积分 {formatBalance(balance)}
                            </Badge>
                          ) : null}
                          {st && !st.ok ? (
                            <Badge variant="destructive" className="text-[10px]">
                              异常
                            </Badge>
                          ) : null}
                        </div>
                        <p
                          className="mt-0.5 truncate text-[11px] text-muted-foreground"
                          title={st?.note ?? ""}
                        >
                          {planName ? `${planName} · ` : ""}
                          积分 {formatBalance(balance)} · 下次刷新 {formatRefreshTime(nextRefreshTime)}
                          {st?.note ? ` · ${st.note}` : ""}
                        </p>
                      </div>
                      <div className="flex shrink-0 items-center gap-1.5">
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => void refresh(rowBusy)}
                          disabled={busy === rowBusy}
                          aria-label="刷新账号与积分"
                        >
                          {busy === rowBusy ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          className="text-destructive hover:text-destructive"
                          onClick={() => void removeAccount(uid)}
                          disabled={busy === uid}
                          aria-label="移除账号"
                        >
                          {busy === uid ? <Loader2 className="animate-spin" /> : <Trash2 />}
                        </Button>
                      </div>
                    </div>
                  );
                })
              )}
            </div>
          </CardContent>
        </Card>
      </section>

      <section className="mt-10 min-w-0 space-y-2.5" aria-label="签到状态与积分">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">签到状态与积分</h2>
        </div>
        <Alert>
          <Info />
          <AlertTitle>该平台无每日签到</AlertTitle>
          <AlertDescription className="space-y-1.5 text-xs">
            <p>妙手（美团智能体桌面应用）没有每日签到功能，积分按月刷新，无需（也无法）手动签到。</p>
            <p className="text-[11px] text-muted-foreground">
              下方为各账号的积分明细（余额 / 套餐 / 下次刷新时间），来自账号池实时查询。
            </p>
          </AlertDescription>
        </Alert>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardDescription className="min-w-0 text-xs">
                积分明细 · 共 {status.length} 个账号
              </CardDescription>
              <span className="shrink-0 text-[11px] text-muted-foreground">
                无每日签到（积分按月刷新）
              </span>
            </div>
          </CardHeader>
          <CardContent className="p-0">
            <div className="divide-y divide-border/50 border-t border-border/50">
              {status.length === 0 ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                  暂无积分数据。导入账号后可查看各账号的积分余额、套餐与刷新时间。
                </p>
              ) : (
                status.map((item) => {
                  const account = accountById.get(item.id);
                  return (
                    <div
                      key={item.id}
                      className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5"
                    >
                      <div className="min-w-0">
                        <div className="flex min-w-0 flex-wrap items-center gap-2">
                          <span className="truncate text-sm font-medium">
                            {account ? accountName(account) : item.id}
                          </span>
                          {item.planName ? (
                            <Badge variant="outline" className="text-[10px]">
                              {item.planName}
                            </Badge>
                          ) : null}
                          {item.ok ? null : (
                            <Badge variant="destructive" className="text-[10px]">
                              异常
                            </Badge>
                          )}
                        </div>
                        <p
                          className="mt-0.5 truncate text-[11px] text-muted-foreground"
                          title={item.note ?? ""}
                        >
                          积分 {formatBalance(item.balance)} · 下次刷新{" "}
                          {formatRefreshTime(item.nextRefreshTime)}
                          {item.note ? ` · ${item.note}` : ""}
                        </p>
                      </div>
                      <div className="flex shrink-0 items-center gap-1.5 text-sm font-medium tabular-nums">
                        <Coins className="size-3.5 text-muted-foreground" />
                        {formatBalance(item.balance)}
                      </div>
                    </div>
                  );
                })
              )}
            </div>
          </CardContent>
        </Card>
      </section>

      <section className="mt-10 min-w-0 space-y-2.5" aria-label="接入信息">
        <div className="min-w-0 px-1">
          <h2 className="text-[13px] font-medium leading-5">接入信息</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            把妙手模型接入任意 OpenAI 兼容客户端。凭证由本机妙手桌面端登录态自动派生，
            无需手动填写上游 token；调用会经本网关按账号池转发。
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            <div className="flex flex-wrap items-center gap-2 border-b border-border/50 px-4 py-3.5 sm:px-5">
              <span className="text-xs text-muted-foreground">Base URL</span>
              <code className="max-w-full overflow-x-auto rounded bg-muted/70 px-2 py-1 text-xs">{baseUrl}</code>
              <CopyButton text={baseUrl} label="Base URL" />
              <span className="ml-2 text-xs text-muted-foreground">API Key</span>
              <code className="rounded bg-muted/70 px-2 py-1 text-xs">{access ? access.apiKey : "—"}</code>
              {access ? <CopyButton text={access.apiKey} label="API Key" /> : null}
            </div>
            <div className="px-4 py-3.5 sm:px-5">
              <div className="text-[12px] font-medium">调用示例（curl）</div>
              <pre className="mt-1.5 min-w-0 overflow-x-auto rounded bg-muted/60 p-2 text-[10px] leading-4">{curlExample}</pre>
              <div className="mt-1.5">
                <CopyButton text={curlExample} label="调用示例" />
              </div>
              <p className="mt-2 text-[11px] leading-5 text-muted-foreground">
                模型名从下方「模型目录」选取；当前账号池有 {accounts.length} 个账号。
                妙手积分按月刷新，额度用尽时会返回上游错误。
              </p>
            </div>
          </CardContent>
        </Card>
      </section>

      <section className="mt-10 min-w-0 space-y-2.5" aria-label="模型目录">
        <div className="flex items-center justify-between gap-2 px-1">
          <div className="min-w-0">
            <h2 className="text-[13px] font-medium leading-5">模型目录</h2>
            <p className="mt-0.5 text-xs text-muted-foreground">
              实时取自妙手桌面端（{models.length} 个模型），与客户端保持同步
            </p>
          </div>
          <Button
            size="sm"
            variant="outline"
            onClick={() => void refreshModels()}
            disabled={busy === "models"}
          >
            {busy === "models" ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新目录
          </Button>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            {models.length === 0 ? (
              <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                暂无模型数据。请先「导入本机登录态」，再点「刷新目录」。
              </p>
            ) : (
              <div className="divide-y divide-border/50">
                {models.map((m) => {
                  const rate = m.rateMultiplier;
                  const free = rate === "0.00" || rate === "0";
                  return (
                    <div
                      key={m.id}
                      className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-2.5 sm:px-5"
                    >
                      <div className="flex min-w-0 flex-wrap items-center gap-2">
                        <Cpu className="size-3.5 shrink-0 text-muted-foreground" />
                        <span className="text-sm font-medium">{m.id}</span>
                        {m.marketingCopy ? (
                          <Badge variant="secondary" className="text-[10px]">
                            {m.marketingCopy}
                          </Badge>
                        ) : null}
                        {free ? (
                          <Badge className="text-[10px]">免费</Badge>
                        ) : rate ? (
                          <Badge variant="outline" className="text-[10px]">
                            倍率 {rate}
                          </Badge>
                        ) : null}
                        {m.supportsImages ? (
                          <Badge variant="outline" className="text-[10px] text-muted-foreground">
                            图像
                          </Badge>
                        ) : null}
                      </div>
                      <div className="flex shrink-0 items-center gap-2 text-[11px] text-muted-foreground">
                        {m.contextWindow ? (
                          <span title="上下文档位">ctx {m.contextWindow.toLocaleString("zh-CN")}</span>
                        ) : null}
                        <Button
                          size="sm"
                          variant="ghost"
                          className="h-7 text-[11px]"
                          onClick={() => {
                            setTestModel(m.id);
                            setTestResult(null);
                          }}
                        >
                          用它试调
                        </Button>
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </CardContent>
        </Card>
      </section>

      <section className="mt-10 min-w-0 space-y-2.5" aria-label="试一试">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">试一试</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            通过妙手反代直接调用桌面端模型（服务端转发，等价于外部客户端调用）
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="space-y-3 p-4 sm:p-5">
            {accounts.length === 0 ? (
              <p className="text-xs text-muted-foreground">
                请先在上方「导入本机登录态」，即可试调模型。
              </p>
            ) : (
              <>
                <div className="flex flex-wrap items-center gap-2">
                  <Select value={testModel} onValueChange={setTestModel}>
                    <SelectTrigger className="w-56">
                      <SelectValue placeholder="选择模型" />
                    </SelectTrigger>
                    <SelectContent>
                      {models.map((m) => (
                        <SelectItem key={m.id} value={m.id}>
                          {m.id}
                          {m.rateMultiplier === "0.00" ? "（免费）" : ""}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <Button size="sm" onClick={() => void runTest()} disabled={testing || !testPrompt.trim()}>
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
                  <pre
                    className={cn(
                      "max-h-64 overflow-auto whitespace-pre-wrap rounded-lg border border-border/60 bg-muted/40 p-3 text-xs leading-5",
                      testResult.ok ? "text-foreground" : "text-destructive",
                    )}
                  >
                    {testResult.text}
                  </pre>
                ) : null}
              </>
            )}
          </CardContent>
        </Card>
      </section>
    </div>
  );
}
