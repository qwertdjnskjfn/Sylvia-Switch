# 三网关拆分设计说明（TRAE Work CN / WorkBuddy / OpenCode）

> 版本：0.1.72（2026-09-22）｜ 对应需求：TRAE / WorkBuddy 模型功能模块全面重构（三项要求）
> 状态：**增量 1 已上线**；增量 2–4 按下表推进。

---

## 一、重构后的模块划分

### 1.1 后端（`crates/wb-switch-core/src/modules/`）

| 网关 | 模块 | 存储 | 对话端点 | 注册表 id / base |
| --- | --- | --- | --- | --- |
| **TRAE Work CN** | `trae_accounts.rs`（账号池/签到/续期/扣分）<br>`trae_proxy.rs`（模型目录/权益）<br>`trae_acp.rs`（会话栈，待用） | `~/.wb-switch/trae_accounts.json`<br>`~/.wb-switch/trae_proxy_config.json` | `GET /trae/v1/models`（目录）<br>`POST /trae/v1/chat/completions`（**V1 未开放**，需 `CreateChatSession` 会话态） | `trae-work-cn` / `/trae/v1` |
| **WorkBuddy** | `workbuddy_proxy.rs` | `~/.wb-switch/workbuddy_proxy_config.json`<br>`~/.wb-switch/workbuddy_exe.json` | `POST /workbuddy/v1/chat/completions`<br>`GET /workbuddy/v1/models`<br>`POST /workbuddy-native/v1/chat/completions`（内置模型，ACP 真流式） | `workbuddy` / `/workbuddy/v1` |
| **OpenCode** | **待建**（增量 3）；当前复用 `workbuddy_proxy.rs` 的 server 发现/健康探测能力 | — | **待建** `/opencode/v1/*` | 待登记 |

公共基建（三网关共用，不重复实现）：
`account_protection.rs`（保护/扣分策略）、`channel_health.rs`（通道健康）、
`gateway_overview.rs`（总览聚合）、`model_usage.rs` / `token_stats.rs`（用量统计）。

### 1.2 前端（`src/`）

| 网关 | 页面 | 路由 | 导航 label |
| --- | --- | --- | --- |
| TRAE Work CN | `pages/TraeWorkCnPage.tsx` | `/trae-work-cn` | TRAE Work CN |
| WorkBuddy | `pages/WorkbuddyPage.tsx` | `/workbuddy` | WorkBuddy |
| OpenCode | `pages/OpencodePage.tsx` | `/opencode` | OpenCode |

- 旧合并页 `TraeWorkbuddyProxyPage.tsx` **已删除**；旧路径 `/trae-workbuddy` → **重定向**到 `/trae-work-cn`。
- 侧栏数据源：`lib/gateway-registry.ts`（纯展示层，三条独立条目，图标 `Blocks` / `Cpu` / `Terminal`）。

---

## 二、各网关的职责边界

| 维度 | TRAE Work CN | WorkBuddy | OpenCode |
| --- | --- | --- | --- |
| **管什么** | TRAE 客户端模型目录（7 场景 × 倍率）、TRAE 账号池与续期、倍率驱动的选型 | WorkBuddy 桌面端模型代理、上游通道与保活、内置模型（ACP）接入 | 本机 OpenCode server 的 provider / 模型清单，**仅免费模型** |
| **不管什么** | 不碰 WorkBuddy / OpenCode / GLM 的账号与密钥 | 不碰 TRAE 账号池；不拥有 provider 清单（归 OpenCode 页呈现） | 不碰任何账号池（OpenCode 无账号概念，只有 provider 凭据） |
| **密钥** | TRAE 自有密钥（`keySource: trae`） | WorkBuddy 自有密钥（可轮换） | 独立密钥（待建，**不复用**上述两者） |
| **保活对象** | TRAE 客户端登录态（快照 refresh + 到期日） | **上游进程/server**（看门狗 + 端口探测） | **本机 OpenCode server 进程**（`/global/health`） |
| **扣分/风控** | 上游限流记入账号扣分态（`note_trae_rate_limit`） | 负缓存 + 连续失败计数（`is_neg_cache_error` / `last_discovery_ok_ms`） | 连续失败计数驱动 server 健康态 |
| **速度优先依据** | `consumption_rate` 倍率升序（**真实数据，已落页面**） | flash/free 类名优先（**占位**，直连后替换为真实 TTFB） | 免费模型优先（成本 0） |
| **通道态** | 目录可用 / 对话未开放（显式徽标） | `direct` / `opencode-fallback`（**当前为兜底**） | 清单可用 / 端点待建（显式徽标） |

### 关键边界：OpenCode 与 WorkBuddy 的关系变化

```
重构前：WorkBuddy 网关 ──借道──> 本机 OpenCode server ──> 上游 provider
重构后：WorkBuddy 网关 ──直连优先──> 上游            （增量 4 目标）
                        └─失败回落──> 本机 OpenCode server ──> 上游 provider
        OpenCode 网关 ─────────────> 本机 OpenCode server（清单/免费模型/保活）
```
⇒ OpenCode server 从「WorkBuddy 的私有传输层」**升格为独立网关的上游**，同时**降格为 WorkBuddy 的兜底链路**。

---

## 三、迁移后账号的归属关系

