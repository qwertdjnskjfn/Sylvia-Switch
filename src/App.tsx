import { Component, Suspense, lazy, useEffect, useState, type ErrorInfo, type ReactNode } from "react";
import { BrowserRouter, HashRouter, Navigate, NavLink, Outlet, Route, Routes } from "react-router-dom";
import { AlertTriangle, ArrowUp, Loader2, RotateCcw, Settings, User, BarChart3 } from "lucide-react";

import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type { UpdateInfo } from "@/lib/types";
import AccountsPage from "@/pages/AccountsPage";
import GlmProxyPage from "@/pages/GlmProxyPage";
import QoderPage from "@/pages/QoderPage";
import KimiPage from "@/pages/KimiPage";
import GlmwebPage from "@/pages/GlmwebPage";
import GrokPage from "@/pages/GrokPage";
import LoomyPage from "@/pages/LoomyPage";
import TraeWorkbuddyProxyPage from "@/pages/TraeWorkbuddyProxyPage";

const StatsPage = lazy(() => import("@/pages/StatsPage"));
const SettingsPage = lazy(() => import("@/pages/SettingsPage"));
import { StatusDot, AppIconMark } from "@/components/product-marks";
import { SidebarThemeSwitcher } from "@/components/theme-switcher";
import { GatewaySwitcher } from "@/components/gateway/gateway-switcher";
import { GatewayNavGroup } from "@/components/gateway/gateway-nav-group";
import { UpdateInstallDialog } from "@/components/update-install-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Toaster } from "@/components/ui/sonner";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { demoModeEnabled, pagesDemoHostingEnabled } from "@/lib/demo-mode";
import { useCreditAutoRefresh } from "@/lib/use-credit-auto-refresh";
import { useWorkbuddyStatusRefresh } from "@/lib/use-workbuddy-status-refresh";
import { useAccountsStore } from "@/stores/accounts";

