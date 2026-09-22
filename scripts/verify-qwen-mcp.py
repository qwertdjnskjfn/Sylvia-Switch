"""MCP 协议层验证：initialize / tools/list / ping / 通知 / 鉴权 / tools/call。
用法: python verify-qwen-mcp.py [--chat]   (--chat 额外跑一次真实对话)"""
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

KEY = json.load(open(Path.home() / ".wb-switch" / "glm_proxy_config.json", encoding="utf-8"))["api_key"]
BASE = "http://127.0.0.1:57890"
PASS, FAIL = [], []


def rpc(method, params=None, req_id=1, key=KEY):
    body = {"jsonrpc": "2.0", "id": req_id, "method": method}
    if params is not None:
        body["params"] = params
    headers = {"Content-Type": "application/json"}
    if key:
        headers["Authorization"] = "Bearer " + key
    req = urllib.request.Request(BASE + "/mcp", data=json.dumps(body).encode(), headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            raw = r.read().decode("utf-8", "replace")
            return r.status, (json.loads(raw) if raw.strip() else None)
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", "replace")
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, raw


def check(name, cond, detail=""):
    (PASS if cond else FAIL).append(name)
    print(("  PASS  " if cond else "  FAIL  ") + name + (("  | " + detail) if detail else ""))


print("=== 1. GET /mcp 能力声明 ===")
req = urllib.request.Request(BASE + "/mcp", headers={"Authorization": "Bearer " + KEY})
info = json.load(urllib.request.urlopen(req, timeout=10))
check("GET /mcp 返回 transport=streamable-http", info.get("transport") == "streamable-http")
check("声明 2 个工具", info.get("tools") == ["qwen_chat", "qwen_models"], str(info.get("tools")))

print("\n=== 2. 鉴权 ===")
st, _ = rpc("ping", key="")
check("无密钥被拒 (401)", st == 401, f"status={st}")
st, _ = rpc("ping", key="gw_live_invalid_key_xyz")
check("错误密钥被拒 (401)", st == 401, f"status={st}")

print("\n=== 3. initialize ===")
st, resp = rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "verify", "version": "1"}})
check("HTTP 200", st == 200, f"status={st}")
check("jsonrpc=2.0 且 id 回显", resp.get("jsonrpc") == "2.0" and resp.get("id") == 1)
res = resp.get("result", {})
check("回显协议版本", res.get("protocolVersion") == "2024-11-05")
check("声明 tools 能力", "tools" in res.get("capabilities", {}))
check("serverInfo.name 正确", res.get("serverInfo", {}).get("name") == "sylvia-qwen-web")

print("\n=== 4. tools/list ===")
st, resp = rpc("tools/list", {}, req_id=2)
tools = resp.get("result", {}).get("tools", [])
check("返回 2 个工具", len(tools) == 2, str([t["name"] for t in tools]))
chat = next((t for t in tools if t["name"] == "qwen_chat"), None)
check("qwen_chat 有 inputSchema", bool(chat and chat.get("inputSchema", {}).get("properties")))
props = (chat or {}).get("inputSchema", {}).get("properties", {})
check("qwen_chat 暴露 messages/model/stream/tools 参数",
      all(k in props for k in ("messages", "model", "stream", "tools")), str(sorted(props.keys())))

print("\n=== 5. ping ===")
st, resp = rpc("ping", {}, req_id=3)
check("ping 返回空 result", st == 200 and resp.get("result") == {})

print("\n=== 6. 通知（无 id）→ 202 ===")
body = json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}).encode()
req = urllib.request.Request(BASE + "/mcp", data=body,
                            headers={"Content-Type": "application/json", "Authorization": "Bearer " + KEY}, method="POST")
try:
    with urllib.request.urlopen(req, timeout=15) as r:
        check("通知返回 202 且无响应体", r.status == 202 and not r.read(), f"status={r.status}")
except urllib.error.HTTPError as e:
    check("通知返回 202", e.code == 202, f"status={e.code}")

print("\n=== 7. 未知方法 → JSON-RPC error ===")
st, resp = rpc("nonexistent/method", {}, req_id=4)
check("返回 -32601", resp.get("error", {}).get("code") == -32601, str(resp.get("error")))

print("\n=== 8. tools/call: qwen_models ===")
st, resp = rpc("tools/call", {"name": "qwen_models", "arguments": {}}, req_id=5)
res = resp.get("result", {})
txt = (res.get("content") or [{}])[0].get("text", "")
check("返回模型目录文本", bool(txt) and "模型" in txt, txt[:80].replace("\n", " "))

print("\n=== 9. tools/call: 未知工具 → -32602 ===")
st, resp = rpc("tools/call", {"name": "no_such_tool", "arguments": {}}, req_id=6)
check("返回 -32602", resp.get("error", {}).get("code") == -32602)

if "--chat" in sys.argv:
    print("\n=== 10. tools/call: qwen_chat（真实对话） ===")
    t0 = time.time()
    st, resp = rpc("tools/call", {"name": "qwen_chat", "arguments": {
        "prompt": "只回复两个字：正常", "model": "qwen3.7-plus"}}, req_id=7)
    res = resp.get("result", {})
    txt = (res.get("content") or [{}])[0].get("text", "")
    print(f"  {time.time()-t0:.1f}s | isError={res.get('isError')}")
    print("  text:", txt[:160].replace("\n", " "))
    check("qwen_chat 返回文本", bool(txt))
    check("qwen_chat 未报错", res.get("isError") is not True, txt[:100])

print(f"\n{'='*50}\n通过 {len(PASS)} / 失败 {len(FAIL)}")
if FAIL:
    print("失败项:", FAIL)
