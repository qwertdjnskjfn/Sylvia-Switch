/**
 * 账号添加对话框（多账号网关 cloud 区块用）——**不经过文件上传**。
 *
 * # 三条直导路径（按「省事程度」排序）
 *
 * | Tab | 交互 | 后端 |
 * | --- | --- | --- |
 * | 本机导入 | 一键：读本机 WorkBuddy 桌面端已登录态 | `POST /api/import-local` |
 * | 浏览器登录 | 一键：拉起授权页 → 轮询 → 自动入库 | `/api/oauth/start` + `/api/oauth/status` |
 * | 手动粘贴 | 内联表单（**参照 MiniMax 通道**）：token 必填，其余可选 | `/workbuddy-pool/v1/accounts/add-direct` |
 *
 * 文件导入仍保留在 cloud 区块（`ImportAccountsDialog`），用于**迁移存量 JSON**；
 * 新增账号不再需要先准备文件。
 *
 * # 地域
 *
 * 前两条由**登录域自动派生**（`.ai→intl` / 其余→cn`）；
 * 手动粘贴那条由表单里选的域决定 —— 选错域 = 进错区，界面上有对应警示。
 */
import { useCallback, useEffect, useState } from "react";
import { CheckCircle2, Loader2, LogIn, MonitorSmartphone, ClipboardPaste } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import * as api from "@/lib/api";
import { cn } from "@/lib/utils";

/** 两个区域对应的登录域（与后端 `domain.ends_with(".ai")` 判据一致）。 */
const DOMAIN_CN = "www.workbuddy.cn";
const DOMAIN_INTL = "www.workbuddy.ai";

type Tab = "local" | "oauth" | "manual";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 添加成功后回调（参数为一句可展示的结果文案）。 */
  onAdded?: (message: string) => void;
}

