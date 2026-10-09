## 2.0.22 — 2026-10-09

### 🔴 P0：隔离实例的 accessToken 过期 ⇒ 签到/余额全 401（本轮最严重）

**现象**：12:50 全量签到，`acc2`(614670) 与 `acc3`(463137) 双双
`签到面板查询失败（invalid access token）`，而主实例正常。

**根因（实测取证）**：`auth.json` 的 `expiresAtMs` ——
| 实例 | 有效期至 | 状态 |
| --- | --- | --- |
| 主（小螺帽） | 13:48 | ✅ 实例在运行，桌面端自动续期 |
| acc2 | **03:10** | ❌ 已过期 588 分钟 |
| acc3 | **03:08** | ❌ 已过期 590 分钟 |

桌面端 accessToken **TTL 仅 3600s**，且**只有桌面端进程运行时才续期**。
主实例常驻所以永远新鲜；隔离实例一关就停摆 ⇒ 1 小时后必然过期。

**修复**：服务端自建续期，不再依赖「用户恰好开着那个实例」。
协议从 asar 逐字抠出（`@mavis/oauth-core`）：
- `endpoint-config.js` → `cn/prod: https://account.minimax.cn/oauth2/token`
- `contracts.js` → `client_id=mcode-public` / `scope=agent.default` / `audience=agent-backend`
- `oauth-client.js#refreshToken()` → 表单
  `grant_type=refresh_token&refresh_token&client_id&scope&audience`

⚠️ **refresh_token 是轮换型的**（用一次即换新；实测第二次拿旧值刷新必
`invalid_grant` HTTP 400）⇒ 刷新成功后**必须原子写回 auth.json**。新增：
`refresh_desktop_credential()` / `ensure_desktop_credential()` /
`write_back_desktop_credential()` / `desktop_token_remaining_ms()`；
签到与余额读取前都会**尽力续期**，仅在「已过期且续不回来」时中止并给出
「需在该实例重新登录」的可操作提示。

**续期闸门**：只有「本账号就是该实例当前登录态」（`desktop_identity_matches`）才允许动
——否则会去刷别人槽位的凭据，把真正主人的 token 轮换掉。
全局 `tokio::Mutex` 串行化：轮换型 token 并发刷新必然「一个成功、另一个 400」。

**实测**：人为把 acc2 的 `expiresAtMs` 改成 2 分钟前 → 签到 `ok=true`，
`expiresAtMs` 自动变为 +60 分钟、`generation` 2→3。✅

### 🔴 P0：「累计积分」显示 0 分 —— 挂在**会删的条目**上

**现象**：卡片上「签到 2026-10-09 · 累计 0 分」，但当天确实领到过。

**根因**：`signin_points_total` 存在**账号条目**里，而条目会被删/重导
（切号、去重、手动移除）。实测：小螺帽 00:42 真领 400（`claimed`，`delta=+400`），
该条目随后被重建 ⇒ 累计归零。

**修复**：新增**按身份记账的积分账本** `minimax_points_ledger.json`
（键 `uid:<realUserID>`，无身份退化 `label:<名字>`）：
- `ledger_add()` —— `claimed` 时与条目同步累加，**跨条目存活**；
- `ledger_points()` —— `uid:`/`label:` 两键取 **max**（历史流水只有名字，避免重复计数）；
- `ledger_backfill_from_history(alias)` —— 从动向流水回填（**幂等**：逐键取 max）；
- `reconcile_points_total()` —— 条目累计与账本对账，**只增不减**；
  `capture_from_desktop()` 导入/更新条目时自动调用 ⇒ 条目重建不再从 0 开始。
- 新增 `POST /api/minimax/points/reconcile` 一键对账。
- 前端「累计」加 title 说明口径：**本机记录的历次 claim 成功之和**，
  同日重复签到不重复累加；权威余额看「积分」。

**实测**：对账后 小螺帽 `0 → 400`（找回 00:42 那笔）。

### P1 账本键不兼容导致回填落空（2.0.19 修）
历史动向早于 `user_id` 字段（只能按名字归并 ⇒ `label:` 键），
而账号查账本用 `uid:` 键 ⇒ 两者对不上，出现「`ledgerTouched>0` 但账号仍是 0」。
修复：`ledger_points()` 两键取 **max**；`ledger_backfill_from_history(alias)`
接收账号池别名表，把历史 `label:` 键**升级**成 `uid:` 键。

