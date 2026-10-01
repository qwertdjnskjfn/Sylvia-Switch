# grok-4.7「不主动思考 / 不思考工具调用」调研与处置建议

> 调研日期：2026-09-27 · 方法：GitHub 一手来源（仓库源码 / PR / issue / release changelog）+ 本地代码与运行时实测
> 结论先行：**你的桥已经包含业界针对此问题的主要修复（#1041/#1029/#901 家族）**，因此优先级不在"升级桥"，
> 而在 ① 请求侧显式要求思考 ② 账号池质量 ③ Anthropic 协议边界的思考双向桥。

---

## 一、问题陈述

模型 `grok-4.7` 经本地桥（grok2api → xAI Build 通道）**能正常回复**，但：
1. 不会主动思考问题本身（无推理过程）；
2. 不会思考如何调用工具（不产生工具调用，或多轮里重复同一调用）。

业界对此已有专门修复与讨论，症状描述可逐条对应（见 §四·A）。

---

## 二、你的现状（本地实测，非推测）

| 项 | 实测值 | 判据 |
| --- | --- | --- |
| 桥 | `chenyme/grok2api` **v3.1.5**（源码 HEAD b88ee1f） | `gateways/grok2api/VERSION` |
| 运行 exe | `gateways/grok2api/backend/grok2api.exe`，构建于 **2026-09-16 01:21** | `stat` |
| 桥监听 | `127.0.0.1:8002`（pid 22860） | `netstat` |
| 思考缓存修复（PR #1041） | ✅ **源码与 exe 均有**（`reasoning_cache.go` 00:44 落盘 → exe 01:21 构建，晚于修复） | 文件存在 + mtime 次序 |
| 工具 schema 根节点规范化（#1029，防 xAI 400 拒工具） | ✅ 在（`normalizeBuildFunctionParametersRoot`） | grep 命中 |
| `reasoning_effort` 透传（#901 家族） | ✅ 在（含 `ReasoningEffortNone` 判定） | grep 命中 |
| 缺思考处置 | ✅ 在（`missing_thinking` 计入质量惩罚 → `withhold` 并换账号重试） | `quality_retry.go` / `egress/service.go` |
| 我方工具调用链路 | ✅ 已有：`grok.rs:849` 注入 `[AVAILABLE_TOOLS]` + `grok.rs:756 sniff_tool_calls`；`glm_web::try_extract_tool_calls`；1.0.3 新增 `anthropic_sniff_tool_calls` | grep |
| 浏览器侧 | `grok_sso_config.enabled=false`（不自动开浏览器） | `~/.wb-switch/grok_sso_config.json` |

**推论**：既然 #1041 已在（它正是修"多轮工具调用丢失 reasoning → reasoning_tokens 归 0 → 无限重复同一调用"），
当前症状更可能来自 **请求侧未声明思考**、**账号池档位不足**，或 **Anthropic 协议边界只做了单向思考转换**。

---

## 三、方案汇总（核心思路 / 适用场景 / 可行性判据）

### A. 上游桥层（治本，但大部分你已具备）

