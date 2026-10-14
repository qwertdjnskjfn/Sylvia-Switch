/**
 * 池健康面板：region 可用性 + 账号健康度。
 *
 * # 它解决的唯一问题
 *
 * **一眼分辨「配额不足」与「服务故障」**。二者症状同形（客户端都是 503 /
 * `无可用账号`），但处置完全相反：
 * - **配额不足**（上游模型频率上限）→ 只能等重置 / 换模型 / 加账号，**重启换号都没用**；
 * - **服务故障**（出口不通 / 熔断 / 期间网络）→ 需要排查链路、可能等熔断退避。
 *
 * 2026-10-13 一天内两次因为分不清这两者而误判（详见 CHANGELOG 2.0.53 / 2.0.54），
 * 本面板即为闭环。
 *
 * # 判据（`judgeRegion`，改这里前先想清楚后果）
 *
 * | region 状态 | 结论 | 依据 |
 * | --- | --- | --- |
 * | 无账号 | ⚪ 结构性缺号 | `accounts.length === 0`——不是限流也不是故障，是池里没配 |
 * | 有健康号 + 无模型限流 | 🟢 可用 | `!disabled && until<=now && breaker<=now`（与后端 `Entry::healthy` 同口径）|
 * | 有健康号 + 有模型限流 | 🟡 **部分模型限流** | 账号能用，但你要的模型可能被限——**「有账号可用」≠「模型能用」** |
 * | 无健康号 + 有熔断/出池 | 🔴 服务故障 | 熔断由**连续失败**喂出，指向链路问题，且需人工介入 |
 * | 无健康号 + 全部被模型级冷却 | 🟡 配额不足 | `model_cools` 命中 ⇒ 上游对**该模型**的频率上限 |
 * | 无健康号 + 仅软冷却 | 🟠 冷却中 | 429 类限流退避，会自行恢复 |
 * | 以上都不命中 | ⚪ 原因未知 | **不猜**——宁可说不知道，也不给错结论 |
 *
 * 🔴 「模型级冷却」**不**计入池健康：被限的是某个模型，账号本身仍健康。
 * 但它**必须**出现在结论文案里——首版把它漏了，导致 intl 区模型已被限流、
 * 面板却显示 🟢 可用（实测打回）。故单列 `partial` 档位 + 明细表「模型限流」列。
 *
 * # 数据来源与刷新
 *
 * - 来源：`GET /workbuddy-pool/v1/status`（服务端读 `accounts.json` +
 *   `workbuddy_gateway_state.json`，含账号级 `model_cools` / `breaker_until`）。
 * - 刷新：**60s 自动轮询**（与同页其它面板一致）+ 手动刷新按钮。
 *   选 60s 的理由：冷却/熔断的时间尺度是分钟级（软冷却 600s 起、模型冷却按上游
 *   重置时间），更快轮询只会空转。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertCircle, Loader2, RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type { WorkbuddyPoolStatusResponse } from "@/lib/api";
import { formatCredits, formatDateTimeShort } from "@/lib/stats-format";
// 🔴 判据一律从共享模块取，**不要在本文件重新实现**——
// 该判据还有第二个消费方（WorkbuddyPage 的 cloud 区块），两份必然漂移。
import { LEVEL_STYLE, activeModelCools, groupByRegion, isAccountHealthy } from "@/lib/pool-health";

/** 轮询间隔（ms）——与同页其它面板一致。 */
const POLL_MS = 60_000;


function StatCell({ label, value, tone }: { label: string; value: string; tone?: string }) {
  return (
    <div className="min-w-0">
      <p className="text-[11px] text-muted-foreground">{label}</p>
      <p className={cn("truncate font-semibold tabular-nums leading-6", tone ?? "text-foreground")}>
        {value}
      </p>
    </div>
  );
}