### P2 签到 `steps` 恒定记录 `credential/token` 一行
续期可能发生在**更早的余额读取**里（`refresh_credit_snapshot` 也尽力续期），
原先只在 `signin_account` 记 `credential/refresh` 会漏记 ⇒ 排查时看不到续期是否发生。
现恒定输出 `remainingMs` + 说明。

## 2.0.18（并入 2.0.22）— 2026-10-09

### 🔴 部署稳定性：`deploy.py` 拉起的实例会在健康断言通过后**自行消失**

**现象**：`[6/7] backend healthy`（真 JSON + 版本正确）→ 几分钟后端口被拒、
`wb-switch.exe count=0`；看门狗随后拉起的**干净环境**实例长期稳定。

**根因**：`start_backend()` 用 `subprocess.Popen` 且**不传 env**，继承到沙箱注入的
`ELECTRON_RUN_AS_NODE` / `NODE_OPTIONS` / `PYTHONPATH` 等变量。

**修复**：
- `deploy.py` 新增 `clean_env()`，spawn 时剔除注入变量（与 `tools-start-backend.py` 同一清单）；
- 新增 **`[8/8] settle re-check`**：静置 20s 再确认服务仍在，
  把「活不活得久」在部署脚本里就暴露出来，而不是留给用户撞 502。
  可用 `DEPLOY_SETTLE_SECS` 调整时长。

## 2.0.21 — 2026-10-09

### 补：MiniMax 鉴权兼容主网关密钥 + 模型可用性测试登记 minimax 渠道

> ⚠️ **根因归属说明（避免后人重犯）**：本渠道 10-05 起的「回复答非所问 / `input_tokens`
> 恒定 21」被误判为「传输层差异（HTTP 版本 / TLS 后端 / 连接池 / 报文 / curl 子进程）」，
> 实际是**鉴权层**问题：10-07 另一路已定位并修复（桌面端在同一条 `auth.json` 记录内
> **原地轮换 accessToken**，账号导入时的冻结快照失效 → 上游 401），`MiniMaxPage`
> 的状态提示已于 10-07 更新为「对话功能已恢复」。
>
> 本版本补的是**同一问题链上遗漏的两处**（此前一直未被发现）：
> ① 渠道未登记进模型可用性测试的三处统一网关注册；
> ② 独立密钥渠道的鉴权不接受主网关密钥 → 模型自测必然 `auth_failed`。

#### ① 补齐三处统一网关注册（`gateway_overview` / `channel_health` / `model_sync`）
渠道清单来自 `GET /api/gateways/overview` → `gateway_overview::GATEWAYS`（原 13 渠道无 minimax）。
| 文件 | 改动 |
| --- | --- |
| `gateway_overview.rs` | `GATEWAYS` 加 `minimax`（base=`/minimax/v1`）；`fetch_models` → `models_payload_filtered(cfg, None)`；`fetch_accounts` 映射 `enabled`+`status`（冷却/带 last_error → 非 ok，总览会正确标不健康）|
| `channel_health.rs` | `ChannelEntry` 加 minimax（traffic_source=logs）；accounts 分派；签到匹配键加 `"minimax"`；`collect_meta` 并入走 `gateway_snapshot` 的那一组 |
| `model_sync.rs` | 新增 `SyncSource`（Auto + ReportOnly，源=桌面端 `config.yaml` 模型目录）+ `fetch_minimax()`；**单测断言 13 → 14** |

#### ② `api.rs::minimax_key_ok` 兼容主网关密钥
`model_probe::run`（`/api/model-test` 实现）用**主网关密钥**回环自调用目标端点，
而本网关是**独立密钥** → 被判 401/鉴权失败（表现为 **2ms 即返回 `auth_failed`**）。
⚠️ 这是独立密钥渠道的通病，`opencode_proxy_auth_result` 早已踩过同坑（其函数注释即记录了
「WorkBuddy 历史上只发主网关密钥，导致后加的独立密钥渠道被判 401」）。**沿用其既有范式**：

