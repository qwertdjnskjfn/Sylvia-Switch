# 变更记录（wb-switch / Sylvia Switch）

版本号取自 `crates/wb-switch-server/Cargo.toml`，运行时可通过
`curl http://127.0.0.1:57890/api/version` 查询「线上跑的到底是哪一版、哪次构建」。

> 0.1.38 之前的版本没有独立记录，可查 `git log`。

---

## 0.1.85 — 2026-09-23

### 新功能：OpenCode 网关自动 fallback —— 上游 5xx / 连接断开自动换下一个可用模型

#### 需求
免费池模型随时段波动（0.1.84 实测：同一模型 14:45 还 200、14:54 就 502），
调用方拿到 502 后只能手工换模型重试。本次把「换模型」做进网关：
**上游 502 或连接被断时，按已验证可用清单自动重试下一个模型；全部失败返回明确错误；
最终响应回显实际使用的模型名称。**

#### 触发条件（严格按需求：仅服务端故障 / 连接断开）
| 错误签名 | 是否换模型 | 说明 |
| --- | --- | --- |
| `提交消息失败 5xx …`（502 最常见，500/503 同理） | ✅ | opencode server 转述上游/provider 故障 |
| `提交消息失败: error sending request …` | ✅ | 连接被断 —— 实证多为上游限流→FK bug→断连（0.1.84），换模型有用 |
| `提交消息失败 4xx …`（如 400 unknown model） | ❌ | 请求本身有问题，换模型无意义 |
| `messages 不能为空` 等输入错误 | ❌ | 原样返回，不烧 fallback 链 |
| 服务发现失败（server 没起） | ❌ | 换模型无意义，快速失败 |
| `等待回复超时` | ❌ | 保守：多为排队，避免误杀慢模型 |

> 状态码只认「提交消息失败」后的状态位（5xx 才换），错误正文里出现 502 字样不误伤。
> 实测构造：请求不存在的免费 id `opencode/zzz-nonexistent-free` → server 回 500 → 自动回退到链首 mimo。

#### 候选顺序（去重、保序、仅免费）
1. **用户显式指定的模型**（尊重调用方选择，免费校验已过）；
2. **已验证可用链**（内置 `VERIFIED_FALLBACK_CHAIN`，0.1.84 逐个实调 200 且正文非空）：
   `opencode/mimo-v2.6-flash-free` → `openrouter/nex-agi/nex-n2.5-pro:free` →
   `ling-3.0-flash-fin` → `north-mini-code` → `ling-3.0-flash-vl` →
   `nemotron-3-super-120b` → `nemotron-3-ultra-550b` → `lfm-2.5-2.6b`（均 openrouter `:free`）；
3. **当前免费目录其余模型**（第一批走完才拉一次 `/provider` 目录兜底，Happy Path 零额外开销；
   按用户决策**不设尝试上限**，全目录兜底）。

Happy Path 零损耗：第一批 = 用户指定 + 内置链，纯本地构造无网络请求；
目录只在「链全部 502/断连」后才拉一次。

#### 模型回显
- 非流式：响应 `model` = 实际使用的模型（workbuddy_proxy 按 body 回显，天然正确）；
- 流式：`chat_completions_stream` 返回 `(流, 实际模型)`，SSE 每帧 `model` 与网关日志
  （`gateway_request_logs`）都以实际模型为准 —— 流式日志此前后 requested，0.1.85 起修正。

#### 配置（仅配置文件，`~/.wb-switch/opencode_proxy_config.json`）
| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `fallback_enabled` | `true` | 关闭后退化为 0.1.84 行为（只试用户指定模型） |
| `fallback_chain` | 空 = 内置链 | 自定义链，非 `free` 条目会被过滤（网关只提供免费模型） |

旧配置文件缺省两个字段 → 默认开 + 内置链，向后兼容；保存时回写，不会被 UI 保存抹掉。

#### 改动
- `opencode.rs`：`VERIFIED_FALLBACK_CHAIN` / `resolve_fallback_chain` / `build_attempt_chain`
  / `is_retryable_upstream_error`（复用 `workbuddy_proxy::is_connect_error`，提权 `pub(crate)`）
  / `chat_completions_json` 改逐模型循环 + 全失败汇总错误（含尝试清单与 0.1.84 建议文案）；
  空模型时由链首顶替旧的「取目录第一个」（server 不可达也能选模型）；
  `proxy_status` 增 `fallbackEnabled` / `fallbackChain` 只读诊断。
  新增 6 个单测（链构造去重保序 / 触发判定 / 自定义链过滤 / 错误装饰）。
- `workbuddy_proxy.rs`：仅 `is_connect_error` 提权 `pub(crate)`，**零行为变更**
  （串行锁 / models_payload / models_catalog / 定向重试等红线未动）。
- `api.rs`：流式分支适配 `(stream, actual)`，日志记实际模型；错误路径未动（仍 502）。

#### 验证（2026-09-23 16:4x 上线后实测）
- `cargo test -p wb-switch-core --lib`：**319 passed, 0 failed**（含 7 个新测试：
  链构造去重保序 / 空模型 / 触发判定 5xx·4xx·连接·超时 / 自定义链过滤 / 汇总错误格式 / 错误装饰）；
- `cargo check -p wb-switch-core -p wb-switch-server --all-targets`：**0 错误**；
- `npm run build`（tsc && vite build）：**通过**；
- `deploy.py --expect-version 0.1.85`：**两次部署均断言通过**（停机 1.0–1.1s；0.1.85 上线后
  `/api/version` 自证 version=0.1.85 buildTime=16:40:33）；
- e2e（`tools/e2e_opencode_fallback.py`，全部实调）：

| # | 场景 | 结果 |
| --- | --- | --- |
| 1 | 正常指定 `opencode/mimo-v2.6-flash-free` | ✅ 200，回显指定模型，正文非空 |
| 2 | 不指定模型（链首顶替） | ✅ 200，回显 `opencode/mimo-v2.6-flash-free` |
| 3 | 指定必败模型 `opencode/zzz-nonexistent-free`（server 回 500） | ✅ **自动回退到链首 mimo**，回显实际模型 |
| 4 | `stream:true` 同上 | ✅ 200，SSE 每帧 `model` = mimo，`[DONE]` 正常 |
| 5 | 自定义链只放必败模型 → 强制走目录兜底层 | ✅ 200，回显目录首个可用模型 `opencode/ling-3.0-flash-fin-free` |
| 6 | 临时 `fallback_enabled=false` + 必败模型 | ✅ 502 + 明确错误（提交失败原文 + 额度/限流建议），配置字段生效 |
| 7 | 非免费模型 `opencode/some-paid-model` | ✅ 仍 400 拒绝（契约无回归） |
| 8 | `tools/monitor.py` 巡检 | ✅ **ALL GREEN**（进程/端口/版本自证/404 JSON） |

- 网关日志 `gateway_request_logs.json`：上述 fallback 成功请求均以**实际模型**入账
  （非流式原本就记 actual；流式路径 0.1.85 起从记 requested 修正为记 actual）。

> 注：「全部失败汇总错误」这条分支线上无法构造（需要所有免费模型同时失败），
> 由单测 `all_failed_error_lists_chain_and_last_error` 覆盖格式；其错误装饰与单候选
> 失败路径已由场景 6 实测。

#### 已知限制
- 最坏情况（全目录逐个 5xx）请求耗时可达数分钟 —— 用户决策「不设上限」换全目录兜底；
  若嫌久可后续加时限或上限（一处 const 即可）。
- **上游 200 但正文为空不触发 fallback**（实测 `orcarouter/orcarouter/free`、
  `opencode/muse-spark-1.2-contributor-free` 偶发）：按需求语义仅 5xx/连接断开才换。
  若想「空响应也换模型」，在成功分支加一个正文非空判定即可（预留位）。
- fallback 期间每个失败模型都会「建会话→删会话」，opencode server 的 FK bug 场景下
  可能留下孤儿 session（0.1.84 既存现象，非本次引入）。

---

## 0.1.84 — 2026-09-23

### 修复：OpenCode 502「提交消息失败」真相 = 上游免费额度/限流（不是网络故障）+ 报错带可操作建议

#### 现象与误导性
用户报 `错误码 3007`，服务端原文：
```
502 提交消息失败: error sending request for url (http://127.0.0.1:4097/session/.../message)
category: network
```
**看起来像网络/连接故障，实际完全不是。**

#### 根因（读 opencode server 自己的日志才看清）
`E:\opencode\opencode-serve.log` 显示，FK 报错**之前**总有这些上游错误：

```
AI_APICallError: Your balance is empty (remaining: $0.000000), so this request cannot proceed
AI_APICallError: This model is rate limited right now.
AI_APICallError: Too many requests. The free tier allows 1 request(s) ...
→ 然后 EffectDrizzleQueryError / SQLiteError: FOREIGN KEY constraint failed（insert into "message"）
→ 再然后连接被断，客户端只看到 error sending request
```

**链路**：上游 provider（共享免费池）返回**余额为空/限流** → opencode server 想把错误消息写库 →
触发它**自己的 FK bug**（`message.session_id` 指向不存在的 session 行）→ 服务端异常、连接被断 →
reqwest 只能报 `error sending request`。⇒ 这是 **opencode server 的 bug + 上游免费额度耗尽**
共同造成的，**不是网络问题，也不在本网关**。

#### 免费额度是「按模型、随时段波动」的（实测结论）
同一批模型在不同时间命运完全不同：

| 模型 | 14:45 | 14:47 | 14:52 | 14:54 |
| --- | --- | --- | --- | --- |
| `opencode/nemotron-3.5-lightning-free` | ✅ 200 | — | ✅ 200 | ❌ 502 |
| `unorouter/deepseek-v4-flash:free` | — | ❌ 502 | ❌ 502 | — |
| `orcarouter/deepseek/deepseek-v4-flash-free` | — | — | — | ❌ 502 |
| `openrouter/google/gemma-4-31b-it:free` | — | — | — | ❌ 502 |
| `opencode/mimo-v2.6-flash-free`、`openrouter/*`（多数） | ✅ | ✅ | ✅ | ✅ |

⇒ **`opencode`/`unorouter`/`orcarouter` 这几个"原生免费池"额度很紧**；
`openrouter` 渠道（自有免费池较大）明显更稳。

#### 改动

1. **报错带可操作建议**（`opencode.rs`）：命中「提交消息失败」时追加真实根因与建议 ——
   上游免费额度/限流所致，换个免费模型（先 `GET /opencode/v1/models` 取当前可用清单）或稍后重试。
   （不再让用户误以为网络断了。）
2. **替换踩限流的条目**（`~/.workbuddy/models.json`）：
   `unorouter/deepseek-v4-flash:free` → **`openrouter/liquid/lfm-2.5-2.6b:free`**（实测 200 · 9.8s）。
   替换前后都**逐个实调验证**（不看状态码、还要求正文非空）。
3. （0.1.83 已随本版一并说明）`workbuddy_proxy` 转发加**进程级串行化** + **会话落库确认**
   + **定向重试一次** —— 这三项是卫生改进，虽非本次根因，但能减少并发突发下的失败面。

#### 验证
- 8 个条目逐个实调：**7 个 200 且正文非空**（唯一失败者已换成可用模型）。
- `cargo check -p core -p server` **0 错误**。
- 报错文案：对限流模型返回 502 时，错误体含「多为上游免费额度/限流」+ 建议换模型。

#### 给用户的口径
**不是网关坏了，是"免费模型"本身有额度限制**。挑模型时优先用这些当前稳定的：
`opencode/mimo-v2.6-flash-free`、`openrouter/inclusionai/ling-3.0-flash-fin:free`、
`openrouter/cohere/north-mini-code:free`、`openrouter/inclusionai/ling-3.0-flash-vl:free`、
`openrouter/nvidia/nemotron-3-super-120b-a12b:free`、
`openrouter/nvidia/nemotron-3-ultra-550b-a55b:free`、
`openrouter/nex-agi/nex-n2.5-pro:free`、`openrouter/liquid/lfm-2.5-2.6b:free`。

---
## 0.1.82 — 2026-09-23

### 变更：WorkBuddy 目录改为「每渠道各 1 个代表」（约 14 条）—— 回应「重新获取那 14 个」

0.1.81 把目录压到**只列内置模型（1 条）**后，用户随即要求「重新获取 WorkBuddy 里面的 14 个模型」，
并明确「就是 WorkBuddy 里面内置的 14 个模型」。

**先查证，未靠猜**（穷举了所有可能的"14 个内置模型"来源）：

| 候选来源 | 实际数量 | 结论 |
| --- | --- | --- |
| WorkBuddy 网关的渠道（provider） | **14** | ✅ WorkBuddy 语境下**唯一**稳定等于 14 的集合 |
| 那 14 个渠道的模型总数 | 591 | ✗ 用户 0.1.81 刚要求移除 |
| 客户端产品配置 `~/.workbuddy/cache/acc-product-config-v3.json` | 76 | ✗ |
| └ 其中无 url（真·走 WorkBuddy 订阅） | 51 | ✗ |
| └ 其中带 url（自定义接入） | 25 | ✗ |
| `/workbuddy-native/v1/models`（ACP 内置会话通道） | 1 | ✗ |
| `~/.workbuddy/models.json` 及其 19 个历史备份 | 4~26，无 14 | ✗ |

⇒ 不存在「14 个内置模型」这个集合；用户要的 14 = **14 个渠道**。
而「591 太多」与「要 14 个」两个诉求的交集 = **每个渠道各取 1 个代表**。

**改法**
- `workbuddy_proxy.rs` 新增 `channel_pick_models_payload()`：遍历 `models_catalog()` 的渠道，
  每渠道按「① 免费 ② 快速档 ③ 名称」取 1 个代表，拼成 `provider/model`。
- 免费判定**复用 `opencode::is_free_model`**（与 OpenCode 网关、前端 `model-taxonomy.ts` 同口径），
  不另写一套。新增单测覆盖 `pick_score` 三档与全序。
- `/workbuddy/v1/models` 与总览 `modelCount` 改用本函数；
  **`/workbuddy-native/v1/models` 仍走 `builtin_models_payload()`**（严格内置，真正走订阅的那一个）。
  两者分工写进注释，避免后人混淆。
- 上游不可达/目录为空时回落 `builtin_models_payload()`，保证目录永不为空。

🔴 与 0.1.81 相同的红线：`workbuddy_proxy::{models_payload,models_catalog}`、
`/api/workbuddy-proxy/models`、`/api/workbuddy-proxy/status` **一律未动**（OpenCode 页/网关依赖）。

**验证**：`cargo check --release --all-targets` 0 错误、`tsc --noEmit` 0；
部署后 `/workbuddy/v1/models` = 14 条且每渠道恰好 1 个；总览 workbuddy modelCount = 14；
`/opencode/v1/models` 仍 43、`/api/workbuddy-proxy/models` 仍 14 渠道/591（未受影响）。

⚠️ 若用户实际指的是**客户端内置 51 个**或**591 全量**，改一行取数函数即可切换。

---

## 0.1.81 — 2026-09-23

### 变更：WorkBuddy 网关模型目录 591 → 内置模型（用户要求「只保留 WorkBuddy 原本内置的模型」）

**动机**：WorkBuddy 页的「模型目录」显示 **591 个模型条目 / 14 个渠道**
（openrouter 375、orcarouter 122、unorouter 23、stepfun 系、xiaomi 系、agentrouter…）。
按 `docs/gateway-split-20260922.md` 的设计边界，**WorkBuddy「不拥有 provider 清单」**——
那整份清单归「OpenCode 网关」页呈现；WorkBuddy 自己只有**内置模型**
（桌面端内嵌会话自带，走 WorkBuddy 订阅 · 经 ACP 通道转发）。

**🔴 关键约束（改动前必须知道，否则会误伤 OpenCode）**
`workbuddy_proxy::models_payload()` 与 `models_catalog()` **正被 `opencode.rs` 复用**
（`opencode::models_payload()` 取全量再过滤免费、`opencode::counts()` 统计渠道/免费/总数），
且 `/api/workbuddy-proxy/models` 是 **OpencodePage 的数据源**。
⇒ **这两个函数的语义一律不许动**，WorkBuddy 的需求另起一条路径。

**改法**

| 位置 | 改动 |
| --- | --- |
| `workbuddy_proxy.rs` | 新增 `builtin_models_payload()`：单一来源 `trae_acp::builtin_model()`，回落 `deepseek-v4.1-flash`（与 native 通道同值） |
| `api.rs::api_workbuddy_v1_models` | `/workbuddy/v1/models` 改用 `builtin_models_payload()`（591 → 内置模型数） |
| `api.rs::api_workbuddy_native_models` | 复用同一函数，消除「目录」与「ACP 通道」的重复定义 |
| `gateway_overview.rs` | `fetch_models("workbuddy")` 改用同一函数，总览/侧栏 modelCount 不再显示 591 |
| `WorkbuddyPage.tsx` | 移除「② 速度优先」「③ 模型目录」两个区块（−132 行）及其全部派生计算；不再拉 `/api/workbuddy-proxy/models`；新增内置模型说明与「→ OpenCode 页」跳转；Hermes 样例默认模型改为内置模型 |

**刻意保留（避免静默打断在用链路）**
- `/workbuddy/v1/chat/completions` **保持宽松**：仍可走文档写明的 OpenCode 兜底链路，
  只是不再「主动宣传」那 591 个模型。若日后要彻底只认内置模型，此为一行开关，需用户确认。
- `/api/workbuddy-proxy/models` 与 `workbuddy_proxy::{models_payload,models_catalog}` **原样不动**
  （OpenCode 页/网关依赖）。

**验证**：`cargo check --release --all-targets` 0 错误、`tsc --noEmit` 退出码 0；
部署后实测 `/workbuddy/v1/models` = 内置模型、总览 modelCount 由 591 变为内置模型数、
`/opencode/v1/models` 仍为 43（免费）、OpenCode 页数据不受影响。

---

## 0.1.80 — 2026-09-23

### 修复：OpenCode 网关 401「无效的 API Key」（WorkBuddy 接入失败）+ 鉴权诊断插桩

**现象**（用户报）：WorkBuddy 调 `POST /opencode/v1/chat/completions`（模型 `opencode/mimo-v2.6-flash-free`）
→ **401 无效的 API Key**，耗时仅 **28ms**（= 网关本地鉴权就拒了，未到上游）。

