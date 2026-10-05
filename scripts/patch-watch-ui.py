# -*- coding: utf-8 -*-
"""一次性补丁：GlmProxyPage 增加「封禁与风险监控」区块。"""
import io

p = 'src/pages/GlmProxyPage.tsx'
t = io.open(p, encoding='utf-8').read()

# 1) 状态与轮询：挂在 loadModel 状态旁
a0 = '  const [loadModel, setLoadModel] = useState("glm-5.3");'
b0 = a0 + '''
  // 1.5.0：AutoClaw 账号封禁监控（后台每 30 分钟自动探测，此处轮询展示）
  const [watch, setWatch] = useState<{ updatedAt: number; accounts: { id: string; label: string; region: string; state: string; code: unknown; detail: string; firstSeenAt: number }[] } | null>(null);
  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const res = await fetch("/api/autoclaw/watch");
        const data = (await res.json()) as { updatedAt: number; accounts: { id: string; label: string; region: string; state: string; detail: string; firstSeenAt: number }[] };
        if (alive) setWatch(data);
      } catch { /* 静默：下一轮再取 */ }
    };
    void load();
    const timer = setInterval(load, 60_000);
    return () => { alive = false; clearInterval(timer); };
  }, []);'''
assert a0 in t, 'a0'
t = t.replace(a0, b0, 1)

# 2) 区块：插在 接入信息 Section 之前
a1 = '            <Section\n              title="接入信息"'
idx = t.find(a1)
assert idx > 0, 'a1 anchor'
watch_block = '''            <Section
              title="封禁与风险监控"
              description="后台每 30 分钟主动探测各账号 chat 层真实状态（410004 封禁 / 810001 额度 / 401 登录态）；状态变化即时桌面通知"
            >
              <CardContent className="p-0">
                {(() => {
                  const accounts = watch?.accounts ?? [];
                  const banned = accounts.filter((a) => a.state === "banned");
                  const quota = accounts.filter((a) => a.state === "quota");
                  const invalid = accounts.filter((a) => a.state === "invalid");
                  const ok = accounts.filter((a) => a.state === "ok");
                  const danger = banned.length + invalid.length;
                  const stateLabel: Record<string, string> = {
                    ok: "正常", banned: "🚫 封禁", quota: "⚠ 额度用尽", invalid: "⚠ 登录态失效", error: "探测异常",
                  };
                  const stateBadge = (s: string) =>
                    s === "banned" ? "destructive" : s === "ok" ? "secondary" : "warning";
                  return (
                    <div className="space-y-1 px-4 py-3 sm:px-5">
                      {accounts.length === 0 ? (
                        <p className="text-xs text-muted-foreground">
                          尚未探测（后台每 30 分钟自动运行；点「立即探测」马上检查一遍）
                        </p>
                      ) : danger > 0 ? (
                        <div className="rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2.5 text-sm text-destructive">
                          ⚠ {danger} 个账号处于危险状态：
                          {banned.map((a) => a.label).join("、")}
                          {banned.length && invalid.length ? "；" : ""}
                          {invalid.map((a) => `${a.label}（登录态失效）`).join("、")}
                          ——相关请求将持续失败，请勿再对其发起调用
                        </div>
                      ) : (
                        <div className="rounded-lg border border-emerald-500/30 bg-emerald-500/10 px-3 py-2.5 text-sm text-emerald-600">
                          ✓ 全部 {ok.length} 个账号 chat 层探测正常
                        </div>
                      )}
                      {accounts.map((a) => (
                        <div key={a.id} className="flex flex-wrap items-center gap-2 border-b border-border/40 py-2 last:border-0">
                          <Badge variant="outline" className="h-5 px-1.5 text-[10px]">
                            {REGION_ICONS[a.region as keyof typeof REGION_ICONS] ?? ""} {a.region === "domestic" ? "国内" : "国际"}
                          </Badge>
                          <span className="text-xs font-medium">{a.label}</span>
                          <Badge variant={stateBadge(a.state) as "destructive" | "secondary" | "warning"} className="h-5 px-1.5 text-[10px]">
                            {stateLabel[a.state] ?? a.state}
                          </Badge>
                          <span className="min-w-0 flex-1 truncate text-[11px] text-muted-foreground" title={a.detail}>
                            {a.detail || "—"}
                          </span>
                          <span className="text-[10px] text-muted-foreground">
                            首次进入：{a.firstSeenAt ? new Date(a.firstSeenAt).toLocaleString() : "—"}
                          </span>
                        </div>
                      ))}
                      <div className="flex items-center gap-2 pt-1">
                        <Button
                          variant="outline"
                          size="sm"
                          onClick={() => {
                            void (async () => {
                              try {
                                const res = await fetch("/api/autoclaw/watch/probe", { method: "POST" });
                                setWatch(await res.json());
                              } catch { /* 忽略 */ }
                            })();
                          }}
                        >
                          立即探测
                        </Button>
                        {watch?.updatedAt ? (
                          <span className="text-[10px] text-muted-foreground">
                            上次探测：{new Date(watch.updatedAt).toLocaleString()}
                          </span>
                        ) : null}
                      </div>
                    </div>
                  );
                })()}
              </CardContent>
            </Section>

''' + a1
t = t[:idx] + watch_block + t[idx + len(a1):]
io.open(p, 'w', encoding='utf-8').write(t)
print('watch section added')
