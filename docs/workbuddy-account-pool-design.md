# WorkBuddy 账号池 · 设计规范

> 版本线 2.1.2 起。回答三件事：**账号管理方式**、**类型区分逻辑**、**与网关流程的集成方式**。

---

## 0. 先纠正一个前提

**账号池已经存在**，不是从零新增。运行位置与本仓库的关系：

| 组件 | 位置 |
| --- | --- |
| 账号库（唯一事实来源） | `~/.wb-switch/accounts.json` |
| 池运行态（冷却/熔断/计数/模型限流） | `~/.wb-switch/workbuddy_gateway_state.json` |
| 池配置 | `~/.wb-switch/workbuddy_gateway_config.json` |
| 实现 | `crates/wb-switch-core/src/modules/workbuddy_gateway.rs` |
| 对外接口 | `/workbuddy-pool/v1/{status,models,config,accounts/pause,chat/completions,…}` |

> ⚠️ 因此**不存在「国外版账号管理方式」与「国内版账号管理方式」两套流程** ——
> 只有**一套流程 + 一个区域字段**。这是本设计最需要先对齐的一点。

---

## 1. 类型区分逻辑（国外版 / 国内版）

### 判据：**由账号的登录域后缀派生**，不是单独维护的字段

```rust
// workbuddy_gateway.rs::pool_account_from_value
let region = if domain.ends_with(".ai") { "intl" } else { "cn" };
```

| 登录域 | 派生 region | 含义 |
| --- | --- | --- |
| `www.workbuddy.ai` | `intl` | 国外版（国际区）|
| `www.workbuddy.cn` | `cn` | 国内版（国服）|
| 其他 / 缺失 | `cn` | 兜底归国内区 |

**为什么用派生而不是独立字段**：`domain` 本就是登录时拿到的**事实**（你从哪个站登的），
再维护一个 `region` 字段只会产生**两处真相**——一旦不同步，就会出现
「界面说是 intl、实际按 cn 发请求」这种极难排查的错位。

### 派生规则的三个已知特性

1. **不可手工指定**：想换区 = 换登录域重新登录（`pool_account_from_value` 无 override 入口）。
2. **区域影响出站路径**：`intl` 区的 chat/RPC 会带上 `intl_proxy` 的判定
   （当前该配置为空 = 直连，见 `intl_proxy_config` 文档）。
3. **同名模型两区各有一套后端映射** —— 同一模型名在 `cn` / `intl` 可能落到不同上游。

### 请求侧的区域约束

客户端在请求体顶层传 `region: "intl" | "cn"`（读完剥掉、不外发上游），
由 `RegionFilter` 约束选号：

| 取值 | 语义 |
| --- | --- |
| `Intl` / `Cn` | **只**在该区选号；选不到就返回错误，**绝不**把请求丢给对侧账号 |
| `Unknown`（缺省）| 无约束，两区都可选 |

> 🔴 这条是硬约束：跨区硬塞会让「我要 1M 上下文版」静默降级成国服版，
> 症状是「模型能力莫名变差」而非报错，极难发现。

---

## 2. 账号管理方式

### 2.1 总表（现状 + 落点）

| 操作 | 现状 | 落点 |
| --- | --- | --- |
| **添加** | ✅ 走通用账号库 OAuth 登录（登录哪个域就进哪个区）| `POST /api/oauth/start` + `/api/oauth/status`；批量可走 `POST /api/import` |
| **删除** | ✅ 通用账号库删除（池是消费者，下次同步即消失）| `POST /api/delete { accountId }` |
| **用户暂停 / 恢复** | ✅ **本次新增**（详见 2.2）| `POST /workbuddy-pool/v1/accounts/pause { uid, paused }` |
| **系统禁用**（自动）| ✅ session 死亡（12153）时自动出池，需重新登录 | 池内部自动，无用户入口（也不该有）|
| **状态监控** | ✅ 池状态 + 账号明细 + 区域聚合 + 我做的健康面板 | `GET /workbuddy-pool/v1/status` |
| **选号模式** | ✅ balance / rotate / pinned | `POST /workbuddy-pool/v1/config { mode, pinnedUid }` |