export function PoolHealthPanel() {
  const [status, setStatus] = useState<WorkbuddyPoolStatusResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [updatedAt, setUpdatedAt] = useState(0);
  /** 正在切换暂停态的账号 uid（禁用该行按钮，防连点）。 */
  const [pauseBusy, setPauseBusy] = useState<string | null>(null);
  /** 操作类错误（暂停/恢复失败）——与整页加载失败分开显示，不遮挡已有数据。 */
  const [opError, setOpError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setStatus(await api.getWorkbuddyPoolStatus());
      setUpdatedAt(Date.now());
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, []);

  const togglePause = useCallback(
    async (uid: string, paused: boolean) => {
      setPauseBusy(uid);
      setOpError(null);
      try {
        await api.setWorkbuddyPoolAccountPause(uid, paused);
        await load();
      } catch (e) {
        setOpError(api.asError(e));
      } finally {
        setPauseBusy(null);
      }
    },
    [load],
  );

  useEffect(() => {
    void load();
    const timer = window.setInterval(() => void load(), POLL_MS);
    return () => window.clearInterval(timer);
  }, [load]);

  const view = useMemo(() => {
    if (!status) return null;
    const now = Date.now();
    const accounts = status.accounts ?? [];
    // 分组与裁决走共享模块（`@/lib/pool-health`）——与 cloud 区块同一份实现。
    const regions = groupByRegion(accounts, now);
    return {
      regions,
      accounts,
      // 账号库里有、但没能进池的记录（诊断）。旧后端无此字段 → 空数组。
      rejected: status.rejected ?? [],
      abnormal: accounts.filter(
        (a) => !isAccountHealthy(a, now) || activeModelCools(a, now).length > 0,
      ).length,
      cooling: status.cooling ?? 0,
      modelCooling: status.model_cooling ?? 0,
      breaker: status.breaker ?? 0,
      healthy: status.healthy ?? 0,
      total: status.total ?? accounts.length,
    };
  }, [status]);

  if (error && !status) {
    return (
      <Card className="rounded-xl shadow-none">
        <CardContent className="flex items-start gap-2.5 p-4 text-sm">
          <AlertCircle className="mt-0.5 size-4 shrink-0 text-destructive" />
          <div className="min-w-0 flex-1">
            <p className="font-medium">池健康数据加载失败</p>
            <p className="mt-0.5 break-all text-muted-foreground">{error}</p>
          </div>
          <Button size="sm" variant="outline" onClick={() => void load()}>重试</Button>
        </CardContent>
      </Card>
    );
  }

  if (!view) {
    return (
      <Card className="rounded-xl shadow-none">
        <CardContent className="flex items-center gap-2 p-4 text-sm text-muted-foreground">
          <Loader2 className="size-4 animate-spin" /> 正在读取账号池状态…
        </CardContent>
      </Card>
    );
  }

  return (
    <Card className="rounded-xl shadow-none">
      <CardContent className="space-y-3.5 p-4">
        {/* 头部 */}
        <div className="flex flex-wrap items-end justify-between gap-2">
          <div className="min-w-0">
            <h3 className="text-[13px] font-medium">账号池健康 · region 可用性</h3>
            <p className="mt-0.5 text-[11px] text-muted-foreground">
              数据源 <code className="font-mono">/workbuddy-pool/v1/status</code>
              （读 <code className="font-mono">accounts.json</code> 与
              <code className="font-mono"> workbuddy_gateway_state.json</code>）· 每 60 秒自动刷新
              {updatedAt > 0 && <> · 更新于 {new Date(updatedAt).toLocaleTimeString("zh-CN")}</>}
            </p>
          </div>
          <Button size="sm" variant="outline" disabled={loading} onClick={() => void load()}>
            {loading ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
            <span className="ml-1.5">刷新</span>
          </Button>
        </div>

        {/* 计数 */}
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <StatCell label="账号总数" value={String(view.total)} />
          <StatCell
            label="可用"
            value={String(view.healthy)}
            tone={view.healthy > 0 ? "text-emerald-600" : "text-red-600"}
          />
          <StatCell
            label="账号级冷却"
            value={String(view.cooling)}
            tone={view.cooling > 0 ? "text-orange-600" : undefined}
          />
          <StatCell
            label="熔断 / 模型限流"
            value={`${view.breaker} / ${view.modelCooling}`}
            tone={view.breaker > 0 ? "text-red-600" : view.modelCooling > 0 ? "text-amber-600" : undefined}
          />
        </div>

        {/* 🔴 核心：region 结论 */}
        <div className="space-y-2">
          {view.regions.map((r) => {
            const s = LEVEL_STYLE[r.verdict.level];
            return (
              <div
                key={r.key}
                className={cn("rounded-lg border border-border/60 p-2.5", s.box)}
              >
                <div className="flex flex-wrap items-center gap-x-2.5 gap-y-1 text-xs">
                  <span className={cn("size-2 shrink-0 rounded-full", s.dot)} />
                  <span className="font-mono font-medium">{r.key}</span>
                  <span className={cn("font-medium", s.text)}>{r.verdict.label}</span>
                  <span className="tabular-nums text-muted-foreground">
                    {r.healthyCount}/{r.accounts.length} 账号可用
                  </span>
                  {r.cooled.length > 0 && (
                    <span className="text-amber-700">
                      模型限流 {r.cooled.length} 个
                    </span>
                  )}
                </div>
                <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground">
                  {r.verdict.detail}
                </p>
              </div>
            );
          })}
        </div>

        {/* 操作错误（暂停/恢复失败）：单独一条，不遮挡已有数据 */}
        {opError && (
          <div className="flex items-start gap-2 rounded-lg border border-destructive/40 bg-destructive/5 px-3 py-2 text-[11px]">
            <AlertCircle className="mt-0.5 size-3.5 shrink-0 text-destructive" />
            <span className="min-w-0 flex-1 break-all text-destructive">{opError}</span>
          </div>
        )}

        {/* 账号明细 */}
        <div className="overflow-x-auto">
          <table className="w-full text-xs">
            <thead>
              <tr className="text-muted-foreground">
                <th className="pb-2 pr-3 text-left font-normal">账号</th>
                <th className="pb-2 pr-3 text-left font-normal">区</th>
                <th className="pb-2 pr-3 text-left font-normal">状态</th>
                <th className="pb-2 pr-3 text-right font-normal">积分</th>
                <th className="pb-2 pr-3 text-right font-normal">成功/失败</th>
                <th className="pb-2 pr-3 text-right font-normal">在途</th>
                <th className="pb-2 pr-3 text-left font-normal">模型限流</th>
                <th className="pb-2 text-right font-normal">操作</th>
              </tr>
            </thead>
            <tbody className="tabular-nums">
              {view.accounts.map((a) => {
                const now = Date.now();
                const healthy = isAccountHealthy(a, now);
                const cools = activeModelCools(a, now);
                return (
                  <tr
                    key={a.uid}
                    className={cn(
                      "border-t border-border/60",
                      a.disabled && "bg-red-50/40",
                      !a.disabled && a.breaker_until > now && "bg-red-50/40",
                      healthy && cools.length > 0 && "bg-amber-50/40",
                    )}
                  >
                    <td className="py-2 pr-3">
                      <span className="font-medium">{a.nickname || a.uid}</span>
                    </td>
                    <td className="py-2 pr-3 font-mono">{a.region}</td>
                    <td className="py-2 pr-3">
                      {a.disabled ? (
                        <span className="text-red-700">已出池</span>
                      ) : a.paused ? (
                        // 用户主动暂停：措辞必须与「出池/故障」区分开，否则会被当成账号坏了
                        <span className="text-slate-600">已暂停（手动）</span>
                      ) : a.breaker_until > now ? (
                        <span className="text-red-700">
                          熔断至 {formatDateTimeShort(a.breaker_until)}
                        </span>
                      ) : a.until > now ? (
                        <span className="text-orange-700">
                          {a.cooldown_kind === "hard_credit" ? "积分冷却" : "限流冷却"}
                          至 {formatDateTimeShort(a.until)}
                        </span>
                      ) : (
                        <span className="text-emerald-700">健康</span>
                      )}
                    </td>
                    <td className="py-2 pr-3 text-right">{formatCredits(a.credits)}</td>
                    <td className="py-2 pr-3 text-right">
                      {a.success_count} / {a.err_total}
                    </td>
                    <td className="py-2 pr-3 text-right">{a.in_flight}</td>
                    <td className="py-2">
                      {cools.length === 0 ? (
                        <span className="text-muted-foreground">—</span>
                      ) : (
                        <span className="text-amber-700">
                          {cools
                            .map((m) => `${m.model}（${formatDateTimeShort(m.until)}）`)
                            .join("、")}
                        </span>
                      )}
                    </td>
                    <td className="py-2 text-right">
                      {/* 暂停是可逆的用户开关；`disabled`（session 死）不给按钮——
                          它只能靠重新登录恢复，给按钮是假的。 */}
                      {a.disabled ? (
                        <span className="text-[11px] text-muted-foreground">需重新登录</span>
                      ) : (
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-6 px-2 text-[11px]"
                          disabled={pauseBusy === a.uid}
                          onClick={() => void togglePause(a.uid, !a.paused)}
                        >
                          {pauseBusy === a.uid ? (
                            <Loader2 className="size-3 animate-spin" />
                          ) : a.paused ? (
                            "恢复"
                          ) : (
                            "暂停"
                          )}
                        </Button>
                      )}
                    </td>
                  </tr>
                );
              })}
              {view.accounts.length === 0 && (
                <tr>
                  <td colSpan={7} className="py-6 text-center text-muted-foreground">
                    账号池为空——请先在「WorkBuddy 多账号」页添加账号
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>

        {/* 未能进池的记录：解决「导入成功、池里却看不到」时无处可查的问题 */}
        {view.rejected.length > 0 && (
          <div className="rounded-lg border border-amber-300 bg-amber-50/50 p-2.5">
            <p className="text-[11px] font-medium text-amber-800">
              账号库里有 {view.rejected.length} 条记录**没能进池**（因此不参与选号）：
            </p>
            <ul className="mt-1.5 space-y-1">
              {view.rejected.map((r) => (
                <li key={`${r.index}-${r.id}`} className="text-[11px] leading-relaxed text-amber-900">
                  <span className="font-medium">{r.nickname || r.id || `第 ${r.index + 1} 条`}</span>
                  {r.domain ? <span className="text-amber-700"> · {r.domain}</span> : null}
                  <span className="text-amber-700"> · platform={r.platform}</span>
                  <br />
                  <span className="text-amber-800">原因：{r.reason}</span>
                </li>
              ))}
            </ul>
          </div>
        )}

        {/* 判读提示：把「怎么用这个面板」直接写在界面上 */}
        <p className="rounded-lg bg-muted/40 px-3 py-2 text-[11px] leading-relaxed text-muted-foreground">
          <strong className="font-medium text-foreground/80">怎么读：</strong>
          <span className="text-amber-700">配额不足</span> = 上游按**模型**限流，换号/重启都没用，
          只能等重置或换模型；<span className="text-red-700">服务故障</span> = 熔断/出池/链路不通，
          需排查出口；<span className="text-emerald-700">可用</span> = 有账号能参与选号
          （不代表你要的那个模型没被限流——请看「模型限流」列）。
        </p>
      </CardContent>
    </Card>
  );
}
