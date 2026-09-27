# -*- coding: utf-8 -*-
"""grok Anthropic 路径 thinking 缺省注入 e2e（1.0.5）。

验证：
  ① 不带 thinking 参数 → 网关缺省注入 → 响应出现 thinking 块（修复前为 0）
  ② 带 `x-grok-thinking: off` → 不注入（逃生开关）
  ③ 客户端显式传 thinking → 原样保留（客户端优先）
  ④ 流式 → 事件里出现 thinking_delta

用法：python tools/e2e_grok_thinking.py [model]
密钥从 ~/.wb-switch/glm_proxy_config.json 读取。
"""
import json
import os
import sys
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:57890/grok/v1/messages"
CFG = os.path.expanduser("~/.wb-switch/glm_proxy_config.json")
KEY = json.load(open(CFG, encoding="utf-8"))["api_key"]
MODEL = sys.argv[1] if len(sys.argv) > 1 else "grok-4.7"
Q = "一个水池有甲乙两个进水管，甲单独注满要 6 小时，乙要 4 小时。两管同开，多久注满？"


def call(body, extra_headers=None, timeout=240):
    headers = {
        "Content-Type": "application/json",
        "x-api-key": KEY,
        "anthropic-version": "2023-06-01",
    }
    headers.update(extra_headers or {})
    req = urllib.request.Request(BASE, data=json.dumps(body).encode(), headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except Exception as e:
        return -1, repr(e)


def blocks(raw):
    try:
        d = json.loads(raw)
    except Exception:
        return None, raw[:200]
    return [(b.get("type"), len(b.get("thinking") or b.get("text") or "")) for b in d.get("content", [])], None


base_body = {"model": MODEL, "max_tokens": 8000, "messages": [{"role": "user", "content": Q}]}

print("=== ① 不带 thinking 参数（应被注入）===")
st, raw = call(base_body)
bl, err = blocks(raw)
if bl is None:
    print(f"[❌] HTTP {st} {err}")
else:
    has = any(t == "thinking" for t, _ in bl)
    print(f"[{'✅' if has else '❌'}] HTTP {st} 块={bl}")

print("=== ② 逃生开关 x-grok-thinking: off（应不注入）===")
st, raw = call(base_body, {"x-grok-thinking": "off"})
bl, err = blocks(raw)
if bl is None:
    print(f"[❌] HTTP {st} {err}")
else:
    has = any(t == "thinking" for t, _ in bl)
    print(f"[{'✅' if not has else '❌'}] HTTP {st} 块={bl}")

print("=== ③ 客户端显式 thinking（应原样保留 1234）===")
body = dict(base_body)
body["thinking"] = {"type": "enabled", "budget_tokens": 1234}
st, raw = call(body)
try:
    d = json.loads(raw)
    ths = [len(b.get("thinking") or "") for b in d.get("content", []) if b.get("type") == "thinking"]
    print(f"[{'✅' if ths else '❌'}] HTTP {st} thinking 块长度={ths}")
except Exception:
    print(f"[❌] HTTP {st} {raw[:200]}")

print("=== ④ 流式（应出现 thinking_delta）===")
body = dict(base_body)
body["stream"] = True
st, raw = call(body)
events, th = set(), 0
for line in raw.splitlines():
    if line.startswith("event: "):
        events.add(line[7:].strip())
    elif line.startswith("data: "):
        try:
            d = json.loads(line[6:])
            if d.get("type") == "content_block_delta":
                th += len(d.get("delta", {}).get("thinking", "") or "")
        except Exception:
            pass
has = bool(th) and "content_block_delta" in events
print(f"[{'✅' if has else '❌'}] HTTP {st} thinking 字符数={th} 事件数={len(events)}")
if st != 200:
    print("   " + raw[:220])
