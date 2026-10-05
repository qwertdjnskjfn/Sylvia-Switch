import { useMemo, useState } from "react";
import { Check, Loader2, TriangleAlert, Zap } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import type { AccountMeta } from "@/lib/types";
import { cn } from "@/lib/utils";

/** 当前登录账号的判定依据（与账号页 isWorkbuddyCurrent 同口径）。 */
export interface CurrentAccountRef {
  uid?: string | null;
  email?: string | null;
  /** 平台账号：池条目 id（形如 `autoclaw:xxx`） */
  id?: string | null;
}

interface Props {
  /** 当前卡片对应的账号 */
  account: AccountMeta;
  /** 全部账号（含自身），组件内部过滤出可切换目标 */
  accounts: AccountMeta[];
  /**
   * 平台账号模式：`autoclaw` / `trae_work_cn`。
   * 传入后走「切换该平台客户端登录态」的通道（不重启 WorkBuddy）。
   */
  platform?: string;
  /** 当前已登录的账号来源（本地模式用 uid/email，平台模式用 id） */
  current?: CurrentAccountRef | null | undefined;
  /** 切换成功后回调 */
  onDone?: () => void;
  /** 打开完整切换弹窗（仅本地 WorkBuddy 账号有） */
  onAdvanced?: (account: AccountMeta) => void;
  disabled?: boolean;
  /** 紧凑样式（卡片 compact 模式） */
  compact?: boolean;
}

/** 判断账号是否就是当前登录账号。 */
function isCurrent(account: AccountMeta, current: Props["current"], platform?: string): boolean {
  if (!current) return false;
  if (platform) {
    return Boolean(current.id && account.id === current.id);
  }
  return Boolean(
    (current.uid && (account.uid === current.uid || account.id === current.uid)) ||
      (current.email && account.email === current.email),
  );
}

/** 直显完整名称（无隐私遮罩，契约 docs/unified-accounts-contract.md §3）。 */
function displayName(account: AccountMeta): string {
  return account.nickname || account.email || account.uid || "未命名账号";
}

/**
 * 一键切换账号。
 * - 本地 WorkBuddy 账号：切换后重启 WorkBuddy；
 * - 平台账号（AutoClaw / TRAE Work CN）：关闭该平台客户端 → 写入登录态 → 重启客户端。
 */