function UpdateCenter({ running }: { running: boolean | undefined }) {
  const version = useAccountsStore((s) => s.status?.version);
  const clientVersion = useAccountsStore((s) => s.status?.workbuddyVersion);
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);

  useEffect(() => {
    let disposed = false;

    async function checkForUpdate() {
      try {
        const result = await api.checkUpdate();
        if (!disposed) setInfo(result.ok ? result : null);
      } catch {
        // 左下角只展示可操作的升级状态，网络错误不打扰正常使用。
      }
    }

    void checkForUpdate();
    const timer = window.setInterval(() => void checkForUpdate(), 30 * 60 * 1000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, []);

  const hasUpdate = Boolean(info?.ok && info.hasUpdate && info.latest);
  // webui（launcher 用 Chrome --app 打开的那个界面）没有桌面宿主，
  // 无法执行自更新安装；此处只隐藏「更新」按钮，版本信息照常展示。
  const canAutoInstall = !api.isWebui();

  return (
    <>
      <section className="mt-auto border-t border-sidebar-border px-2 pt-3 text-xs">
        {/* WorkBuddy 桌面客户端：运行状态 + 客户端版本（来自注册表，读不到显示「版本未知」） */}
        <div className="flex items-center gap-2 text-[13px] text-sidebar-foreground">
          <StatusDot on={Boolean(running)} />
          <span className="min-w-0 flex-1 truncate">WorkBuddy 客户端</span>
          <span className="shrink-0 text-sidebar-foreground/50">
            {clientVersion ? `v${clientVersion}` : "版本未知"}
          </span>
        </div>
        {/* Sylvia Switch 服务自身版本（= 二进制版本，与 /api/version 一致）；
            更新按钮也归这行 —— 更新源就是 Sylvia Switch 自己的 GitHub 仓库 */}
        <div className="mt-1 flex items-center gap-2 text-[11px] text-sidebar-foreground/50">
          <span className="min-w-0 flex-1 truncate">Sylvia Switch 服务</span>
          <span className="shrink-0 tabular-nums">v{version || "?"}</span>
          {hasUpdate && canAutoInstall && (
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  type="button"
                  size="icon"
                  className="size-5 shrink-0 rounded-full p-0"
                  aria-label="更新"
                  onClick={() => setDialogOpen(true)}
                >
                  <ArrowUp className="size-3" strokeWidth={2.5} aria-hidden="true" />
                </Button>
              </TooltipTrigger>
              <TooltipContent side="top">更新 Sylvia Switch</TooltipContent>
            </Tooltip>
          )}
        </div>
      </section>
      <UpdateInstallDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        update={info}
      />
    </>
  );
}

interface ErrorBoundaryProps {
  children: ReactNode;
}

interface ErrorBoundaryState {
  hasError: boolean;
  error: Error | null;
}

class PageErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  constructor(props: ErrorBoundaryProps) {
    super(props);
    this.state = { hasError: false, error: null };
  }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { hasError: true, error };
  }

  componentDidCatch(error: Error, errorInfo: ErrorInfo) {
    console.error("页面渲染异常:", error, errorInfo);
    const msg = error?.message || "";
    if (
      msg.includes("Failed to fetch dynamically imported module") ||
      msg.includes("Importing a module script failed") ||
      msg.includes("dynamically imported module")
    ) {
      window.location.reload();
    }
  }

  render() {
    if (this.state.hasError) {
      return (
        <div className="flex h-96 flex-col items-center justify-center p-6 text-center">
          <div className="mb-4 rounded-full bg-destructive/10 p-3 text-destructive">
            <AlertTriangle className="size-8" />
          </div>
          <h2 className="text-lg font-semibold text-foreground">页面加载出现问题</h2>
          <p className="mt-2 max-w-md text-xs text-muted-foreground">
            {this.state.error?.message || "组件在渲染时发生未捕获的错误。"}
          </p>
          <div className="mt-6 flex gap-3">
            <Button
              variant="outline"
              size="sm"
              onClick={() => {
                this.setState({ hasError: false, error: null });
                window.location.reload();
              }}
            >
              <RotateCcw className="mr-1.5 size-3.5" />
              刷新页面
            </Button>
            <Button
              size="sm"
              onClick={() => {
                this.setState({ hasError: false, error: null });
                window.location.href = "/";
              }}
            >
              返回首页
            </Button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}

function Layout() {
  const running = useAccountsStore((s) => s.status?.running);
  const version = useAccountsStore((s) => s.status?.version);
  const hasUnifiedTitleBar =
    api.isDesktop() && typeof navigator !== "undefined" && navigator.userAgent.includes("Macintosh");
  useCreditAutoRefresh();
  useWorkbuddyStatusRefresh();

  // 标题栏/标签页带上版本号，便于一眼确认跑的是哪一版（版本号取自后端 /api/version）。
  useEffect(() => {
    if (version) document.title = `Sylvia Switch ${version}`;
  }, [version]);

  return (
    <div className="flex h-screen min-h-0 overflow-hidden bg-background">
      {hasUnifiedTitleBar ? (
        <div
          data-tauri-drag-region
          className="fixed inset-x-0 top-0 z-50 h-8"
          aria-hidden="true"
        />
      ) : null}
      <aside
        className={cn(
          "flex min-h-0 w-[220px] shrink-0 flex-col border-r border-sidebar-border bg-sidebar px-3 pb-4",
          hasUnifiedTitleBar ? "pt-20" : "pt-4",
        )}
      >
        <div className="flex items-center gap-2.5 px-1 pb-5">
          <AppIconMark size={36} className="drop-shadow-sm" />
          <div className="min-w-0">
            <div
              className="truncate text-[15px] leading-5 tracking-[-0.02em] text-sidebar-foreground/90"
              style={{
                fontFamily:
                  '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif',
                fontWeight: 640,
              }}
            >
              Sylvia Switch
            </div>
            <div className="mt-1 flex items-center gap-1.5">
              {version ? (
                <span
                  className="shrink-0 rounded-full bg-sidebar-accent px-1.5 py-0.5 text-[10px] leading-none font-medium tabular-nums text-sidebar-foreground/60"
                  title={`Sylvia Switch v${version}`}
                >
                  v{version}
                </span>
              ) : null}
              {pagesDemoHostingEnabled ? (
                <Badge variant="outline" className="h-5 px-1.5 text-[10px] text-muted-foreground">
                  静态演示
                </Badge>
              ) : null}
            </div>
          </div>
        </div>
        <div className="px-2 pb-1">
          <GatewaySwitcher />
        </div>
        <nav className="flex min-h-0 flex-1 flex-col gap-0.5" aria-label="主导航">
          <NavLink
            to="/"
            end
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-xl px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                isActive
                  ? "bg-primary/[0.13] font-medium text-foreground"
                  : "text-muted-foreground hover:bg-foreground/[0.045] hover:text-foreground",
              )
            }
          >
            <User className="size-4" />
            账号管理
          </NavLink>
          <NavLink
            to="/stats"
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-xl px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                isActive
                  ? "bg-primary/[0.13] font-medium text-foreground"
                  : "text-muted-foreground hover:bg-foreground/[0.045] hover:text-foreground",
              )
            }
          >
            <BarChart3 className="size-4" />
            统计
          </NavLink>
          <GatewayNavGroup />
          <NavLink
            to="/settings"
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-xl px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                isActive
                  ? "bg-primary/[0.13] font-medium text-foreground"
                  : "text-muted-foreground hover:bg-foreground/[0.045] hover:text-foreground",
              )
            }
          >
            <Settings className="size-4" />
            设置
          </NavLink>
        </nav>
        <div className="mt-auto space-y-2 pt-2">
          <SidebarThemeSwitcher />
          {demoModeEnabled ? null : <UpdateCenter running={running} />}
        </div>
      </aside>
      <main
        className={cn(
          "min-w-0 flex-1 overflow-y-auto bg-background overscroll-contain",
          hasUnifiedTitleBar && "pt-16 [&>div]:pt-4",
        )}
      >
        <PageErrorBoundary>
          <Suspense
            fallback={
              <div className="flex h-64 items-center justify-center text-sm text-muted-foreground">
                <Loader2 className="mr-2 size-5 animate-spin text-primary" />
                加载页面中…
              </div>
            }
          >
            <Outlet />
          </Suspense>
        </PageErrorBoundary>
      </main>
    </div>
  );
}

export default function App() {
  const Router = pagesDemoHostingEnabled ? HashRouter : BrowserRouter;

  return (
    <TooltipProvider delayDuration={250}>
      <Router>
        <Routes>
          <Route element={<Layout />}>
            <Route path="/" element={<AccountsPage />} />
            {/* 统一统计页：三视角合一；旧路由重定向保证书签/深链不断 */}
            <Route path="/stats" element={<StatsPage />} />
            <Route path="/credit-stats" element={<Navigate to="/stats?tab=credit" replace />} />
            <Route path="/token-stats" element={<Navigate to="/stats?tab=token" replace />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="/glm-proxy" element={<GlmProxyPage />} />
            <Route path="/qoder" element={<QoderPage />} />
            <Route path="/kimi" element={<KimiPage />} />
            <Route path="/glmweb" element={<GlmwebPage />} />
            <Route path="/grok" element={<GrokPage />} />
            <Route path="/loomy" element={<LoomyPage />} />
            <Route path="/trae-workbuddy" element={<TraeWorkbuddyProxyPage />} />
            <Route path="/model-usage" element={<Navigate to="/stats?tab=model" replace />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
        <Toaster />
      </Router>
    </TooltipProvider>
  );
}