| 方案 | 核心思路 | 适用场景 | 可行性判据 |
| --- | --- | --- | --- |
| **ReasoningCache 回填**（[grok2api #1041](https://github.com/chenyme/grok2api/pull/1041)，已并入 v3.1.6） | 捕获 response 里的 `reasoning`（`encrypted_content`）按 `call_id` 缓存，下一轮把 reasoning item 注入到 `function_call` 之前 → 上游才肯继续推理 | Build/xAI 通道的多轮工具调用 | 🟢 **最强证据**：真实 agent 压测 `reasoning_tokens` 从 `178→0→0→0`（死锁重复 `list_directory`）恢复到 `353→886→收敛`；含单测+集成测试。**你本地已有** |
| **思考流式透传**（[#1023](https://github.com/chenyme/grok2api/pull/1023) / [#1025](https://github.com/chenyme/grok2api/pull/1025)） | first-wins 把 reasoning 实时下发到 Chat 与 **Anthropic** 两条协议；跨账号共享 reasoning replay | 流式 + Anthropic 客户端 | 🟢 官方 changelog（v3.1.6）明确；多条同族 PR |
| **工具 schema 根节点规范化**（[#1029](https://github.com/chenyme/grok2api/pull/1029)） | 把 `anyOf/oneOf` 联合根**提升/压平**为 `type: object`，消除 xAI 400 `root schema is an anyOf/oneOf union...` | 工具 schema 复杂时（Codex / MCP 风格） | 🟢 官方修复，注释直指 xAI 报错原文。**你本地已有** |
| **缺思考即 withhold + 换号重试**（[#959](https://github.com/chenyme/grok2api/pull/959) / [#966](https://github.com/chenyme/grok2api/pull/966)） | 把"缺 thinking"当质量缺陷：hold 住不返回，换账号重试；并把缺思考账号降级冷却 | 账号池质量参差 | 🟢 官方口径 + 你代码里 `missing_thinking` 惩罚在跑 |

### B. 代理层「提示词注入 + 文本嗅探回填」（网页/无原生工具通道的唯一可行路径）

| 方案 | 核心思路 | 适用场景 | 可行性判据 |
| --- | --- | --- | --- |
| [gpt4free `tool_support.py`](https://github.com/xtekky/gpt4free/blob/main/g4f/providers/tool_support.py) | 注入 JSON 计划提示 → **整包缓冲** → 解析 → 回填 `ToolCalls` + `finish_reason=tool_calls`；含 `_parse_json_maybe`（补 `}`、删尾逗号、括号配平）、`_strip_code_fences`、**按白名单丢弃幻觉工具名**、四种格式兼容（JSON / XML `<tool_call>` / `[Tool call:]` / ReAct） | 无原生工具的上游 | 🟢 完整闭环且在生产（67k 周下载）。**关键判据**：它明确 `pop("stream")` **放弃流式解析** → 说明"流式分片 + 工具调用"在模拟路径上未被可靠解决 |
| [AIClient2API `GrokConverter.js`](https://github.com/justlovemaki/AIClient2API/blob/main/src/converters/strategies/GrokConverter.js) | `buildToolPrompt` 注入 + `parseToolCalls` 解析 `<tool_call>` + `formatToolHistory` 回填历史；**同时写 `reasoning_content`** | grok-web 通道 | 🟢 8.8k★，且已由此修复 [issue #603](https://github.com/justlovemaki/AIClient2API/issues/603)（grok-web 丢 tool calls → OpenCode 不可用） |
| [cc-proxy `ToolifyParser`](https://github.com/Passerby1011/cc-proxy) | **真流式状态机** + JSON 修复，比分片后置解析更稳 | 需要流式工具调用 | 🟡 被 grok2api [issue #219](https://github.com/chenyme/grok2api/issues/219) 官方 workaround 推荐 |
| [LiteLLM](https://docs.litellm.ai/docs/completion/function_call)（`add_function_to_prompt=True`） | **只注入不回填**：把 tools 写进 prompt，返回纯文本 | 快速验证模型"能否被诱导调用工具" | 🟡 [main.py:5637](https://github.com/BerriAI/litellm/blob/main/litellm/main.py#L5637)；不支持时直接抛 `UnsupportedParamsError`（[utils.py:4382](https://github.com/BerriAI/litellm/blob/main/litellm/utils.py#L4382)）—— 无解析器，不能单独用作方案 |
| [CLIProxyAPI #4977](https://github.com/router-for-me/CLIProxyAPI/pull/4977) | 把 xai 原生 `<|tool_calls_begin|>` markup 从正文**提升**为标准 `tool_calls` | 上游已给 markup 但放在正文里 | 🟡 可借鉴其"提升"思路 |

> **你已有对应实现**：`grok.rs:849`（注入 `[AVAILABLE_TOOLS]` + JSON 格式约定）与 `grok.rs:756 sniff_tool_calls`，
> 以及 1.0.3 的 `anthropic_sniff_tool_calls`。**缺的是容错深度**（见 §五·P1）。

### C. 思考通道恢复（网页/文本通道）

| 方案 | 核心思路 | 适用场景 | 可行性判据 |
| --- | --- | --- | --- |
| 请求体开关 + `reasoning_content` 透传 | 请求侧置 `thinking_enabled: true`（或模型名后缀如 `-think`/`-thinking`），把思考 chunk 累积成 `reasoning_content` | 网页/Cookie 反代通道 | 🟢 [4xiaxia/DeepSeek-Free-API](https://github.com/4xiaxia/DeepSeek-Free-API)（`chat.ts:284` 请求开关、`:636` 透传）；[Kimi-Free-API](https://github.com/xiaoY233/Kimi-Free-API) |
| **Anthropic 双向桥**（下行 `reasoning_content`→`thinking` block，**上行必须回填**） | 只做下行转换会在多轮 400/丢思考；须把 assistant 的 thinking 回填回 `reasoning_content` | 客户端吃 Anthropic 协议（ZCode / Claude Code 类） | 🟢 [claude-code-router #1378](https://github.com/musistudio/claude-code-router/issues/1378)（内置 transformer 单向 → 官方 workaround 插件 `deepseek-thinking-roundtrip`）；同族 [kdx-anthropic-bridge](https://github.com/godkey1314/kdx-anthropic-bridge)、[antigravity-claude-proxy](https://github.com/badri-s2001/antigravity-claude-proxy) |
| CDP 点网页「深度思考」开关 | 模拟点击 UI 启用 thinking | 网页 UI 通道 | 🔴 **未找到可信证据**（仅内容农场，参数疑似杜撰）；现有网页通道项目一律走请求体开关 |
| 「强制思考」提示词提升质量 | 系统提示强制先思考再作答（CoT / plan-then-act） | 所有文本态模型 | 🟡 **证据弱**：只有通用模板（[agentic-design-patterns-skills](https://github.com/hajekim/agentic-design-patterns-skills)）与 ReAct 原论文；**未找到**"显著提升工具调用成功率"的量化证据 |

### D. Agent 循环降级范式（不依赖原生工具）

| 方案 | 核心思路 | 适用场景 | 可行性判据 |
| --- | --- | --- | --- |
| [smolagents `CodeAgent`](https://github.com/huggingface/smolagents) | 模型**吐 Python 代码**调工具（而非 JSON tool_calls），沙箱执行后回灌 | 模型能写代码但不会发结构化工具调用 | 🟢 30k★，README 自述"步数少 30%"，仍保留正文→tool_calls 回填 |
| 文本态工具协议（Cline / Roo 旧格式） | `<tool_name><param>…</param></tool_name>` XML 协议 + 解析 | 上游无原生工具 | 🟢 [Cline v3.35 官方博客](https://cline.bot/blog/cline-v3-35) 明确"**不支持原生工具的模型继续用文本态**"（权威降级表述）；[Roo PR #11207](https://github.com/RooCodeInc/Roo-Code/pull/11207) 已把 native 定为唯一协议 |
| 解析器层（格式识别） | 按模型家族识别标记（Hermes/Functionary/Llama3.1/Mistral），未识别走 Generic | 自建推理服务 | 🟢 [llama.cpp function-calling.md](https://github.com/ggml-org/llama.cpp/blob/master/docs/function-calling.md)（130k★）、[vLLM tool_parsers](https://github.com/vllm-project/vllm/tree/main/vllm/entrypoints/openai/tool_parsers)（93k★） |
| Aider `SEARCH/REPLACE` 文本块 | 完全不用工具调用，用可模糊匹配的编辑块 | 代码编辑场景 | 🟢 [editblock_coder.py](https://github.com/Aider-AI/aider/blob/main/aider/coders/editblock_coder.py)（生产在用），容错靠模糊匹配 |

### E. 明确不适用（省得走弯路）

| 方案 | 为何不适用你 |
| --- | --- |
| 约束解码（outlines / guidance / jsonformer） | 需**本地权重**做 logits 掩码；对网页/Build 通道无控制点。且 jsonformer 已停更（2023-05） |
| [one-api](https://github.com/one-api/one-api) / [new-api](https://github.com/QuantumNous/new-api) | 仅协议转换，README 自述"tools 字段可能无法精确映射"，**无工具模拟实现** |
| 自己再实现一遍 `ReasoningCache` | 你本地已含 #1041（源码 + exe 均在），重复投入无收益 |

---

## 四、推荐路线（按 证据强度 × 你可控性 排序）

### 🔴 P0 —— 先做这两件（成本低、直指症状）

1. **请求侧显式要求思考 + 观测是否真的拿到思考**
   - 做法：agent 场景为 `grok-4.7` 注入默认 `reasoning_effort`（非 `none`；上游支持到 `xhigh`），
     并在网关侧记录响应里 `reasoning_content` 是否非空。
   - 依据：上游以 `reasoning_effort` 控档（#901 家族，你代码里已有 `ReasoningEffortNone` 判定）；
     你的桥对"缺思考"是 `withhold + 换号重试`（#959/#966）—— 若整池都缺思考，客户端只会看到"没思考"的输出。
   - 最小验证：对同一问题分别用 `reasoning_effort=high` 与 `none` 各打一次，比对 `reasoning_content` 长度；
     若两者都为空 → 说明问题在账号档位（转 P0-2）。
2. **核对账号池档位与 quality guard**
   - 做法：查桥的审计/quality guard 面板：`missing_thinking` 计数、被冷却账号数、当前可用账号档位（Free/Basic/Plus）。
   - 依据：桥把 `missing_thinking` 列为 durable 质量惩罚（你代码 `account/service.go:2309`），
     免费档上游常常本就不产 reasoning → 桥无法凭空造出思考。
   - 判定：若可用账号集中在免费档 → **这是"不思考"的根本约束**，只能靠 P1/P2 的提示词与循环范式补偿。

### 🟡 P1 —— 协议与工具链补强（你已有基础，补容错深度）

3. **Anthropic 协议做「思考双向桥」**：下行 `reasoning_content` → `thinking` block；**上行把 assistant 的 thinking 回填**回请求。
   依据：[claude-code-router #1378](https://github.com/musistudio/claude-code-router/issues/1378) 指出单向转换会在多轮丢思考/报 400，
   官方 workaround 即"往返插件"；这也是"不思考如何调用工具"在多轮里的典型表现。
4. **工具调用容错三件套**（把现有 `sniff_tool_calls` 升级）：
   - 解析容错：补 `}` / 删尾逗号 / 括号配平 / 剥代码围栏（[g4f](https://github.com/xtekky/gpt4free/blob/main/g4f/tools/tool_support.py) 的现成策略）；
   - 幻觉工具名按白名单丢弃（同上 `filter_tool_calls_by_names`）；
   - **多格式兼容**（JSON + `<tool_call>` XML + ReAct 文本），单一格式在真实模型上不可靠；
   - 流式：要么**非流式**回填，要么采用 [cc-proxy](https://github.com/Passerby1011/cc-proxy) 的真流式状态机。
     依据：g4f 在模拟路径直接 `pop("stream")` = 社区事实上的"流式分片解析未解决"。
5. **工具 schema 先自检**：确认经网关转发给桥的 `tools` 根节点是 `type: object`（不要 `anyOf/oneOf` 联合）。
   依据：#1029 修复的正是该 400；本地桥已具备规范化，但**上游客户端 → 你的网关**这一段仍可能漏。

### 🟢 P2 —— 范式层（治本但改动大，视 P0 结论决定）

6. 若确认免费档模型原生无思考能力：改用**外部循环**替代模型内生思考 —— ReAct 提示词循环 /
   [smolagents CodeAgent](https://github.com/huggingface/smolagents)（代码即动作）/
   Cline 式文本协议；并把解析失败回灌为 observation 让模型自我纠正（Aider 式模糊匹配）。

---

## 五、证据强度声明

| 结论 | 强度 |
| --- | --- |
| #1041 的症状与修复（reasoning 丢失 → 重复工具调用死锁） | 🟢 一手：PR 描述 + 压测数据 + 单测/集成测试 |
| #1029 / #901 / #959 / #966 已在本地源码与 exe | 🟢 一手：本地文件存在 + mtime 次序 + grep 命中 |
| 文本态回填是网页通道唯一可行路径；流式解析未被可靠解决 | 🟢 一手：多个生产项目源码 |
| Anthropic 思考需双向桥 | 🟢 一手：issue + workaround 插件 |
| 「强制思考提示词」能显著提升工具调用质量 | 🔴 **未找到量化证据**，仅通用模板；如需可先小样本实测 |
| CDP 点击网页「深度思考」开关 | 🔴 **未找到可信证据**（内容农场来源，未采信） |

---

# 六、本地实测结论（2026-09-28 00:1x–00:35，全部走真实网关路径）

## 6.1 实测矩阵

| # | 路径 / 变体 | HTTP | 思考 | 工具调用 | 结论 |
| --- | --- | --- | --- | --- | --- |
| 1 | **OpenAI** `/grok/v1/chat/completions` 默认 | 200 (6.6s) | `reasoning_content` **80 字** | — | 默认就带思考 |
| 2 | 同上 + `reasoning_effort=high` | 200 (8.0s) | **80 字**（与默认一致） | — | Free 档 effort **不生效** |
| 3 | 同上 + `reasoning_effort=none` | **502** | — | — | 上游 400：`This model does not support reasoning_effort=none` → **该模型强制思考** |
| 4 | max_tokens 400 / 2000 / 8000 | 200 | **均 80 字** | — | 不是预算截断，是上游只给~80 字的**思考摘要** |
| 5 | **OpenAI** + `tools` | 200 (1.6s) | 131 字 | ✅ `tool_calls=1` `finish_reason=tool_calls` | 原生工具调用**正常** |
| 6 | **OpenAI** 多轮回填 tool 结果 | 200 | 0 字（简单问题不再思考） | ✅ 无重复调用 | **#1041 指纹不成立**（未复现死锁） |
| 7 | **Anthropic** `/grok/v1/messages`（不带 thinking 参数） | 200 | **0 字**（无 `thinking` 块、无 `thinking_delta`） | ✅ `tool_use` 正常 | ⚠️ 关键对照点 |
| 8 | 同上 + **`thinking:{type:"enabled",budget_tokens:2000}`** | 200 (5.7s) | ✅ **`thinking` 块 73 字** + text | — | **思考是有条件的 opt-in** |
| 9 | 同上 + `budget_tokens:6000` | 200 (16.2s) | ✅ **246 字** | — | **思考长度随 budget 增长**（预算真实生效） |

## 6.2 根因（修正上一轮假设）

**不是网关/桥的缺陷** —— `/grok/v1/messages` 是纯字节透传（`grok::g2a_messages_forward`），
桥（grok2api）按 **Anthropic 规范**要求 thinking 必须**显式 opt-in**：不传 `thinking` 参数就**合法地**不返回思考块。
而 OpenAI 路径默认带 `reasoning_content` —— 这就是"同一模型两条协议命运不同"的全部原因。

⇒ 用户症状的真实成因（按概率排序）：
1. 🔴 **客户端（ZCode，Anthropic 协议）没有开启 thinking**（未传 `thinking:{type:enabled,budget_tokens:N}`）
   → 模型合法地不输出思考 → 主观"不主动思考"；
2. 🔴 同一原因在多轮 agent 里放大：Anthropic 要求**多轮回传 thinking 块**以维持推理连续性，
   客户端既然没开、也就不会回传 → 第二轮起推理上下文断裂 → 表现为"**不会思考如何调用工具**"（与 claude-code-router #1378 同类）；
3. 🟡 附加约束（非缺陷）：账号池 **12/12 全是 `tier=free`**；Free 档 `reasoning_effort` 不区分档位、思考只给 ~80 字摘要。

## 6.3 处置建议（修正后的优先级）

| 优先级 | 动作 | 成本 | 依据 |
| --- | --- | --- | --- |
| 🔴 **A** | **客户端侧开启 thinking**：ZCode（Anthropic）传 `thinking:{type:"enabled",budget_tokens:4000~8000}`；同时确保多轮把它回传 | 0（先验证） | 实测 #8/#9：带上即有思考，且长度随预算增长 |
| 🔴 **B**（推荐落地） | **网关侧默认注入**：`/grok/v1/messages` 若请求**未携带** `thinking`，则注入默认 `{type:enabled, budget_tokens:N}`（配置项，默认建议 4000，并按 `max_tokens` 自适应下调以防 400）；客户端显式指定优先 | 小改 + 部署 | 该路径服务对象本就是 agent/编码客户端，"默认开思考"更符合预期；客户端零改动 |
| 🟡 **C** | **多轮 thinking 回填**：若客户端不回传 thinking 块，网关在请求转换时补回上一轮 thinking（参考 grok2api #1041 的 `ReasoningCache` 与 claude-code-router 的 roundtrip 插件） | 中 | #1378 / #1041 同族证据 |
| 🟢 **D** | 账号池档位：Free 档思考质量受限（80 字摘要 + effort 不生效），如需深度推理需 Basic/Plus 档账号 | — | 实测 #2/#4 + 账号池 12/12 free |

> **上一轮 P1-3"Anthropic 思考双向桥缺失"的假设已被实测否定**：桥侧无需改；
> 真正要补的是**"默认开启思考"这一层**（B）与**多轮回填**（C）。
