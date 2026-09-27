# -*- coding: utf-8 -*-
"""Anthropic /v1/messages 端到端实测（1.0.4 fallback 修复验证）。

用法：python tools/e2e_anthropic_messages.py
密钥从 ~/.wb-switch/glm_proxy_config.json 读取（不硬编码）。
"""
import json
import os
import sys
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:57890"
CFG = os.path.expanduser("~/.wb-switch/glm_proxy_config.json")
KEY = json.load(open(CFG, encoding="utf-8"))["api_key"]
MODEL = sys.argv[1] if len(sys.argv) > 1 else "glm-5.3"


def post(body, stream=False, timeout=180):
    req = urllib.request.Request(
        BASE + "/v1/messages",
        data=json.dumps(body).encode(),
        headers={
            "Content-Type": "application/json",
            "x-api-key": KEY,
            "anthropic-version": "2023-06-01",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except Exception as e:
        return -1, repr(e)


print(f"=== 1) 非流式 /v1/messages  model={MODEL} ===")
st, raw = post({"model": MODEL, "max_tokens": 64,
                "messages": [{"role": "user", "content": "只回复两个字：收到"}]})
if st == 200:
    d = json.loads(raw)
    blocks = [b.get("text", "") for b in d.get("content", []) if b.get("type") == "text"]
    text = "".join(blocks)
    print(f"[{'✅' if text.strip() else '❌'}] HTTP {st} type={d.get('type')} "
          f"model={d.get('model')} content={text[:40]!r}")
else:
    print(f"[❌] HTTP {st} {raw[:220]}")

print(f"=== 2) 流式 /v1/messages  model={MODEL} ===")
st, raw = post({"model": MODEL, "max_tokens": 64, "stream": True,
                "messages": [{"role": "user", "content": "只回复两个字：收到"}]})
events, text, done = [], [], False
for line in raw.splitlines():
    if line.startswith("event: "):
        events.append(line[7:].strip())
    elif line.startswith("data: "):
        try:
            d = json.loads(line[6:])
            if d.get("type") == "content_block_delta":
                text.append(d.get("delta", {}).get("text", ""))
        except Exception:
            pass
need = {"message_start", "content_block_start", "content_block_delta", "message_stop"}
ok = st == 200 and need.issubset(set(events)) and "".join(text).strip()
print(f"[{'✅' if ok else '❌'}] HTTP {st} 事件={sorted(set(events))}")
print(f"    正文={''.join(text)[:40]!r}  缺失事件={sorted(need - set(events)) or '无'}")
if st != 200:
    print("    " + raw[:300])