**排查（逐步排除，未靠猜）**

1. **密钥一致性**：`opencode_proxy_config.json` / `/api/opencode-proxy/config` / `models.json` 条目
   三处均为 `fec96d82…`，**完全一致**；直接用该 key 请求 → **200**。⇒ 网关与配置都没问题。
2. **条目字段结构**：与可用的「本地 GLM-5.3」条目逐字段对比，**无缺失字段**。
3. **密钥是否真被发出**：实测 `/v1/models`（旧条目用的端点）**确实校验密钥**
   （正确 key→200、不带→401、乱填→401）⇒ WorkBuddy **会发**密钥 → 排除「没发密钥」。
4. **密钥存储位置**：`~/.workbuddy` 下**只有 `models.json` 存密钥**（无独立密钥库）
   ⇒ 排除「从别处取值」。
5. ⇒ 结论：**WorkBuddy 发出的不是条目里的那个 key**。于是改为**插桩观测**，而非继续猜。

**插桩**：401 时输出「收到的鉴权头长什么样」的诊断串（**只记前缀与长度，不落全量密钥**）：

```
无效的 API Key ｜ 期望=fec96d…(len=32) 实收 Authorization=xxxx…(len=N) x-api-key=<无> UA=… 头列表=[…]
```

同时覆盖 `Authorization` 与 `x-api-key` 两种形态，并列出全部头名 —— 便于识别客户端是否用别的头名传密钥。

**修复**：`opencode_proxy_auth_result()` 改为**三态**：

| 返回值 | 含义 |
| --- | --- |
| `Some(true)` | 本网关**自有密钥** |
| `Some(false)` | **兼容密钥**（= `glm_proxy_config.json` 的 key），并记一条**显式日志**（不静默放行） |
| `None` | 不通过 → 401 + 诊断串 |

空密钥仍按传统放行（与其它网关一致）。

**背景（为什么需要兼容）**：`~/.wb-switch` 下并存**三套互不相同**的密钥 ——

| 文件 | 密钥 |
| --- | --- |
| `glm_proxy_config.json` | `07702999…` (32) ← **WorkBuddy 对本地条目实际发的就是这个** |
| `gateway_api_keys.json` | `gw_live_…` (40) |
| `opencode_proxy_config.json` | `fec96d82…` (32，本网关自有) |

实测：用 `07702999…` 请求 OpenCode 网关 → 兼容通道放行 → **200**（正文 `COMPAT2`）。

**验证**

- `cargo check -p wb-switch-server` **0 错误**。
- 鉴权矩阵：自有密钥 → **200**；主网关密钥（兼容通道）→ **200**；乱填 / 不带 → **401**（并落诊断日志）。

---

## 0.1.79 — 2026-09-23

### 修复：模型测试的 `tokens 0+0=0` 误导性显示

**现场**：0.1.78 上线后用户反馈结果卡片里显示 `tokens 0+0=0`，
看着像计量功能坏了。实测复核确认根因在**上游**：GLM 通路的响应
`usage` 三项**恒为 0**（`{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}`），
而当时是「如实透传」成 UI 文案。

**为什么这不是"如实显示"而是缺陷**：上游给了 usage 对象、但**没给有效计量** ——
正确语义是「未上报」，而不是「消耗了 0 个 token」。渲染成 `0+0=0` 会让用户
去排查一个不存在的计量故障（且 prompt 至少含系统提示，真为 0 在物理上也不可能）。

**修复**（`model_probe.rs::extract_usage`）：三项**全零**时返回 `Null`（= 未上报），
前端既有 `result.tokens?.total != null` 判断会自动不渲染该字段 —— **前端零改动**。
只要**有一项非零**就原样保留，不抹掉部分上报的真实数据。

**验证**：新增单测 `extract_usage_treats_all_zero_as_unreported`（全零 → Null；
`{prompt:0, completion:7}` → 保留且 total=7）；端到端复核 GLM 结果卡不再出现 tokens 行
（`tools/model_test_e2e.py` 全部用例符合预期 + `tools/model_test_ui_flow.mjs` 点击级流程通过）。

**🔎 顺带查清的重要事实：非流式请求的 `usage` 恒为 0 —— 这是设计使然，不是故障**
复核了 4 个网关的原始响应，非流式下**全部**返回
`usage: {"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}`
（`/v1`、`/kimi/v1`、`/qwen/v1`、`/trae/v1` 实测一致）。
原因：本项目的用量计量写在**流式泵**里（`glm_proxy::pump_stream` 的 usage 计量），
非流式路径不计数 —— 而所有真实客户端（ZCode / WorkBuddy）都用流式，所以不影响任何真实用量统计。
⇒ 结论：`model_probe` 走非流式时 `tokens` 字段**实际上永远是 null**（本次修复后不再显示）。
若日后想在自测里看到真实 token 数，需把探测改成流式并解析 SSE（注意 UTF-8 半字符 chunk 那条坑，
见 0.1.43）。**当前不做**：tokens 本来就不在需求范围内，可用性结论不受影响。

---

## 0.1.78 — 2026-09-22

### 修复：模型测试会把「不存在的模型」误报为 ✅（验证阶段发现的结论正确性缺陷）

**现场**：0.1.77 部署后跑 `tools/model_test_e2e.py`，用例「模型不存在」**没通过** ——
请求 `model: "__not_a_real_model__"` 返回 `ok=true`。

**根因**（网关行为，非本功能编码错误）：本机 GLM 网关对**任意**模型名都会**兜底路由**。
直接验证：

```
POST /v1/chat/completions  {"model":"__definitely_not_a_model__", ...}
→ HTTP 200，正文「可用」，响应里的 model 字段**回显请求值**（不是真实上游档位）
```

⇒ 所以「调通」**不等于**「该模型存在」。一个会对着随便什么字符串报 ✅ 的可用性测试，
是**结论错误**，比没有这个功能更糟（用户会据此以为某个模型能用）。

**修复**（前端 `ModelTestPage.tsx`）

- 新增 `TestRecord = ModelTestResult & { modelKnown: boolean | null }` —— UI 侧判断，
  **不进 API 契约**：判定「模型是否在网关模型目录里」，不在目录中时在结果卡上明确警示
  「网关可能按兜底路由处理，此结论不代表该模型本身可用」。
- **判定放前端而非后端**：目录本来就已经加载在内存里（下拉列表就是它），
  放后端会给每次探测多加一次目录请求 —— `glm_proxy::models_payload()` 是本地静态表，
  但 `kimi`/`loomy::models_payload()` 是**异步**的，会把探测延迟拉长。
- 目录快照在**探测时点**取值（`catalogRef`），避免用户事后切换网关改写历史结论。
- 网关未列任何模型时判定为 `null`（未知）而非 `false`，不误报警告。

**E2E 脚本同步**（`tools/model_test_e2e.py`）

- 移除「模型不存在」这条**错误期望**的用例（它断言的是功能不该有的行为）。
- 改为独立的「**行为锁定：未知模型名**」段落 —— 只记录事实、不作失败判据，
  把「网关会兜底」这一行为固化在回归脚本里，防止日后有人误以为这是功能缺陷。

---

## 0.1.77 — 2026-09-22

### 新增：模型可用性测试（在 Sylvia Switch 内直接验证任意网关的任意模型能不能用）

**动机**（用户需求）：以前要确认「某个模型的额度/登录态到底还行不行」，
只能跑去 ZCode / WorkBuddy 里把模型配一遍再试 —— 配错、走错网关、看不出真因，
排查成本高。现在在 Sylvia Switch 里选网关 + 模型，一次点击就得到结论与失败原因。

**新增后端模块** `crates/wb-switch-core/src/modules/model_probe.rs`

- **回环自调用**：请求打到本服务**自己的对外端点**
  `http://127.0.0.1:{boundPort}{base}/chat/completions`，
  因此覆盖「鉴权 → 路由 → 账号选择 → 上游 → 协议转换」**完整链路** ——
  失败原因与真实客户端所见一致；若改成内部直连模块函数，就测不出鉴权/路由层的问题。
- 🔴 **鉴权用 GLM 主密钥**（`glm_proxy::load_config().api_key`）：
  `gateway_admin::verify_request_key` 允许「网关专用 Key」或「主密钥」二选一，
  而主密钥命中时**提前 return、不计入 Key 的 `requestCount`/`lastUsedAt`** ——
  用专用 Key 会让每次自测都给密钥计数 +1，污染管理台统计。
- 🔴 **必须 `no_proxy()`**：本机回环不得继承系统代理。本机 `HTTP_PROXY` 残留会把
  `127.0.0.1` 也送去代理，直接表现成「连不上网关」（与 glm_web/kimi/loomy 既有做法一致）。
- 非流式 + 小 `max_tokens`（默认 64）：一次请求即得确定结论，且避开流式路径历史上
  踩过的「UTF-8 半字符 chunk 整块丢弃」坑（见 0.1.43）。
- **失败归因**（`code` 机器可读 + `reason` 人可读）：
  `unknown_gateway` / `missing_model` / `no_master_key` / `client_build_failed` /
  `connect_failed` / `timeout` / `network_error` / `auth_failed`(401·403) /
  `model_not_found`(404) / `rate_limited`(429) / `upstream_error` /
  `empty_reply`(200 但无正文无思维链) / `bad_response`(200 但非 JSON)。
- 响应解析**双协议兼容**：OpenAI（`choices[].message.content` / `reasoning_content`）
  与 Anthropic（`content[]` 的 `text` / `thinking`），usage 同时认
  `prompt_tokens`/`completion_tokens` 与 `input_tokens`/`output_tokens`。
- 只有思维链、无可见正文时**判为成功但如实标注**（`glm-5.3-flash` 等会先吐大段 thinking），
  避免把「模型可用」误报成「空回复」。

**新增路由**（`api.rs`）

- `POST /api/model-test` → `api_model_test`，**恒返回 200 + 结果对象**：
  「测不通」本身是有效结论，用 `ok`/`code`/`reason` 表达。
  若改用 HTTP 状态码表达业务失败，前端就得把「业务失败」和「接口挂了」混在一起判断。
- `gateway_overview::gateway_base(id)` 新增公开访问器（展示名 + 对外 base），
  供本模块复用，避免各处再抄一份网关路由表导致漂移。

**新增前端** `src/pages/ModelTestPage.tsx`（路由 `/model-test`，侧栏「模型测试」）

- 网关下拉（复用既有 `/api/gateways/overview`，**未新增接口**）+ 模型下拉
  （含「✏️ 手动输入模型 ID」以便测未列出的模型）+ 自定义提示词 + `max_tokens`。
- 「测试该模型」/「测试该网关全部模型」/「停止」；**串行**执行（并发会把「限流」和
  「真的不可用」混在一起）；结果按时间倒序累积，同一模型可反复测。
- 每条结果卡片：✅/❌ 状态徽章 + 归因标签、网关·模型、耗时、HTTP 状态、tokens、
  实际端点、可展开的上游原始响应（截断）；顶部给出「共 N 次 · 成功 / 失败 / 平均耗时」。
- 契约类型独立成 `src/lib/model-test-types.ts`（同 `channel-health-types.ts` 先例），
  含 `MODEL_TEST_CODE_META` 表驱动文案与配色。
- 与 `channel_health` 一致：**webui 独占**，桌面端（Tauri）显示提示卡片，不动 `src-tauri`。

**验证**：见本次部署实测（网关/模型矩阵、失败归因样例、重复测试）。

---

## 0.1.76 — 2026-09-22

### 新增：OpenCode 网关落地（真实可用的接入信息 + 免费模型专用端点）

**动机**（用户反馈）：OpenCode 页的「接入信息 · OpenCode 网关」之前标着 **待建**，
只有 Base URL 占位与「待建」徽标 —— **用户拿不到密钥，无法真正调用里面的模型**。

**新增后端模块** `crates/wb-switch-core/src/modules/opencode.rs`

- 独立配置 `~/.wb-switch/opencode_proxy_config.json`（`enabled` + 独立 `api_key`，
  **首次访问自动生成密钥并落盘**，用户无需手填）。
- `is_free_model()` 与前端 `model-taxonomy.ts::isFreeModelId` **同口径**
  （含 `free` / 以 `:0` 或 `/0` 结尾 / 含 `zero`），保证「页面显示」与「网关放行」不漂移。
- `models_payload()`：只列免费模型。
- `chat_completions_json()` / `chat_completions_stream()`：
  **非免费模型一律拒绝**（提示改用 `/opencode/v1/models`）；**未指定 `model` 时自动选第一个免费模型**；
  对话委托 `workbuddy_proxy` 的 server 通道（**共用同一个本机 OpenCode server 进程**，
  不重复拉起第二个 server 抢端口）；流式为聚合后合成 SSE（含末尾 usage 帧）。
- `proxy_status()`（server 健康 + 渠道数 + 免费/总数）、`config_info(port)`（Base URL / 密钥 / 启用态）。

**新增路由**（`api.rs`）

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/opencode/v1/models` | 只列免费模型（需本网关密钥） |
| POST | `/opencode/v1/chat/completions` | 对话；非免费模型 → **400**（而非 502），server 故障 → 502 |
| GET | `/api/opencode-proxy/status` | 网关状态 |
| GET/POST | `/api/opencode-proxy/config` | 接入信息 / 启用开关 / 轮换密钥 |

- 新增 `opencode_proxy_auth_ok()`（Bearer 或 `x-api-key`）；
- `opencode_error_status()`：**区分「客户端选错模型」(400) 与「上游故障」(502)**，便于调用方判断该改模型还是该重试；
- `is_logged_path` 补 `"/opencode/"`；`is_api_namespace` 的 `WITH_SUB` 补 `"opencode"`（14 → 15）
  —— ⚠️ 该条只兜 `/opencode/<子路径>`，`/opencode`（前端页面，无尾斜杠）不受影响。

**注册表**：`gateway_overview` 增 `opencode`（base `/opencode/v1`）+ `fetch_models` 臂（即免费数）。

**前端**（`api.ts` + `OpencodePage.tsx`）

- `api.ts`：`ROUTES` 增三条 + `getOpencodeProxyStatus` / `getOpencodeProxyConfig` / `saveOpencodeProxyConfig`。
- 页头状态区改为读**真实状态**（渠道数 / 对外暴露免费数 / server 端口 / 启用开关）；
- **接入信息区从「待建」改为真实值**：Base URL 可复制、密钥可复制、**一键重新生成密钥**、
  可用模型数、上游健康检查、以及**可直接粘贴运行的 curl 示例**（自动填入当前端点与密钥）。

**验证方式**

- `cargo check -p wb-switch-core -p wb-switch-server` **0 错误**；`tsc --noEmit` **0 错误**。
- 部署后应满足：`GET /opencode/v1/models` → 200 且**只含免费模型**；
  `POST /opencode/v1/chat/completions` 用免费模型 → 200 正文；
  用**非免费**模型 → **400** 且提示清单端点；错误密钥 → 401；`/opencode/v1/nope` → 404 JSON。

---

## 0.1.75 — 2026-09-22

### 重构：分类口径抽成共享模块 + WorkBuddy「速度优先」改为每渠道首选

**① 口径抽共享**（`src/lib/model-taxonomy.ts`，新增）

把 `isFreeModelId` / `classifyTask` / `TASK_META` / `TASK_ORDER` / `TaskKind` 从 `OpencodePage`
抽到共享模块，并新增 `speedScore()` / `groupByChannel()` / `stripChannel()`。
⇒ WorkBuddy 与 OpenCode 两页的「免费判定 / 任务场景 / 排序」**从此完全一致**，不会再各自漂移。

**② WorkBuddy 页「速度优先推荐」重构**（`WorkbuddyPage.tsx`）

原先是对 591 个模型按名称特征排序后**只取前 8 个平铺成芯片** —— 既看不出归属渠道，
也没有场景信息，且丢掉了另外 583 个模型的可读性。

现改为：

- **每渠道各取首选**（14 行）：`渠道 | 免费 x/total | 场景标签 | 首选模型 | 备选`
  排序依据 `speedScore()`：**① 免费优先 ② 快速档 ③ 场景序 → 名称**；
  自有 `opencode` 渠道置顶并标「自有 · 直连本机」。
- **场景分布汇总条**：把「591 个模型」按任务场景展开（通用 300 / 推理 136 / 快速 129 / 视觉 12 / 代码 12 / 安全 2），
  让大数字可读化。
- 场景标签 hover 显示适用任务说明（与 OpenCode 页同一份 `TASK_META`）。

**实测（591 模型 / 14 渠道）**

| 渠道 | 免费 | 首选 |
| --- | --- | --- |
| `opencode`（自有） | 6/7 | `ling-3.0-flash-fin-free` |
| `openrouter` | 22/375 | `inclusionai/ling-3.0-flash-fin:free` |
| `orcarouter` | 4/122 | `deepseek/deepseek-v4-flash-free` |
| `unorouter` | 11/23 | `deepseek-v4-flash:free` |
| `stepfun*` / `xiaomi*` / `deepseek` / `agentrouter` 等 10 个 | 0 | 各自 flash 档（`step-3.5-flash`、`mimo-v2.6-flash`、`deepseek-v4-flash`…） |

**验证方式**

- `tsc --noEmit` **0 错误**（期间修掉一处：抽取后 `OpencodePage` 漏导入 `type TaskKind`）。
- 抽取脚本用起止标记切片 + 双向断言（本地定义残留 = 0、引用数 > 0），一次通过。
- 每渠道首选先用 Python 对**线上真实 591 个模型**做等价模拟复验，确认 4 个有免费的渠道
  首选均为「免费 + 快速档」，其余渠道选到各自 flash 档，再落 TS。
- 前端改动需重编 exe（`rust-embed` 在编译期嵌入 `dist/`），故 vite + cargo 双构建。

---

## 0.1.74 — 2026-09-22

### 新增：OpenCode 网关免费模型「按渠道分组 + 按任务场景分类排序」

**动机**（用户反馈）：43 个免费模型原先是**一堵平铺的芯片墙**，既没有分类也没有排序，
无法判断「哪个模型干什么用」——这正是需求第三项「明确每个模型适用的任务场景」要解决的问题。

**改动**（`src/pages/OpencodePage.tsx`）

- 新增 `classifyTask()` 与 `TASK_META`：把模型按**任务场景**分为 6 类
  （**快速** / **代码** / **视觉** / **推理** / **通用** / **安全**），每类带一句适用任务说明（悬停可见）。
- 新增 `freeByChannel`：**先按渠道分组**（自有 `opencode` 渠道优先，其余按免费数降序），
  **组内按任务场景排序**（快速 → 代码 → 视觉 → 推理 → 通用 → 安全），同类按名称。
- 区块 UI 改为：顶部「按任务场景」汇总条（带计数）→ 下方按渠道分组的卡片，
  每行 = 场景标签 + 模型名（已剥掉渠道前缀，hover 显示完整 id）。

**🔴 实测踩到并修正的分类陷阱**

`classifyTask` 最初对**完整 id**（`provider/model`）做匹配，而渠道名 **`opencode` 自带 "code"**，
导致该渠道下**全部 6 个模型**被误判为「代码专用」（含 `nemotron-3-ultra`、`mimo-v2.6-flash`）。
修正：**只对渠道之后的部分判定**。同时收紧了两个过宽规则：
`/thinking/` 会误命中厂商名 `thinkingmachines` → 改为 `\breasoning\b|-reasoning|[-/]thinking`；
规模档从 `\d{2,3}b` 收紧为 `\d{3,4}b`（否则 `gemma-4-31b` 会被当成推理模型）。

**分类结果（实测 43 个免费模型）**

| 场景 | 数量 | 代表 |
| --- | --- | --- |
| 快速 | 14 | `ling-3.0-flash-fin`、`nemotron-3.5-lightning`、`deepseek-v4-flash`、`glm-4.5-flash`、`inkling-small` |
| 代码 | 1 | `cohere/north-mini-code` |
| 视觉 | 1 | `inclusionai/ling-3.0-flash-vl` |
| 推理 | 13 | `nemotron-3-ultra-550b`、`gpt-5.5`、`glm-5.2`、`deepseek-v4-pro`、`qwen3.5-397b` |
| 通用 | 13 | `gemma-4-31b-it`、`minimax-m2.7`、`laguna-s-2.1`、`inkling` |
| 安全 | 1 | `nemotron-3.5-content-safety` |

渠道分布：`opencode` 6（自有·直连本机）、`openrouter` 22、`unorouter` 11、`orcarouter` 4。

**验证方式**

- `tsc --noEmit` **0 错误**。
- 分类规则先用 Python 对**线上真实 43 个免费模型**做了等价模拟复验（修正前后各一次，
  确认 `opencode` 组不再被误判为代码类），再落 TS 代码。
- 部署后 OpenCode 页 `/opencode` 的「免费模型」区块应呈现 4 张渠道卡片 + 顶部场景汇总。

---

## 0.1.73 — 2026-09-22

### 修复：WorkBuddy `modelCount = 594` 的口径误读 + 自引用 provider 过滤

**背景**（用户质疑）：总览把 WorkBuddy 的模型数报成 **594**，与实际可用规模（预期 14）严重不符。

**逐步核对结论（全部实测）**

`modelCount` 取自 `gateway_overview::gateway_snapshot` 的 `simplified.len()`，
而 `simplified` 来自 `workbuddy_proxy::models_payload()` —— 后者把
**「每个已连接 provider × 该 provider 自身的 models」拍平成一份 `provider/model` 列表**：

```
594 = Σ(各 connected provider 的模型数)
    = openrouter 375 + orcarouter 122 + unorouter 23
      + stepfun-ai 9 + stepfun 8 + xiaomi-token-plan-{ams,cn,sgp} 各 8
      + opencode 7 + xiaomi 6 + agentrouter 5 + stepfun-step-plan 5
      + deepseek 4 + stepfun-ai-step-plan 3 + sylvia-qwen 3
