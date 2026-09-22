import { useState } from "react";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import * as api from "@/lib/api";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onImported?: () => void;
}

/** 「导入 Qoder CN 账号」对话框。
 *
 * 三种途径（与 AutoClaw/TRAE 的区域隔离架构同构）：
 * 1. 本机 QoderCN（中国版 IDE）登录态一键导入——自动打 🇨🇳 地域标并入 Qoder CN 专用池；
 * 2. 手动粘贴 dt-/drt- 凭证（抓包或 qoder CLI 导出获取）——gateway=qwen-flash 归入 CN 专用池；
 * 3. 设备码授权：跳转「Qoder 网关」页操作（device-flow 用的就是 Qoder CN 客户端 client_id）。
 */
export function QoderCnImportDialog({ open, onOpenChange, onImported }: Props) {
  const [busy, setBusy] = useState<string | null>(null);
  const [accessToken, setAccessToken] = useState("");
  const [refreshToken, setRefreshToken] = useState("");

  async function importLocalCn() {
    setBusy("local");
    try {
      const res = await api.qoderImportLocalCn();
      toast.success("Qoder CN 账号已导入", {
        description: `${res.account.name || res.account.email || "本机 QoderCN 登录态"} · 已归入 🇨🇳 国内专用池（/qwen-flash/v1）`,
      });
      onImported?.();
      onOpenChange(false);
    } catch (e) {
      toast.error("导入失败", { description: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function importManual() {
    if (!accessToken.trim().startsWith("dt-")) {
      toast.error("凭证格式不对", {
        description: "access token 应为 dt- 前缀（抓包或 qoder CLI 导出获取）",
      });
      return;
    }
    setBusy("manual");
    try {
      const res = await api.qoderImportManual({
        accessToken: accessToken.trim(),
        refreshToken: refreshToken.trim() || undefined,
        gateway: "qwen-flash",
      });
      toast.success("Qoder CN 账号已导入", {
        description: `${res.account.name || res.account.email || "手动凭证"} · 已归入 🇨🇳 国内专用池（/qwen-flash/v1）`,
      });
      setAccessToken("");
      setRefreshToken("");
      onImported?.();
      onOpenChange(false);
    } catch (e) {
      toast.error("导入失败", { description: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>导入 Qoder CN 账号</DialogTitle>
          <DialogDescription>
            中国版账号归入 🇨🇳 国内专用池，只服务 Qwen3.8-Flash 专属路由
            <code className="mx-1 rounded bg-muted px-1 py-0.5">/qwen-flash/v1</code>
            ，与国际版账号完全隔离。
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="rounded-lg border border-border/60 p-3">
            <p className="text-xs font-medium">方式一 · 本机 QoderCN 客户端</p>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              已安装并登录 QoderCN（中国版 IDE）时，一键解密本机登录态导入。
            </p>
            <Button size="sm" className="mt-2" onClick={() => void importLocalCn()} disabled={busy === "local"}>
              {busy === "local" ? <Loader2 className="animate-spin" /> : null}
              导入本机 QoderCN 登录态
            </Button>
          </div>
          <div className="rounded-lg border border-border/60 p-3">
            <p className="text-xs font-medium">方式二 · 手动粘贴凭证（抓包 / CLI 导出）</p>
            <div className="mt-2 space-y-2">
              <Input
                className="h-8 text-xs"
                value={accessToken}
                onChange={(e) => setAccessToken(e.target.value)}
                placeholder="dt- 开头的 access token（必填）"
                aria-label="access token"
              />
              <Input
                className="h-8 text-xs"
                value={refreshToken}
                onChange={(e) => setRefreshToken(e.target.value)}
                placeholder="drt- 开头的 refresh token（选填，用于 401 自动续期）"
                aria-label="refresh token"
              />
              <Button size="sm" variant="outline" onClick={() => void importManual()} disabled={busy === "manual"}>
                {busy === "manual" ? <Loader2 className="animate-spin" /> : null}
                导入凭证
              </Button>
            </div>
          </div>
          <div className="rounded-lg border border-border/60 p-3">
            <p className="text-xs font-medium">方式三 · 设备码授权（OAuth）</p>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              前往「Qoder 网关」页点「设备授权添加账号」——该授权流程用的就是 Qoder CN
              客户端，完成后账号自动标记为国内账号。
            </p>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
