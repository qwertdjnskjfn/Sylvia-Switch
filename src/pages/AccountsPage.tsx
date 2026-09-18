import { useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  Columns3,
  Download,
  Eye,
  EyeOff,
  FileDown,
  FileUp,
  Loader2,
  QrCode,
  RefreshCw,
  Rows3,
  Search,
  X,
} from "lucide-react";

import { AccountCard } from "@/components/account-card";
import { DemoAction } from "@/components/demo-action";
import { WorkBuddyMark } from "@/components/product-marks";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import {
  ACCOUNT_EXTRAS_CHANGE_EVENT,
  getAccountNotes,
  getPrivacyMask,
  setAccountNote,
  setPrivacyMask,
} from "@/lib/account-extras";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ExportAccountsDialog } from "@/components/export-accounts-dialog";
import { ImportAccountsDialog } from "@/components/import-accounts-dialog";
import { OAuthLoginDialog } from "@/components/oauth-login-dialog";
import { SwitchAccountDialog } from "@/components/switch-account-dialog";
import * as api from "@/lib/api";
import type { AccountMeta, AppStatus, CheckinConfig, CreditExpiry, PlatformCheckinItem, TravelConfig, TravelStatus } from "@/lib/types";
import { cn } from "@/lib/utils";
import { useAccountsStore } from "@/stores/accounts";

function expiringSoonAmount(credit?: CreditExpiry): number {
  return credit?.ok ? credit.expiringSoonRemaining ?? 0 : 0;
}

function hasExpiringSoonCredits(credit?: CreditExpiry): boolean {
  return credit?.ok === true && expiringSoonAmount(credit) > 0;
}

function soonestRelevantExpiry(credit?: CreditExpiry): number {
  const soonestExpiringCredit = (credit?.resources ?? [])
    .filter((resource) => resource.remaining > 0 && resource.expiringSoon && resource.expireAt != null)
    .map((resource) => resource.expireAt as number)
    .reduce((soonest, expireAt) => Math.min(soonest, expireAt), Number.POSITIVE_INFINITY);
  return Number.isFinite(soonestExpiringCredit)
    ? soonestExpiringCredit
    : credit?.soonestExpireAt ?? Number.POSITIVE_INFINITY;
}

function creditPriorityRank(credit?: CreditExpiry): number {
  if (!credit?.ok) return 3;
  if (hasExpiringSoonCredits(credit)) return 0;
  if (credit.expired) return 1;
  return 2;
}

function isWorkbuddyCurrent(account: AccountMeta, current: AppStatus["current"] | undefined): boolean {
  if (!current) return false;
  return Boolean(
    (current.uid && (account.uid === current.uid || account.id === current.uid)) ||
      (current.email && account.email === current.email),
  );
}

/** 并行查询今日签到；失败的账号不写入，由调用方保留原值。 */
async function fetchTodayCheckinMap(
  accountIds: string[],
  isStale?: () => boolean,
): Promise<Record<string, boolean>> {
  const entries = await Promise.all(
    accountIds.map(async (id) => {
      try {
        const res = await api.getCheckinStatus(id);
        if (isStale?.() || !res.ok) return null;
        return [id, res.todayCheckedIn] as const;
      } catch {
        return null;
      }
    }),
  );
  const next: Record<string, boolean> = {};
  for (const entry of entries) {
    if (entry) next[entry[0]] = entry[1];
  }
  return next;
}

/** 并行查询各账号今日旅行状态；失败的账号不写入，由调用方保留原值。 */
async function fetchTravelMap(
  accountIds: string[],
  isStale?: () => boolean,
): Promise<Record<string, TravelStatus>> {
  const entries = await Promise.all(
    accountIds.map(async (id) => {
      try {
        const res = await api.getTravelStatus(id);
        if (isStale?.()) return null;
        return [id, res] as const;
      } catch {
        return null;
      }
    }),
  );
  const next: Record<string, TravelStatus> = {};
  for (const entry of entries) {
    if (entry) next[entry[0]] = entry[1];
  }
  return next;
}