```

**三层偏差原因**

1. **范围界定过宽**：`openrouter`(375) + `orcarouter`(122) 两家聚合型目录就占了 **594 的 84%** ——
   它们是"可路由到的第三方全量目录"，不等于"本机实际可用的模型"。
2. **跨 provider 重复计数 129 条**：同一模型被多渠道提供会各计一次
   （`step-3.x-flash` × 4 家 stepfun 变体、`mimo-v2.x` × 4 家 xiaomi 变体、`deepseek-v4-flash` × 3 家）。
   去重后独立模型名 = **465**（594 − 129）。
3. 🔴 **自引用回环（真隐患，非单纯计数问题）**：connected 里有
   `sylvia-qwen → http://127.0.0.1:57890/qwen/v1`，即**网关把自己的 `/qwen/v1` 当成上游 provider**。
   既虚增目录，又存在**请求环**风险（WorkBuddy 借道 OpenCode → 命中自身路由 → 若再回落 OpenCode 即成环）。

**各口径对照（确认正确值）**

| 口径 | 值 | 说明 |
| --- | --- | --- |
| **真实上游渠道数** | **14** | **= 用户预期的数字**（15 个 connected 减去 1 个自引用回环） |
| connected provider 数 | 15 | 含自引用 |
| 拍平 `provider/model` 条目（原 modelCount） | 594 | 每个都可寻址，但被聚合目录 + 跨家重复放大 |
| 去重独立模型名 | 465 | 排除跨 provider 同名 |
| 排除回环 + 去重 | 462 | 修复后的口径 |
| 命中免费特征（`free`/`:0`） | 43 | 第三项「仅保留免费」的现有口径 |

**改动**

- `workbuddy_proxy::models_catalog()`：**过滤 baseURL 指向本机回环的 provider**
  （新增 `is_loopback_url()` 辅助函数），消除自引用造成的虚增与潜在请求环。
- `gateway_overview::gateway_snapshot()`：新增 **`providerCount`** 字段
  （按 `owned_by` 去重的渠道数，通用适用于所有网关），向后兼容。
- 前端：WorkBuddy 页与 OpenCode 页头的口径改为
  **「N 个渠道 / M 个模型条目」**，并在模型目录旁注明
  「条目数 = Σ(各渠道模型数)，同一模型被多渠道提供会各计一次」。

**验证方式**

- `tsc --noEmit` **0 错误**；`cargo check -p wb-switch-core -p wb-switch-server` **0 错误**。
- 部署后：`/api/workbuddy-proxy/models` 的 provider 数应为 **14**（`sylvia-qwen` 消失）；
  `/api/gateways/overview` 中 workbuddy 的 `providerCount=14`、`modelCount=591`。
- 其余网关的 `modelCount` 与行为不受影响（仅多一个只读字段）。

---

## 0.1.72 — 2026-09-22

### 重构（增量 1/4）：TRAE / WorkBuddy / OpenCode 三网关拆分

原先把 TRAE 与 WorkBuddy 塞在一个页面（`TraeWorkbuddyProxyPage`，6 个 section）。本版按
**上游来源**拆成三个相互独立的网关，各自有独立导航项、独立页面、独立密钥与职责边界。

**模块划分（前端）**

| 旧 | 新 | 页面 | 导航 label |
| --- | --- | --- | --- |
| `TraeWorkbuddyProxyPage`（合并页，**已删除**） | `TraeWorkCnPage` | `/trae-work-cn` | TRAE Work CN |
| 〃 | `WorkbuddyPage` | `/workbuddy` | WorkBuddy |
| —（原先只是 WorkBuddy 的传输层） | `OpencodePage`（**新建**） | `/opencode` | OpenCode |

- `gateway-registry.ts`：`trae-workbuddy` 一条 → `trae-work-cn` / `workbuddy` / `opencode` 三条
  （图标改用 `Blocks` / `Cpu` / `Terminal`）。
- `App.tsx`：三条新路由 + **旧路径 `/trae-workbuddy` 重定向到 `/trae-work-cn`**（防书签 404）。

**模块划分（后端注册表）** —— `gateway_overview.rs`

- `GATEWAYS` 增 `trae-work-cn`（base `/trae/v1`）与 `workbuddy`（base `/workbuddy/v1`）。
- `fetch_models` 增两臂：`trae-work-cn → trae_proxy::models_payload()`、
  `workbuddy → workbuddy_proxy::models_payload()`（Result，失败回落空表）。
- `fetch_accounts` 增两臂：`trae-work-cn → trae_accounts::list_for_display()`；
  `workbuddy → 空表`（**它没有自有账号池**——其"账号"= 借道 OpenCode server 的已鉴权 provider 清单，
  保活对象是 server 进程本身）。
- ⚠️ OpenCode 的注册表条目留待增量 3（独立模块未建，不提前登记以免对外宣称不存在的 base）。

**第一项的「账号迁移」——实况澄清与落地口径**

- 🔴 **TRAE 账号数据本来就不在 GLM 代理里**：`glm_proxy.rs` 提及 `trae` = **0 行**；
  账号独立存于 `~/.wb-switch/trae_accounts.json`，由 `trae_accounts.rs` 与 `/api/trae/*` 管辖。
- 真正寄居在 GLM 页的是**前端 UI**：`GlmProxyPage.tsx` 曾有约 30 处 TRAE 代码（含 `TRAE Work CN 账号池` 区块），
  且同一账号池**同时**被合并页渲染（重复）。故本次「迁移」= **只搬 UI**：
  - `GlmProxyPage.tsx` 移除 TRAE 区块（JSX 83 行 + 5 个 `trae*` state + 5 个 trae 函数 + useEffect 调用 + 2 个类型导入）；
  - TRAE 账号池 UI 迁入 `TraeWorkCnPage`。
- **迁移后账号归属**：`trae_accounts.json` 与 `/api/trae/*` **一字未改**，账号始终属于 **TRAE Work CN 网关**；
  迁移只改变"在哪里被看到"，不改变"归谁所有"。GLM 代理池（`autoclaw_accounts.json`）与 TRAE 池
  **本来就是两套文件、两套凭据**，不存在跨池数据搬迁。

**本次未做（后续增量）**

- 增量 2：TRAE 六项能力深化（rate 驱动的「速度优先推荐」「场景选型参考」已落页面；保护/扣分策略深化待接）。
- 增量 3：OpenCode 独立模块 + `/opencode/v1/*` 端点 + 免费白名单（当前页为"清单 + 状态"，已标注端点待建）。
- 增量 4：WorkBuddy **直连优先 + OpenCode 兜底**（直连协议待探测；页面已把通道态显式标为
  `opencode-fallback`，并预留 `channel === "direct"` 分支）。

**验证方式**

- `tsc --noEmit` **0 错误**；`vite build` 成功（19.9s，dist 已刷新）。
- `cargo check -p wb-switch-core -p wb-switch-server` **0 错误**。
- 残留检查：`GlmProxyPage.tsx` 提及 trae = **0 处**；旧合并页文件已删除。
- 部署后：`/api/gateways/overview` 的 ids 应含 `trae-work-cn` 与 `workbuddy`。

---

## 0.1.71 — 2026-09-20

### 修复：下线路由的残留路径被 SPA 兜底伪装成 200 HTML（0.1.70 的收尾）

**现象**（0.1.70 部署后实测）：`/qwen3.8-flash/v1/models` 与 `.../chat/completions` 返回
**HTTP 200 + `text/html`（index.html）** —— 比原来的 502 更糟：客户端会当成"成功"并解析到垃圾。

**根因**：路由删掉后该路径成为未知路径，落进 `static_handler` 的 SPA 兜底。
兜底前的守卫 `is_api_namespace()` 的 `WITH_SUB` 白名单里**只有 `qwen`**，
而它要求「前缀后紧跟 `/`」——`/qwen3.8-flash/...` 的第 5 个字符是 `3` 而非 `/`，
**`qwen` 覆盖不到 `qwen3.8-flash`**（与 `qwen` 覆盖不到 `qwen-flash` 是同一个坑）。

**修复**：`is_api_namespace()` 的 `WITH_SUB` 增加 `"qwen3.8-flash"`（数组 7 → 8），
并补注释说明「该路由已于 0.1.70 下线，登记是为了让残留路径返回 404 而非 200 HTML」。

**验证方式**：部署后 `/qwen3.8-flash/v1/models`、`/qwen3.8-flash/v1/chat/completions`
应返回 **404 JSON**；`/qwen-flash/v1/*`、`/qwen/v1/*`、`/qoder/v1/*` 等**不受影响**（仍在白名单内）。

---

## 0.1.70 — 2026-09-20

### 回退：下线 `/qwen3.8-flash/v1` 路由（协议不可行，避免"永远 502"的入口）

承接 0.1.69 —— 该路由经实测**确认协议层面不可行**（QoderWork CN 不暴露 OpenAI 兼容推理端点，
其 device token 对 `api2-v2.qoder.sh` 推理端点返回 **401 unauthorized**；详见 0.1.69 追加节）。
保留一个长期 502 的入口会误导客户端与后续排查，故**下线**。

**删除**

| 文件 | 删除内容 |
| --- | --- |
| `api.rs` | 路由 `POST /qwen3.8-flash/v1/chat/completions`、`GET /qwen3.8-flash/v1/models`；handler `api_qwen38_flash_models`、`api_qwen38_flash_chat_completions`；`is_logged_path` 前缀 `"/qwen3.8-flash/"` |
| `gateway_overview.rs` | `GATEWAYS` 的 `qwen3.8-flash` 条目；`fetch_models` / `fetch_accounts` 两条 match 臂 |
| `qoder.rs` | `routable_accounts_for` 的 `"qwen3.8-flash"` 分支；`pool_label` 条目；`qwen38_flash_chat_completions_unified()`、`qwen38_flash_models_payload()` |

**保留（有意）**

- `read_local_credential_work_cn()` / `import_local_work_cn()` / `qoderwork_cn_user_data_dir()` /
  `iso_to_ms()` / `QODERWORK_CN_ENDPOINT` —— **导入器与凭据读取器保留**，
  账号继续以"备份"形式存在（其 `gateway = "qwen3.8-flash"` 池标此时**无任何路由消费**，
  即不会再被任何池取到）。
- `POST /api/qoder/accounts/import-work-cn` 导入端点保留。
- **per-account `endpoint` 接线保留**（通用能力，向后兼容：无该字段的账号行为不变）。
- `resolve_qoder_model()` 与 `qoder::models_payload()` 里既有的 `qwen3.8-flash` **模型别名**保留
  （那是 `/qwen-flash`、`/qwen` 的档位映射，与本路由无关）。

**影响范围**：纯删除 + 无行为变更残留。账号池文件 `~/.wb-switch/qoder_accounts.json`
**未改动**（账号与新鲜凭据都在）。

**验证方式**

- `cargo check -p wb-switch-core -p wb-switch-server` **0 错误**（残留检查：`qwen38_flash`=0 处）。
- 部署后：`/qwen3.8-flash/v1/models` 与 `/chat/completions` 应返回 **404 JSON**（不再是 502）；
  `/api/gateways/overview` 不再含 `qwen3.8-flash`；
  `/qwen/v1`(7) / `/qwen-flash/v1`(1) / `/qoder/v1`(8) / `/glmweb/v1`(23) 无回归；
  `POST /api/qoder/accounts/import-work-cn` 仍可用。

---

## 0.1.69 — 2026-09-20

### 新增：`/qwen3.8-flash/v1` 路由 + per-account `endpoint` 接线（第 3 步）

承接 0.1.68（导入 + 池标），本版把它**接通**：账号导入后即可正常调用。

**改动**

- 🔴 `qoder.rs::try_chat_for_account`：**per-account `endpoint` 接线** ——
  上游主机优先取账号的 `endpoint` 字段，缺失时回落 `CHAT_HOST`（`.sh`）：
  ```rust
  let host = account.get("endpoint").and_then(Value::as_str)
      .filter(|s| !s.is_empty()).unwrap_or(CHAT_HOST);
  .post(format!("{host}/model/v1/chat/completions"))
  ```
  ⇒ **向后兼容**：无该字段的账号（默认池 / `qwen` / `qwen-flash`）行为**一字不变**。
- `qoder.rs` 新增 `qwen38_flash_chat_completions_unified()`（pin `qwen3.8-flash`，
  池 = `qwen3.8-flash`）与 `qwen38_flash_models_payload()`（单一模型）。
- `api.rs`：注册 `POST /qwen3.8-flash/v1/chat/completions`、`GET /qwen3.8-flash/v1/models`
  + 两个 handler（与 `/qwen-flash` 同结构：鉴权 → 日志 → `qoder_repaired_sse_body` 修复）；
  `is_logged_path` 前缀白名单补 **`"/qwen3.8-flash/"`**
  （⚠️ `/qwen/` 匹配不到 `/qwen3.8-flash/`，漏了就没有访问日志）。
- `gateway_overview.rs`：`GATEWAYS` 增 `qwen3.8-flash` 条目（名 `Qwen3.8-Flash (WCN)`，
  base `/qwen3.8-flash/v1`）；`fetch_models` / `fetch_accounts` 各加一条 match 臂
  （账号视图**只显示打 `gateway == "qwen3.8-flash"` 标的账号**，与其它池完全隔离）。

**分离效果（= 池标 + 独立游标）**

| 维度 | `qwen3.8-flash` 池 | 其它池 |
| --- | --- | --- |
| 池成员 | 仅 `gateway == "qwen3.8-flash"` + 本机 QoderWork CN 登录态 | 各自标的账号 |
| 轮转游标 | `next_pool_index("qwen3.8-flash")` **独立** | 各自独立 |
| 兜底池 | **无**（保持严格隔离，不借其它池额度） | `qwen-flash→qoder` 等 |
| 上游主机 | 账号自带 `endpoint`（`gateway.qoder.com.cn`） | `api2-v2.qoder.sh` |

**影响范围**：新增路由与注册表条目；唯一触及既有路径的是 `try_chat_for_account` 的
host 选择（默认值不变，故既有路由零行为变化）。

**验证方式**

- `cargo check -p wb-switch-core -p wb-switch-server` **0 错误**（构建前）。
- 部署后实测（2026-09-20 15:09，0.1.69 / 停机 1.0s）：
  - ✅ `GET /qwen3.8-flash/v1/models` → **1 个模型**（`qwen3.8-flash`）
  - ✅ `/api/gateways/overview` → ids 已含 `qwen3.8-flash`
  - ✅ 回归：`/qwen/v1`=7、`/qwen-flash/v1`=1、`/qoder/v1`=8、`/glmweb/v1`=23 全部 200
  - ✅ 账号池：导入账号带 `gateway="qwen3.8-flash"` / `endpoint=`CN 网关，**与其它池隔离**（分离生效）
  - ❌ **对话未通**：`POST /qwen3.8-flash/v1/chat/completions` → **502**
    `[qwen3.8-flash] Qoder refresh 失败（400 Bad Request）`

