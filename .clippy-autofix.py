"""clippy JSON 批量修复：解析 --message-format=json，按 span 精确应用建议替换。
只处理机械性类别（同类型转换、多余 mut、冗余闭包等），逐条精确匹配后替换。
"""
import json, subprocess, sys, os, collections

WS = r"E:\WorkBuddy自动签到助手\workbuddy-switch"
os.chdir(WS)

# 允许自动修的警告类别（文本前缀匹配）
ALLOW = [
    "casting to the same type",
    "digits grouped inconsistently",
    "variable does not need to be mutable",
    "this expression creates a reference which is immediately dereferenced",
    "redundant closure",
    "using `contains()` instead of",
    "unneeded `return` statement",
    "useless use of `format!`",
    "useless conversion to the same type",
    "manual implementation of `.is_multiple_of()`",
    "this `map_or` can be simplified",
    "doc list item without indentation",
    "empty line after doc comment",
]

def run_clippy_json():
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = r"E:\wb-target"
    env["PATH"] = r"E:\mingw64\bin;<user-home>\.cargo\bin;" + env.get("PATH", "")
    r = subprocess.run(
        ["cargo", "clippy", "-p", "wb-switch-core", "-p", "wb-switch-server",
         "--message-format=json"],
        capture_output=True, text=True, encoding="utf-8", errors="replace",
        env=env, cwd=WS, timeout=1200)
    diags = []
    for line in r.stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if d.get("reason") == "compiler-message" and "message" in d:
            diags.append(d["message"])
    return diags

def collect_fixes(diags):
    """收集 (file, line_start, col_start, line_end, col_end, replacement) —— 1-based"""
    fixes = []
    for m in diags:
        msg = m.get("message", "")
        if not any(msg.startswith(a) for a in ALLOW):
            continue
        spans = m.get("spans", [])
        primary = next((s for s in spans if s.get("is_primary")), None)
        if not primary or not primary.get("file_name", "").endswith(".rs"):
            continue
        # 找 suggested replacement（children 里的 help 建议）
        repl = None
        for ch in m.get("children", []):
            sr = ch.get("spans", [])
            for s in sr:
                if s.get("suggested_replacement") is not None and s.get("suggestion_applicability") in (
                        "MachineApplicable", "MaybeIncorrect", "Unspecified"):
                    repl = s
                    break
            if repl:
                break
        if not repl:
            continue
        fixes.append({
            "file": primary["file_name"],
            "ls": primary["line_start"], "cs": primary["column_start"],
            "le": primary["line_end"], "ce": primary["column_end"],
            "rs": repl.get("line_start"), "cs2": repl.get("column_start"),
            "re": repl.get("line_end"), "ce2": repl.get("column_end"),
            "text": repl.get("suggested_replacement", ""),
            "msg": msg[:60],
        })
    return fixes

def apply_fixes(fixes):
    byfile = collections.defaultdict(list)
    for f in fixes:
        byfile[f["file"]].append(f)
    applied = skipped = 0
    for fname, fs in byfile.items():
        try:
            lines = open(fname, encoding="utf-8").read().split("\n")
        except OSError:
            continue
        # 同文件内按位置倒序应用（避免行号漂移）；重叠跳过
        fs.sort(key=lambda f: (f["ls"], f["cs"]), reverse=True)
        last_pos = None
        for f in fs:
            pos = (f["ls"], f["cs"])
            if last_pos and pos >= last_pos:
                skipped += 1
                continue
            last_pos = pos
            if f["ls"] != f["le"] or f["rs"] != f["le"] or f["cs2"] != f["cs"] or f["ce2"] != f["ce"]:
                # 跨行替换或替换列与诊断列不一致：跳过（保守）
                skipped += 1
                continue
            li = f["ls"] - 1
            if li >= len(lines):
                skipped += 1
                continue
            line = lines[li]
            # 列是 1-based 字符列；中文按字符计（clippy 用的是字符列）
            before = line[:f["cs"]-1]
            after = line[f["ce"]-1:]
            lines[li] = before + f["text"] + after
            applied += 1
        open(fname, "w", encoding="utf-8", newline="").write("\n".join(lines))
    return applied, skipped

for round_i in range(1, 4):
    diags = run_clippy_json()
    total = sum(1 for d in diags if d.get("message", "").startswith("warning"))
    fixes = collect_fixes(diags)
    if not fixes:
        print(f"round {round_i}: 无可自动修复项，剩余警告 {total}")
        break
    applied, skipped = apply_fixes(fixes)
    print(f"round {round_i}: 警告 {total}，应用 {applied}，跳过 {skipped}")

print("done")
