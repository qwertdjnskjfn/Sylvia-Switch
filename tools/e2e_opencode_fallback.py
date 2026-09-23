# -*- coding: utf-8 -*-
"""OpenCode 网关 fallback e2e 实测（0.1.85）。"""
import json
import os
import sys
import urllib.request
import urllib.error

BASE = "http://127.0.0.1:57890/opencode/v1"
# 密钥从运行时配置读，不硬编码（避免进 git）
_CFG = os.path.expanduser("~/.wb-switch/opencode_proxy_config.json")
with open(_CFG, encoding="utf-8") as _f:
    KEY = json.load(_f).get("api_key", "")
if not KEY:
    print("FATAL: api_key not found in " + _CFG)
    sys.exit(2)


def post(body: dict, timeout: int = 180):
    req = urllib.request.Request(
        BASE + "/chat/completions",
        data=json.dumps(body).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            "Authorization": "Bearer " + KEY,
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except Exception as e:
        return -1, "transport-error: %r" % (e,)


def probe(model: str):
    """发一个最小请求，返回 (http_status, model_echo, ok, err_msg, content_head)。"""
    st, raw = post(
        {
            "model": model,
            "messages": [{"role": "user", "content": "回复ok两个字即可"}],
        }
    )
    try:
        d = json.loads(raw)
    except Exception:
        return st, None, False, raw[:200], ""
    if st == 200:
        echo = d.get("model")
        content = (
            d.get("choices", [{}])[0].get("message", {}).get("content", "") or ""
        )
        return st, echo, bool(content.strip()), "", content[:60]
    msg = d.get("error", {}).get("message", raw[:300])
    return st, None, False, msg, ""


def show(tag: str, r):
    st, echo, ok, err, head = r
    status = "✅" if (st == 200 and ok) else "❌"
    print(f"[{status}] {tag}: HTTP {st} model={echo!r} nonEmpty={ok}")
    if err:
        print("    err: " + err.replace("\n", " ⏎ ")[:400])
    if head:
        print("    content: " + head.replace("\n", " "))
    return st, echo, ok, err, head


if __name__ == "__main__":
    action = sys.argv[1] if len(sys.argv) > 1 else "normal"
    if action == "normal":
        show("正常路径 · 指定 mimo", probe("opencode/mimo-v2.6-flash-free"))
    elif action == "probe":
        # 探测候选「当前正在失败」的模型（0.1.84 表里的 502 常客）
        for m in [
            "unorouter/deepseek-v4-flash:free",
            "orcarouter/deepseek/deepseek-v4-flash-free",
            "openrouter/google/gemma-4-31b-it:free",
            "orcarouter/orcarouter/free",
        ]:
            st, echo, ok, err, head = probe(m)
            tag = "✅可用" if (st == 200 and ok) else ("❌失败" if st != 200 else "⚠️空响应")
            print(f"{tag} {m}: HTTP {st}")
            if st != 200:
                print("    err: " + err.replace("\n", " ⏎ ")[:260])
    elif action == "fallback":
        model = sys.argv[2]
        show(f"fallback · 指定 {model}", probe(model))
    elif action == "stream":
        model = sys.argv[2]
        req = urllib.request.Request(
            BASE + "/chat/completions",
            data=json.dumps(
                {
                    "model": model,
                    "stream": True,
                    "messages": [{"role": "user", "content": "回复ok两个字即可"}],
                }
            ).encode("utf-8"),
            headers={
                "Content-Type": "application/json",
                "Authorization": "Bearer " + KEY,
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=180) as resp:
                st = resp.status
                raw = resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as e:
            st = e.code
            raw = e.read().decode("utf-8", "replace")
        models = set()
        done = False
        for line in raw.splitlines():
            if line.startswith("data: ") and line != "data: [DONE]":
                try:
                    models.add(json.loads(line[6:]).get("model"))
                except Exception:
                    pass
            elif line == "data: [DONE]":
                done = True
        print(f"stream HTTP {st} models_in_frames={models} done={done}")
        if st != 200:
            print("    err: " + raw.replace("\n", " ⏎ ")[:400])