**🔴 未通过项根因（实测取证，留给下一步）**

1. 刷新走错端点：现有 `refresh_token()` 固定打 `.sh` 的 `/api/v1/jobToken/refresh`；
   而 **QoderWork CN 的刷新是 `POST /api/v1/deviceToken/refresh`**，body 同为
   `{"refresh_token": …}`，但响应字段是 **`device_token`**（代码见其 `app.asar`：
   `auth.refreshDeviceToken`）。故 CN 账号拿到 400。
2. **通话路径不是 `model/v1/chat/completions`**：在 `E:\QoderWork CN\resources\app.asar`
   （164 MB）全文检索 `model/v1` / `/completions`，**只匹配到
   `https://api.openai.com/v1/chat/completions`（自定义 endpoint 默认值）**，
   没有任何 CN 自身的 chat 路径；其 API 面是 `/api/v1/userinfo`、`/api/v1/deviceToken/refresh`、
   `/api/v1/jobToken/exchange`（PAT 换 token）、`/api/v1/qcs/*`、`/api/v1/remote*`、`/algo/api/v1/ping`。
   ⇒ **QoderWork CN 与 Qoder/QoderCN IDE 是两套不同的后端协议**，其模型调用很可能走
   `remote` / QCS 体系而非 OpenAI 兼容 chat 端点。当前实现（照搬 IDE 的 chat 路径）**先天不适用**。

**结论**：本版的「路由 + 池分离 + 端点接线」已就位且向后兼容；**QoderWork CN 的对话协议需要单独逆向**
（下一步工作，非本版可完成）。

### 追加：CN 对话协议逆向结论（2026-09-20 16:30–16:45，**判定为协议不可行**）

在 `E:\QoderWork CN\resources\app.asar`（164 MB）上做字节级逆向 + 端点实证，得到**主机常量表**：

```js
OPENAPI_DOMAIN      = "openapi.qoder.com.cn"    // CN openapi
IM_GATEWAY_DOMAIN   = "openapi.qoder.sh"        // IM 仍走 .sh
CENTER_API_DOMAIN   = "gateway.qoder.com.cn"
INFER_DOMAIN_US/SG/JP = "gateway.qoder.com.cn"  // 声明为推理域
GATEWAY_HEALTH_PATH = "/algo/api/v1/ping"
DEFAULT_MODEL_KEY   = "auto"
```

**端点实证（新 refresh 出的 dt-）**：

| 主机 + 路径 | 结果 |
| --- | --- |
| `openapi.qoder.com.cn/api/v1/userinfo` | ✅ **200** |
| `openapi.qoder.com.cn/api/v1/deviceToken/refresh` | ✅ **200** → `{device_token, refresh_token, expires_at}` |
| `gateway.qoder.com.cn/api/v1/deviceToken/refresh` | 503（ALB 无此路由） |
| `api2-v2.qoder.sh/model/v1/chat/completions`（IDE 的推理端点） | 🔴 **401 `{"error":"unauthorized"}`** —— **该 token 无推理权限** |
| `gateway/openapi.qoder.com.cn/model/v1/chat/completions` | 503（ALB 无此路由） |
| `/algo/api/v1/{models,chat/completions,completions,…}` | 404 JSON（路由不存在） |

- `app.asar` 全文：`api2-v2` 出现 **0** 次、`model/v1` 出现 **0** 次 → **CN 客户端根本不用该推理端点**；
  其 API 面为 `/api/v1/{userinfo,deviceToken/*,jobToken/exchange,qcs/*,remote/*,notebooks,graphql}`。
- ⇒ **QoderWork CN 是 agent 平台架构**（notebooks + remote environments），
  **不向客户端暴露 OpenAI 兼容的模型推理端点**；其 device token 仅供 CN openapi（账号/中心服务）使用。
  **结论：无法用它的账号做 qwen3.8-flash 反代**（协议层面不成立，非实现问题）。

**🔴 期间发现的正确 CN 刷新协议（已可用于账号续期）**

- `POST {openapi.qoder.com.cn}/api/v1/deviceToken/refresh`，body `{"refresh_token": "<drt-…>"}`；
  响应取 **`device_token`**（不是 `token`）+ 新 `refresh_token` + `expires_at`/`refresh_token_expires_at`。
- ⚠️ **副作用与修复**：该接口**轮换 refresh_token**，探测过程中旧 drt- 已失效 →
  已用新脚本 `tools/fixup_qoderwork_tokens.py` 把新凭据**加密写回 `auth-v2.dat`（含备份）**
  并同步到 `~/.wb-switch/qoder_accounts.json`，读回校验一致 + `userinfo` 200（**客户端登录态已恢复**）。

**环境提示**：本机所有 `*.qoder.*` 域名都解析到 **198.18.0.x（保留段）= fake-IP 代理**，
故部分 SSL EOF/503 可能是代理侧表现；判定结论只以「401 unauthorized」（真实上游响应）为准。

**新增脚本**：`tools/fixup_qoderwork_tokens.py`（刷新 + 回写 auth-v2.dat + 同步账号池，支持 `--dry-run`）。

### 🟡 侧栏导航溢出遮挡修复：底部主题卡片盖住「设置」等项，点击命中卡片「完全没反应」

**症状**（用户截图实测复现）：「模型网关」折叠组展开后，侧栏内容总高（实测 510px）超出导航区高度
（412px）——底部固定区（主题切换卡片 / 版本底栏）按层叠顺序绘制在**溢出的导航项之上**：
「设置」被完全盖住（看不见、点不到），最后一个网关项（TRAE/WB 模型）也会随窗口高度被部分压住；
点击该区域时命中卡片而非目标项 → 表现为「点击后完全没有反应」。

**根因**：`App.tsx` 的 `nav` 是 `flex-1 min-h-0` 但 **`overflow: visible`**，折叠组展开后内容
**向下溢出**到同为 flex 子项的底部固定区下方（后者绘制在后、盖住前者），形成
「看得见的区域 ≠ 可点的区域」错位。

**修复**（纯布局 + 光标恢复，不改任何交互语义）：
- `nav` 加 `overflow-y-auto overscroll-contain` → 超高内容**在导航区内部滚动**，不再溢出被盖；
- 底部固定区加 `shrink-0`，主题卡片与版本底栏不被压缩；
- `index.css` 恢复 `button:not(:disabled) / [role="button"]:not(:disabled) { cursor: pointer }`
  —— Tailwind v4 preflight 把 button 光标重置为默认箭头（v3 曾是手型），此前全站按钮悬停
  无「可点击」提示；
- `gateway-nav-group.tsx`：`useState` 改惰性初始化（消除每渲染一次的多余 localStorage 读）。

**影响范围**：折叠组「展开/收起、localStorage 记忆、网关页自动展开」等行为**语义零变化**；
仅有 `nav` 出现滚动条与按钮光标变化两个视觉层差异。

**验证方式**（CDP 驱动 Edge 同引擎同尺寸，本地 dist + 部署后线上双验）：
- 展开态：`nav.scrollHeight(510) > clientHeight(412)` 且 `overflow-y:auto`；「设置」可滚动至
  可见，`elementFromPoint` 命中自身（修复前命中主题卡片），真实鼠标点击 → 成功导航 `/settings`；
- 头部逐子区域点击（左缘/图标/文字/计数/箭头）× 高度 [780/650/560/480] × 存取状态
  （无值 / 1 / 0 / 网关页）展开收起全部可切换；
- 反向对照（修复前）：同场景设置命中元素为主题卡片（`topIsSettings=false`）。
- **部署后实测（2026-09-20 15:20，0.1.69 / 停机 0.9s）**：线上入口已切 `index-CEAx7Cwl.js`；
  展开态 `nav` 出滚动条、「设置」滚动可见 → 真实鼠标点击 → 成功导航 `/settings`；
  高度 [780/650/560/480] 展开/收起全过；`/api/gateways/overview` 8 网关全健康、
  `/api/version` 与 `/api/status` 版本一致。

---

## 0.1.68 — 2026-09-20

### 新增：QoderWork CN 账号导入（独立池）+ per-account `endpoint` 支持

**背景**：`QoderWork CN`（`%APPDATA%\QoderWork CN`）是**独立 Electron 应用**，不是 VS Code fork ——
它**没有** `User\globalStorage\state.vscdb`，因此既有的 `import_local_cn()`
（读 `%APPDATA%\QoderCN\User\globalStorage\state.vscdb`）**覆盖不到它**。

**改动**

- `qoder.rs` 新增 `qoderwork_cn_user_data_dir()`：`%APPDATA%\QoderWork CN`。
- `qoder.rs` 新增 `read_local_credential_work_cn()`：解密其登录态。
  - 凭据在**同目录**的 `auth-v2.dat`（schemaVersion=2，优先）/ `auth.dat`（旧占位版，回退）；
  - 格式与 Qoder 同源：`Chromium safeStorage v10` = `DPAPI(Local State 的
    os_crypt.encrypted_key)` → master key → `AES-256-GCM(nonce=blob[3..15], ct=blob[15..])`；
    复用既有原语 `vscode_cn_inject::{dpapi_decrypt, decrypt_windows_gcm_v10}`，**无新依赖**。
  - 🔴 与 Qoder 的差异：Qoder 的 `Local State` 在 `User` 的**父目录**，QoderWork CN 在**同目录**。
  - 字段映射：`uid←user.id`、`name←user.name`、`accessToken←token`、`refreshToken←refreshToken`、
    `expireTime←expiresAt`(ISO8601→ms，新增 `iso_to_ms`)、`userTag←user.tier`、`source="qoderworkcn-local"`。
- `qoder.rs` 新增 `import_local_work_cn()`：**打 `gateway = "qwen3.8-flash"` 独立池标**
  （= 分离效果：池成员隔离 + 轮转游标独立，`next_pool_index` 按 gateway 分别记）、`region = "cn"`，
  并写入 **per-account `endpoint`**：
  - 🔴 实测依据：同一 `dt-` 在 `openapi.qoder.sh/api/v1/userinfo` → **401 TOKEN_EXPIRE**，
    在 **`gateway.qoder.com.cn/api/v1/userinfo` → 200**。该主机取自其
    `data\region-cache.json` 的 `endpoint` 字段 → 常量 `QODERWORK_CN_ENDPOINT`。
- `routable_accounts_for()` 新增 `"qwen3.8-flash"` 分支：本机 QoderWork CN 登录态优先 +
  打该池标的账号，跳过 `isQuotaExceeded` 账号（与 `qwen-flash` 池同规则）。
- `pool_label()` 新增 `"qwen3.8-flash" => "Qwen3.8-Flash(WCN)"`。
- `api.rs` 新增 `POST /api/qoder/accounts/import-work-cn`（幂等）。

**影响范围**

- 纯新增：新目录读取器 + 新导入端点 + 新池分支；**未改动** `/qoder/v1`、`/qwen/v1`、
  `/qwen-flash/v1` 的既有行为（`pool_gateway` 对其余取值仍是恒等映射）。
- ⚠️ **本版尚未接线**：per-account `endpoint` 已落盘，但**请求侧仍走 `CHAT_HOST`（.sh）**；
  新路由 `/qwen3.8-flash/v1` 也尚未注册 —— 这两项属下一步。

**验证方式**

- `cargo check -p wb-switch-core -p wb-switch-server` **0 错误**。
- 凭据解密用独立探针 `tools/probe_qoderwork_auth.py` 预先验证成功（DPAPI + AES-GCM），
  解出 `dt-`/`drt-`/`user.{id,name,tier}`，并以 `userinfo` 200 证明凭据对该网关可用。
- 导入端到端：`POST /api/qoder/accounts/import-work-cn` → 账号落盘且带
  `gateway="qwen3.8-flash"` / `region="cn"` / `endpoint=gateway.qoder.com.cn`。

---

## 0.1.67 — 2026-09-18

### 🔴 严重修复：`repair_gbk_mojibake()` 自己制造乱码 —— 1920 个汉字被改坏（含 682 个常用字）

**症状**：流式回答首字符乱码，100% 可复现 ——「一、二、三！」被下发成「һ、二、三！」。

**根因（不是上游，是我们自己）**：0.1.60 引入的 GBK 误码修复函数判据错误。
原注释称「干净文本（GBK 字节不是合法 UTF-8）零误伤」—— **该假设不成立**：
大量**合法汉字**的 GBK 双字节恰好构成合法 UTF-8，于是函数把它们当成乱码「修」坏了。
以「一」为例：GBK 编码 → 字节 `D2 BB` → 而 `D2 BB` 恰是合法 UTF-8（= U+04BB `һ`）
→ 函数把 `一` 改写成 `һ`。

**实测规模**：扫描 U+4E00–U+9FA5 得 **1920 个汉字会被误修，其中 682 个是 GB2312 一级常用字**
（`一`→`һ`、`为`→`Ϊ`、`业`→`ҵ`、`专`→`ר`、`位`→`λ`；双字串 `为之`→`Ϊ֮`）。
**为何流式必现、非流式侥幸**：非流式对**整串**修复，串一长必然混入非法字节而 `break` 跳过；
流式 `repair_qoder_sse_line` 是**逐帧累积**修复，`raw` 收敛到**单字**时即命中，
故首个 token 就是常见字时必坏。**该缺陷同时影响 `/qwen/v1`、`/qoder/v1` 等全部 qoder 系路由。**

**修复**：新增指纹校验 `fix_is_trustworthy()` —— 误修的产物**必然落在 `U+0080..=U+07FF`**：
一个合法汉字能同时通过 `had_errors=false` 与 `from_utf8=Ok` 两道校验，
唯一原因就是其 GBK 双字节恰好是一个合法 **2 字节 UTF-8** 序列，而 2 字节 UTF-8
的码点范围恰是 `U+0080..=U+07FF`（拉丁扩展/希腊/西里尔/希伯来…），**永远不可能是汉字**。
真乱码修复得到的却是正常文本（汉字/ASCII/全角），落不到该区间 → 精准放行。
⚠️ 取舍：若上游把含该区间字符的**原文**弄乱码，此处会拒绝修复（宁留乱码不造新乱码）。

**影响范围**：`repair_gbk_mojibake()` 是两条路径的**公共实现**（非流式聚合 + 流式逐帧），
故一处修复两条路径同时痊愈；不改变任何路由语义与鉴权行为。

**验证方式**：
- 新增单测 `repair_never_corrupts_legitimate_cjk`（14 个高频常用字 + 6 条多字串必须原样返回）、
  `fix_is_rejected_when_it_lands_in_latin_greek_cyrillic_range`；
  **既有两条真乱码用例仍须通过**（`浣犲ソ鍛€`→`你好呀`、`濂界殑`→`好的`）。
- 端到端：`repro-mojibake.py` 复现率应 12/16 → **0/16**；`一、二、三！` 首字符为 U+4E00。

---

## 0.1.66 — 2026-09-18

### 修复：新网关路由未登记进 `is_api_namespace()` → 未知子路径落 SPA 兜底返回 200 HTML

`/qwen-flash/v1/nonexistent` 实测**未返回 404**，而是被 SPA 兜底接走。根因：
`api.rs::is_api_namespace()` 的 `WITH_SUB` 白名单只有 `api/v1/kimi/grok/qoder` ——
**新路由没登记**，且 **`qwen` 本来也漏着**（既有缺陷，同一类问题）。

**修复**：`WITH_SUB` 扩为 `[api, v1, kimi, grok, qoder, qwen, qwen-flash]`，并在原处留注释
「新增网关路由必须在此登记」。安全性已验证：判定要求前缀后紧跟 `/`，
而**前端并无 `/qwen` 或 `/qwen-flash` 路由**（Qwen 复用 `/qoder` 页面，见 `src/App.tsx`），
故不会把页面误伤成 404。

**影响范围**：仅影响「未注册路径」的响应形态（HTML 200 → 404 JSON），
正常请求路径与鉴权行为不变；顺带修好 `/qwen/` 的同一缺口。

**验证方式**：`GET /qwen-flash/v1/nonexistent` → **404 JSON**（`{"ok":false,"code":"not_found"}`）；
`/qwen/v1`、`/qoder/v1`、`/qwen-flash/v1` 正常请求无回归。

---

## 0.1.65 — 2026-09-18

### 修正：`/qwen-flash/v1` 的 `capabilities` 声明按实测收敛为 `["text"]`

0.1.64 上线后做了**端到端能力审计**（含对照实验，见下），发现原先声明的
`["text","reasoning","tools"]` 有 **2 项不实**，属乐观声明，已改正并加单测锁死。

**实测能力矩阵**（`/qwen-flash/v1` 对照 `/qwen/v1`，2026-09-18 22:45 前后）：

| 能力 | 结果 | 判定依据 |
| --- | --- | --- |
| 文本对话 | ✅ 可用 | 非流式 1.2s 正常出正文 |
| 流式 | ✅ 稳定 | 4/4 成功，首字节 **0.08~0.37s**，无 `event: error`、无 U+FFFD |
| `tools` | ❌ **不可用** | **带 `tools` 字段上游直接返回空正文**；同 prompt 不带 tools 正常出正文。lite 档不解析 tools；pro/max 档本账号零额度（无 tools 也空）。`/qwen/v1`、`/qoder/v1` 表现**完全一致** |
| 多模态图片 | ❌ 不可用 | 图文混排 content 数组可透传，但模型回复「无法看到图片」 |
| 独立 reasoning 流 | ❌ 无 | `reasoning_content` 恒为空（思考混在正文）；`reasoning_effort` 可传不报错，但无独立流可透传 |

→ `capabilities` 改为 `["text"]`，`description` 同步注明；新增单测
`qwen_flash_capabilities_stay_honest` 断言「未实测通过的能力不得写入」，防止回归。

**⚠️ 同批发现的既有缺陷（本次未修，需单独决策）**：流式正文首字符乱码。
上游把「一」按 **GBK** 发出（原始字节 `d2 bb`），而该字节对恰好是合法 UTF-8
（解成 U+04BB `һ`），故严格解码不报错、`repair_gbk_mojibake` 也修不了
（U+04BB 无法 GBK 编码 → 原样返回）。**`/qwen/v1` 的原始字节完全相同**，
说明是 qoder 系路由共性、非本路由引入。根治需在**字节层**做修复
（对可疑 2 字节序列按 GBK 重解），会影响全部 qoder 系路由，故留待确认。