export function QuickSwitchMenu({
  account,
  accounts,
  platform,
  current,
  onDone,
  onAdvanced,
  disabled = false,
  compact = false,
}: Props) {
  const [switchingId, setSwitchingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState(false);

  /** 可切换目标：排除自身与当前已登录账号。 */
  const targets = useMemo(
    () => accounts.filter((a) => a.id !== account.id && !isCurrent(a, current, platform)),
    [accounts, account.id, current, platform],
  );
  const currentAccount = useMemo(
    () => accounts.find((a) => isCurrent(a, current, platform)),
    [accounts, current, platform],
  );

  async function quickSwitch(target: AccountMeta) {
    if (switchingId) return;
    setSwitchingId(target.id);
    setError(null);
    const nickname = displayName(target);
    try {
      if (platform) {
        const res = await api.switchPlatformAccount(
          platform as "autoclaw" | "trae_work_cn",
          target.id,
        );
        const parts: string[] = [];
        if (res.closedProcesses > 0) parts.push(`已重启客户端（关闭 ${res.closedProcesses} 个进程）`);
        else parts.push("客户端未在运行，登录态已就绪");
        if (res.backup) parts.push(`备份: ${res.backup}`);
        if (res.note) parts.push(res.note);
        toast.success(`已切换至「${nickname}」`, {
          description: parts.join("；"),
          duration: 6000,
        });
      } else {
        const res = await api.switchAccount({ accountId: target.id });
        const parts: string[] = [];
        if (res.backup) parts.push(`备份: ${res.backup}`);
        toast.success(`已切换至「${nickname}」`, {
          description: parts.length ? parts.join("；") : "WorkBuddy 已重启为目标账号。",
        });
      }
      setOpen(false);
      onDone?.();
    } catch (e) {
      const message = api.asError(e);
      setError(message);
      toast.error(`切换至「${nickname}」失败`, { description: message });
    } finally {
      setSwitchingId(null);
    }
  }

  const noTargets = targets.length === 0;
  const platformLabel =
    platform === "autoclaw" ? "AutoClaw" : platform === "trae_work_cn" ? "TRAE Work CN" : null;
  const switchHint = platformLabel
    ? `切换到该 ${platformLabel} 账号（会重启 ${platformLabel} 客户端）`
    : "选择一个账号直接切换（会重启 WorkBuddy）";
  /** 平台账号名带「平台 · 」前缀，下拉里去掉避免拥挤。 */
  const shortName = (account: AccountMeta) => {
    const text = displayName(account);
    return platformLabel ? text.replace(`${platformLabel} · `, "") : text;
  };

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <Tooltip>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <Button
              variant="outline"
              size="sm"
              className={cn(
                "rounded-full text-xs",
                compact ? "h-7 w-7 px-0" : "h-7 px-2.5 pr-3.5",
              )}
              disabled={disabled || noTargets}
              aria-label={noTargets ? "没有可切换的账号" : "一键切换账号"}
              aria-busy={switchingId !== null}
            >
              {switchingId ? (
                <Loader2 className={cn("animate-spin", compact ? "size-3.5" : "size-4")} />
              ) : (
                <Zap className={compact ? "size-3.5" : "size-4"} />
              )}
              {compact ? null : <span>{switchingId ? "切换中…" : "一键切换"}</span>}
            </Button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent side="top">
          {noTargets
            ? accounts.length <= 1
              ? "仅有一个账号，无需切换"
              : "没有可切换的账号（其余账号均未登录或与当前相同）"
            : switchHint}
        </TooltipContent>
      </Tooltip>

      <DropdownMenuContent align="end" className="w-64">
        <div className="flex items-center justify-between gap-2 px-2 py-1.5 text-xs font-normal text-muted-foreground">
          <span>一键切换{platformLabel ? `（${platformLabel}）` : "账号"}</span>
          {currentAccount ? (
            <span className="min-w-0 truncate" title={displayName(currentAccount)}>
              当前：{shortName(currentAccount)}
            </span>
          ) : (
            <span className="text-amber-600">未检测到当前账号</span>
          )}
        </div>
        <DropdownMenuSeparator />

        {noTargets ? (
          <div className="px-2.5 py-3 text-xs leading-5 text-muted-foreground">
            {accounts.length <= 1
              ? "当前只有一个账号，添加更多账号后即可一键切换。"
              : "没有其他可切换的账号，请先添加或导入账号。"}
          </div>
        ) : (
          targets.map((target) => {
            const busy = switchingId === target.id;
            const name = shortName(target);
            const expired = typeof target.expiresAt === "number" && target.expiresAt < Date.now();
            return (
              <DropdownMenuItem
                key={target.id}
                className="gap-2"
                disabled={switchingId !== null}
                onSelect={(event) => {
                  event.preventDefault();
                  void quickSwitch(target);
                }}
              >
                {busy ? (
                  <Loader2 className="size-3.5 shrink-0 animate-spin" />
                ) : target.needsRelogin || expired ? (
                  <TriangleAlert className="size-3.5 shrink-0 text-amber-600" />
                ) : (
                  <span className="size-3.5 shrink-0 rounded-full border border-muted-foreground/40" />
                )}
                <span className="min-w-0 flex-1 truncate" title={name}>
                  {name}
                </span>
                {busy ? (
                  <span className="shrink-0 text-[10px] text-muted-foreground">切换中</span>
                ) : target.needsRelogin || expired ? (
                  <Badge variant="warning" className="h-4 shrink-0 px-1 text-[10px]">
                    需重登
                  </Badge>
                ) : null}
              </DropdownMenuItem>
            );
          })
        )}

        {error ? (
          <div className="mx-2 mb-1 mt-1 rounded-md border border-destructive/30 bg-destructive/5 px-2 py-1.5 text-[11px] leading-4 break-all text-destructive">
            切换失败：{error}
          </div>
        ) : null}

        {onAdvanced ? (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              className="gap-2 text-xs"
              disabled={switchingId !== null}
              onSelect={() => onAdvanced(account)}
            >
              <Check className="size-3.5 shrink-0 opacity-0" />
              <span className="min-w-0 flex-1 truncate">高级选项（复制会话…）</span>
            </DropdownMenuItem>
          </>
        ) : null}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