### 2.2 🔴 本次新增：用户暂停（`paused`）——与系统禁用严格分离

**问题**：原实现里只有一个 `disabled`，且**只由 SessionDead 自动置位，
没有任何复位路径**。用户想「先别用这个号」时无手段；硬用一个语义还会造成误诊：

| | `disabled`（系统）| `paused`（用户，**新增**）|
| --- | --- | --- |
| 谁置位 | 上游判定凭证失效（12153）| **用户**手动 |
| 含义 | 「这号废了」| 「我想先不用它」|
| 恢复路径 | 重新登录换凭证 | **一个开关** |
| 展示措辞 | 已出池 | 已暂停（手动）|

**关键实现约束**（单测 `user_pause_is_separate_from_system_disabled` 固化）：

1. `healthy = !disabled && !paused && 未冷却 && 未熔断` —— 两者都退出选号；
2. 暂停**绝不写** `disabled`（否则会被误报成凭证失效、引导用户去重登）；
3. **恢复时不得顺手清冷却/熔断**——只为不改语义；顺手清会把一个已熔断的号
   立刻重新入池，把「我不用它」变成「它又在报错」；
4. 状态文件 `paused` 字段带 `serde(default)` ⇒ **向后兼容**，旧 state 读为 `false`。

### 2.3 🔴 脱敏 uid 的回写问题（实现细节，易踩）

`pool_status()` 输出的 uid 是**脱敏**的（`mask_uid`：前 8 位 + `…`）。
前端只能拿到这个值，若写操作要求完整 uid 就会「界面看得到、点不动」。

**解法**：写接口接受**完整 uid 或唯一前缀**，由 `Pool::resolve_uid` 反解；
**多命中即 400 拒绝**（不猜），避免误停用别的账号。

---

## 3. 与现有网关流程的集成方式

### 3.1 集成点只有一个：**选号函数**

```
客户端请求
  └─ 协议入口（/chat/completions、/messages、/responses）
       └─ 鉴权（workbuddy_pool_auth_ok_region：区域参与密钥校验）
            └─ 【集成点】pick_account_for_region(model, tried, region_filter)
                 ├─ 区域过滤：只在该区候选里挑
                 ├─ 逐条闸门：disabled / paused / 冷却 / 熔断 / 在途 / 模型限流 / tried
                 └─ 选出 → 用该账号凭证打上游 → 失败则记错误、换号重试
```

**新增 `paused` 的接入方式是「加一条闸门」**——`Entry::healthy()` 里加一个 `!paused`，
其余流程**一行未改**。这是刻意的：账号池的状态判定集中在一处，
新增状态不应散落到各个调用点（否则必然出现「某条路径忘了判」的漏洞）。

### 3.2 账号库 → 池的同步

`accounts.json` 是**唯一事实来源**；池通过 `sync_accounts()` 增量同步：

| 账号库变化 | 池的反应 |
| --- | --- |
| 新增（含 OAuth 登录）| 加入池，初始状态健康、计数归零 |
| 删除 | 从池中移除 |
| token 轮换 | 由池**写回**账号库（`writeback_tokens_at`）——刷新型凭证必须原子写回 |

### 3.3 诊断出口

| 出口 | 用途 |
| --- | --- |
| `region_block_reasons(model, region)` | 选不到号时**逐条说明为什么**（禁用/暂停/模型冷却/冷却/熔断/在途），拼进客户端 503 文案 |
| 统计页「账号池健康」面板 | 区域结论 + 账号明细 + 暂停/恢复操作 |
| 判据：**`lat=0` vs `lat≈5s`** | 0ms = 预检拒绝（配额/暂停/无号）；5s = 出口不通（链路）|

---

## 4. 状态监控指标（`/workbuddy-pool/v1/status`）

| 层 | 字段 |
| --- | --- |
| 池级 | `total` / `healthy` / `cooling` / `model_cooling` / `breaker` |
| 区域级 | `regions.{cn,intl}.{accounts,healthy,models}` |
| 账号级 | `uid`(脱敏) / `nickname` / `region` / `credits` / `credit_expires_at` / `cooldown_kind` / `until` / `breaker_until` / `in_flight` / **`paused`** / `disabled` / `reason` / `model_cools[]` / `success_count` / `err_total` |

