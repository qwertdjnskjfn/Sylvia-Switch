# 变更记录（wb-switch / Sylvia Switch）

版本号取自 `crates/wb-switch-server/Cargo.toml`，运行时可通过
`curl http://127.0.0.1:57890/api/version` 查询「线上跑的到底是哪一版、哪次构建」。

> 0.1.38 之前的版本没有独立记录，可查 `git log`。

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