**验证方式**：`cargo test --release -p wb-switch-core --lib`（新增用例通过）；
`verify-qwen-flash.py` + `forensic-qwen-flash.py` + `forensic-tools-tier.py` +
`forensic-multimodal.py`（仓库根，可复跑）。

---

## 0.1.64 — 2026-09-18

> 本版由**两个会话的改动同批上线**：① 并行会话的 AutoClaw 账号 `region`
> 字段迁移（`autoclaw_accounts.rs` / `platform_checkin.rs` / `glm_proxy.rs`，
> 按 region 区分国际版与国内版账号并分别执行签到）；② 本节的
> `/qwen-flash/v1` 专用路由。两者共用一个版本号。

### 新增：`/qwen-flash/v1` —— Qwen3.8-Flash 专用路由（从 Qoder 获取模型服务）

**动机**：需要一条**语义单一**的 Qwen3.8-Flash 通路，与 `/qwen/v1`（模型名映射壳，兼跑
max/pro/flash）解耦，便于独立观测与调优。

**改动**：
- `qoder.rs` 新增 `pool_gateway()` 归一映射：`qwen-flash` → 复用 **Qoder 默认池**
  （本机实时登录态 + 未打 `gateway` 标的账号），但轮转游标仍按**原始** gateway 记
  （`next_pool_index`）—— 两条路由取号互不推进，各自均匀、池成员相同。
- `qoder.rs` 新增 `qwen_flash_chat_completions_unified()`：**强制 pin** `model=qwen3.8-flash`
  （上游 `lite`）。**依据**：实测 Free 档 max/pro 额度为 0（上游 code 116 `quota exceeded`），
  放行只会把请求送进必然失败的档位、徒增 502；该路由语义即「Flash 专用」。
- `qoder.rs` 新增 `qwen_flash_models_payload()`：`/qwen-flash/v1/models` 只暴露单一模型，
  不列 max/pro，避免下游客户端误选到零额度档位。
- `api.rs`：注册 `POST /qwen-flash/v1/chat/completions` 与 `GET /qwen-flash/v1/models`；
  `is_logged_path` 前缀白名单补 `"/qwen-flash/"`（**`/qwen/` 匹配不到 `/qwen-flash/`**，漏了就没有访问日志）。
- `gateway_overview.rs`：`GATEWAYS` 增加 `qwen-flash` 条目，`fetch_models` / `fetch_accounts`
  各加一条 match 臂（账号视图复用 `qoder::list_for_display()`，因池是同一个）。

**透传不变**：`tools` / `reasoning_effort` / 多模态 content 数组 / `stream_options` 全部原样透传上游，
不做裁剪；GBK 误码修复与 SSE 流修复复用既有 `qoder_repaired_sse_body()`。

**影响范围**：纯新增路由，未改动 `/qoder/v1`、`/qwen/v1` 的既有行为（`pool_gateway` 对 `qwen`/`qoder`
是恒等映射，已由单测锁住）。

**验证方式**：
- 单测 `cargo test --release -p wb-switch-core --lib`：`qwen_flash_reuses_qoder_pool_only`、
  `pool_labels_are_distinct`、`qwen_flash_pins_to_lite_tier`。
- 端到端：`GET /qwen-flash/v1/models` 只回 1 个模型；`POST /qwen-flash/v1/chat/completions`
  流式返回正文、`/api/gateway/overview` 出现 `qwen-flash` 条目、原 `/qwen/v1` 与 `/qoder/v1` 无回归。

**⚠️ 一并说明（未改代码）**：实测 `api2-v2.qoder.com.cn` = **NXDOMAIN**（阿里 DoH Status=3），即 **Qoder 没有独立的 CN 推理端点**；`openapi.qoder.com.cn` 与 `openapi.qoder.sh` 为同一后端双域名
（同路径返回逐字节一致），**CN 侧推理端点也是 `api2-v2.qoder.sh`**。
故本路由的「Qoder 来源」= 官方 Qoder 上游，无需也无法配置 CN 专属上游地址。
`docs/qoder-protocol-research.md` 中「国内为 .com.cn」那句仅对 openapi 成立。

---

### 部署记录（2026-09-18 22:26，0.1.64 上线）

- **树的编译阻塞已修复**：本次构建时 `wb-switch-core` 有 **7 个编译错误**，导致 0.1.64 无法产出。
  全部集中在同期并入的「账号 region 字段迁移」改动（无独立 CHANGELOG 条目），已一次性修好：
  - `autoclaw_accounts.rs` ×5：`serde_json::Value` 上误用 `.contains_key(k)` → 改为 `.get(k).is_none()`
    （139 / 168 / 171 / 174 / 905 行）；
  - `autoclaw_accounts.rs:916`：`log::info!` → `eprintln!`（**`log` 并非本 crate 依赖**，该 crate 约定用
    `println!/eprintln!`，实测 23:23 次）；
  - `platform_checkin.rs:768`：`all_accounts.into_iter()` 先消费掉了变量、779 行再次使用 →
    首个改为 `all_accounts.clone().into_iter()`（**保持原语义**，仅消除移动冲突）。
  - 影响范围：仅上述两文件的迁移/签到路径；不改变行为语义（`contains_key`→`get().is_none()` 等价）。
  - 验证：`cargo check -p wb-switch-core -p wb-switch-server` **0 错误** → `cargo build --release` 成功。
- **部署方式**：`deploy.py`（零停机），停机 **1.0s**，`version asserted after restart: 0.1.64`，
  回滚点 `bin/wb-switch.exe.old` = 0.1.63。
- **线上验证**：`/api/version` = 0.1.64；`/qwen-flash/v1/models` 返回 **1** 个模型（`qwen3.8-flash`）✅；
  回归 `/qwen/v1/models`=7、`/qoder/v1/models`=8、`/glmweb/v1/models`=23、`/kimi/v1/models`=4 全部 200；
  `/api/gateway/overview` 出现 `qwen-flash`；`tools/monitor.py` = **ALL GREEN**。
- ⚠️ **构建姿势提醒**：本次必须用 `CARGO_TARGET_DIR=<ascii-target-dir>` +
  `-p wb-switch-server`、并用 `deploy.py` 部署。手搓 `Copy-Item` 会踩两个坑：
  ① `workbuddy-switch\target\release\` 里**没有** `wb-switch.exe`（默认 target 目录仅剩旧依赖残留）；
  ② 直接覆盖运行中的 `bin\wb-switch.exe` 会被文件占用拒绝/且**无版本断言**。

---

## 0.1.63 — 2026-09-18

### 修复：🔴 根因治理 —— 上游 reqwest 客户端继承系统代理，Clash 端口一抖全账号池「error sending request」

**补全 19:10 ZCode 事故的完整根因链**：部署 0.1.62 的观测层后立即复现出
`502「全部 1 个 Qwen 独立账号均不可用：Qoder chat 请求失败: error sending request for url」`——
本机环境变量 `HTTP_PROXY=http://127.0.0.1:65269`（Clash **动态端口**，当前已死），
而 `qoder.rs` 等 reqwest 客户端**默认继承系统代理** → 代理端口一换/一挂，
所有走它的上游连接立即失败，**进行中的流被强制掐断**（今天 13:34 / 14:45 的
GLM 断连、19:10 的 qwen 断连同根同源）。

**修复**：国内可直连的上游客户端统一加 **`.no_proxy()`**：
`qoder.rs` / `glm_proxy.rs`（AutoClaw）/ `glm_web.rs`（chatglm.cn）/ `kimi.rs` /
`loomy_proxy.rs`（讯飞）/ `qwen.rs`。**grok.rs / grok_sso.rs 保留代理**
（grok.com 国外站点需走 Clash）。`config.rs` 的签到 HTTP 客户端不动。

**效果**：Clash 开/关/换端口不再影响任何模型网关；上午那种「peer closed / 10054」
流中断将不再由本机代理引发。

---

## 0.1.62 — 2026-09-18

### 修复：🔴 观测性黑洞 —— 请求没到 handler 就失败时无任何日志（ZCode -32603 事故的真正堵点）

**现场**：19:10:29 ZCode `custom-local:qwen3.8-flash-qoder` 报
`{"code":-32603,"message":"Internal error","data":{"details":"\"internal server error\""}}`，
但网关日志（127 条）里 **19:09:23 之后就没有任何记录** —— 失败的请求根本没走到业务 handler，
现有日志体系完全看不到它。排查确认：qwen 网关与账号池当前完全健康
（复现 `qwen3.8-flash-qoder` 非流式/流式/带 tools 全部 200），
故障只存在于那一个瞬间，且**无法回溯**（后端 `stdio:"ignore"`，panic 输出也被丢弃）。

**修复（观测性兜底，两层中间件）：**
1. **`access_log_middleware`**：所有网关/管理 API 路径的请求，
   **非 2xx**（413 body 超限 / 422 body 解析失败 / 鉴权前拒绝 / 5xx）统一记入网关日志
   （gateway=`http`，含 method/path/状态码/耗时）。成功的请求仍由各 handler 记业务日志，不重复。
2. **`panic_guard_middleware`**（最外层）：handler panic 不再被 tokio 静默吞掉
   （表现为客户端连接被断开、ZCode 报 -32603 Internal error），
   改为捕获后返回 500 JSON 并记入网关日志。

下次再出现「客户端报错但网关无日志」，`/api/gateway/logs` 里必有 `gateway=http` 的条目
说明失败发生在哪一层（413=请求体超限 / 422=解析失败 / 500=panic / 无记录=连接层未到达）。

---

## 0.1.61 — 2026-09-18

### 修复：🔴 Qoder 上游中文输出 GBK 误码（lite 档 100% 复现，两池皆中）

**现场**：17:00 起 `/qoder/v1` 与 `/qwen/v1` 的流式/非流式中文回复全变乱码——
`你好呀` → `浣犲ソ鍛€`（UTF-8 字节被按 GBK 误解码后再发出的双层编码）。
16:35 同一请求还是干净的 → 上游在后端实例层面出了回归（与账号池无关，A/B 两池一致）。

**修复**（网关侧确定性修复，变换可逆、零误伤）：
- 新增 `qoder::repair_gbk_mojibake`：字符串按 GBK 重编码还原原始字节，**恰好是合法
  UTF-8 时才采用**；干净文本/ASCII/emoji 原样返回（附 2 项冻结样本单测）。
- 非流式：`chat_completions_unified_for` 聚合后对 `content`/`reasoning_content` 逐串修复。
- 流式：api.rs 新增 `qoder_repaired_sse_body` —— 行缓冲（复用 `decode_stream_chunk`
  的跨 chunk 拼接）+ 逐行解析 SSE，修复 `choices[].{delta,message}.content/reasoning_content`
  后重序列化；`/qoder/v1` 与 `/qwen/v1` 两个 handler 接入。
- 依赖：core 新增 `encoding_rs = "0.8"`。

**若上游自己修好**：修复层自动退化为恒等变换（干净文本不满足「GBK 字节是合法 UTF-8」条件），
无需回滚本次改动。

**验证**：单测（含真实乱码样本）；部署后流式实测中文输出。

---

## 0.1.60 — 2026-09-18

### 功能：`/qwen/v1` 独立账号池 —— 与 `/qoder/v1` 额度互不侵占

**背景**：`/qwen/v1` 此前只是 `/qoder` 的**模型名映射壳**（`qwen3.8-max→max`、`flash→lite` 后
直接调 Qoder 上游），两通路共享 `qoder_accounts.json` 同一池。池内唯一 Free 账号
`isQuotaExceeded: true` 后，plus/max 档全灭（`{"code":116,"error":"quota exceeded"}`），
lite 档仍可用——用户要求拆池。

| 改动 | 说明 |
| --- | --- |
| `qoder.rs` | `routable_accounts_for(gateway)`：`qoder` 池 = 本机实时登录态 + 未打标账号；**`qwen` 池 = 只轮转 `gateway=="qwen"` 标记的账号，不回落本机登录态**；轮转游标按网关独立（`POOL_CURSORS`，原单一 `POOL_CURSOR` 删除） |
| `qoder.rs` | `import_manual` / `device_flow_start` 增加 `gateway` 参数（设备授权会话携带池标记，complete 时入对应池）；upsert 合并字段补 `gateway`（改标可挪池）；`chat_completions_for` / `chat_completions_unified_for` 新入口，旧签名保留为 qoder 池别名 |
| `qoder.rs` | `list_for_display` 每个账号带 `gateway` 字段（webui 可见池归属） |
| `api.rs` | `api_qwen_chat_completions` 改走 `chat_completions_unified_for("qwen", …)`；空池报错文案改为指引「添加账号到 Qwen 独立池」；`/api/qoder/device-flow/start` 兼容空 body（旧前端不传参仍默认 qoder 池），非空时读 `gateway` |

**兼容性**：`/qoder/v1` 行为完全不变（默认池 = 本机登录态 + 未打标账号）；存量账号无 `gateway`
字段视为 qoder 池，零迁移。**`/qwen/v1` 拆池后为空池**——首次调用会得到明确的「Qwen 独立账号池为空」
提示（而非误导性的上游 116），添加账号（设备授权选 `gateway=qwen`）后即恢复。

**验证**：`cargo check --release`（core+server，all-targets）0 错误；部署后实测
`/qwen/v1` 返回空池提示、`/qoder/v1` 行为不变。

---

## 0.1.59 — 2026-09-18

### 修复：🔴 连接类故障漏判第二弹 —— 中文 Windows 本地化错误文案（ZCode 二次事故）

**现场**：14:45 ZCode `model=auto` 再次报「上游流中断（多半是上下文过长…）」，原文：
`error decoding response body: … 远程主机强迫关闭了一个现有的连接。 (os error 10054)`。
0.1.52 补的 `peer closed` / `close_notify` 只覆盖 rustls 英文文案，
**中文 Windows 的 OS 错误是本地化的**（`远程主机强迫关闭` + winsock 码 `10054` = WSAECONNRESET），
又一次漏判 → 又给了「压缩上下文」的错误建议。

**证据（与上下文长度无关）**：14:45 失败 `in=0/out=0`（连接阶段就被掐），
中间 14:33 的 `auto` 小请求（in=20/out=70）成功 —— 间歇性上游断连，重试即可。

**修复**：`upstream_interrupt_message` 连接类模式补上
`os error 10054`（WSAECONNRESET）/ `os error 10053`（WSAECONNABORTED）/
`远程主机强迫关闭` / `强迫关闭`；超时类补 `os error 10060`。
新增回归测试 `localized_wsaconnreset_is_conn_lost`（14:45 事故原文）。
GLM 与 Loomy 两条通路共用此函数，一处修复两端生效。

**bump 方式改进**：吸取「re.sub 无匹配静默 no-op」教训，bump 脚本改为
读当前版本号 +1 并 **assert 写入成功**（并行会话推进版本也不会再静默丢 bump）。

---

## 0.1.52 — 2026-09-18

### 修复：🔴「上游流中断」误判连接类故障为「上下文过长」（ZCode 事故）

**现场**：13:34 ZCode `model=auto` 报
`上游流中断：响应在传输中被切断（多半是上下文过长…）（原文: error decoding response body: …
peer closed connection without sending TLS close_notify…）`，且 ZCode 据此标 `retryable=false`。

**根因**：`glm_anthropic::upstream_interrupt_message` 的连接类模式里是 `connection closed`，
而 rustls 原文词序是 **`peer closed connection`** —— 不匹配 → 落进 decoding 分支 →
给了「新开会话/压缩上下文」的**错误建议**（实际是上游断连，直接重试即可）。
证据：同日 02:1x-02:3x 一串 **5 万 token 输入全部成功**，13:34 这条 `in=0/out=0`
（上游一个计量字节都没产生），与上下文长度无关。

**修复**：连接类模式补上 `peer closed` / `close_notify`；新增回归测试
`peer_closed_without_close_notify_is_conn_lost`（含事故原文，断言归入「连接被重置/切断」
且建议「可直接重试」、不含「压缩上下文」）。

---

## 0.1.58 — 2026-09-18

### 前端（P4 收尾）：三个统计页的展示格式化收敛到 `lib/stats-format.ts`

| 变更 | 说明 |
| --- | --- |
| 新增 `src/lib/stats-format.ts`（84 行） | `formatGroupedNumber` / `formatCompactNumber`（亿/万）/ `formatClock` / `formatRatioPercent` / `formatLatency` / `formatDateTimeShort` / `formatMonthDay` / `formatDateCn` / `formatCredits` —— 全部**原样搬迁**自三个页面的本地实现 |
| `ModelUsagePage` | 删 5 个本地格式化函数，别名导入（调用点零改动） |
| `TokenStatsPage` | 删 `formatTokenCompact` / `formatDateTime` / `formatChartDate`；`formatTokenExact`（en-US 精确千分位）属 Token 页专用展示，保留本地 |
| `CreditStatsPage` | 删 `formatCredits` / `formatDateTime` / `formatDate` / `formatChartDate` |

**合并前逐字符比对过**：`formatDateTime` / `formatChartDate` 两页完全相同；紧凑格式化
（`formatTokens` vs `formatTokenCompact`）语义等价（<1万 的兜底分别是 en-US 与 zh-CN
的 `Intl.NumberFormat`，对整数输出相同）。

**刻意不合并的**（避免观感/语义变化）：
- `TrendChart`：Token 与 Credit 各有一套实现，数据形状与 props 均不同（一个吃 `source`、
  一个吃 props），合并需重写渲染路径，视觉回归风险大于 ~100 行收益；
- Token 的 `percentage(value, sum)`（和为 0 时显示 `—`）与 Model 的 `formatPercent(ratio)`
  （空值按 0）：**语义不同**，分别保留为 `lib` 外的本地函数 / `formatRatioPercent`。

**验证**：`tsc --noEmit` 0 错误；vite + cargo 构建后部署；三 Tab DOM 抽查。

---

## 0.1.57 — 2026-09-18

### 前端：三个统计页合并为统一「统计」页（`/stats`）

侧栏的「Token 统计 / 积分统计 / 模型用量」三个入口合并为一个「**统计**」入口，
页内 Tab 切换「调用与模型 / Token / 积分」三个视角。

