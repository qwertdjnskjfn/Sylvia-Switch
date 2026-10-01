# -*- coding: utf-8 -*-
"""一次性补丁：GLM 代理 接入信息 → 国内版/国际版两组接入（模型清单 + region 字段说明 + curl 示例）。"""
import io

p = 'src/pages/GlmProxyPage.tsx'
t = io.open(p, encoding='utf-8').read()

start = t.find('            <Section title="接入信息"')
end_marker = ('              </CardContent>\n'
              '            </Section>\n'
              '\n'
              '            <Section title="试一试"')
end = t.find(end_marker, start)
assert start >= 0 and end > start, (start, end)
end += len('              </CardContent>\n            </Section>')

new_block = '''            <Section
              title="接入信息"
              description="Base URL 与密钥两区通用；区域版本靠请求体 region 字段锁定，或直接选用对应区域的模型"
            >
              <CardContent className="p-0">
                <Row label="接口地址（Base URL）" hint="OpenAI 兼容端点，支持流式与工具调用；两区通用">
                  <div className="flex items-center gap-1.5">
                    <code className="max-w-[240px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={cfg.endpoint}>
                      {cfg.endpoint}
                    </code>
                    <CopyButton text={cfg.endpoint} label="接口地址" />
                  </div>
                </Row>
                <Row label="访问密钥" hint="请求头 Authorization: Bearer <密钥>；两区通用">
                  <div className="flex items-center gap-1.5">
                    <code className="max-w-[200px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={cfg.apiKey}>
                      {cfg.apiKey}
                    </code>
                    <CopyButton text={cfg.apiKey} label="访问密钥" />
                    <Button size="sm" variant="outline" onClick={() => void save({ regenerateKey: true })} disabled={saving}>
                      {saving ? <Loader2 className="animate-spin" /> : <RefreshCw />}重新生成
                    </Button>
                  </div>
                </Row>
                {(() => {
                  const domestic = cfg.models.filter((m) => m.region === "domestic");
                  const international = cfg.models.filter((m) => m.region === "international");
                  const blocks = [
                    {
                      key: "domestic",
                      icon: REGION_ICONS.domestic,
                      label: REGION_LABELS.domestic,
                      models: domestic,
                      regionValue: "domestic",
                      aliasValue: "cn",
                      sampleModel: domestic[0]?.id || "glm-5.3",
                    },
                    {
                      key: "international",
                      icon: REGION_ICONS.international,
                      label: REGION_LABELS.international,
                      models: international,
                      regionValue: "international",
                      aliasValue: "intl",
                      sampleModel: international[0]?.id || "auto",
                    },
                  ];
                  const curl = (model: string, region: string) =>
                    `curl ${cfg.endpoint}/chat/completions \\\\\\n  -H "Authorization: Bearer ${cfg.apiKey}" \\\\\\n  -H "Content-Type: application/json" \\\\\\n  -d '{"model":"${model}","region":"${region}","messages":[{"role":"user","content":"你好"}]}'`;
                  return blocks.map((b) => (
                    <div key={b.key} className="border-b border-border/40 px-4 py-3 sm:px-5">
                      <div className="flex items-center gap-2">
                        <span className="text-[12px] font-medium">
                          {b.icon} {b.label}接入
                        </span>
                        <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">
                          {b.models.length} 个模型
                        </Badge>
                        {b.models.length === 0 ? (
                          <span className="text-[11px] text-muted-foreground">该区域当前没有可用模型</span>
                        ) : null}
                      </div>
                      {b.models.length > 0 ? (
                        <>
                          <p className="mt-1.5 text-[11px] leading-5 text-muted-foreground">
                            请求体加{' '}
                            <code className="rounded bg-muted/70 px-1">"region":"{b.regionValue}"</code>
                            （兼容 "{b.aliasValue}"）锁定{b.label}账号池；不传 region 时按模型目录归属自动路由。
                          </p>
                          <div className="mt-1.5 flex flex-wrap gap-1.5">
                            {b.models.map((m) => (
                              <span
                                key={m.routeId || m.id}
                                className="rounded bg-muted/70 px-2 py-1 text-xs"
                                title={`${m.alias || m.id} · 路由 ${m.routeId}`}
                              >
                                {m.id}
                              </span>
                            ))}
                          </div>
                          <div className="mt-2 flex items-start justify-between gap-3">
                            <pre className="min-w-0 overflow-x-auto rounded-lg bg-muted/60 p-2.5 text-[11px] leading-5">
                              {curl(b.sampleModel, b.regionValue)}
                            </pre>
                            <CopyButton text={curl(b.sampleModel, b.regionValue)} label={`${b.label}示例`} />
                          </div>
                        </>
                      ) : null}
                    </div>
                  ));
                })()}
                <div className="mx-4 border-b-0 py-3.5 sm:mx-5">
                  <p className="text-sm font-medium leading-5">可用模型</p>
                  <p className="mb-2 mt-0.5 text-xs text-muted-foreground">
                    来自 AutoClaw 模型目录（🌐 国际 / 🇨🇳 国内分别路由到对应区域的账号池与网关），客户端按模型名直接调用
                  </p>
                  <div className="flex flex-wrap gap-1.5">
                    {cfg.models.map((m) => (
                      <span
                        key={m.id}
                        className="rounded bg-muted/70 px-2 py-1 text-xs"
                        title={`${m.alias || m.id} · 路由 ${m.routeId}${m.region ? ` · ${REGION_LABELS[m.region]}` : ''}`}
                      >
                        {m.region ? `${REGION_ICONS[m.region]} ` : ''}{m.id}
                      </span>
                    ))}
                  </div>
                </div>
              </CardContent>
            </Section>'''

t = t[:start] + new_block + t[end:]
io.open(p, 'w', encoding='utf-8').write(t)
print('接入信息 two-region rewrite done')
