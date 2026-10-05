# -*- coding: utf-8 -*-
"""一次性补丁：MiniMaxPage 显示积分余额/消耗 + 模型目录区块。"""
import io

p = 'src/pages/MiniMaxPage.tsx'
t = io.open(p, encoding='utf-8').read()

# 1) MmAccount 类型补字段
old = '''  signinLastDate: string;
  signinPointsTotal: number;
};'''
new = '''  signinLastDate: string;
  signinPointsTotal: number;
  balance?: number | null;
  creditConsumed?: number | null;
  balanceAt?: number | null;
};'''
assert old in t, 'type anchor'
t = t.replace(old, new, 1)

# 2) 账号行：积分余额徽标（替代纯累计视角）
old = '''                  <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">
                    签到 {a.signinLastDate ? a.signinLastDate : "未签"} · 累计 {a.signinPointsTotal} 分
                  </Badge>'''
new = '''                  {a.balance != null ? (
                    <Badge variant="secondary" className="h-5 px-1.5 text-[10px]" title={`已消耗 ${a.creditConsumed ?? 0} 分（credit/details 明细）`}>
                      积分 {a.balance} · 已用 {a.creditConsumed ?? 0}
                    </Badge>
                  ) : null}
                  <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">
                    签到 {a.signinLastDate ? a.signinLastDate : "未签"} · 累计签到 {a.signinPointsTotal} 分
                  </Badge>'''
assert old in t, 'badge anchor'
t = t.replace(old, new, 1)

# 3) 模型目录 Section（插在 接入信息 Section 之后）
anchor = '''            <Section
              title="账号池"'''
idx = t.find(anchor)
assert idx > 0, 'account pool anchor'
models_block = '''            <Section
              title="模型目录"
              description="来自桌面端 config.yaml 的真实模型表（本机实勘）；对话通道默认走 MiniMax-M3.1-Flash-Preview"
            >
              <CardContent className="p-0">
                <div className="space-y-1.5 px-4 py-3 sm:px-5">
                  {(status.models || []).map((m) => (
                    <div key={m} className="flex flex-wrap items-center gap-2 rounded bg-muted/60 px-2.5 py-1.5 text-xs">
                      <span className="font-medium">{m}</span>
                      {m === "MiniMax-M3.1-Flash-Preview" ? (
                        <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">默认 · 对话通道在用</Badge>
                      ) : null}
                      {m === "MiniMax-M3" ? (
                        <span className="text-[11px] text-muted-foreground">512k 上下文 · 可选 1M（higher_usage）· 支持图片附件</span>
                      ) : null}
                      {m === "MiniMax-M2.7" ? (
                        <span className="text-[11px] text-muted-foreground">200k 上下文 · 128k 输出 · 强制思考</span>
                      ) : null}
                      {m === "MiniMax-M2.7-highspeed" ? (
                        <span className="text-[11px] text-muted-foreground">M2.7 高速版</span>
                      ) : null}
                    </div>
                  ))}
                  {(status.models || []).length === 0 ? (
                    <p className="text-xs text-muted-foreground">未读取到桌面端模型表（检查 ~/.minimax/config.yaml 是否存在）</p>
                  ) : null}
                </div>
              </CardContent>
            </Section>

''' + anchor
t = t[:idx] + models_block + t[idx + len(anchor):]

io.open(p, 'w', encoding='utf-8').write(t)
print('MiniMaxPage models+balance ok')
