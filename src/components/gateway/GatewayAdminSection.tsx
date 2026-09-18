import { useCallback, useEffect, useState } from "react";
import {
  Check,
  Copy,
  Loader2,
  Plus,
  ScrollText,
  Trash2,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import * as api from "@/lib/api";
import type { GatewayApiKey, GatewayLogEntry, GatewayOverview } from "@/lib/types";

function formatTime(ts: number | null | undefined): string {
  if (!ts) return "—";
  return new Date(ts).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** 网关管理区块：API Keys + 请求日志 + 概览（Qoder/Kimi/Grok 页面共用）。
 *  gateway 传入 "qoder"、"kimi" 或 "grok" 以过滤日志；title 用于区块标题。 */
export function GatewayAdminSection({
  gateway = "grok",
  title,
  description: _description,
}: {
  gateway?: "qoder" | "kimi" | "glmweb" | "grok";
  title?: string;
  description?: string;
}) {
  const [keys, setKeys] = useState<GatewayApiKey[]>([]);
  const [logs, setLogs] = useState<GatewayLogEntry[]>([]);
  const [overview, setOverview] = useState<GatewayOverview | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [newName, setNewName] = useState("");
  const [creating, setCreating] = useState(false);
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [keysRes, logsRes, overviewRes] = await Promise.all([
        api.getGatewayKeys(),
        api.getGatewayLogs(gateway, 30),
        api.getGatewayOverview(),
      ]);
      setKeys(keysRes.keys ?? []);
      setLogs(logsRes.logs ?? []);
      setOverview(overviewRes);
    } catch (e) {
      setError(api.asError(e));
    }
  }, [gateway]);

  useEffect(() => {
    void load();
  }, [load]);

  async function createKey() {
    setCreating(true);
    setError(null);
    try {
      const res = await api.createGatewayKey(newName.trim());
      setNewName("");
      await load();
      // 创建响应里带完整密钥：复制到剪贴板方便一次取走
      if (res.key?.key) {
        try {
          await navigator.clipboard.writeText(res.key.key);
          setCopiedId(res.key.id);
          setTimeout(() => setCopiedId(null), 4000);
        } catch {
          /* 剪贴板不可用则忽略，列表里有脱敏展示 */
        }
      }
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setCreating(false);
    }
  }

  async function deleteKey(id: string) {
    setBusy(id);
    try {
      await api.deleteGatewayKey(id);
      await load();
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <section className="mt-10 min-w-0 space-y-2.5" aria-label="网关管理">
      <div className="flex min-w-0 flex-wrap items-baseline justify-between gap-2 px-1">
        <h2 className="text-[13px] font-medium leading-5">
          <ScrollText className="mr-1.5 inline size-3.5 align-[-2px]" />
          {title || "API Keys 与请求日志"}
        </h2>
        {overview ? (
          <span className="text-xs text-muted-foreground">
            近 24h：{overview.requests24h} 次请求（成功 {overview.ok24h} / 失败 {overview.err24h}）·
            账号池 Qoder {overview.accountPools.qoder} / Kimi {overview.accountPools.kimi}
          </span>
        ) : null}
      </div>

      {error ? <p className="px-1 text-xs text-destructive">{error}</p> : null}

      {/* API Keys */}
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
            <CardDescription className="min-w-0 text-xs">
              网关专用密钥（与 GLM 代理主密钥并行有效）；创建后自动复制，仅此一次可见
            </CardDescription>
            <div className="flex items-center gap-1.5">
              <Input
                className="h-8 w-40 text-xs"
                placeholder="密钥名称（可选）"
                value={newName}
                onChange={(e) => setNewName(e.target.value)}
                aria-label="密钥名称"
              />
              <Button size="sm" variant="outline" onClick={() => void createKey()} disabled={creating}>
                {creating ? <Loader2 className="animate-spin" /> : <Plus />}创建密钥
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent className="p-0">
          <div className="divide-y divide-border/50 border-t border-border/50">
            {keys.length === 0 ? (
              <p className="px-4 py-4 text-center text-xs text-muted-foreground sm:px-5">
                尚未创建网关密钥（当前可用 GLM 代理主密钥访问两个网关）。
              </p>
            ) : (
              keys.map((key) => (
                <div key={key.id} className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-2.5 sm:px-5">
                  <div className="min-w-0">
                    <div className="flex min-w-0 flex-wrap items-center gap-2">
                      <span className="truncate text-[13px] font-medium">{key.name}</span>
                      <code className="rounded bg-muted/70 px-1.5 py-0.5 text-[11px]" title={key.keyPreview}>
                        {key.keyPreview}
                      </code>
                      {copiedId === key.id ? (
                        <Badge variant="success" className="text-[10px]">
                          <Check className="mr-0.5 inline size-3" />已复制
                        </Badge>
                      ) : null}
                    </div>
                    <p className="mt-0.5 text-[11px] text-muted-foreground">
                      创建 {formatTime(key.createdAt)} · 上次使用 {formatTime(key.lastUsedAt)} ·{" "}
                      {key.requestCount} 次请求
                    </p>
                  </div>
                  <Button size="sm" variant="ghost" className="text-destructive hover:text-destructive" onClick={() => void deleteKey(key.id)} disabled={busy === key.id} aria-label="删除密钥">
                    {busy === key.id ? <Loader2 className="animate-spin" /> : <Trash2 />}
                  </Button>
                </div>
              ))
            )}
          </div>
        </CardContent>
      </Card>

      {/* 请求日志 */}
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardHeader className="gap-0 border-b px-4 pt-3 pb-2 sm:px-5">
          <CardDescription className="text-xs">
            本网关最近 30 条请求（保留最近 500 条；不含对话内容，只记元数据）
          </CardDescription>
        </CardHeader>
        <CardContent className="p-0">
          <div className="divide-y divide-border/50">
            {logs.length === 0 ? (
              <p className="px-4 py-4 text-center text-xs text-muted-foreground sm:px-5">
                暂无请求记录。
              </p>
            ) : (
              logs.map((log, index) => (
                <div key={`${log.ts}-${index}`} className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-2 sm:px-5">
                  <div className="flex min-w-0 flex-wrap items-center gap-2 text-xs">
                    <Badge variant={log.ok ? "success" : "destructive"} className="text-[10px]">
                      {log.ok ? "成功" : "失败"}
                    </Badge>
                    <span className="font-medium">{log.model}</span>
                    <span className="text-muted-foreground">{formatTime(log.ts)}</span>
                  </div>
                  <div className="flex min-w-0 items-center gap-2 text-[11px] text-muted-foreground">
                    {log.error ? (
                      <span className="max-w-[280px] truncate text-destructive" title={log.error}>
                        {log.error}
                      </span>
                    ) : null}
                    <span className="tabular-nums">{log.latencyMs}ms</span>
                  </div>
                </div>
              ))
            )}
          </div>
        </CardContent>
      </Card>
    </section>
  );
}

/** 复制按钮（Qoder 手动导入等场景） */
export function CopySmallButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      size="sm"
      variant="ghost"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 3000);
        } catch {
          /* ignore */
        }
      }}
      aria-label={`复制${label}`}
    >
      {copied ? <Check /> : <Copy />}
    </Button>
  );
}