---

## 5. 账号管理能力矩阵（2026-10-13 补齐）

全部入口已收到**多账号网关（cloud）**区块，无需跳页。

| 能力 | 接口 | 说明 |
| --- | --- | --- |
| **查看** | `GET /workbuddy-pool/v1/status` | 池级 + 区域级 + 账号级（uid 脱敏）|
| **添加** | `POST /api/oauth/start` → `/api/oauth/status` | **登录即入库**：从哪个域登录就进哪个区（这是 region 的派生来源）|
| | `POST /api/import` | 批量导入；缺失 `access_token` 的记录会被跳过 |
| **编辑** | `POST /workbuddy-pool/v1/accounts/update` | 白名单字段，**PATCH 语义**（见 5.1）|
| **删除** | `POST /workbuddy-pool/v1/accounts/delete` | 账号库级；池在下一次 `sync_accounts()` 自动移除（不依赖重启）|
| **暂停/恢复** | `POST /workbuddy-pool/v1/accounts/pause` | 用户意图，见 §2.2 |
| **清冷却** | `POST /workbuddy-pool/v1/accounts/clear-cooldown` | 手动干预，见 5.3 |

### 5.1 可编辑字段（白名单，服务端强制）

| 字段 | 可改 | 备注 |
| --- | --- | --- |
| `nickname` | ✅ | 允许清空（展示回落 uid）|
| `domain` | ✅ ⚠️ | **决定区域归属**（`.ai→intl` / 其余→cn）——改它等于让账号换区，UI 有显式提示 |
| `access_token` / `refresh_token` | ✅ | 仅在需手工替换凭证时填；**不许清空**（清空等于弄死账号）|
| `expiresAt` / `refreshExpiresAt` | ✅ | ms epoch，必须为正 |

**不可编辑**：`id`（删除/导出主键）、`uid`、`auth_raw` / `profile_raw`（上游原始态，
池 uid 也取自 `auth_raw.uid`）、`createdAt`（审计）。改动它们会让删除、导出与池选号**对不上号**。

**写语义**：PATCH（只改传入字段）+ **全有或全无**（任一字段越界/校验不过 ⇒ 整体拒绝，
不做部分写入——否则会出现「以为改了 A，结果 B 也悄悄变了」）。

### 5.2 权限模型

| 层 | 说明 |
| --- | --- |
| 网络面 | 网关仅绑 `127.0.0.1`，不构成新的暴露面 |
| 管理面 | `/workbuddy-pool/v1/accounts/*`、`/workbuddy-pool/v1/config` 属 **webui 自用组**，同 `/config` 口径 |
| 对外入口 | `/workbuddy-pool/v1/{chat,responses,messages}/completions` 需**网关 key**（且 region 参与密钥隔离）|

> 即：**「能打开这个页面」= 拥有池管理权**。无独立的 per-account ACL ——
> 本地单机管理面，按机器边界授权。

### 5.3 状态处理（五种状态，语义与处置各不相同）

| 状态 | 谁置位 | 恢复方式 | 可否手动改 |
| --- | --- | --- | --- |
| **冷却中**（soft）| 429 限流，指数退避 600s 起 | 到期自动恢复 | ✅ 清冷却 |
| **熔断中**（breaker）| 连续失败喂出，指数加长 | 到期自动恢复 | ✅ 清冷却（计数也清）|
| **模型级冷却** | 上游「该模型频率上限」 | 按上游给出的重置时间 | ✅ 清冷却 |
| **出池**（`disabled`）| session 死亡（12153）| **重新登录换凭证** | ❌ 不能改（是凭证失效的事实）|
| **暂停**（`paused`）| **用户**手动 | 一个开关 | ✅ 用户开关 |

🔴 **三条不可动摇的约束**（均有单测）：
1. 暂停**绝不写** `disabled`；
2. **恢复暂停不得顺手清冷却**（否则把「我不用它」变成「它又在报错」）；
3. **清冷却绝不清** `disabled` / `paused` / 出池原因（`reason`）。