### 3.1 结论（含对原前提的纠正）

| 池 | 文件 | 所属网关 | 本次是否搬迁 |
| --- | --- | --- | --- |
| TRAE 账号池 | `~/.wb-switch/trae_accounts.json` | **TRAE Work CN 网关** | **否 —— 数据本就独立** |
| GLM/AutoClaw 池 | `~/.wb-switch/autoclaw_accounts.json` | GLM 代理 | 否 |
| Qoder 池 | `~/.wb-switch/qoder_accounts.json` | Qoder / Qwen 网关 | 否 |
| WorkBuddy | **无账号池** | —（"账号"= provider 清单） | — |
| OpenCode | **无账号池** | —（凭据在 provider 配置内） | — |

**事实认定**：需求原文提到「GLM 代理中现有的 TRAE Work CN 账号池」——
实测 `glm_proxy.rs` 提及 `trae` = **0 行**，TRAE 账号从来就在**自己的文件与自己的模块**里。
所谓"在 GLM 代理中"，指的是 **`GlmProxyPage.tsx` 页面里渲染了 TRAE 账号池的 UI**（约 30 处代码），
且同一账号池**同时**出现在合并页 —— 属于**UI 重复寄居**，不是数据归属错位。

### 3.2 本次迁移的实际动作

- **搬 UI**：`GlmProxyPage.tsx` 移除 TRAE 区块（JSX 83 行 + 5 个 `trae*` state + 5 个函数 + useEffect 调用 + 2 个类型导入）；
  TRAE 账号池 UI 落位到 `TraeWorkCnPage`。
- **不动数据**：`trae_accounts.json` 与 `/api/trae/*` 一字未改。
- **归属关系不变**：账号始终属于 TRAE Work CN 网关；迁移只改「在哪被看到」，不改「归谁所有」。

---

## 四、变更影响

### 4.1 兼容性

| 面 | 影响 | 处置 |
| --- | --- | --- |
| 旧前端路径 `/trae-workbuddy` | 会 404 | 已加重定向 → `/trae-work-cn` |
| 后端 API | **零破坏**：`/api/trae/*`、`/api/trae-proxy/*`、`/api/workbuddy-proxy/*` 全部未改 | — |
| 对话端点 | `/trae/v1/*`、`/workbuddy/v1/*`、`/workbuddy-native/v1/*` 未改 | — |
| 账号文件 | 未改（无数据搬迁） | — |
| 网关总览 | 新增 `trae-work-cn`、`workbuddy` 两条 id（前端分组会自动并入） | 由 `gateway-registry.ts` 三条件驱动显示 |
| 密钥 | 三网关各自独立；TRAE 仍在迁移窗口内兼容旧密钥 | 窗口结束后切到 TRAE 自有密钥 |

### 4.2 遗留与风险

| 项 | 状态 | 风险/说明 |
| --- | --- | --- |
| TRAE 对话转发 | ❌ 未开放 | 上游要求 `CreateChatSession` 会话态；单发请求固定回 `3003 all models failed`。需补会话栈（`trae_acp.rs` 已有部分能力）。 |
| WorkBuddy 直连 | ⏳ 未实现 | 现走 OpenCode 兜底。**直连协议未知** —— 需逆向 WorkBuddy 桌面端的模型请求（其 AI 内核即内嵌 OpenCode，provider 表由 WorkBuddy 注入）。 |
| OpenCode 独立端点 | ⏳ 未建 | 当前页只呈现清单与状态，已在 UI 显式标注「待建」，避免对外宣称不存在的 base。 |
| TRAE 模型数 | 23（实测去重）vs 需求写的 24 | 7 场景合并去重得 23；其中 `refactor_*` 4 个是 `glm-4.7` 的场景别名，**真实独立模型约 19 个**。差 1 待复核（可能含某场景独有条目）。 |
| 版本号占用 | 0.1.71 被并行会话于 2026-09-22 重用 | 本项目多人共用工作树的老问题；本次按惯例取 **0.1.72**。 |

### 4.3 后续增量与验收口径

| 增量 | 内容 | 验收 |
| --- | --- | --- |
| 2 | TRAE 六项能力深化（保护/扣分策略接入账号行、选型参考补"任务场景↔模型"建议文案） | 页面能显示保护态与扣分原因；选型建议可读 |
| 3 | OpenCode 独立模块 + `/opencode/v1/{models,chat/completions}` + 免费白名单 | `/opencode/v1/models` **只返回免费模型**；白名单可覆盖 |
| 4 | WorkBuddy 直连优先 + OpenCode 兜底 | 直连成功时通道态= `direct`；直连失败自动回落且页面如实标注 |

---

## 五、验证记录（增量 1）

| 检查 | 结果 |
| --- | --- |
| `tsc --noEmit` | ✅ 0 错误 |
| `vite build` | ✅ 成功（19.9s） |
| `cargo check -p wb-switch-core -p wb-switch-server` | ✅ 0 错误 |
| `GlmProxyPage.tsx` 残留 trae 提及 | ✅ 0 处 |
| 旧合并页文件 | ✅ 已删除 |
| 三路由可达 | ✅ `/trae-work-cn`、`/workbuddy`、`/opencode` 已注册（含旧路径重定向） |
