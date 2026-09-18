import { useCallback, useEffect, useState } from "react";
import {
  AlertTriangle,
  CircleCheck,
  Loader2,
  Plus,
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

interface GlmwebTokenInfo {
  index: number;
  type: string;
  preview: string;
  hasCachedAccess: boolean;
  accessExpiresAt: number | null;
}

interface GlmwebPoolStatus {
  accounts: number;
  guestSlots: number;
  refreshAccounts: number;
  withCachedAccess: number;
  // v0.1.46：池健康快照
  needRenew?: number;
  defaultModel?: string;
  degraded?: boolean;
}

function formatExpires(ts: number | null | undefined): string {
  if (!ts) return "—";
  return new Date(ts).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export default function GlmwebPage() {
  const [tokens, setTokens] = useState<GlmwebTokenInfo[]>([]);
  const [pool, setPool] = useState<GlmwebPoolStatus | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [showAdd, setShowAdd] = useState(false);
  const [newToken, setNewToken] = useState("");
  const [testModel, setTestModel] = useState("glm-5.3-flash");
  const [testPrompt, setTestPrompt] = useState("用一句话介绍你自己");
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);
  const [testing, setTesting] = useState(false);

  const loadTokens = useCallback(async () => {
    const res = await api.getGlmwebTokens();
    setTokens(res.tokens ?? []);
    setPool(res.pool ?? null);
  }, []);

  useEffect(() => {
    void loadTokens().catch(() => undefined);
  }, [loadTokens]);

  async function addToken() {
    if (!newToken.trim()) {
      setMsg({ type: "err", text: "请粘贴 refresh token（或填 guest 作为游客槽位）" });
      return;
    }
    setBusy("add");
    try {
      await api.glmwebAddToken(newToken.trim());
      setNewToken("");
      setShowAdd(false);
      await loadTokens();
      setMsg({ type: "ok", text: "token 已加入池中" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function removeToken(index: number) {
    setBusy(String(index));
    try {
      await api.glmwebRemoveToken(index);
      await loadTokens();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function runTest() {
    if (!testPrompt.trim()) return;
    setTesting(true);
    setTestResult(null);
    try {
      const res = await api.glmwebTestCompletion(testModel, testPrompt.trim());
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
          <h1 className="text-[28px] font-semibold tracking-tight">ChatGLM 网关</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            把 chatglm.cn 网页接口封装为 OpenAI 兼容接口：
            <code className="mx-1 rounded bg-muted/70 px-1.5 py-0.5 text-xs">http://127.0.0.1:57890/glmweb/v1</code>
            鉴权密钥与 GLM 代理相同。默认模型
            <code className="mx-1 rounded bg-muted/70 px-1.5 py-0.5 text-xs">{pool?.defaultModel ?? "glm-5.3-flash"}</code>
            （GLM5.3Flash）。未添加正式 token 时会退化为游客模式（模型与用量受限），可用浏览器同步工具自动续期。
          </p>
        </div>
      </header>

      {/* v0.1.46：退化为游客模式时显式告警（正式账号为 0） */}
      {pool?.degraded ? (
        <div className="mb-5 flex items-start gap-2 rounded-xl border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-sm">
          <AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-600" />
          <div className="min-w-0">
            <div className="font-medium">当前处于游客模式（正式账号 0 个）</div>
            <div className="mt-1 text-xs leading-5 text-muted-foreground">
              游客模式可免登录调用，但模型与用量受限、并发会话会被上游 429 拒绝。
              请在下方添加 refresh token，或使用「ChatGLM Token 自动同步」浏览器扩展自动续期。
            </div>
          </div>
        </div>
      ) : null}

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

      <section className="min-w-0 space-y-2.5" aria-label="token 池">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">Token 池</h2>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardDescription className="min-w-0 text-xs">
                {pool
                  ? `共 ${pool.accounts} 个槽位（游客 ${pool.guestSlots} · 正式 ${pool.refreshAccounts}），${pool.withCachedAccess} 个已缓存 access token`
                  : "加载中…"}
              </CardDescription>
              <Button size="sm" variant="outline" onClick={() => setShowAdd((v) => !v)}>
                <Plus />添加 token
              </Button>
            </div>
            {showAdd ? (
              <div className="mt-3 space-y-2 rounded-lg border border-border/60 bg-muted/30 px-3 py-3">
                <Input
                  className="h-8 text-xs"
                  placeholder="refresh token（登录 chatglm.cn 后从 Local Storage 取 chatglm_refresh_token；填 guest 为游客槽位）"
                  value={newToken}
                  onChange={(e) => setNewToken(e.target.value)}
                />
                <div className="flex gap-2">
                  <Button size="sm" onClick={() => void addToken()} disabled={busy === "add"}>
                    {busy === "add" ? <Loader2 className="animate-spin" /> : <CircleCheck />}保存并加入池
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setShowAdd(false)}>取消</Button>
                </div>
                <p className="text-[11px] text-muted-foreground">
                  多账号池自动轮转，上游轮换 refresh token 时自动写回；游客槽位的临时凭据不落盘展示。
                </p>
              </div>
            ) : null}
          </CardHeader>
          <CardContent className="p-0">
            <div className="divide-y divide-border/50 border-t border-border/50">
              {tokens.length === 0 ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                  Token 池为空。调用时会自动使用游客模式（部分模型与用量受限）。
                </p>
              ) : (
                tokens.map((token) => (
                  <div key={token.index} className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
                    <div className="min-w-0">
                      <div className="flex min-w-0 flex-wrap items-center gap-2">
                        <span className="text-sm font-medium">
                          #{token.index + 1} {token.type === "guest" ? "游客槽位" : "正式账号"}
                        </span>
                        <Badge variant="outline" className="text-[10px]">
                          {token.type}
                        </Badge>
                        {token.hasCachedAccess ? (
                          <Badge variant="secondary" className="text-[10px]">已缓存 access</Badge>
                        ) : null}
                      </div>
                      <p className="mt-0.5 truncate text-[11px] text-muted-foreground">
                        {token.preview} · access 至 {formatExpires(token.accessExpiresAt)}
                      </p>
                    </div>
                    <Button size="sm" variant="ghost" className="text-destructive hover:text-destructive" onClick={() => void removeToken(token.index)} disabled={busy === String(token.index)} aria-label="移除槽位">
                      {busy === String(token.index) ? <Loader2 className="animate-spin" /> : <Trash2 />}
                    </Button>
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
                className="h-8 w-44 text-xs"
                value={testModel}
                onChange={(e) => setTestModel(e.target.value)}
                placeholder="模型（glm-5.3-flash）"
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
                {testing ? <Loader2 className="animate-spin" /> : <CircleCheck />}发送
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

      <GatewayAdminSection gateway="glmweb" />

      <section className="mt-10" aria-label="接入说明">
        <Alert>
          <AlertTitle>接入方式（OpenAI 兼容）</AlertTitle>
          <AlertDescription className="space-y-1.5 text-xs">
            <p>
              Base URL：
              <code className="ml-1 rounded bg-muted/70 px-1.5 py-0.5">http://127.0.0.1:57890/glmweb/v1</code>
              ，密钥与「GLM 代理」页相同；模型列表见
              <code className="mx-1 rounded bg-muted/70 px-1 py-0.5">/glmweb/v1/models</code>
              （glm-5.3 / glm-4.7-flash 等 22 个）。
            </p>
            <p>
              游客模式免登录可用（模型与用量受限）；登录 chatglm.cn 取 refresh token
              后可解锁全部能力，上游轮换时自动写回。
            </p>
          </AlertDescription>
        </Alert>
      </section>
    </div>
  );
}
