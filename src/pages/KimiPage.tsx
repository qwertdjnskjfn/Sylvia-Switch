import { useCallback, useEffect, useState } from "react";
import {
  CircleCheck,
  KeyRound,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
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
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import { GatewayAdminSection } from "@/components/gateway/GatewayAdminSection";
import type { KimiAccountInfo } from "@/lib/types";

function formatExpires(ts: number | null | undefined): string {
  if (!ts) return "—";
  return new Date(ts).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export default function KimiPage() {
  const [accounts, setAccounts] = useState<KimiAccountInfo[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [showAdd, setShowAdd] = useState(false);
  const [newName, setNewName] = useState("");
  const [newToken, setNewToken] = useState("");
  const [testModel, setTestModel] = useState("kimi-k2.6");
  const [testPrompt, setTestPrompt] = useState("用一句话介绍你自己");
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);
  const [testing, setTesting] = useState(false);

  const loadAccounts = useCallback(async () => {
    const res = await api.getKimiAccounts();
    setAccounts(res.accounts ?? []);
  }, []);

  useEffect(() => {
    void loadAccounts().catch(() => undefined);
  }, [loadAccounts]);

  async function addAccount() {
    if (!newToken.trim()) {
      setMsg({ type: "err", text: "请粘贴 refresh token 或 access token" });
      return;
    }
    setBusy("add");
    try {
      await api.kimiAddAccount(newName.trim() || undefined, newToken.trim());
      setNewName("");
      setNewToken("");
      setShowAdd(false);
      await loadAccounts();
      setMsg({ type: "ok", text: "账号已加入账号池" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function removeAccount(id: string) {
    setBusy(id);
    try {
      await api.kimiRemoveAccount(id);
      await loadAccounts();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function toggleAccount(account: KimiAccountInfo) {
    setBusy(account.id);
    try {
      await api.kimiEnableAccount(account.id, !account.enabled);
      await loadAccounts();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function verifyAccount(id: string) {
    setBusy(id);
    try {
      await api.kimiVerifyAccount(id);
      await loadAccounts();
      setMsg({ type: "ok", text: "验证通过，登录态可用" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
      await loadAccounts();
    } finally {
      setBusy(null);
    }
  }

  async function runTest() {
    if (!testPrompt.trim()) return;
    setTesting(true);
    setTestResult(null);
    try {
      const res = await api.kimiTestCompletion(testModel, testPrompt.trim());
      setTestResult({ ok: true, text: `${res.content}\n\n—— ${res.model} · ${res.latencyMs}ms` });
    } catch (e) {
      setTestResult({ ok: false, text: api.asError(e) });
    } finally {
      setTesting(false);
    }
  }

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-10 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-12">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">Kimi 网关</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            把多个 Kimi Web 账号汇聚为 OpenAI 兼容接口：
            <code className="mx-1 rounded bg-muted/70 px-1.5 py-0.5 text-xs">http://127.0.0.1:57890/kimi/v1</code>
            鉴权密钥与 GLM 代理相同；支持 thinking / search / agent
            模型族，账号池按健康状态与并发水位调度，refresh token 自动续期。
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

      <section className="min-w-0 space-y-2.5" aria-label="Kimi 账号池">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">账号池</h2>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardDescription className="min-w-0 text-xs">
                共 {accounts.length} 个账号 · token 脱敏展示
              </CardDescription>
              <Button size="sm" variant="outline" onClick={() => setShowAdd((v) => !v)}>
                <Plus />添加账号
              </Button>
            </div>
            {showAdd ? (
              <div className="mt-3 space-y-2 rounded-lg border border-border/60 bg-muted/30 px-3 py-3">
                <div className="grid gap-2 sm:grid-cols-2">
                  <Input
                    className="h-8 text-xs"
                    placeholder="备注名（可选，如：Kimi 小号A）"
                    value={newName}
                    onChange={(e) => setNewName(e.target.value)}
                  />
                  <Input
                    className="h-8 text-xs"
                    placeholder="refresh token（推荐）或 access token（必填）"
                    value={newToken}
                    onChange={(e) => setNewToken(e.target.value)}
                  />
                </div>
                <div className="flex gap-2">
                  <Button size="sm" onClick={() => void addAccount()} disabled={busy === "add"}>
                    {busy === "add" ? <Loader2 className="animate-spin" /> : <CircleCheck />}保存并加入账号池
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setShowAdd(false)}>取消</Button>
                </div>
                <p className="text-[11px] text-muted-foreground">
                  登录 kimi.com 后从浏览器 DevTools → Application → Local Storage 或 Cookie 中取
                  refresh_token（推荐，可自动续期）或 access_token。
                </p>
              </div>
            ) : null}
          </CardHeader>
          <CardContent className="p-0">
            <div className="divide-y divide-border/50 border-t border-border/50">
              {accounts.length === 0 ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                  账号池为空。点「添加账号」粘贴 Kimi 的 refresh token 开始使用。
                </p>
              ) : (
                accounts.map((account) => (
                  <div key={account.id} className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
                    <div className="min-w-0">
                      <div className="flex min-w-0 flex-wrap items-center gap-2">
                        <span className="truncate text-sm font-medium">{account.name}</span>
                        <Badge variant="outline" className="text-[10px]">
                          {account.tokenType === "refresh" ? "refresh" : "access"}
                        </Badge>
                        {account.status === "ok" ? (
                          <Badge variant="secondary" className="text-[10px]">正常</Badge>
                        ) : (
                          <Badge variant="destructive" className="text-[10px]">异常</Badge>
                        )}
                        {!account.enabled ? (
                          <Badge variant="outline" className="text-[10px]">已禁用</Badge>
                        ) : null}
                        {account.cooldownSeconds > 0 ? (
                          <Badge variant="warning" className="text-[10px]">
                            冷却 {account.cooldownSeconds}s
                          </Badge>
                        ) : null}
                      </div>
                      <p className="mt-0.5 truncate text-[11px] text-muted-foreground" title={account.error || ""}>
                        {account.tokenPreview} · 并发上限 {account.maxConcurrency} · 间隔{" "}
                        {account.minIntervalSeconds}s · 缓存 token 至{" "}
                        {formatExpires(account.accessExpiresAt)}
                        {account.error ? ` · ${account.error}` : ""}
                      </p>
                    </div>
                    <div className="flex shrink-0 items-center gap-1.5">
                      <Button size="sm" variant="ghost" onClick={() => void toggleAccount(account)} disabled={busy === account.id}>
                        {account.enabled ? "禁用" : "启用"}
                      </Button>
                      <Button size="sm" variant="ghost" onClick={() => void verifyAccount(account.id)} disabled={busy === account.id} aria-label="验证登录态">
                        {busy === account.id ? <Loader2 className="animate-spin" /> : <KeyRound />}
                      </Button>
                      <Button size="sm" variant="ghost" className="text-destructive hover:text-destructive" onClick={() => void removeAccount(account.id)} disabled={busy === account.id} aria-label="移除账号">
                        <Trash2 />
                      </Button>
                    </div>
                  </div>
                ))
              )}
            </div>
          </CardContent>
        </Card>
      </section>

      <section className="mt-10 min-w-0 space-y-2.5" aria-label="试一试">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">试一试</h2>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-3 sm:px-5">
            <div className="flex flex-wrap items-center gap-2">
              <Input
                className="h-8 w-56 text-xs"
                value={testModel}
                onChange={(e) => setTestModel(e.target.value)}
                placeholder="模型（kimi-k2.6）"
                aria-label="模型"
              />
              <Input
                className="h-8 min-w-0 flex-1 text-xs"
                value={testPrompt}
                onChange={(e) => setTestPrompt(e.target.value)}
                placeholder="输入测试内容"
                aria-label="测试内容"
              />
              <Button size="sm" onClick={() => void runTest()} disabled={testing}>
                {testing ? <Loader2 className="animate-spin" /> : <RefreshCw />}发送
              </Button>
            </div>
          </CardHeader>
          {testResult ? (
            <CardContent className="px-4 pb-4 sm:px-5">
              <p className={cn("whitespace-pre-wrap rounded-lg border px-3 py-2.5 text-xs", testResult.ok ? "border-border/60" : "border-destructive/40 text-destructive")}>
                {testResult.text}
              </p>
            </CardContent>
          ) : null}
        </Card>
      </section>

      <section className="mt-10" aria-label="接入说明">
        <Alert>
          <AlertTitle>接入方式（OpenAI 兼容）</AlertTitle>
          <AlertDescription className="space-y-1.5 text-xs">
            <p>
              Base URL：
              <code className="ml-1 rounded bg-muted/70 px-1.5 py-0.5">http://127.0.0.1:57890/kimi/v1</code>
              ，密钥与「GLM 代理」页相同。模型名以
              <code className="mx-1 rounded bg-muted/70 px-1 py-0.5">/kimi/v1/models</code>
              动态返回为准（kimi-k2.6 / -thinking / -search / -agent 等后缀族）。
            </p>
            <p>thinking 模型的推理内容经 reasoning_content 字段透出；联网搜索优先使用 -search 后缀模型。</p>
          </AlertDescription>
        </Alert>
      </section>

      <GatewayAdminSection gateway="kimi" />
    </div>
  );
}