| 情形 | 处理 |
| --- | --- |
| `cfg.api_key` 为空（未配置）| 放行（本机直连模式，同 opencode 口径）|
| 命中 MiniMax 自家密钥 | 放行（原行为）|
| **命中主网关密钥** | **放行 + 记一条请求日志**（`log_gateway_request`，非静默放行）|
| 其他 | 拒绝 |

覆盖 `/minimax/v1/models` 与 `/minimax/v1/chat/completions` 两处调用点（同一函数）。

#### ③ 顺手修 `deploy.py` 第 8 步的既有 bug
`fetch_version()` 返回 `(ok, info)` 二元组，而新加的「静置 20s 稳定性复查」把它当 dict 用
→ `AttributeError: 'tuple' object has no attribute 'get'`（部署本身已成功，仅后处理崩溃）。
已改为解包 `s_ok, s_info = sinfo`。

#### 验证
- `cargo test -p wb-switch-core` → **783 passed / 0 failed**（含同步后的 14 网关断言）；
- 端到端模型自测（`/api/model-test`，页面同接口）：
  | 模型 | 耗时 | 结果 |
  | --- | --- | --- |
  | `MiniMax-M3` | 24755ms | ✅ 回复"收到" |
  | `MiniMax-M2.7` | 12253ms | ✅ |
  | `MiniMax-M2.7-highspeed` | 8590ms | ✅ |
  | `MiniMax-M3.1-Flash-Preview` | **1849ms** | ✅ |

## 2.0.19 — 2026-10-09（账本键兼容，见 2.0.20 条目）

### 修复：模型可用性测试缺 MiniMax 渠道（补齐三处统一网关注册）

#### 现象
「模型可用性测试」页的网关下拉里**没有 MiniMax**，无法对其模型做可用性自测。

#### 根因
渠道清单来自 `GET /api/gateways/overview` → `gateway_overview::GATEWAYS` 常量表，
该表 **13 个渠道中没有 minimax**（`/minimax/v1` 反代虽已存在，但未登记进总览）。
连带 `channel_health` 与 `model_sync` 两处也未登记。

#### 改动（三处统一网关注册，参照 dumate 接入先例）
| 文件 | 改动 |
| --- | --- |
| `gateway_overview.rs` | ① `GATEWAYS` 表新增 `minimax`（base=`/minimax/v1`, page=`/minimax`）<br>② `fetch_models` 加分支：`models_payload_filtered(cfg, None)`（桌面端 config.yaml 真实目录，回退硬编码表）<br>③ `fetch_accounts` 加分支：账号池映射为 `enabled` + `status`（冷却中/带 last_error → 非 ok，总览会正确标不健康）|
| `channel_health.rs` | ① `ChannelEntry` 新增 minimax（traffic_source=logs）<br>② accounts 分派同上<br>③ 签到匹配键加 `"minimax"`<br>④ `collect_meta` 的 match 合并进走 `gateway_snapshot` 的那一组 |
| `model_sync.rs` | ① 新增 `SyncSource`（Auto + ReportOnly，源=桌面端 config.yaml 模型目录）<br>② 新增 `fetch_minimax()`（纯本地读配置，复用 `models_from_openai_list`）<br>③ 单测断言 13 → **14**（`status_payload_lists_all_13_gateways` 改名并同步计数与清单）|

#### 效果
- 模型可用性测试页网关下拉**新增 MiniMax**，其模型（MiniMax-M3 / M2.7 / M2.7-highspeed /
  M3.1-Flash-Preview）可逐个勾选自测；
- 渠道健康度页新增 MiniMax 卡片；
- 模型同步清单新增 minimax 行（Auto/ReportOnly）。

⚠️ **如实说明**：MiniMax **对话链路当前上游侧异常**（2026-10-05 排查结论：请求恒定
`usage.input_tokens=21`、答非所问）。本次修的是**渠道登记缺失**，让「模型目录/账号池/
健康度」这些可靠部分可见可测；**对话自测仍会失败** —— 那是真实状态，本次不做粉饰。

#### 验证
`cargo test -p wb-switch-core` → **783 passed / 0 failed**（含同步后的 14 网关断言）。