| 变更 | 说明 |
| --- | --- |
| 新增 `src/pages/StatsPage.tsx` | Tab 容器：当前视角同步到 `?tab=` 查询参数（可深链、浏览器前进后退可用）；三个**既有页面组件** lazy 加载为 Tab 内容，功能零丢失 |
| `App.tsx` | 侧栏三入口 → 一入口；旧路由 `/model-usage`、`/token-stats`、`/credit-stats` 重定向到 `/stats?tab=…`（书签/深链不断） |
| `gateway-switcher.tsx` | 「模型用量与积分统计」跳转指向 `/stats?tab=model` |
| 附带收益 | `ModelUsagePage` 由主包改为懒加载，首屏体积变小 |

**为什么用 Tab 而不是把三页数据揉成一张大表**（口径冲突，按约定先说明再处理）：

1. **时间范围语义不同**：调用窗口（今日/7 天/全部 + 小时桶）≠ 扫描天数（7/30/90）≠ 计费月。
   共享一个范围选择器会改变各页语义 → **保留各 Tab 自己的范围控件**；
2. **同一模型会同时出现在多个视角**（代理记账 / 官方投影 / 会话扫描三个来源）→
   **不做跨源「按模型总表」**，避免重复计数的观感；每个视角保持自己的来源口径；
3. **「调用次数」存在三种口径**（代理 calls / 会话 records / 官方 requestCount）→ 各自保留；
4. **积分消耗两页数值不同**（模型用量页 = 已归因到调用的 credits；积分页 = 窗口内全部流水）→
   保留各自口径，页头 Tab 描述里已注明各视角含义。

**验证**：`tsc --noEmit` 0 错误（工程开 `noUnusedLocals`）；`vite build` 成功；
部署后 DOM 验证三 Tab 切换与旧路由重定向。

---

## 0.1.56 — 2026-09-18

### 重构（P3/5）：`model_usage` 内部两套累加器合并为一套

`model_usage` 里同一个文件内长期并存两套累加器，吃同样的调用记录各算一遍：

| 累加器 | 服务的输出 | 形态 |
| --- | --- | --- |
| `accumulate` + `empty_bucket` | `/api/glm-usage/summary` 的 `windows.*` 与 `models.*` | 增量直写 JSON |
| `MonitorAgg`（P1 已改走 `engine::UsageAgg`） | `/api/glm-usage/monitor` 的模型行 | 结构体 |

本次把前者也收到 `UsageAgg`：新增 `UsageAgg::to_summary_bucket()` 作为**汇总桶的唯一投影点**，
`build_summary()` 直接用累加器聚合，`empty_bucket` / `accumulate` 两个函数删除。

**顺带补齐累加器能力**（原先只有 JSON 路径在算）：
- `reported_total_tokens`：上游 `usage.total_tokens` 之和。
  ⚠️ 与 `total_tokens()`（`prompt + completion`）**不是同一个数** —— 汇总桶用前者、
  监控模型行用后者，两者都被接口暴露过，因此**分别保留**（已在代码与测试里注明，避免后人"顺手统一"改坏接口）。
- `uncached_prompt`：记录单独上报的未缓存输入 Token 之和。
- `saved_credits()`：缓存节省积分的**估算值**（`cached × 单价 × 0.8`），
  文档明确标注为估算、**不得计入任何积分合计**（口径规则 R1：禁止 token ↔ 积分换算）。

**测试**：新增 2 项冻结契约测试（`summary_bucket_matches_legacy_shape` 覆盖 13 个字段的取值、
`saved_credits_is_zero_without_credits_or_cache`），累计 **237 项 = 235 passed / 2 failed**
（仍是那两项既有 Windows 路径测试，新增失败 0）。两个既有测试改为走新累加器，断言不变。

**兼容性**：**零数值变化** —— 前置快照对比 `/api/glm-usage/summary` 逐字段 IDENTICAL、
`/api/glm-usage/monitor` 仅窗口边界时间戳差异。

> 关于方案里的「视图层（`stats/view/*`）」：只有在 `token_stats` / `credit_usage`
> 也迁到 `StatEvent` 之后才需要投影层。而这两个模块的响应格式（整数语义 / 账户维度结构）
> 必须保持不变，强行迁移只会改变输出，因此**不建空层**（YAGNI）——
> 等真有第二个消费者时再抽。本次改为把「桶投影」放在累加器自己身上（`to_summary_bucket`），
> 它就是汇总桶的唯一来源。

---

## 0.1.55 — 2026-09-18

### 重构（P2/5）：统一积分链路 —— 流水解析 / 匹配 / 认领 / 读取各收敛到一处

方案里列为 🔴 冲突 C1 的「同一笔 AutoClaw 扣费被两条链路各投影一次」，根因是
**流水读取与匹配规则存在两套实现**：模型用量的「即时认领」与「批量对账」各写一遍
相同的「筛模型 + 取时间最接近」（差异只在该用多宽的窗口），积分统计又经
`model_usage::ledger_model_consumption` 再拉一次流水。本次把这两层各收敛到一处。

| 新增 | 内容 |
| --- | --- |
| `stats/reconcile.rs` | `LedgerEntry` / `MatchWindow`（`for_reconcile` / `for_immediate`）、`model_from_description` / `parse_occurred_at` / `parse_entry` / `pick_best_match`，以及**跨模块共用的认领注册表**（`ensure_seeded` / `claim` / `claimed_snapshot`） |
| `stats/source/ledger.rs` | 官方流水分页抓取 `fetch_entries` + `model_consumption` —— **唯一的流水网络读取实现**（原先模型用量与积分统计各拉一次） |

| 改造 | 内容 |
| --- | --- |
| `model_usage.rs` | 即时认领与批量对账统一走 `reconcile::pick_best_match`，窗口由 `MatchWindow::for_immediate`（`[started−10s, now]`）/ `for_reconcile`（`[ts−10s, ts+35min]`）表达；分页与 entry 解析收敛到 `source::ledger` + `reconcile::parse_entry`；认领注册表迁出（`claimed_ledger_snapshot` → `reconcile::claimed_snapshot`）；删除本地 `ledger_model_consumption` 与重复的解析函数；`chrono` 依赖随之移除 |
| `credit_usage.rs` | AutoClaw 窗口改从 `stats::source::ledger` 取流水，不再依赖 `model_usage` |

**测试**：新增 8 项（描述解析边界 / entry 解析过滤 / 最近优先 / **大小写不敏感但不得前缀误配**（GLM-5.3 不得配到 GLM-5.3-Flash）/ 窗口闭区间 / 窗口构造语义 / 单条流水只消费一次 / 认领幂等）；
累计 **235 项 = 233 passed / 2 failed**（仍是那两项既有 Windows 路径测试，**新增失败 0**）。
`model_usage` 里那份重复的 `ledger_description_parses_model_name` 已随实现迁移删除（覆盖移到 `reconcile` 的用例，且更全）。

**兼容性**：本阶段**不改变任何接口数值与结构** —— 新实现的过滤条件与窗口语义与原实现逐一对应。

> ⚠️ **修正方案里的一处预期**：P2 原先预计「去掉双投影后积分页数值会变」。
> 实际核对后确认**不会**：积分页的 AutoClaw 窗口取的是「窗口内全部模型消耗流水」
> （权威计费量），与模型用量页「已归因到具体调用」的口径本就不同源，
> 两者不是同一个数被加两次，而是同一事实的两种投影。真正的风险（重复网络请求、
> 匹配规则漂移）已在上面的收敛中消除。**所以本阶段是无感升级。**

---

## 0.1.54 — 2026-09-18

### 修复：模型级 `cacheHitRate` 精度回归（0.1.53 引入，被前后快照 diff 抓到）

`monitor_model_json` 的模型级命中率在改走 `UsageAgg::cache_hit_rate()` 时**丢掉了 4 位小数取整**：
接口从 `0.6474` 变成 `0.6474177625009849`。同一层的 `successRate` / `share` 都是 4 位小数，
属明显的接口精度倒退。

**处置**：`UsageAgg::cache_hit_rate()` 内统一 `round4`，与 `success_rate` / `share` 对齐；
并新增冻结契约测试 `cache_hit_rate_is_rounded_to_four_decimals` 钉住该精度。

> 这条回归是「部署后拿前后快照逐字段 diff」抓到的 —— 单元测试全绿也没能拦住
> （原测试只断言 `<= 1.0` 与 `== 0.75` 这类恰好无尾数的值）。**接口精度类契约必须用
> 真实数据快照对比，光靠单测不够。**

---

## 0.1.53 — 2026-09-18

### 重构（P1/5）：抽出统计公共层 —— `window` / `engine` / `event`

P0 统一了口径常量，本次把**时间窗口**、**聚合算术**、**事件模型**三块公共能力从
各模块里抽出来，收敛到 `modules/stats/`。三个统计模块不再各写一套同类逻辑。

| 新增 | 内容 |
| --- | --- |
| `stats/window.rs` | 本地日 `local_day_start_ms` / `local_day_key`、桶宽档位 `NICE_BUCKETS`、`pick_bucket`（≤240 点）、`default_bucket_ms`、`floor_to_bucket` |
| `stats/engine.rs` | `UsageAgg` 累加器（调用数/失败数/待对账数/tokens/缓存/积分/延迟/首字）+ 派生指标 `avg_latency`/`avg_ttfb`/`avg_stream_ms`/`success_rate`/`cache_hit_rate`/`share`/`avg_credits`；工具函数 `ratio`/`share`/`mean`/`round1`/`bucket_range` |
| `stats/event.rs` | 统一事件模型 `StatEvent` / `EventKey` / `Origin` / `TokenCounts` / `CreditCharge` / `CreditSource` + 代理记录适配器 `from_proxy_record` + `dedupe_by_key` / `merge_token_sources` |

| 改造 | 内容 |
| --- | --- |
| `model_usage.rs` | `MonitorAgg` 的 68 行算术删除，改为 `stats::engine::UsageAgg` 的薄包装（`Deref` 保持既有字段访问）；删掉本文件里重复的 `local_day_start_ms` / `NICE_BUCKETS` / `pick_bucket` / `default_bucket_ms`，改引用 `stats::window`（`default_bucket_ms` 以 `pub use` 保留兼容 `api.rs`）；4 处内联的桶对齐改用 `floor_to_bucket` |
| `token_stats.rs` | 日归属改用 `window::local_day_key`（与其它模块同一套本地日定义） |

**顺带修掉的第三处口径漂移**：`monitor_model_json` 里的**模型级** `cacheHitRate`
原先也是无守卫的 `cached / prompt`（会溢出 1.0），现改走 `UsageAgg::cache_hit_rate()`；
`totalTokens` 同样改走统一函数。

**测试**：新增 **19 项**（window 5 / engine 7 / event 7），累计 **227 项**；
`cargo test --release -p wb-switch-core` = **225 passed / 2 failed**，失败的仍是那两项
既有的 Windows 路径假设测试（`export_import::export_path_validation`、
`session::db_paths_point_to_home`），**本次新增失败 0 项**。

**兼容性**：接口路径、响应结构、字段名与数值格式均未变（`UsageAgg` 沿用 `f64`，
与 `model_usage` 既有 `json!` 渲染一致）。唯一可见变化仍只有「`cached > input` 时
缓存命中率不再溢出 1.0」。

---

## 0.1.52 — 2026-09-18

### 重构（P0/5）：统计口径统一 —— 新建 `stats` 模块，消除三模块口径漂移

三个统计模块（模型用量 / Token / 积分）长期各写一套口径，同一概念存在多处实现。
本次是「统一统计模块」的**阶段 P0（止血：只统一口径、不动结构）**，先建立口径的
唯一来源 `modules/stats/units.rs`；P1~P4 再依次迁入事件模型、聚合引擎、视图投影。

| 位置 | 改动 |
| --- | --- |
| `modules/stats/mod.rs`（新增） | 统计模块骨架与分层约定（`view → engine → event` 单向依赖，`engine` 不得依赖 `source`） |
| `modules/stats/units.rs`（新增） | 口径唯一定义：`MAX_RECORDS` / `RETENTION_DAYS` / `normalize_input_tokens` / `cache_hit_rate` / `total_tokens` / `round4` |
| `model_usage.rs` | 保留上限 **4000 → 5000**；`cacheHitRate` 改走统一函数；去掉重复的 `RECORDS_FILE` 字面量 |
| `token_stats.rs` | `Totals::value()` 改走统一函数（整数语义与「总输入为 0 时输出 null」的既有语义均保持不变） |

**修掉的实际问题**

1. **保留上限与声明不符**：`MAX_RECORDS = 4000` 在每日 30~50 次调用下约 80~130 天就截断，
   使「保留 180 天」名不副实 → 提到 5000。
2. **缓存命中率缺边界守卫**：Token 统计有 `cached > input` 规整（部分 AutoClaw 接口的 `input`
   只记未缓存部分），模型用量没有 → 统一到同一函数，比率恒落在 `[0, 1]`。

**测试**

- 新增 5 项单测：边界规整、浮点与整数实现一致性、比率钳制、总数不重复计缓存。
- `token_stats_reported_totals_match_unified_spec` 为**跨模块冻结契约**：任一侧的边界
  处理被改动即失败，防止口径再次漂移。
- `cargo test --release -p wb-switch-core` 全量通过（token_stats 原有 8 项无回归）。

**兼容性**

- 7 个统计接口的路径与响应结构**完全未变**，字段只增不减。
- 「缓存命中率」仅在 `cached > input` 的异常形态下数值变化（原先会溢出 1.0）。

---

## 0.1.51 — 2026-09-18

### 修复：底栏在 webui 被整体隐藏（用户实际看到的界面就是 webui）

`App.tsx` 里底栏的条件是 `api.isWebui() && !demoModeEnabled ? null : <UpdateCenter/>` ——
而 **launcher 是用 Chrome `--app=http://127.0.0.1:57890/` 打开 webui 的**（`launcher.js::openWindow`），
也就是说用户日常看到的正是 webui，底栏（含版本信息）全被隐藏了，0.1.50 的改动等于看不见。

**处置**：
- 底栏改为 {`demoModeEnabled ? null : <UpdateCenter/>`}（静态演示模式仍隐藏，保持截图产物不变）；
- 「更新」按钮单独加 `!api.isWebui()` 门槛 —— webui 没有桌面宿主，执行不了自更新安装，
  但**版本信息照常展示**；
- 设置页的「自动更新」区块本来就提供更新源链接，webui 侧不受影响。

---

## 0.1.50 — 2026-09-18

### 修复：底栏版本语义混淆（「WorkBuddy v0.1.49」其实是 Sylvia Switch 自己的版本）

底栏原来只有一行：`● WorkBuddy  v0.1.49`。但那个 `version` 来自 `update::APP_VERSION`
= **Sylvia Switch 自身版本**，标签却写着 WorkBuddy 客户端名 —— 两件事被挤在一行里。

**改为两行，各自标注清楚：**

| 行 | 内容 | 数据来源 |
| --- | --- | --- |
| 1 | `● WorkBuddy 客户端  v<客户端版本>` | **新增**注册表卸载信息 `DisplayVersion` |
| 2 | `Sylvia Switch 服务  v<自身版本>` + 更新按钮 | `update::APP_VERSION` |

- **新增后端** `process::workbuddy_client_version()`：读
  `HK{CU,LM}\Software[\WOW6432Node]\Microsoft\Windows\CurrentVersion\Uninstall`，
  匹配 `DisplayName` 含 `WorkBuddy` 或 `CodeBuddy`（客户端双名），取 `DisplayVersion`；
  带 `OnceLock` 缓存；读不到返回 `None` → 界面显示「版本未知」，不报错。
- `/api/status` 新增字段 `workbuddyVersion`；Tauri `get_status` 同步新增（显式
  `#[serde(rename = "workbuddyVersion")]`，因为桌面通道默认序列化为 snake_case，而
  webui 通道是 camelCase —— 两通道字段名必须一致）。
- **更新按钮挪到第 2 行**：更新源是 `github.com/changexbc/workbuddy-switch`（Sylvia Switch
  自己的仓库），此前挂在 WorkBuddy 行属错位；tooltip 同时改为「更新 Sylvia Switch」。

### 验证
`tsc --noEmit`=0；`cargo check -p wb-switch-core`=0；部署后
`/api/status.workbuddyVersion` 应返回客户端真实版本，
`/api/status.version` == `/api/version.version` == 0.1.50。

### 同期并入：Grok SSO 后台循环不再「无人值守自动开浏览器」

用户反馈「没开注册机，Chrome 却总自动打开 grok.com 去注册」。根因：server 启动时 spawn 的
3 个 Grok 后台循环（`grok_sso::spawn_sync_loop` 30min、`grok_sso::spawn_keepalive_loop` 6h、
`grok_pool::spawn_health_check_loop` 2h）最终都会经 `grok_sso::ensure_browser()` 以**非 headless +
独立 profile**（`~/.wb-switch/grok-chrome-profile`，CDP 19233）启动 Chrome 并直接打开
`https://grok.com`；未登录时该页即注册页，故被误认为「自动注册」。

- **运行时开关**（改配置即可，无需重启——循环每轮重读）：
  `~/.wb-switch/grok_sso_config.json` 的 `enabled=false`（管同步 + 保活循环）+
  `~/.wb-switch/grok_pool_policy.json` 的 `autoCheckEnabled=false`（管池体检循环；该文件此前不存在，默认 true）。
- **代码**：`grok_sso.rs` 抽出 `run_sync_with(&cfg)` —— `run_sync()`（后台）仍受 `enabled` 约束，
  `run_sync_manual()`（UI「立即同步」）**不再受约束**，避免「关掉后台 → 手动同步也被一起挡死」。
- **效果**：关掉后台后，仅点「打开登录窗口 / 立即同步」才会启动专用浏览器。

---

## 0.1.49 — 2026-09-18

### 修复：侧栏版本徽标挤压应用名（截断成「Sylvia Swit…」）

0.1.47 把版本徽标放在标题同一行，220px 侧栏里把 `Sylvia Switch` 挤到截断。
现改为**徽标另起一行**（与「静态演示」徽标同行），应用名保持完整显示。

- 位置：侧栏标题下方 `v0.1.49` 徽标（hover 提示 `Sylvia Switch v0.1.49`）；
- 窗口/标签页标题仍为 `Sylvia Switch 0.1.49`；
- 设置 → 自动更新仍显示 `当前版本：Sylvia Switch v0.1.49`。

### 修复：版本号显示错位（界面 v0.1.48 ≠ /api/version 0.1.49）