## 6. 区域隔离 / 自动切换 / 轮询模式 / 导入路径（2026-10-13 补齐）

### 6.1 严格区域隔离（硬约束，已实现）

| 层 | 实现 | 语义 |
| --- | --- | --- |
| 选号 | `RegionFilter::allows()` | `Intl` 只选 intl 账号，`Cn` 只选 cn 账号，**候选集直接按区过滤** |
| 缺号时 | `pick_account_for_region` 返回 `None` | 报「region=X 区内无可用账号」——**绝不**把请求丢给对侧账号 |
| 请求侧 | 请求体 `region` 字段（读完剥掉不外发）| 客户端显式指定区域 |

**为什么必须硬隔离**：两区同名模型可能落到不同上游。若跨区硬塞，
「我要 1M 上下文版」会**静默降级**成国服版——症状是「模型能力莫名变差」而非报错，
极难发现。

> 唯一例外是上游 `11102`（model service info not found）时的**自动翻转**：
> 说明本轮账号区域与模型区域不匹配，把 filter 翻到对侧继续在本轮重试。
> 这是**上游明确告知错配**后的自愈，不是「随便借一个号」。

### 6.2 自动切换机制（已实现）

```
一次池请求的换号循环：

选号 → 占在途租约 → 打上游
  ├─ 2xx 成功        → 记 note_success → 返回（命中即锁定）
  ├─ 非致命错误      → note_error（按 ErrKind 冷却该号）
  │                   → uid 进 tried → 循环，选中**下一个**账号
  ├─ 致命错误        → 直接回客户端（不换号）
  └─ 租约占不到      → 换号重试
全部 tried 完        → 报「区内无可用账号」+ 逐条原因
```

| 错误类别 | 是否换号 | 副作用 |
| --- | --- | --- |
| 软限流（429）| ✅ 换号 | 该号按指数退避冷却（600s 起）|
| 模型频率上限 | ✅ 换号 | 该号**该模型**冷却至上游重置时间 |
| 5xx | ✅ 换号 | 喂熔断计数（不设账号级冷却）|
| 传输层（超时/DNS）| ✅ 换号 | **不喂熔断**（避免瞬时网络问题误杀账号）|
| 401 / 上下文超长 | ❌ 不换号 | 明确错误直接回客户端 |

🔴 **关键前提：切换的目标必须存在。** 该区**只有 1 个账号时，自动切换无号可切**
——机制在，容量不在。这是**账号供给问题，不是代码问题**：
`intl` 区当前只有 1 个账号，故该区一旦被限流/失效，整区不可用
（2026-10-13 当日三次报障均源于此）。**根治 = 给该区补第二个账号。**

### 6.3 轮询模式（配置面板已暴露）

| 模式 | 选号策略 | 适用 |
| --- | --- | --- |
| `balance`（默认）| 到期分层（最早到期优先）+ 档内加权 + 防惊群 + LRU 兜底 | 保护即将到期的账号，默认 |
| `rotate` | **烧号轮询**：当前号可用就继续用，不可用才换下一个 | 想均摊消耗、明确要「轮着用」 |
| `pinned` | 指定 uid；不可用时回落普通选号 | 定点调试 |

> 「自动轮询账号」若指**轮着烧号** ⇒ 把选号模式切到 `rotate`（cloud 区块 → 网关配置）。
> 若指**账号健康巡检** ⇒ 那是 `balance_refresh_minutes`（默认 15 分钟一次余额巡检）。

### 6.4 添加 / 导入路径（三条直导 + 一条文件）

**新增账号不再需要先准备文件。** cloud 区块的「添加账号」提供三条直导路径：

| 路径 | 交互 | 接口 | 适用 |
| --- | --- | --- | --- |
| **本机导入** | 一键：读本机 WorkBuddy 桌面端已登录态 | `POST /api/import-local` | 桌面端已登录 |
| **浏览器登录** | 一键：拉起授权页 → 2s 轮询 → 自动入库 | `/api/oauth/start` + `/api/oauth/status` | 要登一个新号 |
| **手动粘贴** | 内联表单，**必填仅 token**（形态参照 MiniMax `/accounts/add`）| `POST /workbuddy-pool/v1/accounts/add-direct` | 手上已有 token |
| 文件导入 | 选 JSON → 预览 → 勾选 | `POST /api/import` | **仅**用于迁移存量 JSON |

