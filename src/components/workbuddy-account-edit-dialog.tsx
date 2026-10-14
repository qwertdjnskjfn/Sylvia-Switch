/**
 * 账号编辑弹窗（多账号网关 cloud 区块用）。
 *
 * # 只提交「改过的字段」
 *
 * 服务端是 **PATCH 语义**（只改传入的字段），且**全有或全无**（任一字段非法 ⇒ 整体拒绝）。
 * 所以这里逐字段比对原值，只为真正变化的字段发送 —— 避免把一个没动的
 * `access_token` 原样回写（那会白白把「过期时间不倒退」的既有保护绕过去）。
 *
 * # 可编辑范围
 *
 * 白名单在服务端强制（`account::EDITABLE_FIELDS`），本组件只暴露其中对人友好的几个：
 * `nickname` / `domain` / `access_token` / `refresh_token` / `expiresAt`。
 *
 * ⚠️ `domain` **决定区域归属**（`.ai→intl` / 其余→cn）—— 改它等于让该账号换区，
 * 界面上必须显式提示（下方红字）。
 *
 * 不可编辑：`id`（删除/导出主键）、`uid`、`auth_raw` / `profile_raw`（上游原始态）、
 * `createdAt`（审计）。改动它们会让删除/导出/池选号对不上号。
 */
import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import * as api from "@/lib/api";
import type { WorkbuddyPoolAccount } from "@/lib/types";

/** 两个区域对应的登录域（与后端 `domain.ends_with(".ai")` 判据一致）。 */
const DOMAIN_CN = "www.workbuddy.cn";
const DOMAIN_INTL = "www.workbuddy.ai";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 要编辑的账号（来自 `status.accounts[]`，uid 是脱敏值）。 */
  account: WorkbuddyPoolAccount | null;
  /** 保存成功后回调（参数为一句可展示的结果文案）。 */
  onSaved?: (message: string) => void;
}

export function WorkbuddyAccountEditDialog({ open, onOpenChange, account, onSaved }: Props) {
  const [nickname, setNickname] = useState("");
  const [domain, setDomain] = useState(DOMAIN_CN);
  const [accessToken, setAccessToken] = useState("");
  const [refreshToken, setRefreshToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  // 每次打开都用当前账号重置表单（避免残留上一次的输入）
  useEffect(() => {
    if (!open || !account) return;
    setNickname(account.nickname ?? "");
    // 账号记录里没有 domain（status 只给派生后的 region），故由 region 反推一个初值；
    // 用户不改就等同于「不变」——下方 patch 只提交真正变化的字段。
    setDomain(account.region === "intl" ? DOMAIN_INTL : DOMAIN_CN);
    setAccessToken("");
    setRefreshToken("");
    setError("");
    setBusy(false);
  }, [open, account]);

  if (!account) return null;

  const regionChanged =
    (domain === DOMAIN_INTL ? "intl" : "cn") !== (account.region === "intl" ? "intl" : "cn");

  async function save() {
    if (!account) return;
    const patch: api.WorkbuddyAccountPatch = {};
    if (nickname !== (account.nickname ?? "")) patch.nickname = nickname;
    // domain 只在被改动时提交（region 反推的初值可能与真实 domain 字面不同，
    // 但只要 region 判定一致就没变——不能因为「字面不同」就误提交）
    if (regionChanged) patch.domain = domain;
    if (accessToken.trim()) patch.access_token = accessToken.trim();
    if (refreshToken.trim()) patch.refresh_token = refreshToken.trim();

    if (Object.keys(patch).length === 0) {
      setError("没有修改任何字段。");
      return;
    }

    setBusy(true);
    setError("");
    try {
      await api.updateWorkbuddyPoolAccount(account.uid, patch);
      const changed = Object.keys(patch).join(" / ");
      onSaved?.(`已更新 ${account.nickname || account.uid}（${changed}）`);
      onOpenChange(false);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="min-w-0">
        <DialogHeader>
          <DialogTitle>编辑账号</DialogTitle>
          <DialogDescription>
            {account.nickname || account.uid} · 只提交改动过的字段
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-3.5">
          <div className="space-y-1.5">
            <Label htmlFor="wb-acc-nickname">昵称</Label>
            <Input
              id="wb-acc-nickname"
              value={nickname}
              onChange={(e) => setNickname(e.target.value)}
              placeholder="留空则展示 uid"
            />
          </div>

          <div className="space-y-1.5">
            <Label htmlFor="wb-acc-domain">登录域（决定区域）</Label>
            <select
              id="wb-acc-domain"
              className="h-9 w-full rounded-md border bg-transparent px-2 text-sm"
              value={domain}
              onChange={(e) => setDomain(e.target.value)}
            >
              <option value={DOMAIN_CN}>国内版 · {DOMAIN_CN}</option>
              <option value={DOMAIN_INTL}>国际版 · {DOMAIN_INTL}</option>
            </select>
            {regionChanged ? (
              <p className="text-[11px] leading-relaxed text-amber-600">
                ⚠️ 这会改变该账号的**区域归属**（
                {account.region === "intl" ? "国际 → 国内" : "国内 → 国际"}）。
                凭证若不属于该区，之后请求会失败——请确认你确实换了登录域。
              </p>
            ) : (
              <p className="text-[11px] text-muted-foreground">
                当前区域：{account.region === "intl" ? "国际版" : "国内版"}
              </p>
            )}
          </div>

          <div className="space-y-1.5">
            <Label htmlFor="wb-acc-at">access_token（留空 = 不改）</Label>
            <Input
              id="wb-acc-at"
              type="password"
              value={accessToken}
              onChange={(e) => setAccessToken(e.target.value)}
              placeholder="仅在需要手工替换凭证时填写"
            />
          </div>

          <div className="space-y-1.5">
            <Label htmlFor="wb-acc-rt">refresh_token（留空 = 不改）</Label>
            <Input
              id="wb-acc-rt"
              type="password"
              value={refreshToken}
              onChange={(e) => setRefreshToken(e.target.value)}
              placeholder="仅在需要手工替换凭证时填写"
            />
          </div>

          <p className="rounded-lg bg-muted/40 px-3 py-2 text-[11px] leading-relaxed text-muted-foreground">
            <strong className="font-medium text-foreground/80">不可编辑：</strong>
            id（删除/导出主键）、uid、auth_raw / profile_raw（上游原始态）、createdAt（审计）。
            改动它们会让删除、导出与池选号对不上号。
          </p>

          {error ? (
            <Alert variant="destructive">
              <AlertDescription className="break-all text-xs">{error}</AlertDescription>
            </Alert>
          ) : null}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            取消
          </Button>
          <Button onClick={() => void save()} disabled={busy}>
            {busy ? <Loader2 className="animate-spin" /> : null}
            保存
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