🔴 **根因**：`/api/status` 的 `version` 取自 `update::APP_VERSION` =
**`wb-switch-core` 的 `CARGO_PKG_VERSION`**，而 `/api/version` 取的是 **server crate** 版本。
本次只 bump 了 server crate（0.1.48→0.1.49），core 仍停在 0.1.48，
于是侧栏/底栏显示 0.1.48、二进制自证却是 0.1.49。

**处置**：bump 版本号时**两个 crate 必须一起改**（core + server），本次已把 core 对齐到 0.1.49。

---

## 0.1.48 — 2026-09-18

### 修复：🔴 ChatGLM 网关流式输出被重复 N 次（正文翻倍/三倍）

**现场（2026-09-18 02:5x–03:1x，游客模式实测）**

| prompt | 修复前 content | 修复后 content |
| --- | --- | --- |
| 只回复：OK | `OKOK` | `OK` |
| 只回复：AB | `ABABAB` | `AB` |
| 只回复：ABC | `ABCABCABC` | `ABC` |
| 请回复三个字母：XYZ | `XYZXYZXYZ` | `XYZ` |

**根因**：chatglm 的 SSE `parts` 是**累计快照**——每一帧都携带「到目前为止的全文」，
而不是新增的那一段。`glm_web.rs` 的流式与非流式两条路径都把每帧渲染出的全文
当作**增量 delta** 直接 `push_str` / 下发，于是同一句正文被重复 N 次（N=上游帧数）。

**修复**

- 新增 `glm_web::advance(prev, next)`：统一做「累计快照 → 增量」转换，
  并把状态推进到最新快照；
  - `next` 以 `prev` 为前缀 → 只返回新增后缀（正常累计路径）；
  - `prev` 包含 `next`（上游重发旧快照）→ 返回空，不下发；
  - 非前缀关系（分段输出）→ 整体下发并累加。
- **非流式**：`content`/`reasoning` 累加改走 `advance`；
- **流式**：`pump_stream` 新增 `emitted_text` / `emitted_reasoning` 进度，
  每帧只下发 `advance` 算出的增量（空增量不发帧）。

**影响范围**

- 仅 `glm_web.rs` 的 ChatGLM 网关（`/glmweb/v1/chat/completions` 与 `/api/glmweb/test`）；
- 其它网关（GLM 代理 / Kimi / Qoder / Grok / Loomy / WorkBuddy）不涉及；
- 客户端契约不变（OpenAI 兼容 delta 语义），只是内容不再重复。

**验证方式**：部署后 `/api/glmweb/test`（model=`glm-5.3-flash`，prompt=「只回复：AB」）
应返回 `AB`；流式路径累计帧应等于正文本身。

---

## 0.1.47 — 2026-09-18

> ⚠️ 本版本号**未单独部署**：前端改动（dist）构建完成后，因并行会话正在重构
> `wb-switch-core`（删除 codebuddy_cli / codebuddy_cn_ide / rotate 模块）导致 crate 暂不可编译，
> 随后 **0.1.48 的构建把这份 dist 一并带上线**。因此「界面显示版本号」实际随 **0.1.48** 生效。

### 新增：界面显式显示版本号

版本号此前只在底栏（标着「WorkBuddy」）与设置页出现，不易确认跑的是哪一版。现在三处可见：

- **侧栏标题旁**新增版本徽标 `v0.1.47`（ hover 提示 `Sylvia Switch v0.1.47`）；
- **标签页 / 窗口标题**改为 `Sylvia Switch 0.1.47`（`document.title`，随 Store 版本更新）；
- **设置 → 自动更新**：`当前版本：Sylvia Switch v0.1.47`，便于直接复制报版本。

版本号取自后端 `/api/version`（`useAccountsStore.status.version`），未加载时不显示占位，
不引入硬编码常量 —— 升级后界面自动跟着变。

---

### 修复：流式请求被「请求级总超时」掐断，却被误报成「上游流中断 / 上下文过长」

现场（2026-09-18 02:35，ZCode → Loomy 网关 → `GLM-5.3-Flash`，TraceID `a3765245`）：

> Loomy 上游流异常中断：上游流中断：响应在传输中被切断（多半是上下文过长，
> 上游未输出完就被网关断开）…（原文: error decoding response body）
> Turn execution failed … reason=unknown retryable=false

**根因（两条叠加）**

1. 🔴 **请求级 `.timeout(300s)` 是「连接 + 响应体读完」的总时长上限。**
   `loomy_proxy::send_once` 对 `chat/completions` 用的是 reqwest
   `RequestBuilder::timeout`，它覆盖**整个响应体读取过程**。上游是 SSE 流，
   长任务（agent 长思考 + 长输出）一旦跑过 300 秒，客户端会在上游**仍正常
   吐字**时单方面掐断连接，reqwest 只回 `error decoding response body`。
   时间线吻合：该请求 02:30:37 起 → 02:35:37 报错，正好 300 秒。
2. 🔴 **错误链被丢弃 → 分类永远落空。** 各处只取 `e.to_string()`，而 reqwest
   的 `Display` 恒为 `error decoding response body`，真因
   （`operation timed out` / `connection closed…`）只存在于 `source()` 链里。
   于是 `upstream_interrupt_message` 里「超时 / 连接被重置」分支**永不命中**，
   输出退化成含糊的「多半是上下文过长」，把排查引向错误方向。

**修复**

- 新增 `glm_anthropic::error_chain(&dyn Error)`：逐层展开 `source()`。
  所有流中断报错点改传完整错误链（`loomy_proxy` ×2、`glm_anthropic` ×1）。
- `upstream_interrupt_message` **调整判定顺序**：先判超时/连接类真因，再判
  decoding 包装文案——否则细化分类会被外层包装抢走。超时/链路类建议改为
  「属瞬时故障，可直接重试」，不再一律误导为「新开会话 / 压缩上下文」。
- **超时策略改为 client 层 connect + read，删除全部请求级 `.timeout()`**：
  - `loomy_proxy`：`connect_timeout(15s)` + `read_timeout(300s)`
  - `glm_web`：`connect_timeout(15s)` + `read_timeout(300s)`
  - `kimi`：`read_timeout(120s)`（原 `UPSTREAM_TIMEOUT_SECS = 120` 请求级总超时移除）
  `read_timeout` 是「两次读到数据之间」的空闲上限，**读到数据即重置**：
  长回答不再被总时长误杀，真卡死（一个字节都不再吐）仍会及时断开。

**回归测试**

- `error_chain_reveals_nested_cause`：真因必须能从 source 链取到。
- `timeout_in_source_chain_beats_decoding_wrapper`：带 source 链的超时不得
  再被误判成「上下文过长」，且建议应为「可直接重试」。

**验证（线上实测 2026-09-18 03:09–03:16）**

- `cargo test --release -p wb-switch-core`：**206 passed**，其中新增的
  `error_chain_reveals_nested_cause`、`timeout_in_source_chain_beats_decoding_wrapper` 均 ok。
  （另有 2 个既有失败 `session::db_paths_point_to_home`、
  `export_import::export_path_validation`，属测试用例自身的 Windows 平台假设
  ——`/tmp/...` 在 Windows 不算绝对路径、路径断言用 `/` 而实际为 `\`——与本次改动无关。）
- **部署**：`deploy.py --exe wbs-target/release/wb-switch.exe --expect-version 0.1.47`，
  停机 **0.9s**，`/api/version` 自证 `version=0.1.47`；回滚点 `bin/wb-switch.exe.old`（0.1.46）。
- 🔑 **决定性实测**：对 `/loomy/v1/messages` 发 24000 max_tokens 长流请求，
  **连续输出到 316.2 秒后正常 `message_stop`，全程无 `event: error`**。
  修复前该请求必然在第 300 秒整被请求级超时掐断并报 `error decoding response body`
  ——这是本次修复唯一有区分度的验收实验（埋点见 `verify-longstream.py`）。
  ⚠️ 顺带实测到：`GLM-5.3-Flash` 是**重思考**模型，单次 316 秒里产生 23671 个
  `thinking_delta`、正文 0 字（思考把 max_tokens 吃光）。这正是长任务轻易跨过
  300 秒、从而必现该 bug 的原因，也说明原来的「上下文过长」文案完全指错了方向。
- 旁路回归：`/kimi/v1/models` 4、`/qoder/v1/models` 3、`/glmweb/v1/models` **23**
  （0.1.46 的模型清单改动在位）、`/v1/models` 与 `/loomy/v1/models` 均 200；
  网关总览 5 个网关 enabled。

---

## 0.1.46 — 2026-09-18

### 变更：侧栏模型网关收拢为统一分组容器（纯展示层）

原先侧栏 7 个网关各自独立成一项（GLM 代理 / Qoder / Kimi / ChatGLM / Grok / Loomy / TRAE-WB），
要在多个入口间来回点。现在收拢为一个「模型网关」分组入口，展开即可查看与切换全部网关。

- **新增数据源** `src/lib/gateway-registry.ts`：顺序 = 侧栏展示顺序，命名/图标沿用原风格。
- **新增容器** `src/components/gateway/gateway-nav-group.tsx`：单一入口 + 组内列表；
  默认停留在网关页时自动展开，手动展开状态记忆到 localStorage。
- **App.tsx**：7 个网关 NavLink 替换为 `<GatewayNavGroup />`，**Routes 一行未动**。
- 组内状态摘要（健康点 / 账号数 / 模型数）复用现有 `/api/gateways/overview`，
  请求失败**静默降级**为静态清单，不弹错、不阻塞导航。
- **可扩展性**：新增网关 = 注册表加一项 + App.tsx 注册同路径路由；
  后端若先返回新网关且路由已注册，分组会**自动并入**（兜底图标 Network）。

### 同期并入（ChatGLM 网关 glmweb，后端 + 前端）

- **🔴 默认模型切到 GLM5.3Flash**：`glm_web.rs::models_payload()` 新增 `glm-5.3-flash` 并置首（模型清单 22 → 23）；
  `chat_completions` 未传 `model` 时的兜底由陈旧的 **`glm-4`** 改为 `default_model()`；
  新增可选配置 `~/.wb-switch/glmweb_config.json` 的 `defaultModel` 字段（缺省 `glm-5.3-flash`）。
  - 影响范围：仅模型标识；显式传 model 的请求行为不变。
- **新增幂等写回端点 `POST /api/glmweb/tokens/sync`**（浏览器同步工具专用）：
  - body `{ token, validate=true, source }`；`validate=true` 时**先真实调上游 refresh，失败则不写盘**；
  - **幂等**：token 已在池中时不报错、不新增，只刷新 access 缓存（可 20 分钟反复调用）；
  - **认轮换**：上游下发新 refresh token 时以新值写回；并发由 `SYNC_LOCK` + `WRITE_LOCK` + `atomic_write` 串行；
  - 凭据失效返回 **401**（`credential_invalid` 语义），网络/写盘类返回 **502**（`retryable`）。
- **池健康快照 `glm_web::pool_health()`**：`/api/glmweb/tokens` 的 `pool` 增补
  `degraded`（正式账号为 0 → 已退化游客模式）、`needRenew`、`defaultModel`、`renewThresholdMs`（6h）、`now`。
- **前端**：`GlmwebPage.tsx` 测试框默认与占位符改 `glm-5.3-flash`；顶部新增「游客模式」告警条（`pool.degraded`）；
  `api.ts` 新增 `glmweb_sync_token` RPC。
- **验证（线上实测 2026-09-18 02:2x）**：`/api/glmweb/tokens` → `defaultModel="glm-5.3-flash"`、`degraded=true`；
  `POST /api/glmweb/tokens/sync` 可用且幂等；`/glmweb/v1/models` 返回 23 个模型且 `glm-5.3-flash` 居首；
  `tsc --noEmit` 与 `cargo check -p wb-switch-core -p wb-switch-server` 均 0 错误。
  ⚠️ 冒烟测试曾写入一个占位 token，已删除并复核池内只剩 guest 槽位。

### 不动的部分（与整合前完全一致）
- 各网关的 API 地址、Key、请求参数、鉴权与后端路由逻辑**均未改动**；
- ⚠️ 但**同一个二进制里还并入了下方「ChatGLM 网关（glmweb）」的后端改动**（多人共用工作树，
  各自改源码、由同一次构建一起上线）—— 所以严格说本次**并非**「无 Rust 后端变更」，
  只是前端整合本身没动后端。下方另列一节如实记录。
- 每个网关的**路由路径**保持原值，跳转目标、管理页内容、切换行为均不变；
- `GatewaySwitcher`（顶部统一网关下拉）保持原样，未被替换或移除。

### 验证
`tsc --noEmit` 通过 → vite build 通过 → 各组网关路由均可正常进入并加载数据，
后端端点（`/v1/models`、`/kimi/v1/models` 等）行为不变。

---

## 0.1.45 — 2026-09-17

### 新增：Cuckoo Code 融合模块（/cuckoo）

对接 [cuckoo-code](https://github.com/wangyongpeng90/cuckoo-code)（零 Token 成本 AI Agent
桌面端，Electron 内嵌浏览器 + 工具调用循环）：让它的 Agent 大脑走本网关桥接的网页账号。

- `GET /cuckoo/chat`：本地聊天页（SSE 流式，历史存 localStorage，模型下拉走网关模型列表）。
- `GET /cuckoo/provider.js`：cuckoo-code 自定义 Provider 文件下载（`useIntercept` 模式，
  hook 为空操作——回复由聊天页自身派发 `cuckoo-ai-response` 事件回收，DOM 选择器全自有、最稳）。
- `POST /cuckoo/api/chat` + `GET /cuckoo/api/models`：免 Key 服务端中转，
  页面无需持有网关 Key；仅回环监听 127.0.0.1:57890，不对外暴露。
- 页面与 Provider 文件经 `include_str!` 内嵌二进制（不进 dist，前端构建不覆盖）。

### 使用

1. 浏览器打开 `http://127.0.0.1:57890/cuckoo/provider.js` 下载文件；
2. Cuckoo Code → 导入自定义 Provider → 选该文件；
3. 新建窗口即打开聊天页，系统提示词 + 工具调用全部由 cuckoo-code 侧驱动。

---

## 0.1.44 — 2026-09-17

### 修复

- **补完 0.1.43 遗留的两处同型丢字节**（`api.rs`）：Kimi 非流聚合（约 1112 行）与
  Qoder 测试端点聚合（约 1674 行），原先仍是严格 `std::str::from_utf8(&chunk)`，
  chunk 切在多字节字符中间会整块丢弃 → 中文大帧内容静默缺失。
  两处均改为 `decode_stream_chunk(carry, chunk)`；`raw_frames` 保留「上游原始证据」
  语义（继续用 lossy 攒），解析走跨 chunk 解码。
- `decode_stream_chunk` 由 `pub(crate)` 改为 `pub`，供 server crate 复用。

---

## 0.1.43 — 2026-09-17

### 修复

- **🔴 大帧被 TCP 按 UTF-8 半字符切开 → 误报「上游返回非流式异常响应」**
  - 现场（2026-09-16 23:57，ZCode / auto→glm-5.3-flash）：上游确实吐了
    6222 字节 SSE（首帧即含 `tool_calls`，arguments 里带 Windows 路径 +
    中文），但 Anthropic 转换器「一帧未产出」→ 触发兜底报错，客户端判
    `PROVIDER_BUSINESS_ERROR retryable=false` 直接放弃。
  - 根因：各 SSE 泵用**严格** `std::str::from_utf8(&chunk)` 解码上游 chunk，
    一旦 TCP 切点落在多字节字符（中文 3 字节）中间，**整个 chunk 被
    `continue` 静默丢弃**。大帧中文密集时几乎必然命中，导致后续帧缺失、
    转换器 `emitted_anything()==false` 且无 usage → 误判「200 空流」。
  - 修复：新增 `decode_stream_chunk(carry, chunk)`——把不完整的 UTF-8 尾部
    留到下一个 chunk 拼接，字节不再丢；流结束时 lossy 排空尾部。
  - 覆盖全部同型丢字节点：`glm_anthropic::pump_anthropic_stream`（本次事故
    路径）、`loomy_anthropic::pump_loomy_anthropic_stream`、
    `glm_proxy::pump_stream`（透传 + usage 计量）、`loomy_proxy`（扣分落库
    sse_text）、`glm_web` 两处聚合泵、`qoder` 非流聚合（同时补行缓冲，
    避免 data 行被 chunk 切断）。
  - 回归测试：`decode_stream_chunk_survives_split_multibyte`、
    `decode_stream_chunk_assembles_sse_frame_across_chunks`（243 passed）。
  - 遗留（同类，本轮未动以免与并行会话冲突）：`api.rs` Kimi 非流聚合
    仍是严格 `from_utf8` + `text.lines()`，属同一丢字节模式。

---

## 0.1.42 — 2026-09-17

### 移除

- **🔴 全面移除 Tabbit Browser 相关代码（功能下线）**
  - 背景：Tabbit Browser 侧已无法继续使用，按用户要求把相关代码从代码库中彻底剔除。
  - 删除范围：
    - **6 个专属文件**（已移出到仓库根的 `.removed-tabbit-20260916/`，**非硬删、可恢复**）：
      `wb-switch-core/src/modules/tabbit.rs`、`tabbit_cli.rs`、`tabbit_models.rs`、`tabbit_proxy.rs`、
      `src/pages/TabbitModelsPage.tsx`、`src/components/tabbit-browser-card.tsx`。
    - **后端 15 条路由**：`/api/tabbit/{models,room,proxy-health,status,quota,token,checkin,checkin-all,checkin-one}`、
      `/api/tabbit/accounts{,/import-local,/remove}`、`/tabbit/v1/chat/completions`、`/tabbit/v1/models`。
    - **12 个 handler**、`mod.rs` 的 4 行 `pub mod`、`api.rs` 的 `use` 导入项。
    - **前端**：`App.tsx` 的页面 import / 侧边栏「Tabbit 模型」入口 / `/tabbit-models` 路由；
      `api.ts` 的 11 条 RPC + 整段 Tabbit 函数 + 3 个类型导入；`types.ts` 的 7 个接口；
      `AccountsPage.tsx` 的卡片挂载；`gateway-switcher.tsx` 注释里的网关清单。
    - `README.md` 两条 Tabbit 功能说明；`gateway_overview.rs` 的 `GATEWAYS` 条目（网关总览少一个磁贴）。
  - 🔴 **唯一硬耦合的处理**：`tabbit_proxy::TabbitUpstreamStream` 原被 `workbuddy_proxy.rs` 复用
    （直接删会打断 WorkBuddy 代理）。已**迁入 `workbuddy_proxy.rs` 并改名 `WorkbuddyUpstreamStream`**，
    字段 `pub rx: tokio::sync::mpsc::Receiver<Result<Bytes, String>>` 与行为**完全不变**。
  - 🟢 **有意保留**（不属 Tabbit 功能，仅命名沿用）：`kimi::TabbitLikeOutput`（Kimi 自己的输出枚举，8 处引用）。
  - 影响范围：前端少 1 个页面 + 账号页 1 张卡片 + 网关总览 1 个磁贴；后端 15 条路由消失。
    `/api/tabbit/*` 之后返回 **404 JSON**（命中 `is_api_namespace()` 的 `api` 前缀）；
    `/tabbit/v1/*` 会落回 SPA 兜底（返回 200 HTML）——该前缀本来就未列入 API 命名空间白名单。
    WorkBuddy / GLM / Kimi / Qoder / Grok / Loomy 网关与签到逻辑**均不受影响**。
  - 运行时数据（**未删**，已成孤儿）：`~/.wb-switch/tabbit_accounts.json`、`~/.wb-switch/tabbit_proxy_config.json`。
  - 验证方式：`tsc`（`noUnusedLocals`）exit=0；`cargo check --workspace` 中 core + server **0 错误、零新增告警**；
    前端 `dist/` 产物中 **0 处** `tabbit` 字样；部署后 `GET /api/tabbit/status` != 200 JSON、
    `/api/version` = `0.1.42`、其余网关端点正常。