export default function AccountsPage() {
  const {
    accounts,
    status,
    loading,
    error,
    fetchAll,
    deleteAccount,
    importLocal,
    creditMap,
    creditLoadingMap,
    creditUpdatedAtMap,
    refreshingCredits,
    ensureCredits,
    refreshCredits,
  } = useAccountsStore();
  const [oauthOpen, setOauthOpen] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [switchAccount, setSwitchAccount] = useState<AccountMeta | null>(null);
  const [importing, setImporting] = useState(false);
  const [autoCheckinConfig, setAutoCheckinConfig] = useState<CheckinConfig | null>(null);
  const [autoCheckinSaving, setAutoCheckinSaving] = useState(false);
  /** 账号 id -> 今日是否已签到（undefined=查询中/未知） */
  const [checkinMap, setCheckinMap] = useState<Record<string, boolean>>({});
  /** 本机平台账号（AutoClaw / TRAE Work CN）签到状态 + 积分 */
  const [platformCheckins, setPlatformCheckins] = useState<PlatformCheckinItem[]>([]);
  const [platformCheckinBusyId, setPlatformCheckinBusyId] = useState<string | null>(null);
  /** 平台卡片积分刷新中 */
  const [platformCreditLoadingId, setPlatformCreditLoadingId] = useState<string | null>(null);
  const [autoTravelConfig, setAutoTravelConfig] = useState<TravelConfig | null>(null);
  const [autoTravelSaving, setAutoTravelSaving] = useState(false);
  /** 账号 id -> 今日旅行状态（undefined=查询中/未知） */
  const [travelMap, setTravelMap] = useState<Record<string, TravelStatus>>({});
  /** 刷新按钮触发的批量签到进行中 */
  const [checkinAllRunning, setCheckinAllRunning] = useState(false);
  /** 删除账号确认目标（null=关闭） */
  const [deleteTarget, setDeleteTarget] = useState<AccountMeta | null>(null);
  /** 紧凑模式：卡片更小、同屏更多列；默认开启，持久化到 localStorage */
  const [compact, setCompact] = useState<boolean>(() => {
    try {
      return localStorage.getItem("wb-switch.compact") !== "0";
    } catch {
      return true;
    }
  });

  // 搜索、过滤、排序与辅助功能状态
  const [searchQuery, setSearchQuery] = useState("");
  const [filterType, setFilterType] = useState<"all" | "unchecked" | "expiring" | "active">("all");
  const [sortType, setSortType] = useState<"default" | "credits-desc" | "credits-asc" | "recent-expiry">("default");
  const [privacyMask, setPrivacyMaskState] = useState<boolean>(getPrivacyMask);
  const [accountNotes, setAccountNotes] = useState<Record<string, string>>(getAccountNotes);
  const [editingNoteAccount, setEditingNoteAccount] = useState<AccountMeta | null>(null);
  const [noteInput, setNoteInput] = useState("");

  useEffect(() => {
    const onExtrasChange = () => {
      setAccountNotes(getAccountNotes());
      setPrivacyMaskState(getPrivacyMask());
    };
    window.addEventListener(ACCOUNT_EXTRAS_CHANGE_EVENT, onExtrasChange);
    return () => window.removeEventListener(ACCOUNT_EXTRAS_CHANGE_EVENT, onExtrasChange);
  }, []);

  function togglePrivacyMask() {
    setPrivacyMask(!privacyMask);
  }

  function openEditNote(account: AccountMeta) {
    setEditingNoteAccount(account);
    setNoteInput(accountNotes[account.id] || "");
  }

  function saveNote() {
    if (!editingNoteAccount) return;
    setAccountNote(editingNoteAccount.id, noteInput);
    setEditingNoteAccount(null);
    toast.success("备注已保存");
  }

  function clearNote() {
    if (!editingNoteAccount) return;
    setAccountNote(editingNoteAccount.id, "");
    setEditingNoteAccount(null);
    toast.success("备注已清除");
  }

  function toggleCompact() {
    setCompact((value) => {
      const next = !value;
      try {
        localStorage.setItem("wb-switch.compact", next ? "1" : "0");
      } catch {
        /* 存储不可用时静默 */
      }
      return next;
    });
  }

  useEffect(() => {
    void fetchAll();
  }, [fetchAll]);

  useEffect(() => {
    let cancelled = false;
    void api
      .getAutoCheckinConfig()
      .then((config) => {
        if (!cancelled) setAutoCheckinConfig(config);
      })
      .catch((e) => {
        if (!cancelled) {
          toast.error("自动签到配置加载失败", { description: api.asError(e) });
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    void api
      .getAutoTravelConfig()
      .then((config) => {
        if (!cancelled) setAutoTravelConfig(config);
      })
      .catch((e) => {
        if (!cancelled) {
          toast.error("自动旅行配置加载失败", { description: api.asError(e) });
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  /** 首次启动自动导入本机账号（本会话只尝试一次，无本机账号时静默） */
  const autoImportTried = useRef(false);
  useEffect(() => {
    if (autoImportTried.current || loading || accounts.length > 0) return;
    autoImportTried.current = true;
    void importLocal()
      .then(() => void fetchAll())
      .catch(() => {
        /* 本机无 WorkBuddy 登录态时静默，不打扰用户 */
      });
  }, [accounts.length, loading, importLocal, fetchAll]);

  async function loadPlatformCheckins() {
    try {
      setPlatformCheckins(await api.getPlatformCheckinItems());
    } catch {
      setPlatformCheckins([]);
    }
  }

  // 账号列表变化后并行查询各账号今日签到状态，并开启 30 秒静默轮询与切屏/唤醒激活更新
  useEffect(() => {
    if (!accounts.length) return;
    let cancelled = false;
    const updateCheckin = () => {
      void fetchTodayCheckinMap(
        accounts.map((account) => account.id),
        () => cancelled,
      ).then((next) => {
        if (!cancelled && Object.keys(next).length > 0) {
          setCheckinMap((prev) => ({ ...prev, ...next }));
        }
      });
      void loadPlatformCheckins();
    };

    updateCheckin();
    const interval = window.setInterval(updateCheckin, 30_000);

    const onVisibilityChange = () => {
      if (document.visibilityState === "visible") {
        updateCheckin();
      }
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    window.addEventListener("focus", updateCheckin);

    return () => {
      cancelled = true;
      window.clearInterval(interval);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      window.removeEventListener("focus", updateCheckin);
    };
  }, [accounts]);

  async function onPlatformCheckin(item: PlatformCheckinItem) {
    if (platformCheckinBusyId !== null) return;
    setPlatformCheckinBusyId(item.accountId);
    try {
      const res = await api.checkin(item.accountId);
      const label =
        res.result === "success"
          ? "签到成功"
          : res.result === "already"
            ? "今天已签到"
            : "签到失败";
      const description = `${item.email}${res.error ? `：${res.error}` : ""}`;
      if (res.result === "error") toast.error(label, { description });
      else toast.success(label, { description });
      void loadPlatformCheckins();
    } catch (e) {
      toast.error("签到失败", { description: api.asError(e) });
    } finally {
      setPlatformCheckinBusyId(null);
    }
  }

  /** AccountCard 回调适配：按 id 找回平台账号条目 */
  async function onPlatformCardCheckin(a: AccountMeta) {
    const item = platformCheckins.find((p) => p.accountId === a.id);
    if (item) await onPlatformCheckin(item);
  }

  /** 某平台的全部账号（供一键切换列出目标）。 */
  function platformAccountsOf(platform: string): AccountMeta[] {
    return platformCheckins
      .filter((p) => p.platform === platform)
      .map((p) => ({
        id: p.accountId,
        uid: null,
        email: p.email,
        nickname: p.email,
        enterpriseName: null,
        expiresAt: null,
        refreshExpiresAt: null,
        refreshedAt: null,
        createdAt: null,
        needsRelogin: false,
        needsReloginReason: null,
      }));
  }

  function onPlatformCardRefresh(a: AccountMeta) {
    if (platformCreditLoadingId !== null) return;
    setPlatformCreditLoadingId(a.id);
    void loadPlatformCheckins()
      .then(() => toast.success("平台积分已刷新", { description: a.nickname || a.id }))
      .catch(() => toast.error("平台积分刷新失败"))
      .finally(() => setPlatformCreditLoadingId(null));
  }

  async function loadTravelMap(accountIds: string[], isStale?: () => boolean) {
    const next = await fetchTravelMap(accountIds, isStale);
    if (!isStale?.() && Object.keys(next).length > 0) {
      setTravelMap((prev) => ({ ...prev, ...next }));
    }
  }

  // 账号列表变化后并行查询旅行状态；后台领取后每 60 秒再拉一次，避免卡片停在「旅行中」。
  useEffect(() => {
    if (!accounts.length) return;
    let cancelled = false;
    const ids = accounts.map((account) => account.id);
    void loadTravelMap(ids, () => cancelled);
    const timer = window.setInterval(() => {
      void loadTravelMap(ids, () => cancelled);
    }, 60_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [accounts]);

  // 只给尚未缓存的账号拉积分；切回首页不重复请求。点「刷新积分」才强制更新。
  useEffect(() => {
    if (!accounts.length) return;
    void ensureCredits(accounts.map((account) => account.id));
  }, [accounts, ensureCredits]);

  async function onImport() {
    setImporting(true);
    try {
      const acc = await importLocal();
      toast.success("账号已导入", { description: acc.nickname || acc.email || acc.id });
    } catch (e) {
      toast.error("导入失败", { description: api.asError(e) });
    } finally {
      setImporting(false);
    }
  }

  async function onAutoCheckinChange(enabled: boolean) {
    if (!autoCheckinConfig || autoCheckinSaving) return;
    const previous = autoCheckinConfig;
    const next = { ...previous, enabled };
    setAutoCheckinConfig(next);
    setAutoCheckinSaving(true);
    try {
      setAutoCheckinConfig(await api.saveAutoCheckinConfig(next));
    } catch (e) {
      setAutoCheckinConfig(previous);
      toast.error("自动签到设置保存失败", { description: api.asError(e) });
    } finally {
      setAutoCheckinSaving(false);
    }
  }

  async function onAutoTravelChange(enabled: boolean) {
    if (!autoTravelConfig || autoTravelSaving) return;
    const previous = autoTravelConfig;
    const next = { ...previous, enabled };
    setAutoTravelConfig(next);
    setAutoTravelSaving(true);
    try {
      setAutoTravelConfig(await api.saveAutoTravelConfig(next));
      if (enabled) {
        toast.success("自动旅行已开启", { description: "正在按官方状态派发或领取" });
        window.setTimeout(() => {
          void loadTravelMap(accounts.map((account) => account.id));
        }, 2500);
      }
    } catch (e) {
      setAutoTravelConfig(previous);
      toast.error("自动旅行设置保存失败", { description: api.asError(e) });
    } finally {
      setAutoTravelSaving(false);
    }
  }

  /** 导出完成提示（含安全提醒）。 */
  function onExported(count: number) {
    const text = `已导出 ${count} 个账号。文件含登录 token，等同密码，请勿上传网盘或发送给他人。`;
    toast.success("导出成功", { description: text });
  }

  /** 导入完成提示：计数 + token 可能过期提醒，并刷新列表。 */
  function onImported(result: { imported: number; skipped: number; overwritten: number }) {
    void fetchAll();
    const overwriteText = result.overwritten > 0 ? `（覆盖 ${result.overwritten} 个）` : "";
    const text = `已导入 ${result.imported} 个${overwriteText}，跳过 ${result.skipped} 个。token 可能已过期，切换后可能需要重新登录。`;
    toast.success("导入成功", { description: text });
  }

  async function onDelete(a: AccountMeta) {
    // 桌面 App（Tauri WebView）不支持 window.confirm，改用 Dialog 确认
    setDeleteTarget(a);
  }

  async function confirmDelete() {
    if (!deleteTarget) return;
    const a = deleteTarget;
    setDeleteTarget(null);
    try {
      await deleteAccount(a.id);
      toast.success("账号已删除");
    } catch (e) {
      toast.error("删除失败", { description: api.asError(e) });
    }
  }

  async function onCheckin(a: AccountMeta) {
    try {
      const res = await api.checkin(a.id);
      const label =
        res.result === "success"
          ? "签到成功"
          : res.result === "already"
            ? "今天已签到"
            : "签到失败";
      const description = `${a.nickname || a.email || a.id}${res.error ? `：${res.error}` : ""}`;
      if (res.result === "error") toast.error(label, { description });
      else toast.success(label, { description });
      // 刷新该账号的今日签到状态
      try {
        const st = await api.getCheckinStatus(a.id);
        if (st.ok) setCheckinMap((prev) => ({ ...prev, [a.id]: st.todayCheckedIn }));
      } catch {
        /* ignore */
      }
      void fetchAll();
      // 签到成功/已签到会带来积分变动，force 刷新该账号积分
      if (res.result !== "error") void refreshCredits([a.id]);
    } catch (e) {
      toast.error("签到失败", { description: api.asError(e) });
    }
  }

  async function onRefresh(a: AccountMeta) {
    try {
      const res = await api.refreshAccountToken(a.id);
      const label = a.nickname || a.email || a.id;
      if (res.needsRelogin) {
        toast.error("Token 刷新失败", { description: `${label}：需重新登录${res.needsReloginReason ? `（${res.needsReloginReason}）` : ""}` });
      } else {
        toast.success("Token 已刷新", { description: label });
      }
      void fetchAll();
    } catch (e) {
      toast.error("Token 刷新失败", { description: api.asError(e) });
    }
  }

  /** 刷新按钮：先跑一轮批量签到并重查今日签到状态，再强制刷新全部积分。 */
  async function onRefreshCredits() {
    if (!accounts.length || refreshingCredits || checkinAllRunning) return;
    setCheckinAllRunning(true);
    try {
      try {
        const res = await api.checkinAll();
        const entries = res.accounts ?? [];
        const success = entries.filter((e) => e.result === "success").length;
        const already = entries.filter((e) => e.result === "already").length;
        const failed = entries.filter((e) => e.result === "error").length;
        const parts: string[] = [];
        if (success > 0) parts.push(`${success} 个签到成功`);
        if (already > 0) parts.push(`${already} 个已签到`);
        if (failed > 0) parts.push(`${failed} 个失败`);
        const summary = parts.length > 0 ? parts.join("，") : "无账号需要签到";
        if (entries.length > 0 && failed === entries.length) {
          toast.error("签到失败", { description: summary });
        } else {
          toast.success("签到完成", { description: summary });
        }
        // 批量签到后重查全部账号的今日签到状态，无需切换页面即反映最新结果
        const next = await fetchTodayCheckinMap(accounts.map((account) => account.id));
        if (Object.keys(next).length > 0) {
          setCheckinMap((prev) => ({ ...prev, ...next }));
        }
        void loadPlatformCheckins();
      } catch (e) {
        toast.error("批量签到失败", { description: api.asError(e) });
      }
      await refreshCredits(accounts.map((account) => account.id));
      await loadTravelMap(accounts.map((account) => account.id));
      toast.success("积分到期情况已刷新");
    } finally {
      setCheckinAllRunning(false);
    }
  }

  const current = status?.current;
  const creditOrderingReady =
    accounts.length > 0 &&
    accounts.every((account) => Boolean(creditMap[account.id]) && !creditLoadingMap[account.id]);
  const orderedAccounts = creditOrderingReady
    ? accounts
        .map((account, index) => ({ account, index }))
        .sort((left, right) => {
          const leftCredit = creditMap[left.account.id];
          const rightCredit = creditMap[right.account.id];
          const rankDifference = creditPriorityRank(leftCredit) - creditPriorityRank(rightCredit);
          if (rankDifference !== 0) return rankDifference;

          const leftExpiry = soonestRelevantExpiry(leftCredit);
          const rightExpiry = soonestRelevantExpiry(rightCredit);
          if (leftExpiry !== rightExpiry) return leftExpiry - rightExpiry;

          const amountDifference = expiringSoonAmount(rightCredit) - expiringSoonAmount(leftCredit);
          if (amountDifference !== 0) return amountDifference;
          return left.index - right.index;
        })
        .map(({ account }) => account)
    : accounts;

  // 统计各标签数量
  const uncheckedCount = useMemo(
    () => accounts.filter((a) => checkinMap[a.id] === false).length,
    [accounts, checkinMap],
  );
  const expiringCount = useMemo(
    () => accounts.filter((a) => hasExpiringSoonCredits(creditMap[a.id])).length,
    [accounts, creditMap],
  );
  const activeCount = useMemo(
    () => accounts.filter((a) => isWorkbuddyCurrent(a, current)).length,
    [accounts, current],
  );

  // 综合搜索、过滤与排序
  const filteredAndSortedAccounts = useMemo(() => {
    let list = accounts;

    // 1. 关键字模糊搜索
    const query = searchQuery.trim().toLowerCase();
    if (query) {
      list = list.filter((a) => {
        const note = (accountNotes[a.id] || "").toLowerCase();
        const nickname = (a.nickname || "").toLowerCase();
        const email = (a.email || "").toLowerCase();
        const uid = (a.uid || "").toLowerCase();
        const id = (a.id || "").toLowerCase();
        return (
          note.includes(query) ||
          nickname.includes(query) ||
          email.includes(query) ||
          uid.includes(query) ||
          id.includes(query)
        );
      });
    }

    // 2. 状态标签过滤
    if (filterType === "unchecked") {
      list = list.filter((a) => checkinMap[a.id] === false);
    } else if (filterType === "expiring") {
      list = list.filter((a) => hasExpiringSoonCredits(creditMap[a.id]));
    } else if (filterType === "active") {
      list = list.filter((a) => isWorkbuddyCurrent(a, current));
    }

    // 3. 排序
    const sorted = [...list];
    if (sortType === "credits-desc") {
      sorted.sort((a, b) => {
        const crA = creditMap[a.id]?.totalRemaining ?? 0;
        const crB = creditMap[b.id]?.totalRemaining ?? 0;
        return crB - crA;
      });
    } else if (sortType === "credits-asc") {
      sorted.sort((a, b) => {
        const crA = creditMap[a.id]?.totalRemaining ?? 0;
        const crB = creditMap[b.id]?.totalRemaining ?? 0;
        return crA - crB;
      });
    } else if (sortType === "recent-expiry") {
      sorted.sort((a, b) => {
        const expA = soonestRelevantExpiry(creditMap[a.id]);
        const expB = soonestRelevantExpiry(creditMap[b.id]);
        return expA - expB;
      });
    } else {
      // 默认排序：即将过期优先
      if (creditOrderingReady) {
        sorted.sort((left, right) => {
          const leftCredit = creditMap[left.id];
          const rightCredit = creditMap[right.id];
          const rankDiff = creditPriorityRank(leftCredit) - creditPriorityRank(rightCredit);
          if (rankDiff !== 0) return rankDiff;
          const leftExpiry = soonestRelevantExpiry(leftCredit);
          const rightExpiry = soonestRelevantExpiry(rightCredit);
          if (leftExpiry !== rightExpiry) return leftExpiry - rightExpiry;
          return expiringSoonAmount(rightCredit) - expiringSoonAmount(leftCredit);
        });
      }
    }

    return sorted;
  }, [
    accounts,
    accountNotes,
    searchQuery,
    filterType,
    sortType,
    creditMap,
    checkinMap,
    creditOrderingReady,
    current,
  ]);
  const priorityAccountId =
    creditOrderingReady
      ? orderedAccounts.find((account) => hasExpiringSoonCredits(creditMap[account.id]))?.id
      : undefined;
  const workbuddyCurrentName = current
    ? current.nickname || current.email || current.uid || "未知账号"
    : "未登录";
  return (
    <div className="mx-auto w-full max-w-[1180px] px-6 py-8 sm:px-8 sm:py-9">
      <header className="mb-6">
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <h1 className="text-[28px] font-semibold tracking-tight">账号管理</h1>
            <p className="mt-2 text-sm leading-6 text-muted-foreground">
              统一管理 WorkBuddy、AutoClaw 与 TRAE Work CN 账号、积分和签到状态。
            </p>
          </div>
          <div className="flex shrink-0 items-center gap-4 pt-1">
            <div className="flex items-center gap-2.5">
              <span className="group relative inline-flex cursor-default">
                <span
                  className={
                    status?.running
                      ? "inline-flex rounded-[22%] bg-primary p-[2px] shadow-sm shadow-primary/40"
                      : "inline-flex rounded-[22%] bg-muted-foreground/30 p-[2px]"
                  }
                >
                  <WorkBuddyMark size={28} />
                </span>
                <span className="pointer-events-none absolute right-0 top-full z-50 mt-2 hidden whitespace-nowrap rounded-md bg-popover px-2.5 py-1.5 text-xs text-popover-foreground shadow-lg ring-1 ring-black/5 group-hover:block">
                  WorkBuddy：{status?.running ? "运行中" : "未运行"} · 当前账号：{workbuddyCurrentName}
                </span>
              </span>
            </div>
          </div>
        </div>
      </header>

      <div className="relative mb-6 overflow-visible rounded-2xl border border-border bg-muted/30 px-5 py-5 shadow-[0_6px_20px_rgba(15,23,42,.025)]">
        <div className="pointer-events-none absolute inset-0 overflow-hidden rounded-2xl">
          <div className="absolute -right-12 -top-20 size-44 rounded-full border-[28px] border-slate-400/[0.035]" />
        </div>
        <div className="relative flex flex-wrap items-center gap-x-5 gap-y-4">
          <div className="min-w-[190px] flex-1">
            <h2 className="text-sm font-semibold text-foreground">添加与迁移账号</h2>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              快速接入新账号，或从已有环境恢复。AutoClaw / TRAE Work CN 无需导入：
              登录本机客户端后自动出现在下方「平台账号」。
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2.5">
            <DemoAction>
              <Button
                className="h-10 bg-primary px-4 text-primary-foreground shadow-sm hover:bg-primary/90"
                onClick={() => setOauthOpen(true)}
              >
                <QrCode />OAuth 扫码添加
              </Button>
            </DemoAction>
            <DemoAction>
              <Button className="h-10 px-4" onClick={onImport} disabled={importing} variant="outline">
                {importing ? <Loader2 className="animate-spin" /> : <Download />}导入本机账号
              </Button>
            </DemoAction>
          </div>
          <div className="flex items-center gap-1">
            <DemoAction>
              <Button variant="ghost" size="sm" className="h-9 px-2.5" onClick={() => setImportOpen(true)} title="从备份文件导入账号">
                <FileUp />导入备份
              </Button>
            </DemoAction>
            <DemoAction>
              <Button variant="ghost" size="sm" className="h-9 px-2.5" onClick={() => setExportOpen(true)} disabled={accounts.length === 0} title="导出账号备份">
                <FileDown />导出
              </Button>
            </DemoAction>
          </div>
        </div>
      </div>

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>加载失败</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <section className="mt-7 min-w-0" aria-labelledby="accounts-list-title">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
          <div className="flex items-center gap-2">
            <h2 id="accounts-list-title" className="text-base font-semibold tracking-tight">账号</h2>
            <Badge
              variant="secondary"
              className="h-6 min-w-6 rounded-full border-0 px-1.5 text-[11px] tabular-nums text-muted-foreground shadow-none"
              aria-label={`${accounts.length} 个账号`}
            >
              {accounts.length}
            </Badge>
          </div>
          <TooltipProvider delayDuration={400}>
            <div className="ml-auto flex items-center gap-1">
              <div className="mr-1 flex items-center gap-2.5">
                <label htmlFor="accounts-auto-checkin" className="cursor-pointer text-xs font-medium text-muted-foreground">
                  自动签到
                </label>
                <DemoAction>
                  <Switch
                    id="accounts-auto-checkin"
                    checked={autoCheckinConfig?.enabled ?? false}
                    disabled={!autoCheckinConfig || autoCheckinSaving}
                    onCheckedChange={(enabled) => void onAutoCheckinChange(enabled)}
                    aria-label="自动签到"
                  />
                </DemoAction>
                {autoCheckinSaving && <Loader2 className="size-3.5 animate-spin text-muted-foreground" aria-label="正在保存自动签到设置" />}
              </div>
              <div className="mr-1 flex items-center gap-2.5">
                <label htmlFor="accounts-auto-travel" className="cursor-pointer text-xs font-medium text-muted-foreground">
                  自动旅行
                </label>
                <DemoAction>
                  <Switch
                    id="accounts-auto-travel"
                    checked={autoTravelConfig?.enabled ?? false}
                    disabled={!autoTravelConfig || autoTravelSaving}
                    onCheckedChange={(enabled) => void onAutoTravelChange(enabled)}
                    aria-label="自动旅行"
                  />
                </DemoAction>
                {autoTravelSaving && <Loader2 className="size-3.5 animate-spin text-muted-foreground" aria-label="正在保存自动旅行设置" />}
              </div>
              <Separator orientation="vertical" className="mx-2 h-5" />
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    className={cn("size-9 rounded-lg", compact && "bg-accent text-accent-foreground")}
                    onClick={toggleCompact}
                    aria-label={compact ? "切换为宽松模式" : "切换为紧凑模式"}
                  >
                    {compact ? <Rows3 /> : <Columns3 />}
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="top">{compact ? "切换为宽松模式" : "切换为紧凑模式"}</TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <span>
                    <DemoAction>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="size-9 rounded-lg"
                        disabled={refreshingCredits || checkinAllRunning || accounts.length === 0}
                        onClick={() => void onRefreshCredits()}
                        aria-label="签到并刷新全部账号积分"
                      >
                        <RefreshCw className={refreshingCredits || checkinAllRunning ? "animate-spin" : undefined} />
                      </Button>
                    </DemoAction>
                  </span>
                </TooltipTrigger>
                <TooltipContent side="top">{api.isDemoMode() ? "演示模式下不可操作" : "签到并刷新全部账号积分"}</TooltipContent>
              </Tooltip>
            </div>
          </TooltipProvider>
        </div>

        {/* 搜索与多维过滤工具栏 */}
        {accounts.length > 0 && (
          <div className="mb-5 flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
            {/* 左侧：搜索输入框 */}
            <div className="relative flex-1 max-w-sm">
              <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="搜索账号昵称、邮箱、UID、备注…"
                className="h-9 pl-9 pr-8 text-xs bg-background/60"
              />
              {searchQuery && (
                <button
                  type="button"
                  onClick={() => setSearchQuery("")}
                  className="absolute right-2.5 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                  aria-label="清空搜索"
                >
                  <X className="size-3.5" />
                </button>
              )}
            </div>

            {/* 中间/右侧：过滤标签组 + 排序 + 隐私脱敏 */}
            <div className="flex flex-wrap items-center gap-2">
              {/* 过滤 Pills */}
              <div className="flex items-center rounded-lg border border-border/60 bg-muted/30 p-0.5 text-xs">
                <button
                  type="button"
                  onClick={() => setFilterType("all")}
                  className={cn(
                    "rounded-md px-2.5 py-1 transition-all",
                    filterType === "all"
                      ? "bg-background font-medium text-foreground shadow-xs"
                      : "text-muted-foreground hover:text-foreground",
                  )}
                >
                  全部
                  <span className="ml-1 text-[10px] text-muted-foreground">({accounts.length})</span>
                </button>
                <button
                  type="button"
                  onClick={() => setFilterType("unchecked")}
                  className={cn(
                    "rounded-md px-2.5 py-1 transition-all",
                    filterType === "unchecked"
                      ? "bg-background font-medium text-foreground shadow-xs"
                      : "text-muted-foreground hover:text-foreground",
                  )}
                >
                  未签到
                  {uncheckedCount > 0 && (
                    <span className="ml-1 rounded-full bg-amber-500/15 px-1 py-0.2 text-[10px] font-semibold text-amber-600 dark:text-amber-400">
                      {uncheckedCount}
                    </span>
                  )}
                </button>
                <button
                  type="button"
                  onClick={() => setFilterType("expiring")}
                  className={cn(
                    "rounded-md px-2.5 py-1 transition-all",
                    filterType === "expiring"
                      ? "bg-background font-medium text-foreground shadow-xs"
                      : "text-muted-foreground hover:text-foreground",
                  )}
                >
                  即将到期
                  {expiringCount > 0 && (
                    <span className="ml-1 rounded-full bg-orange-500/15 px-1 py-0.2 text-[10px] font-semibold text-orange-600 dark:text-orange-400">
                      {expiringCount}
                    </span>
                  )}
                </button>
                <button
                  type="button"
                  onClick={() => setFilterType("active")}
                  className={cn(
                    "rounded-md px-2.5 py-1 transition-all",
                    filterType === "active"
                      ? "bg-background font-medium text-foreground shadow-xs"
                      : "text-muted-foreground hover:text-foreground",
                  )}
                >
                  当前活跃
                  {activeCount > 0 && (
                    <span className="ml-1 text-[10px] text-muted-foreground">({activeCount})</span>
                  )}
                </button>
              </div>

              {/* 排序下拉 */}
              <Select value={sortType} onValueChange={(v: any) => setSortType(v)}>
                <SelectTrigger size="sm" className="h-9 w-[130px] text-xs bg-background/60">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="default">默认优先级</SelectItem>
                  <SelectItem value="credits-desc">积分最多</SelectItem>
                  <SelectItem value="credits-asc">积分最少</SelectItem>
                  <SelectItem value="recent-expiry">到期最近</SelectItem>
                </SelectContent>
              </Select>

              {/* 隐私脱敏开关 */}
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    type="button"
                    variant="outline"
                    size="icon"
                    className={cn("size-9 rounded-lg transition-colors", privacyMask && "bg-primary/10 text-primary border-primary/30")}
                    onClick={togglePrivacyMask}
                    aria-label={privacyMask ? "关闭隐私脱敏" : "开启隐私脱敏"}
                  >
                    {privacyMask ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="top">{privacyMask ? "已开启隐私脱敏（点击关闭）" : "开启隐私脱敏模式"}</TooltipContent>
              </Tooltip>
            </div>
          </div>
        )}

        {loading && accounts.length === 0 ? (
          /* 高保真骨架屏 */
          <div
            className={cn(
              "grid min-w-0 items-start gap-5",
              compact
                ? "grid-cols-[repeat(auto-fit,minmax(min(100%,300px),1fr))]"
                : "grid-cols-[repeat(auto-fit,minmax(min(100%,340px),1fr))]",
            )}
          >
            {[1, 2, 3, 4].map((i) => (
              <div
                key={i}
                className="flex flex-col rounded-2xl border border-border bg-card p-4 shadow-xs"
              >
                <div className="flex items-center gap-3 border-b border-border/50 pb-3">
                  {!compact && <Skeleton className="size-12 rounded-full" />}
                  <div className="flex-1 space-y-2">
                    <Skeleton className="h-4 w-3/4 rounded" />
                    <Skeleton className="h-3 w-1/2 rounded" />
                  </div>
                </div>
                <div className="space-y-2 pt-3">
                  <Skeleton className="h-6 w-1/3 rounded" />
                  <Skeleton className="h-3.5 w-full rounded" />
                  <Skeleton className="h-3.5 w-4/5 rounded" />
                </div>
                <div className="mt-4 flex justify-between gap-2 border-t border-border/50 pt-3">
                  <Skeleton className="h-8 w-20 rounded-lg" />
                  <Skeleton className="h-8 w-20 rounded-lg" />
                </div>
              </div>
            ))}
          </div>
        ) : accounts.length === 0 ? (
          <div className="rounded-xl border border-dashed px-4 py-16 text-center text-sm text-muted-foreground">
            暂无账号。点击上方按钮导入本机账号或扫码登录。
          </div>
        ) : filteredAndSortedAccounts.length === 0 ? (
          <div className="rounded-xl border border-dashed px-4 py-16 text-center text-sm text-muted-foreground">
            <p>未找到符合条件的账号</p>
            <Button
              variant="outline"
              size="sm"
              className="mt-3 text-xs"
              onClick={() => {
                setSearchQuery("");
                setFilterType("all");
              }}
            >
              清空搜索与筛选
            </Button>
          </div>
        ) : (
          <div className={cn("grid min-w-0 items-start gap-5", compact ? "grid-cols-[repeat(auto-fit,minmax(min(100%,300px),1fr))]" : "grid-cols-[repeat(auto-fit,minmax(min(100%,340px),1fr))]")}>
            {filteredAndSortedAccounts.map((a) => (
              <AccountCard
                key={a.id}
                account={a}
                compact={compact}
                note={accountNotes[a.id]}
                privacyMask={privacyMask}
                onEditNote={openEditNote}
                onDelete={onDelete}
                onSwitch={setSwitchAccount}
                onCheckin={onCheckin}
                onRefresh={onRefresh}
                todayCheckedIn={checkinMap[a.id]}
                travelStatus={travelMap[a.id]}
                credit={creditMap[a.id]}
                creditLoading={creditLoadingMap[a.id]}
                creditUpdatedAt={creditUpdatedAtMap[a.id]}
                creditPriority={a.id === priorityAccountId}
                workbuddyActive={isWorkbuddyCurrent(a, current)}
                onQuickSwitch={{
                  accounts,
                  current,
                  // 切换会重启 WorkBuddy：稍等片刻再拉状态，避免读到切换前的旧账号
                  done: () => {
                    window.setTimeout(() => void fetchAll(), 2500);
                  },
                }}
                featuresDisabled={false}
              />
            ))}
          </div>
        )}
      </section>

      {platformCheckins.length > 0 ? (
        <section className="mt-2 space-y-4" aria-labelledby="platform-checkin-title">
          <div>
            <h2 id="platform-checkin-title" className="text-lg font-semibold">
              平台账号
            </h2>
            <p className="text-sm text-muted-foreground">
              本机登录的 AutoClaw / TRAE Work CN，与 WorkBuddy 账号一起自动签到
            </p>
          </div>
          <div className={cn("grid min-w-0 items-start gap-5", compact ? "grid-cols-[repeat(auto-fit,minmax(min(100%,300px),1fr))]" : "grid-cols-[repeat(auto-fit,minmax(min(100%,340px),1fr))]")}>
            {platformCheckins.map((item) => {
              const meta: AccountMeta = {
                id: item.accountId,
                uid: null,
                email: item.email,
                nickname: item.email,
                enterpriseName: null,
                expiresAt: null,
                refreshExpiresAt: null,
                refreshedAt: null,
                createdAt: null,
                needsRelogin: false,
                needsReloginReason: null,
              };
              return (
                <AccountCard
                  key={item.accountId}
                  account={meta}
                  platform={item.platform}
                  compact={compact}
                  privacyMask={privacyMask}
                  todayCheckedIn={item.ok ? item.todayCheckedIn : undefined}
                  credit={item.credit}
                  creditUpdatedAt={item.credit?.updatedAt}
                  creditLoading={platformCreditLoadingId === item.accountId}
                  onCheckin={onPlatformCardCheckin}
                  onRefresh={onPlatformCardRefresh}
                  onDelete={() => {}}
                  onQuickSwitchPlatform={{
                    platform: item.platform,
                    accounts: platformAccountsOf(item.platform),
                    // 本机客户端当前登录的账号（用于标注「当前」与排除重复切换）
                    currentId: platformCheckins.find(
                      (p) => p.platform === item.platform && p.localLogin,
                    )?.accountId,
                    done: () => {
                      // 切换会重启客户端：稍等再刷新，避免读到切换前的旧登录态
                      window.setTimeout(() => void loadPlatformCheckins(), 3000);
                    },
                  }}
                  featuresDisabled={false}
                />
              );
            })}
          </div>
        </section>
      ) : null}

      {/* 修改账号备注弹窗 */}
      <Dialog open={editingNoteAccount !== null} onOpenChange={(open) => !open && setEditingNoteAccount(null)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>编辑账号备注</DialogTitle>
            <DialogDescription>
              为账号 {editingNoteAccount?.nickname || editingNoteAccount?.email || editingNoteAccount?.id} 设置一个易记的别名。
            </DialogDescription>
          </DialogHeader>
          <div className="py-2">
            <Input
              value={noteInput}
              onChange={(e) => setNoteInput(e.target.value)}
              placeholder="例如：主力工作号、备用小号…"
              maxLength={20}
              autoFocus
              onKeyDown={(e) => {
                if (e.key === "Enter") saveNote();
              }}
            />
          </div>
          <DialogFooter className="flex gap-2 sm:justify-between">
            <Button variant="ghost" size="sm" onClick={clearNote} disabled={!accountNotes[editingNoteAccount?.id || ""]}>
              清除备注
            </Button>
            <div className="flex gap-2">
              <Button variant="outline" size="sm" onClick={() => setEditingNoteAccount(null)}>
                取消
              </Button>
              <Button size="sm" onClick={saveNote}>
                保存
              </Button>
            </div>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <OAuthLoginDialog open={oauthOpen} onOpenChange={setOauthOpen} />
      <ExportAccountsDialog
        open={exportOpen}
        onOpenChange={setExportOpen}
        accounts={accounts}
        onExported={onExported}
      />
      <ImportAccountsDialog
        open={importOpen}
        onOpenChange={setImportOpen}
        onImported={onImported}
      />
      <SwitchAccountDialog
        open={switchAccount !== null}
        onOpenChange={(o) => {
          if (!o) setSwitchAccount(null);
        }}
        account={switchAccount}
        onDone={() => {
          void fetchAll();
        }}
      />

      {/* 删除账号确认 */}
      <Dialog open={deleteTarget !== null} onOpenChange={(o) => !o && setDeleteTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>删除账号</DialogTitle>
            <DialogDescription>
              确定删除账号「{deleteTarget?.nickname || deleteTarget?.email || deleteTarget?.id}」？
              此操作不可撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteTarget(null)}>
              取消
            </Button>
            <Button variant="destructive" onClick={() => void confirmDelete()}>
              删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
