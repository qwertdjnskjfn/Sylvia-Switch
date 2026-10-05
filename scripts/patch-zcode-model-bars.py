# -*- coding: utf-8 -*-
"""一次性补丁：ZCodePage 账号行下加分模型用量明细（进度条，仿 ZCode 官方 UI）。"""
import io

p = 'src/pages/ZcodePage.tsx'
t = io.open(p, encoding='utf-8').read()

# 1) 模块级 helper：数值解析 / 百分比 / 日期（放 fmtQuota 后）
old = '''/** 时间展示：毫秒/秒级 epoch 兜底秒 ×1000，解析失败原样「—」。 */'''
new = '''/** 字符串/数值 → f64（容错：去千分位逗号）。 */
function numOf(v: string | number | null | undefined): number | null {
  if (v === null || v === undefined || v === "") return null;
  const n = typeof v === "number" ? v : Number(String(v).replace(/,/g, ""));
  return Number.isFinite(n) ? n : null;
}

/** 剩余百分比（remaining/total），total 缺失/为 0 → 100%（视为满额）。 */
function remainPct(item: { total?: unknown; remaining?: unknown; percentUsed?: unknown }): number {
  const pu = numOf(item.percentUsed as string | number | null);
  const rem = numOf(item.remaining as string | number | null);
  const tot = numOf(item.total as string | number | null);
  if (pu !== null) return Math.max(0, Math.min(100, pu));
  if (rem !== null && tot !== null && tot > 0) return Math.max(0, Math.min(100, (rem / tot) * 100));
  if (rem !== null && tot !== null && tot === 0) return 100;
  return 100;
}

/** 到期/周期时间展示：毫秒/秒级 epoch → 「10月7日 23:59」；字符串原样。 */
function fmtPeriod(v: unknown): string {
  if (v === null || v === undefined || v === "") return "";
  const n = typeof v === "number" ? v : Number(v);
  if (Number.isFinite(n) && n > 0) {
    const ms = n < 1e12 ? n * 1000 : n;
    const d = new Date(ms);
    if (!Number.isNaN(d.getTime())) {
      return `${d.getMonth() + 1}月${d.getDate()}日 ${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
    }
  }
  return String(v);
}

/** 时间展示：毫秒/秒级 epoch 兜底秒 ×1000，解析失败原样「—」。 */'''
assert old in t, 'helper anchor'
t = t.replace(old, new, 1)

# 2) 账号行外壳：flex 行 → 包一层容器 + 行内子块
old_row_open = '''                return (
                  <div
                    key={acc.id}
                    className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-2 px-4 py-2.5 sm:px-5"
                  >'''
new_row_open = '''                const channels = (rowQuota?.channels ?? []).filter(
                  (c) => Array.isArray(c.items) && (c.items as unknown[]).length > 0,
                );
                return (
                  <div key={acc.id} className="min-w-0">
                  <div
                    className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-2 px-4 py-2.5 sm:px-5"
                  >'''
assert old_row_open in t, 'row open anchor'
t = t.replace(old_row_open, new_row_open, 1)

# 3) 行尾：明细块 + 关闭外壳
old_row_close = '''                        {removing ? <Loader2 className="animate-spin" /> : <Trash2 />}
                        删除
                      </Button>
                    </div>
                  </div>
                );
              })
            )}'''
new_row_close = '''                        {removing ? <Loader2 className="animate-spin" /> : <Trash2 />}
                        删除
                      </Button>
                    </div>
                  </div>
                  {/* 1.5.3 分模型用量明细（仿 ZCode 官方 UI：模型名 + 百分比 + 进度条 + 剩余/总量 + 到期） */}
                  {channels.map((ch) => (
                    <div key={String(ch.name)} className="border-t border-border/40 px-4 py-2.5 sm:px-5">
                      <div className="flex items-center gap-2">
                        <span className="text-[11px] font-medium text-foreground">{String(ch.name)}</span>
                        {ch.error ? <span className="text-[10px] text-destructive">{String(ch.error)}</span> : null}
                      </div>
                      {(ch.items as Array<Record<string, unknown>>).map((it, idx) => {
                        const name = String(it.name ?? "套餐");
                        const pct = remainPct(it);
                        const rem = numOf(it.remaining as string | number | null);
                        const tot = numOf(it.total as string | number | null);
                        const period = fmtPeriod(it.periodEnd ?? it.expireAtMs);
                        return (
                          <div key={`${name}-${idx}`} className="mt-2 rounded-lg border border-border/40 p-2.5">
                            <div className="text-[12px] font-medium">{name}</div>
                            <div className="mt-0.5 flex items-end justify-between">
                              <span className="text-base font-semibold leading-none">
                                {pct.toFixed(0)}%
                              </span>
                              {period ? (
                                <span className="text-[10px] text-muted-foreground">{period}</span>
                              ) : null}
                            </div>
                            <div className="mt-1.5 h-1.5 w-full overflow-hidden rounded-full bg-muted">
                              <div
                                className={cn(
                                  "h-full rounded-full",
                                  pct > 30 ? "bg-emerald-500" : pct > 10 ? "bg-amber-500" : "bg-destructive",
                                )}
                                style={{ width: `${pct}%` }}
                              />
                            </div>
                            <div className="mt-1 text-[10px] tabular-nums text-muted-foreground">
                              {rem !== null ? fmtQuota(rem) : "—"} / {tot !== null ? fmtQuota(tot) : "—"}
                            </div>
                          </div>
                        );
                      })}
                    </div>
                  ))}
                  </div>
                );
              })
            )}'''
assert old_row_close in t, 'row close anchor'
t = t.replace(old_row_close, new_row_close, 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('per-model breakdown added')