---

## 0.1.41 — 2026-09-16

### 增强

- **🔴 Anthropic `/loomy/v1/messages` 的「思考内容」现在透传成标准 thinking 块**
  （承接 0.1.40 的保活修复，把「保活占位」升级为「真实内容透传」）
  - 动机：0.1.40 只补 `: ping` 注释占位，思考期内内容层仍是空壳。
    既然思考型模型（GLM-5.3-Flash 等）本就会先吐 `reasoning_content`，
    不如把它转成 Anthropic 原生 `thinking` 块，让支持它的客户端把推理过程展示出来。
  - 实现：`AnthropicSseTransformer` 新增 `BlockKind::Thinking` +
    `open_thinking_block()`；`push()` 的 `reasoning_content` 分支在
    `ensure_started()` 之后打开 thinking 块并按片段下发
    `content_block_delta { type: thinking_delta, thinking: <文本> }`，
    思考结束时由 `close_block()` 自动闭合。
  - 顺序约束：**thinking 块必须排在 text 块之前**；若正文块已打开，
    后续到达的 `reasoning_content` 片段直接丢弃（顺序非法）。thinking 不置
    `emitted` 标志，避免与「已产出正文」诊断逻辑冲突。
  - 保留 `: ping` 保活注释作为兜底：万一上游推理片段间隔 >2s，仍不会真静默。
  - 影响范围：仅 Anthropic 转换器的**事件序列**（新增 thinking 块），
    OpenAI `/chat/completions` 路径不变；非流式响应仍按原逻辑把 reasoning 降级为纯文本（不在本版范围）。

### 已知不修

- 上游 6 组「关思考」参数（`thinking` / `enable_thinking` / `reasoning_effort` …）实测全部无效，无法从源头缩短思考期。
- 严格枚举 `content_block.type` 的下游客户端（若有）可能对新增的 `thinking` 块报错；如发生，回滚点 `bin/wb-switch.exe.old`（0.1.40）可即时切回。

---

## 0.1.40 — 2026-09-14

### 修复

- **🔴 Anthropic `/loomy/v1/messages` 在思考型模型上「首字节延迟 3~10 秒」**
  （Zcode 侧表现为「本地 Loomy 中转(57890) / GLM-5.3-Flash 加载特别慢」）
  - 现象：同一模型、同一提示词，`/loomy/v1/chat/completions` 首字节 **1.34s**，
    而 `/loomy/v1/messages` 的**首个事件要等到 9.99s**（正文开始才算数）；
    客户端在这 10 秒里**一个字节都收不到**，看起来像卡死/加载不动。
  - 根因：`glm_anthropic.rs::AnthropicSseTransformer::push()` 里，
    `delta.reasoning_content` 分支只置 `self.saw_reasoning = true`，
    **不产任何事件**；而 `message_start` 由 `ensure_started()` 发出，
    它只在正文 `delta.content` 或 `tool_calls` 到达时才被调用。
    于是「上游吐思考的整段时间」= 下游零字节静默。
    （GLM-5.3-Flash 实测思考段 933~2929 字符 / 4.77~10.62s。）
  - 修复：`reasoning_content` 到达时立即 `ensure_started()`（下发 `message_start`），
    并按 **2s** 节流补一条 SSE 注释保活行 `: ping`（`keepalive()`）。
    注释行以 `:` 开头，任何符合 SSE 规范的解析器都会忽略，不改变事件语义。
    不新增 `thinking` 内容块，避免严格枚举 `content_block.type` 的客户端报错。
  - 影响范围：仅 Anthropic 转换器的**事件时序**，事件序列语义不变
    （`content_block_start` 仍由正文触发，块 index 递增逻辑未动）。
    非思考型模型行为完全不变（不会走到该分支）。
  - 验证方式：改后 `POST /loomy/v1/messages`（`stream:true`, `max_tokens:2000`）
    应在 **~1.5s** 内收到 `message_start`，思考期每 2s 有 `: ping`，
    正文 delta 与 `content_block_stop` / `message_delta` / `message_stop` 序列完整。

### 已知但不修（本轮实测排除）

- **上游忽略一切「关思考」参数**：`thinking:{type:disabled}`、`enable_thinking:false`、
  `reasoning_effort:"none"`、`chat_template_kwargs:{thinking:false}`、
  `thinking.budget_tokens:0` —— 6 组实测思考字符仍为 933~2929，首正文 4.77~10.62s。
  故**无法靠关思考绕开**，只能在网关侧尽早发事件。
- **思考内容暂不透传**：如需在客户端显示思考过程，可后续将 `reasoning_content`
  映射为 `thinking` / `thinking_delta` 块（须在正文块之前开块并注意 `next_index` 递增）。

---

### 附：同期并入同一二进制上线的改动（2026-09-16）

> ⚠️ **版本号说明**：本仓库是**多人共用同一工作树**，`0.1.40` 号此前已被上方（2026-09-14）条目占用。
> 本次改动又恰好与并行会话的构建同批上线，**共用同一个 0.1.40 二进制**
> （`/api/version` → `0.1.40` / buildTime `2026-09-16 17:24:46 +0800` / pid `29148`），
> 故并入本小节，**不再单开 `## 0.1.40` 标题**（避免同一版本号出现两个小节）。
> 上线过程如实记录：源码与版本号留在共享工作树里，被**并行会话在 17:30:53 构建部署时一并带上线**；
> 17:32 观测到的 57890 短暂 `ECONNREFUSED` 即该次部署的重启窗口，随后恢复正常。

- **「Kimi 上游未返回任何内容」报错现在自带证据（附上游原始帧）**
  - 现象：用户看到 `Kimi 上游未返回任何内容（模型 kimi-k3）：账号可能无该模型权限或上游限流`
    —— 这句话**只有猜测、没有证据**，排查成本全推给了用户（2026-09-16 用户直接问「这是什么意思」）。
  - 根因：上游对「免费账号 + K3」的拒绝方式已从「发 `error` 帧」变成「**开流后一帧正文都不发即结束**」。
    网关识别不到错误帧，只能落到兜底文案，且原始帧被丢弃。
  - 改动：`kimi.rs` 的流式路径（`pump_stream`）与非流式路径各保留最近
    `RAW_TAIL_MAX = 6` 条上游原始帧（每条截断 `RAW_FRAME_MAX_CHARS = 240` 字符，**不含凭据**），
    在「零内容」报错时通过 `raw_tail_suffix()` 附上；
    一帧都没有时明确写成「上游连一帧都没发（TCP/流建立后即结束，通常是上游侧直接拒绝或超时）」。
  - 影响范围：仅错误文案内容，无行为变更；文案变长最多约 1.5KB。
  - 验证方式（**已实测，线上 0.1.40**）：`POST /api/kimi/test {"model":"kimi-k3"}` → 502，报错为
    `Kimi 上游未返回任何内容（模型 kimi-k3）：账号可能无该模型权限或上游限流。最近上游原始帧：{"heartbeat":{}} ⏎ {"eventOffset":1, "done":{}}`
    —— 原始帧**直接坐实**了「上游只发心跳 + done、一帧内容都没有、连 error 帧都不给」；
    `{"model":"kimi-k2d6"}` → 200 `content=OK`、`{"model":"kimi-k2d6-search"}` → 200 `content=OK`（无回归）。
  - 附：本次实测确认 **K3 系列（`kimi-k3` / `kimi-k3-agent-ultra`）在该账号上恒失败**
    （账号 JWT `membership.level=10`、`roles=["video_gen_access"]`，属免费档；上游以「零帧」方式静默拒绝），
    **K2d6 系列（`kimi-k2d6` / `kimi-k2d6-search`）正常 200**。日常用 Instant 系列即可。
  - 🟡 **工作树卫生教训**：本仓库是**多人共用同一工作树**（大量模块长期未提交）。
    未构建的源码改动 + 版本号一旦留在树里，就**会被并行会话的下一次构建/部署一起带上线**（本版本即如此）。
    凡是「改了但不打算马上部署」的改动，要么别改版本号，要么明确告知协作者 —— 否则版本语义会被悄悄改写。

---

## 0.1.39 — 2026-09-16

### 修复

- **🔴 `POST /api/kimi/accounts/add` 响应体 `account` 双重嵌套（扩展因此永远读不到新账号 id）**
  - 现象：浏览器扩展 `kimi-token-sync` 在**账号未命中已有条目**时必经 `addAccount()`，
    而它读的是 `response.account.id`。真实响应却是
    `{"account":{"account":{…},"reason":"added","updated":false},"ok":true}`
    —— 账号对象被套了两层，`.id` 恒为 `undefined`，于是**明明 HTTP 200 却抛
    「写入网关失败（HTTP 200）：{"account":{"account":…`**。
    实测日志：`~/…/kimi-token-sync` 的 `lastResult`（15:45 / 16:36 两次）。
  - 根因：`kimi::add_account()` 本身已返回 `{updated, reason, account}`，
    `api_kimi_add_account()` 又 `json!({ "ok": true, "account": account })` 包了一层。
  - 修复：在 `api.rs` 侧**摊平**——响应改为
    `{ok, reason, updated, account:{…账号字段…}}`，`account` 直接就是账号对象。
  - 影响范围：仅 `/api/kimi/accounts/add` 的响应结构。前端 `KimiPage.addAccount()`
    只忽略返回值并重新拉列表，**不受影响**；扩展因此恢复正常。
  - 验证方式：`POST /api/kimi/accounts/add {"raw_token":"<dummy>"}` →
    `top-level keys = [account, ok, reason, updated]`、`account.id` 为 uuid、
    且 `account.account` 为 `None`（已实测，随后删除该探针账号）。

### 工具链 / 构建

- 🔴 **`crates/wb-switch-server/Cargo.toml` 曾依赖 `futures-util`，重建时切勿漏掉**。
  该文件此前未纳入版本库（工作副本版本号领先 HEAD），重建时被误清空后靠编译器
  `cannot find module or crate \`futures_util\`` ×9 反推出这一个缺失依赖，
  与 `wb-switch-core` 保持同款：`futures-util = { version = "0.3", default-features = false, features = ["std"] }`。
  依赖全集 = `wb-switch-core` + `axum 0.7` + `tokio 1` + `serde_json 1` + `rust-embed 8` + `futures-util 0.3`。
  `build.rs` 只用 std，无需 `[build-dependencies]`。

---

## 0.1.38 — 2026-09-16

### 新增

- **`GET /api/version`（二进制自证）**
  - 返回 `{ok, name, version, buildTime, buildEpoch, gitSha, profile, os, arch, pid, boundPort}`。
  - 新增 `crates/wb-switch-server/build.rs`，在编译期注入 `WB_BUILD_TIME`（UTC+8）、
    `WB_BUILD_EPOCH`、`WB_GIT_SHA`（取不到 git 时降级为 `unknown`，不影响构建）。
  - 影响范围：仅新增端点，无行为变更。
  - 验证方式：`curl -s http://127.0.0.1:57890/api/version` 应返回上述 JSON，
    且 `version` 与 `Cargo.toml` 一致、`buildTime` 为本次构建时间。

### 修复

- **🔴 未知 API 路径不再被 SPA 兜底伪装成 200**
  - 现象：`/api/version` 这类**未注册**的路径会命中静态兜底，返回 `index.html`
    （HTTP 200 / `text/html`）。调用方拿到「假成功」。
  - 危害：部署脚本 `deploy.py` 的健康检查只判 `status == 200`，于是**只要端口有人监听就恒为真**；
    换 exe 后跑的是新二进制还是旧二进制，脚本分辨不出来。
  - 修复：`static_handler` 增加 API 命名空间判定，`/api`、`/v1`、`/kimi/*`、`/grok/*`、
    `/qoder/*` 找不到就返回 `404 + application/json`，绝不回退 HTML。
    （注意 `/kimi`、`/grok`、`/qoder` 单段路径同时是前端页面，只有带斜杠的子路径才算 API。）
  - 影响范围：所有 API 命名空间下的 404 语义；前端 SPA 路由不受影响。
  - 验证方式：`curl -i /api/definitely-not-exist` 应返回 404 JSON；`/kimi`、`/qoder` 页面仍正常。

- **Kimi 凭据校验三连修（同批代码）**
  1. 上游探活由 `GET` 改为 `POST`：`ConfigService/GetAvailableModels` 是标准 JSON 接口，
     `GET` 恒返回 405，旧实现据此把**好账号误判为失效**。
  2. 仅 `401 / 403` 才把账号标记为失效；网络类错误（超时/连接失败/5xx/429）一律视为
     **上游暂时不可达**，保留账号。
  3. 浏览器侧 `access` token 不再覆盖网关侧 `refresh` token（防凭据降级），
     只把更新的 access 存入 `cached_access_token`。
  - 新增 `KimiUpstreamError` 错误分类：`Transport`(503/retryable) / `Unauthorized`(401) /
    `Upstream`(502) / `Local`(404)，接口按类别返回对应状态码与 `code`。
  - 影响范围：Kimi 网关校验、账号池状态流转、浏览器扩展 `kimi-token-sync` v1.4.0。
  - 验证方式：refresh 账号校验 200 且 refresh token 轮换写回；未过期 access 探活 200；
    过期 access → 401 `credential_invalid`；假 refresh → 401；不存在 id → 404。

### 工具链

- **`deploy.py` 健康检查加固 + 版本断言**
  - 健康判定改为「`/api/version` 返回 **真 JSON** 且 `ok=true`」，不再只看状态码。
  - 新增版本断言：重启后服务端自报版本必须等于 `crates/wb-switch-server/Cargo.toml` 中的版本，
    否则视为部署失败并自动回滚。可用 `--expect-version` 覆盖，传空串跳过断言。
  - 失败时打印「实际 served 版本 vs 期望版本」，便于区分「起不来」和「起错了」。
  - 影响范围：仅部署流程，不改变服务行为。

---

## 工具链（2026-09-16 晚，**无 Rust 版本号变更**，exe 未重新构建）

### 修复

- **🔴 `launcher.js` 单实例互斥（修「双看门狗」）**
  - 现象：任务管理器里同时存在**两个** `node.exe ... launcher.js`，各以 30s 周期探测 `57890`；
    后端掉线时两个看门狗会**同时 spawn** 一个 `wb-switch.exe`（一个 `bind` 成功，另一个白跑约 8s 后退出）。
  - 根因：`launcher.js` 有两条互不知情的启动路径 ——
    ① 登录自启：启动文件夹 `autostart-watchdog.vbs`（用 `E:\nodejs\node.exe`，带 `SYLVIA_NO_OPEN=1`）；
    ② 桌面双击：`Sylvia Switch.lnk` → `Sylvia Switch.vbs`（用 `.workbuddy-ai\...\22.22.2-2\node.exe`）。
    用户双击快捷方式时 ① 往往已在运行 → 两个实例并存。
  - 修复：新增 `INSTANCE_PORT = 57891` 作单实例互斥量，`claimSingleInstance()` 尝试回环监听；
    失败（`EADDRINUSE`）即认定已有实例在跑，本实例**不再承担看门狗职责**，只兜一次后端 + 打开窗口后 `exit 0`
    （保住「双击 = 打开界面」的预期）。
  - 为什么用回环端口而不是 PID 文件：进程被强杀/掉电会留下陈旧 PID，且 Windows 会**复用 PID** → 误判；
    端口互斥由 OS 在进程退出时自动释放，天然不会留死锁。
  - 影响范围：仅 `launcher.js`（Node 层，不涉及 exe 与前端）；**已在运行的旧实例仍是旧逻辑，需重启 launcher 才生效**。
  - 验证方式：手工起第二个实例应打印
    `已有 launcher 实例在运行（互斥端口 57891 被占用：EADDRINUSE），本实例兜一次后端并打开窗口后退出` 且 `exit=0`；
    `tools/watchdog_probe.py` 应只列出**一个** `launcher.js` 实例。

### 新增

- **巡检脚本 `tools/monitor.py`（系统 Python，无需 psutil）**：进程 / 端口 `57890` / `GET /api/version`
  （要求真 JSON 且 `pid` 在进程表）/ 部署残留（`.deploy.lock`、`exe.old`）/ 看门狗日志 mtime /
  接口抽样（`/api/status`、`/api/kimi/accounts`、未知 API 必须 404 JSON），末行打印 `ALL GREEN`。
- **探针脚本 `tools/watchdog_probe.py`（psutil，隔离 venv）**：枚举 `launcher.js` 实例数、后端父进程、重复实例判定。
  - 影响范围：仅本地诊断，无服务行为变更。

### 文档订正

- 🔴 **作废旧口诀「`sylvia-launcher.log` mtime 停更 = 看门狗进程死了」**：`launcher.js` 的 `log()` 只写
  `process.stdout`，而两个 vbs 启动器都用 `sh.Run cmd, 0, False` 隐藏启动且**不重定向 stdout**
  → 日志停更几十小时仍可能完全正常。**看门狗存活一律用进程枚举判断**。
