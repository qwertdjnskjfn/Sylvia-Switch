import { lazy, Suspense } from "react";
import { useSearchParams } from "react-router-dom";
import { BarChart3, Coins, Loader2, MessagesSquare, Waypoints } from "lucide-react";

import { cn } from "@/lib/utils";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";

/**
 * 统一统计页：模型用量 / Token / 积分 / WorkBuddy 逆向渠道 四个视角合并为一个入口。
 *
 * - 每个 Tab 复用既有页面组件（功能零丢失，数据口径各自保留）；
 * - 当前 Tab 同步到 `?tab=` 查询参数（可深链、可前进后退）；
 * - 旧路由 `/model-usage`、`/token-stats`、`/credit-stats` 由 App.tsx 重定向到这里。
 *
 * 各视角**不跨源合并数据**（口径不同：代理记账 / 会话扫描 / 官方投影，
 * 同一模型会同时出现在多个视角，合并会造成重复计数的观感），
 * 因此只统一「入口与布局」，数据展示保持各视角自己的口径。
 *
 * `workbuddy` 视角为**独立渠道面板**（2026-10-13 新增）：它按 `channel` 前缀
 * 过滤，服务的是 WorkBuddy 逆向渠道（ACP 直连 + OpenCode 借道两条链路），
 * 与「调用与模型」的账号口径互不混算，故单开一页而非并入既有页面。
 */

const ModelUsagePage = lazy(() => import("@/pages/ModelUsagePage"));
const WorkBuddyMonitorPage = lazy(() => import("@/pages/WorkBuddyMonitorPage"));
const TokenStatsPage = lazy(() => import("@/pages/TokenStatsPage"));
const CreditStatsPage = lazy(() => import("@/pages/CreditStatsPage"));

const TABS = [
  {
    key: "model",
    label: "调用与模型",
    icon: BarChart3,
    hint: "代理转发的每次调用：Token、延迟与积分归因",
  },
  {
    key: "token",
    label: "Token",
    icon: MessagesSquare,
    hint: "本地会话日志扫描：按项目 / 会话 / 模型的 Token 分布",
  },
  {
    key: "credit",
    label: "积分",
    icon: Coins,
    hint: "账号积分余额、消耗走势与到期提醒",
  },
  {
    key: "workbuddy",
    label: "WorkBuddy 逆向",
    icon: Waypoints,
    hint: "WorkBuddy 逆向渠道：模型使用频率、Token 与失败归因",
  },
] as const;

type TabKey = (typeof TABS)[number]["key"];

const VALID_TABS = new Set<string>(TABS.map((tab) => tab.key));
const DEFAULT_TAB: TabKey = "model";

function parseTab(raw: string | null): TabKey {
  return raw && VALID_TABS.has(raw) ? (raw as TabKey) : DEFAULT_TAB;
}

function StatsLoading() {
  return (
    <div className="flex h-64 items-center justify-center text-sm text-muted-foreground">
      <Loader2 className="mr-2 size-5 animate-spin text-primary" />
      加载统计视图…
    </div>
  );
}

export default function StatsPage() {
  const [params, setParams] = useSearchParams();
  const tab = parseTab(params.get("tab"));
  const active = TABS.find((item) => item.key === tab) ?? TABS[0];

  function handleTabChange(next: string) {
    setParams(
      (prev) => {
        const merged = new URLSearchParams(prev);
        merged.set("tab", next);
        return merged;
      },
      { replace: false },
    );
  }

  return (
    <div className="min-w-0">
      {/* 统一页头：只承载 Tab 切换；各视角的标题 / 刷新 / 时间范围由子页面自带
          （三者的刷新节奏与时间范围语义不同，强行共享会改变各自语义）。 */}
      <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 pt-5 sm:px-8">
        <div className="flex flex-wrap items-end justify-between gap-x-4 gap-y-3">
          <div className="min-w-0">
            <h1 className="text-lg font-semibold tracking-[-0.01em] text-foreground">统计</h1>
            <p className="mt-0.5 text-xs text-muted-foreground">{active.hint}</p>
          </div>
          <Tabs value={tab} onValueChange={handleTabChange}>
            <TabsList>
              {TABS.map((item) => {
                const Icon = item.icon;
                return (
                  <TabsTrigger key={item.key} value={item.key} className="gap-1.5 px-3">
                    <Icon className={cn("size-3.5", tab === item.key ? "" : "opacity-60")} />
                    {item.label}
                  </TabsTrigger>
                );
              })}
            </TabsList>
          </Tabs>
        </div>
      </div>

      <Suspense fallback={<StatsLoading />}>
        {tab === "model" ? (
          <ModelUsagePage />
        ) : tab === "token" ? (
          <TokenStatsPage />
        ) : tab === "workbuddy" ? (
          <WorkBuddyMonitorPage />
        ) : (
          <CreditStatsPage />
        )}
      </Suspense>
    </div>
  );
}
