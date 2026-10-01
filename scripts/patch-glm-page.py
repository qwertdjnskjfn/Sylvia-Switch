# -*- coding: utf-8 -*-
"""一次性补丁：GlmProxyPage 模型选型参考 → 国内版/国际版两组展示 + 按类选用。"""
import io, sys

p = 'src/pages/GlmProxyPage.tsx'
t = io.open(p, encoding='utf-8').read()

# 1) regionFilter 状态
a0 = '  const [loadModel, setLoadModel] = useState("glm-5.3");'
b0 = a0 + '\n  /** 1.3.0：模型目录区域筛选（参照 WorkBuddy 池页的分组方式） */\n  const [regionFilter, setRegionFilter] = useState<"all" | "domestic" | "international">("all");'
assert a0 in t, 'a0 anchor'
t = t.replace(a0, b0, 1)

# 2) 整卡重写：定位 模型选型参考 Section 的起止
start = t.find('            <Section\n              title="模型选型参考"')
end_marker = ('                  </p>\n'
              '                </div>\n'
              '              </CardContent>\n'
              '            </Section>\n'
              '\n'
              '            <Section title="接入信息"')
end = t.find(end_marker, start)
assert start >= 0 and end > start, (start, end)
end += len(end_marker)

new_block = '''            <Section
              title="模型选型参考"
              description="按国内版/国际版分组展示 AutoClaw 模型目录；点「选用」把模型设为下方试一试的目标"
            >
              <CardContent className="p-0">
                <div className="flex flex-wrap items-center gap-1.5 px-4 pt-3 sm:px-5">
                  {(["all", "domestic", "international"] as const).map((key) => (
                    <button
                      key={key}
                      onClick={() => setRegionFilter(key)}
                      className={
                        "h-7 rounded-full border px-3 text-[11px] transition-colors " +
                        (regionFilter === key
                          ? "border-primary bg-primary text-primary-foreground"
                          : "border-border/60 text-muted-foreground hover:bg-muted/60")
                      }
                    >
                      {key === "all" ? "全部" : `${REGION_ICONS[key]} ${REGION_LABELS[key]}`}
                    </button>
                  ))}
                  <span className="ml-auto text-[11px] text-muted-foreground">
                    消耗等级与模态来自 AutoClaw 模型目录；速度取自近 7 天真实调用
                  </span>
                </div>
                {(() => {
                  const domestic = cfg.models.filter((m) => m.region === "domestic");
                  const international = cfg.models.filter((m) => m.region === "international");
                  const groups: { key: string; icon: string; label: string; models: typeof cfg.models }[] = [
                    { key: "domestic", icon: REGION_ICONS.domestic, label: REGION_LABELS.domestic, models: domestic },
                    { key: "international", icon: REGION_ICONS.international, label: REGION_LABELS.international, models: international },
                  ];
                  const visible = regionFilter === "all" ? groups : groups.filter((g) => g.key === regionFilter);
                  return visible.map((g) => (
                    <div key={g.key} className="min-w-0">
                      <div className="flex items-center gap-2 border-b border-border/40 px-4 pb-1.5 pt-3 sm:px-5">
                        <span className="text-[12px] font-medium">
                          {g.icon} {g.label}
                        </span>
                        <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">
                          {g.models.length} 个模型
                        </Badge>
                        {g.models.length === 0 ? (
                          <span className="text-[11px] text-muted-foreground">
                            该区域当前没有可用模型（检查对应区域账号池）
                          </span>
                        ) : null}
                      </div>
                      {g.models.length > 0 ? (
                        <div className="min-w-0 overflow-x-auto">
                          <table className="w-full min-w-[760px] text-left text-xs">
                            <thead>
                              <tr className="border-b border-border/60 text-muted-foreground">
                                <th className="px-4 py-2.5 font-medium sm:px-5">模型</th>
                                <th className="px-3 py-2.5 font-medium">消耗</th>
                                <th className="px-3 py-2.5 font-medium">输入模态</th>
                                <th className="px-3 py-2.5 text-right font-medium">上下文</th>
                                <th className="px-3 py-2.5 text-right font-medium">实测首字</th>
                                <th className="px-3 py-2.5 text-right font-medium">实测总耗时</th>
                                <th className="px-3 py-2.5 text-right font-medium">调用次数</th>
                                <th className="px-3 py-2.5 font-medium">建议</th>
                                <th className="px-4 py-2.5 text-right font-medium sm:px-5">操作</th>
                              </tr>
                            </thead>
                            <tbody>
                              {g.models.map((m) => {
                                const stat = speedStats[m.id];
                                const inputs = (m.inputs ?? ["text"]).join(" + ");
                                const selected = loadModel === m.id;
                                return (
                                  <tr
                                    key={m.id}
                                    className={
                                      "border-b border-border/40 last:border-0 " +
                                      (selected ? "bg-primary/5" : "")
                                    }
                                  >
                                    <td className="px-4 py-2.5 sm:px-5">
                                      <span className="flex flex-wrap items-baseline gap-1.5">
                                        <span className="font-medium">{m.alias}</span>
                                        <code className="text-[11px] text-muted-foreground">{m.id}</code>
                                        {m.region ? (
                                          <Badge variant="outline" className="h-5 px-1.5 text-[10px]" title={`该模型只路由到${REGION_LABELS[m.region]}账号池`}>
                                            {REGION_ICONS[m.region]} {REGION_LABELS[m.region]}
                                          </Badge>
                                        ) : null}
                                        {cfg.speedFirst && cfg.speedFirstSources.includes(m.id) ? (
                                          <Badge variant="warning" className="h-5 px-1.5 text-[10px]">
                                            已改投 {cfg.speedFirstTarget}
                                          </Badge>
                                        ) : null}
                                      </span>
                                      {m.autoModel ? (
                                        <p className="mt-0.5 text-[11px] text-muted-foreground">
                                          目录标注后端：{m.autoModel}
                                        </p>
                                      ) : null}
                                    </td>
                                    <td className="px-3 py-2.5">
                                      <Badge
                                        variant={m.consumption === "高" ? "destructive" : "secondary"}
                                        className="h-5 px-1.5 text-[10px]"
                                      >
                                        {m.consumption ?? "—"}
                                      </Badge>
                                    </td>
                                    <td className="px-3 py-2.5 text-muted-foreground">{inputs}</td>
                                    <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                                      {m.contextWindow ? formatTokens(m.contextWindow) : "—"}
                                    </td>
                                    <td className="px-3 py-2.5 text-right tabular-nums">
                                      {stat && stat.avgTtfbMs > 0 ? formatLatency(stat.avgTtfbMs) : "—"}
                                    </td>
                                    <td className="px-3 py-2.5 text-right tabular-nums">
                                      {stat ? formatLatency(stat.avgLatencyMs) : "—"}
                                    </td>
                                    <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                                      {stat ? formatNumber(stat.calls) : "—"}
                                    </td>
                                    <td className="px-3 py-2.5 text-muted-foreground">{MODEL_ADVICE[m.id] ?? "—"}</td>
                                    <td className="px-4 py-2.5 text-right sm:px-5">
                                      <Button
                                        variant={selected ? "default" : "outline"}
                                        size="sm"
                                        className="h-6 px-2 text-[10px]"
                                        onClick={() => {
                                          setLoadModel(m.id);
                                          setMsg({
                                            type: "ok",
                                            text: `已选用 ${m.alias || m.id}（${m.region ? REGION_LABELS[m.region] : "未分区"}）为试一试模型`,
                                          });
                                        }}
                                      >
                                        {selected ? "已选用" : "选用"}
                                      </Button>
                                    </td>
                                  </tr>
                                );
                              })}
                            </tbody>
                          </table>
                        </div>
                      ) : null}
                    </div>
                  ));
                })()}
                <div className="mx-4 border-t border-border/50 py-3 sm:mx-5">
                  <p className="text-xs leading-5 text-muted-foreground">
                    提速要点：① 日常轮次用 <span className="text-foreground">Auto / Auto-Fast</span>；
                    ② 客户端上下文里工具 schema 常占数万 token（每轮重算），禁用用不到的插件收益最大；
                    ③ 上游不支持关闭思考与提示缓存，参数层面无法再优化。
                  </p>
                </div>
              </CardContent>
            </Section>'''

t = t[:start] + new_block + t[end:]
io.open(p, 'w', encoding='utf-8').write(t)
print('GlmProxyPage grouped rewrite done')
