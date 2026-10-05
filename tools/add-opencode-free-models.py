#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
把 Sylvia 网关 opencode 渠道的免费模型加入 WorkBuddy（~/.workbuddy/models.json）。

背景（2026-10-02 实测）：
- WorkBuddy 的模型配置 = ~/.workbuddy/models.json（**数组**，每条一个模型）
- 现有 25 个模型全部指向 http://127.0.0.1:57890/v1
- opencode 渠道在 /opencode/v1/models 提供 45 个模型（含 41 个 free），
  但**不在 /v1 里** → WorkBuddy 看不到
- 两者**同一把 key 即可鉴权**，所以只要在 models.json 里加条目、
  把 url 指向 /opencode/v1 就行 —— **不需要改网关代码**

🔴 关键实测结论：这些免费模型 **不支持工具调用（tools）**。
   带 tools 请求时它们无视参数、直接编造答案（finish_reason=stop，
   无 tool_calls）。故一律写 supportsToolCall=false。
   它们是「聊天/问答」模型，不能当 agent 用。

用法：
    python tools/add-opencode-free-models.py --dry-run   # 预览（推荐先跑）
    python tools/add-opencode-free-models.py             # 写入（自动备份）

    # 覆盖默认清单：
    python tools/add-opencode-free-models.py --models a,b,c
    # 用测速结果自动挑（需网关在线）：
    python tools/add-opencode-free-models.py --probe

⚙️ 该脚本幂等：已存在的 id 会跳过；写入前有安全闸（条目数不得减少）。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import time

GATEWAY = "http://127.0.0.1:57890"
KEY = "7ed8e51d612d49a4a50065f2c9308a80"
OPENCODE_BASE = GATEWAY + "/opencode/v1"

# 2026-10-02 实测可用（各测 2 次，均有真实输出）。速度取两次中的较快值。
DEFAULT_MODELS = [
    ("openrouter/apodex/apodex-1.1-mini:free", "Apodex 1.1 Mini", 3.7),
    ("openrouter/cohere/north-mini-code:free", "Cohere North Mini Code", 4.0),
    ("openrouter/thinkingmachines/inkling:free", "Inkling", 4.3),
    ("opencode/mimo-v2.6-flash-free", "MiMo v2.6 Flash", 4.6),
    ("opencode/fledge-alpha-free", "Fledge Alpha", 4.6),
    ("opencode/longcat-2.5-preview-free", "LongCat 2.5 Preview", 5.1),
    ("opencode/nemotron-3-ultra-free", "Nemotron 3 Ultra", 6.4),
    ("opencode/nemotron-3.5-lightning-free", "Nemotron 3.5 Lightning", 16.7),
]

CONTEXT_WINDOW = 128_000  # 免费模型保守取值
MAX_OUTPUT = 16_000


def models_json() -> str:
    return os.path.expanduser("~/.workbuddy/models.json")


def build_entry(model_id: str, label: str, seconds: float) -> dict:
    return {
        "apiKey": KEY,
        "contextWindow": {
            "defaultLength": CONTEXT_WINDOW,
            "supportedLengths": [CONTEXT_WINDOW],
        },
        "id": model_id,
        "maxAllowedSize": CONTEXT_WINDOW,
        "maxInputTokens": CONTEXT_WINDOW,
        "maxOutputTokens": MAX_OUTPUT,
        # 名字里标注「免费·无工具」：一眼看出它不是 agent 模型
        "name": f"免费·{label}（无工具 {seconds:.1f}s）",
        "reasoning": {
            "canDisableThinking": False,
            "defaultEffort": "low",
            "effort": "low",
            "summary": "none",
            "supportedEfforts": ["low"],
        },
        "supportsImages": False,
        "supportsReasoning": False,
        # 🔴 实测：全部不支持 tools（会无视 tools 直接编造答案）
        "supportsToolCall": False,
        "url": OPENCODE_BASE,
        "vendor": "ainb",
    }