#### 6.4.1 手动粘贴的字段与去重

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `access_token` | ✅ | 主凭据 |
| `domain` | ✅ | **决定区域归属**（`.ai→intl` / 其余→cn`）|
| `refresh_token` | | 有就填，便于自动续期 |
| `nickname` / `uid` | | 选填；`uid` 留空时由 `id` 兜底（`account_uid` 的三级回退）|
| `expires_at` | | 留空 ⇒ 后端从 JWT 的 `exp` 解析 |

🔴 **去重按 `access_token`**（不是 uid/email）：手动添加通常**两者都没有**，
若沿用 `upsert_collected_account` 的身份去重，重复点击就会得到 N 个重复账号
（实测踩中，单测 `add_by_token_creates_and_dedups_by_token` 守住）。
同一 token 再次提交 = 原地覆盖（沿用原 `id`/`uid`/`createdAt`）。

⚠️ `domain` 选错 = 进错区：海外账号只连海外服务、国内账号只连国内服务，**事后不能互通**。
UI 在该字段下有常驻警示。

⚠️ 文件导入仍要求 **JSON 数组**，每项含 `access_token`、`domain` 与 `auth_raw.uid`/`uid`/`id` 之一；
**脱敏导出的文件不含 token，会被整条跳过**（导入前体检见 `tools-import-file-check.py`）。
#### 6.4.2 国内版 / 国际版：数据差异与登录入口

**记录 schema 完全一致**（同一个 `accounts.json` 条目形状），差异只在 `domain` 值：

| | 国内版 | 国际版 |
| --- | --- | --- |
| `domain` | `www.workbuddy.cn` | `www.workbuddy.ai` |
| API 基址 | `https://www.codebuddy.cn` | `https://www.workbuddy.ai` |
| 区域头 | 不带 | `Origin` / `Referer` / `X-Auth-Refresh-Source` |
| 池内 region | `cn` | `intl` |
| keycloak realm | 国服 | 国际（**跨区 refresh 必然 `invalid_grant`**）|

⇒ **兼容策略：零 schema 变更**。用 `domain` 后缀作为唯一判据（与池选号 / 签到 / credits 同口径），
**不引入** `isIntl` 之类的冗余布尔字段——那会产生第二处真相。

**三条添加路径的区域能力**（2.1.8 起）：

| 路径 | 能否指定版本 | 原因 |
| --- | --- | --- |
| 浏览器登录 | ✅ **可选**（cn / intl）| `oauth_start(region)` 按区域选 API 基址与区域头 |
| 手动粘贴 | ✅ **可选**（域下拉）| 表单里直接选 `domain` |
| 本机导入 | ❌ **不可选** | 只能读桌面端**当前登录的那一个**（`domain` 来自 auth 文件）；要换版本先在桌面端切换 |

🔴 **domain 兜底**：上游若不回 `domain`，必须按所选区域回填
（intl → `www.workbuddy.ai`），否则记录里是空串 ⇒ 池按后缀派生 region 落回 `cn`
⇒ **国际号被当成国内号**。兜底必须在**建请求头之前**算（`apply_region_headers` 按 domain 判区域）。


## 7. 边界与未做的事

- **不能让用户手工指定 region**（见 §1 的理由：避免第二处真相）。
- **`disabled` 不给用户开关**——它是凭证失效的**事实**，给按钮是假的；
  恢复只能靠重新登录。
- **暂停不做批量**（当前账号量级个位数，一个按钮足够；上量后再考虑）。
- **删除不做池内单独删除**——池是消费者，删账号库记录即自动同步，
  在池里做「单独移除」会产生第三处真相。
- **单区单账号是结构性单点**（当前 intl 区即如此）：该号暂停/限流/失效 = 整区不可用。
  这是**账号供给问题**，不是代码问题；解除需为 intl 区补第二个账号。