export function WorkbuddyAccountAddDialog({ open, onOpenChange, onAdded }: Props) {
  const [tab, setTab] = useState<Tab>("local");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [hint, setHint] = useState("");

  // —— 手动粘贴表单（参照 MiniMax：必填仅 token）——
  const [fDomain, setFDomain] = useState(DOMAIN_CN);
  const [fToken, setFToken] = useState("");
  const [fRefresh, setFRefresh] = useState("");
  const [fNick, setFNick] = useState("");
  const [fUid, setFUid] = useState("");

  // —— 浏览器登录（OAuth 设备流）——
  const [login, setLogin] = useState<{ loginId: string; url: string; endpoint?: string } | null>(null);
  const [polling, setPolling] = useState(false);
  /**
   * 登录目标区域（`cn` / `intl`）——决定后端打哪个 API 基址。
   * 🔴 不传 intl 就只能登国内版：后端会打到国服端点，凭证也会被判成国内号。
   */
  const [fLoginRegion, setFLoginRegion] = useState<"cn" | "intl">("cn");

  const reset = useCallback(() => {
    setError("");
    setHint("");
    setBusy(false);
    setFToken("");
    setFRefresh("");
    setFNick("");
    setFUid("");
    setFLoginRegion("cn");
    setLogin(null);
    setPolling(false);
  }, []);

  useEffect(() => {
    if (open) reset();
  }, [open, reset]);

  /** OAuth 轮询：登录完成后自动入库并关闭。 */
  useEffect(() => {
    if (!polling || !login) return;
    let stopped = false;
    const timer = window.setInterval(async () => {
      try {
        const st = await api.oauthStatus(login.loginId);
        if (stopped) return;
        if (st.done) {
          window.clearInterval(timer);
          setPolling(false);
          if (st.error) {
            setError(st.error);
          } else {
            const name = st.result?.nickname || st.result?.email || "账号";
            const regionLabel = st.region === "intl" ? "国际版" : "国内版";
            onAdded?.(`已通过浏览器登录添加：${name}（${regionLabel}）`);
            onOpenChange(false);
          }
        }
      } catch (e) {
        window.clearInterval(timer);
        setPolling(false);
        setError(api.asError(e));
      }
    }, 2000);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, [polling, login, onAdded, onOpenChange]);

  async function importLocal() {
    setBusy(true);
    setError("");
    setHint("");
    try {
      const res = await api.importLocal();
      const name = res.account?.nickname || res.account?.email || "账号";
      // 本机导入只能读到桌面端**当前登录**的那一个区 —— 如实告知，别让用户以为导错了。
      const regionLabel = res.region === "intl" ? "国际版" : "国内版";
      onAdded?.(`已从本机导入：${name}（${regionLabel}）`);
      onOpenChange(false);
    } catch (e) {
      // 最常见原因：本机没开 WorkBuddy 桌面端 / 未登录
      setError(`${api.asError(e)}（需本机已安装并登录 WorkBuddy 桌面端）`);
    } finally {
      setBusy(false);
    }
  }

  async function startOauth() {
    setBusy(true);
    setError("");
    setHint("");
    try {
      const res = await api.oauthStart(fLoginRegion);
      setLogin({ loginId: res.loginId, url: res.verificationUri, endpoint: res.endpoint });
      window.open(res.verificationUri, "_blank", "noopener,noreferrer");
      setPolling(true);
      setHint(
        `已打开授权页（${fLoginRegion === "intl" ? "国际版" : "国内版"} · ${
          res.endpoint ?? "默认端点"
        }），完成登录后这里会自动收账（约 1～2 秒轮询一次）。`,
      );
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  async function addManual() {
    const token = fToken.trim();
    if (!token) {
      setError("access_token 不能为空。");
      return;
    }
    setBusy(true);
    setError("");
    try {
      await api.addWorkbuddyAccountDirect({
        access_token: token,
        refresh_token: fRefresh.trim() || undefined,
        domain: fDomain,
        nickname: fNick.trim() || undefined,
        uid: fUid.trim() || undefined,
      });
      const region = fDomain.endsWith(".ai") ? "国际区" : "国内区";
      onAdded?.(`已添加 ${fNick.trim() || "账号"}（${region}）`);
      onOpenChange(false);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  const TABS: { key: Tab; label: string; icon: typeof LogIn }[] = [
    { key: "local", label: "本机导入", icon: MonitorSmartphone },
    { key: "oauth", label: "浏览器登录", icon: LogIn },
    { key: "manual", label: "手动粘贴", icon: ClipboardPaste },
  ];

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="min-w-0">
        <DialogHeader>
          <DialogTitle>添加账号</DialogTitle>
          <DialogDescription>
            三条直导路径，均无需上传文件；地域按登录域自动归类。
          </DialogDescription>
        </DialogHeader>

        {/* Tab 切换 */}
        <div className="flex gap-1 rounded-lg bg-muted/50 p-1">
          {TABS.map((t) => (
            <button
              key={t.key}
              type="button"
              onClick={() => {
                setTab(t.key);
                setError("");
                setHint("");
              }}
              className={cn(
                "flex flex-1 items-center justify-center gap-1.5 rounded-md px-2 py-1.5 text-xs font-medium transition-colors",
                tab === t.key
                  ? "bg-background text-foreground shadow-sm"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              <t.icon className="size-3.5" />
              {t.label}
            </button>
          ))}
        </div>

        {/* —— 本机导入 —— */}
        {tab === "local" ? (
          <div className="space-y-3">
            <p className="text-xs leading-relaxed text-muted-foreground">
              读取本机 <strong>WorkBuddy 桌面端</strong> 的已登录凭证，一键入库。
              适合「桌面端已经登着、想把它加进网关」的场景。
            </p>
            <p className="rounded-lg bg-muted/40 px-3 py-2 text-[11px] leading-relaxed text-muted-foreground">
              ⚠️ 本机导入只能读到桌面端<strong>当前登录的那一个版本</strong>，这里没有版本可选 ——
              要换版本请先在 WorkBuddy 桌面端里切换登录，再回来点导入。
              需要添加另一个版本，请改用「浏览器登录」或「手动粘贴」。
            </p>
            <Button className="w-full" onClick={() => void importLocal()} disabled={busy}>
              {busy ? <Loader2 className="animate-spin" /> : <MonitorSmartphone />}
              从本机导入
            </Button>
          </div>
        ) : null}

        {/* —— 浏览器登录 —— */}
        {tab === "oauth" ? (
          <div className="space-y-3">
            <p className="text-xs leading-relaxed text-muted-foreground">
              拉起 WorkBuddy 授权页，登录完成后凭证自动入库。
              适合「要登一个新账号」。
            </p>

            {/* 区域选择：决定后端打哪个 API 基址。不选 intl 就登不了国际版。 */}
            <div className="space-y-1.5">
              <Label>登录版本</Label>
              <div className="flex gap-2">
                {(
                  [
                    { key: "cn", label: "国内版", hint: "codebuddy.cn" },
                    { key: "intl", label: "国际版", hint: "workbuddy.ai" },
                  ] as const
                ).map((r) => (
                  <button
                    key={r.key}
                    type="button"
                    disabled={login !== null}
                    onClick={() => setFLoginRegion(r.key)}
                    className={cn(
                      "flex-1 rounded-lg border px-3 py-2 text-left transition-colors disabled:opacity-50",
                      fLoginRegion === r.key
                        ? "border-primary bg-primary/5"
                        : "border-border hover:bg-muted/40",
                    )}
                  >
                    <div className="text-xs font-medium">{r.label}</div>
                    <div className="font-mono text-[10px] text-muted-foreground">{r.hint}</div>
                  </button>
                ))}
              </div>
              <p className="text-[11px] leading-relaxed text-amber-600">
                ⚠️ 选错版本 = 进了错的区：海外账号只连海外服务、国内账号只连国内服务，
                事后不能互通。登完可在下方账号表确认它落在哪个区。
              </p>
            </div>
            {login ? (
              <div className="space-y-2">
                <div className="flex items-center gap-2 rounded-lg border border-emerald-500/30 bg-emerald-500/10 px-3 py-2 text-xs text-emerald-700">
                  {polling ? (
                    <>
                      <Loader2 className="size-3.5 animate-spin" />
                      等待授权完成…
                    </>
                  ) : (
                    <>
                      <CheckCircle2 className="size-3.5" />
                      授权完成
                    </>
                  )}
                </div>
                <a
                  href={login.url}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="block break-all text-xs text-primary underline"
                >
                  {login.url}
                </a>
              </div>
            ) : (
              <Button className="w-full" onClick={() => void startOauth()} disabled={busy}>
                {busy ? <Loader2 className="animate-spin" /> : <LogIn />}
                开始登录
              </Button>
            )}
          </div>
        ) : null}

        {/* —— 手动粘贴（参照 MiniMax 内联表单）—— */}
        {tab === "manual" ? (
          <div className="space-y-3">
            <p className="text-xs leading-relaxed text-muted-foreground">
              手上有 token 时最快：粘进去即可。可在浏览器 devtools 的
              <code className="mx-1 font-mono">localStorage</code>里取。
            </p>

            <div className="space-y-1.5">
              <Label htmlFor="wb-add-domain">登录域（决定区域）</Label>
              <select
                id="wb-add-domain"
                className="h-9 w-full rounded-md border bg-transparent px-2 text-sm"
                value={fDomain}
                onChange={(e) => setFDomain(e.target.value)}
              >
                <option value={DOMAIN_CN}>国内版 · {DOMAIN_CN}</option>
                <option value={DOMAIN_INTL}>国际版 · {DOMAIN_INTL}</option>
              </select>
              <p className="text-[11px] leading-relaxed text-amber-600">
                ⚠️ 选错域 = 进错区：海外账号只连海外服务，国内账号只连国内服务，之后不能互通。
              </p>
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="wb-add-token">access_token（必填）</Label>
              <textarea
                id="wb-add-token"
                value={fToken}
                onChange={(e) => setFToken(e.target.value)}
                placeholder="粘贴 access_token…"
                rows={3}
                className="w-full min-w-0 rounded-md border bg-transparent px-3 py-2 font-mono text-xs"
              />
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="wb-add-refresh">refresh_token（选填）</Label>
              <Input
                id="wb-add-refresh"
                value={fRefresh}
                onChange={(e) => setFRefresh(e.target.value)}
                placeholder="有就填，便于自动续期"
                className="font-mono text-xs"
              />
            </div>

            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <div className="space-y-1.5">
                <Label htmlFor="wb-add-nick">昵称（选填）</Label>
                <Input
                  id="wb-add-nick"
                  value={fNick}
                  onChange={(e) => setFNick(e.target.value)}
                  placeholder="如：主号"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="wb-add-uid">uid（选填）</Label>
                <Input
                  id="wb-add-uid"
                  value={fUid}
                  onChange={(e) => setFUid(e.target.value)}
                  placeholder="知道就填"
                  className="font-mono text-xs"
                />
              </div>
            </div>

            <Button className="w-full" onClick={() => void addManual()} disabled={busy || !fToken.trim()}>
              {busy ? <Loader2 className="animate-spin" /> : null}
              添加
            </Button>
          </div>
        ) : null}

        {hint ? (
          <p className="text-[11px] leading-relaxed text-muted-foreground">{hint}</p>
        ) : null}

        {error ? (
          <Alert variant="destructive">
            <AlertDescription className="break-all text-xs">{error}</AlertDescription>
          </Alert>
        ) : null}
      </DialogContent>
    </Dialog>
  );
}