def probe_all(timeout: int = 75) -> list[tuple[str, str, float]]:
    """向 opencode 渠道实测每个免费模型，返回真有输出的（id, label, 秒）。"""
    import urllib.request, urllib.error
    from concurrent.futures import ThreadPoolExecutor

    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    req = urllib.request.Request(GATEWAY + "/opencode/v1/models",
                                 headers={"Authorization": "Bearer " + KEY})
    ids = [m["id"] for m in json.loads(opener.open(req, timeout=30).read().decode())["data"]
           if "free" in m["id"].lower()]

    def one(mid: str):
        body = {"model": mid, "max_tokens": 64,
                "messages": [{"role": "user", "content": "只回复两个字：收到"}]}
        r = urllib.request.Request(OPENCODE_BASE + "/chat/completions",
                                   data=json.dumps(body).encode(),
                                   headers={"Content-Type": "application/json",
                                            "Authorization": "Bearer " + KEY}, method="POST")
        t0 = time.time()
        try:
            d = json.loads(opener.open(r, timeout=timeout).read().decode("utf-8", "replace"))
            c = ((d.get("choices") or [{}])[0].get("message") or {}).get("content") or ""
            return (mid, time.time() - t0, bool(c.strip()))
        except Exception:
            return (mid, time.time() - t0, False)

    out = []
    with ThreadPoolExecutor(max_workers=4) as ex:
        for mid, secs, ok in ex.map(one, ids):
            if ok:
                out.append((mid, mid.split("/")[-1].replace("-free", "").replace(":free", ""), secs))
    out.sort(key=lambda x: x[2])
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description="Add opencode free models into WorkBuddy")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--models", default="", help="逗号分隔的模型 id，覆盖默认清单")
    ap.add_argument("--probe", action="store_true", help="先实测挑可用的（较慢）")
    ap.add_argument("--remove", default="", help="逗号分隔的 id，从配置中删除（清理测试遗留）")
    args = ap.parse_args()

    path = models_json()
    if not os.path.exists(path):
        print(f"[!] 找不到 WorkBuddy 模型配置：{path}")
        return 2

    if args.models.strip():
        wanted = [(m.strip(), m.strip().split("/")[-1], 0.0)
                  for m in args.models.split(",") if m.strip()]
    elif args.probe:
        print("[i] 实测中（并发 4）…")
        wanted = probe_all()
        print(f"[i] 实测可用 {len(wanted)} 个")
    else:
        wanted = list(DEFAULT_MODELS)

    with open(path, encoding="utf-8") as f:
        models = json.load(f)
    if not isinstance(models, list):
        print("[!] models.json 不是数组，拒绝改动")
        return 3

    # 删除（清理测试遗留的假条目，如 opencode/zzz-nonexistent-free）
    drop = {m.strip() for m in args.remove.split(",") if m.strip()}
    removed = []
    if drop:
        keep = []
        for m in models:
            mid = m.get("id") if isinstance(m, dict) else None
            if mid in drop:
                removed.append(mid)
            else:
                keep.append(m)
        # 只允许删**显式列出**的 id；漏删/多删都要能被看见
        not_found = drop - set(removed)
        if not_found:
            print(f"[warn] 未找到（跳过）: {sorted(not_found)}")
        models[:] = keep

    before = len(models)
    existing = {m.get("id") for m in models if isinstance(m, dict)}

    added = []
    for mid, label, secs in wanted:
        if mid in existing:
            continue
        models.append(build_entry(mid, label, secs))
        added.append(mid)

    after = len(models)
    print(f"原有模型: {before + len(removed)} 个")
    if removed:
        print(f"将删除  : {len(removed)} 个")
        for m in removed:
            print("   -", m)
    print(f"将新增  : {len(added)} 个")
    for m in added:
        print("   +", m)
    print()
    print(f"安全闸: {before + len(removed)} → {after}"
          f"（删除仅限 --remove 显式列出，新增只增不减）")

    if after < before:
        print("\n[ABORT] 条目数意外减少，拒写。")
        return 4
    if not added and not removed:
        print("\n[=] 无需改动。")
        return 0
    if args.dry_run:
        print("\n[dry-run] 未写盘。")
        return 0

    backup = f"{path}.bak-opencode-free-{time.strftime('%Y%m%d-%H%M%S')}"
    shutil.copy2(path, backup)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(models, f, ensure_ascii=False, indent=2)
    print(f"\n[ok] 已写入 {after} 个模型。备份：{backup}")
    print("\n下一步：重启 WorkBuddy 客户端 → 模型列表里出现「免费·…（无工具）」")
    print("⚠️ 这些模型不支持工具调用，只能用普通对话。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
