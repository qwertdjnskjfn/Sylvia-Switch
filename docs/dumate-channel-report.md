# DuMate（百度搭子）渠道 · 交付报告

> 版本 1.0.10（2026-10-01）。契约：`docs/dumate-channel-contract.md`（唯一事实来源，
> §0 含上游协议实测记录）。本报告为该渠道的收尾文档。

## 概述

DuMate（百度搭子）是百度的桌面 AI 办公智能体（Electron 客户端），内嵌 OpenCode 引擎
（`dumate-opencode.exe`，动态端口）。本次交付与 WorkBuddy / AutoClaw / TRAE Work CN
同级的完整渠道「四件套」：

| # | 交付 | 落点 |
| --- | --- | --- |
| 1 | **每日签到**：+500 积分，幂等，进统一签到调度 | `platform_checkin.rs` 新增 `dumate` 平台 |
| 2 | **积分查询**：总额 / 已用 / 订阅包 / 加油包 / 限流状态 | `dumate.rs::get_credits()` + `/api/dumate/credits` |
| 3 | **模型反代**：`/dumate/v1` OpenAI 兼容网关（models + chat/completions，含 SSE 合成） | `api.rs` + `dumate.rs` |
| 4 | **独立前端页面** `/dumate`（侧栏入口）+ 接入统一模型网关三处注册 | `DuMatePage.tsx` + gateway_overview / model_sync / channel_health |

## 上游协议实测记录

**实测时间：2026-10-01**（DuMate 桌面端运行中，引擎 `127.0.0.1:52002`）。
协议来源：开源仓库 **Wang-JQ77/dumate-api**、**daily-auto-checkin**、
**Minatoxiaohu/agent-auto-signin**（协议形态与端点据此定位），再经本机逐一实测确认。
以下数据引自契约 §0，实现照此，勿自行发明。

### 凭据解密（qianfan cookie）

- 凭据文件：`%APPDATA%\qianfan-desktop-app\auth.json`，顶层
  `{cookies(base64), activeProviderType, accountProfiles, activeProfileId}`；
  同目录 `.cookie-key` 为 32 字节 AES 密钥（明文文件，**不是** DPAPI）。
- 解密算法：`base64decode(cookies)` → `iv=[0..12)`、`tag=[12..28)`、`cipher=[28..)` →
  **AES-256-GCM**（key=`.cookie-key`）→ UTF-8 JSON 数组，元素字段
  `{name,value,domain,hostOnly,path,secure,httpOnly,session,expirationDate,sameSite}`。
- 本机实测解出 **51 条 cookie**。

### 签到 / 积分端点（console.bce.baidu.com）

- 请求头：`Cookie`（域匹配 + 未过期 + 特殊字符值加引号）+ `csrftoken`
  （**取 `bce-user-info` cookie 的值**，而非名为 csrftoken 的 cookie）+
  `X-Requested-With: XMLHttpRequest` + `Origin`/`Referer: https://console.bce.baidu.com/` +
  `Content-Type: application/json`。
- `GET /api/dumate/points/loginBonusInfo` →
  `{code:0,success:true,result:{totalTimes,totalPoints,hasIssued,signInDays:["2026-10-01"]}}`；
  HTTP 302/401 或 code=401 → 登录态失效。
- `POST /api/dumate/points/loginBonus`（body `{}`）→ 成功判定
  `code==0 || errno==0 || (result && !result.error) || typeof points=="number"`；
  **+500 积分**；重复领取返回已领语义 → already。
- `GET /api/dumate/points/quota_overview` →
  `{code:0,result:{isSubscribed,usedPoints:"55.73",totalPoints:"16020.00",modelThrottleInfo:{...},subscription:[...],incremental:[...]}}`
  —— **数字是字符串**；`incremental[].packageType:"grant_point"`、`source:"login_bonus"`。

### 模型引擎会话协议（127.0.0.1:{动态端口}）

- **发现**：读 `dumate-opencode.exe` / `dumate-main-server.exe` 进程环境变量
  （tasklist 找 PID → `OpenProcess(QUERY_INFORMATION|VM_READ)` →
  `NtQueryInformationProcess(ProcessBasicInformation)` → PEB+0x20=ProcessParameters →
  PP+0x80=Environment 指针 → `ReadProcessMemory` 读 UTF-16 环境块，≤2MB、NUL 分隔）。
  取 `DUMATE_HOST_URL`（如 `http://127.0.0.1:52002`）或 `OPENCODE_SERVER_PORT`；
  `DUMATE_INAPP_KEY`（64 字符）。**实测 main-server 无 HOST_URL、opencode 有 → 两个进程都要试**。
  覆盖顺序：进程环境 → 本进程环境变量 → 配置文件静态值 → 默认端口 52795 探活。
- 探活：`GET /global/health` → `{"healthy":true,"version":"local"}`。
- 会话：`POST /session`（body `{"title":"...","directory":""}`）→ 200 `{id:"ses_...",...}`。
- 对话：`POST /session/{id}/message`（body
  `{"parts":[{"type":"text","text":"..."}],"headers":{"X-Dumate-AutoModel-Level":"L0|L1|L2"}}`）
  → **同步阻塞直接返回最终 assistant 消息（实测 3.9s，勿做轮询）**；parts 可能多条，
  取所有 `type=="text"` 拼接；`usage` 从 `info.tokens` 取（input/output/total）。
- 清理：`DELETE /session/{id}` → 200 `true`（成功与失败路径都清理）。
- **档位**：lite=L0 / turbo=L1 / ultra=L2，档位在**请求体 headers 字段**，不是 HTTP 头。
- 鉴权头：`X-Dumate-Inapp-Key: <key>` + `Content-Type: application/json`。
- 本机无 HTTP_PROXY 陈旧配置问题（实测环境无代理变量）；v1 不做透传代理，仅状态里报告。

## 实现说明（模块清单）

| 文件 | 职责 |
| --- | --- |
| `crates/wb-switch-core/src/modules/dumate_credentials.rs` | qianfan cookie 的 AES-256-GCM 解密（auth.json + .cookie-key）与 `bce_cookie_header()`（Cookie 头组装 + csrftoken 提取），失败给中文人话 |
| `crates/wb-switch-core/src/modules/dumate.rs` | 渠道核心：引擎发现（进程环境读取 + 缓存）、`/dumate/v1` 数据层（会话反代 / SSE 合成 / 积分查询 / 状态聚合 / 配置读写） |
| `crates/wb-switch-core/src/modules/mod.rs` | 挂载 `dumate` / `dumate_credentials` 两个新模块 |
| `crates/wb-switch-core/src/modules/platform_checkin.rs` | 签到接线：`PLATFORM_DUMATE` 常量、6 个分发函数 arm、`discover_dumate_account()`（`dumate:local` 单账号）、`get_dumate_status()` / `perform_dumate_checkin()` / `get_dumate_credits()` |
| `crates/wb-switch-server/src/api.rs` | 7 条路由（见下）+ handler + 网关三态鉴权 + `is_logged_path` 覆盖 `/dumate/v1` |
| `crates/wb-switch-core/src/modules/gateway_overview.rs` | GATEWAYS 注册 `dumate`（base `/dumate/v1`）+ fetch_models / fetch_accounts arm |
| `crates/wb-switch-core/src/modules/model_sync.rs` | SYNC_SOURCES 加 `dumate` 行（12→13）+ `fetch_dumate()`，测试断言同步更新 |
| `crates/wb-switch-core/src/modules/channel_health.rs` | CHANNELS 加 `dumate` 行（11→12，traffic_source=logs）+ 签到键 arm |
| `src/lib/types.ts` / `src/lib/api.ts` | `DumateStatus` / `DumateCredits` / `DumateCheckinStatus` 类型 + ROUTES 六条 + 导出函数 + `dumateTestChat`（fetch 直连网关） |
| `src/pages/DuMatePage.tsx` | 四卡页面：通道状态 / 每日签到 / 积分余额 / 模型测试 |
| `src/App.tsx` / `src/lib/gateway-registry.ts` | `/dumate` 路由 + 侧栏入口 |

**api.rs 7 条路由**：

| 方法 | 路径 | 作用 |
| --- | --- | --- |
| GET | `/dumate/v1/models` | OpenAI 模型目录（4 模型） |
| POST | `/dumate/v1/chat/completions` | 对话（JSON / SSE 分流），成功后 `log_gateway_request("dumate", ...)` |
| GET | `/api/dumate/status` | 渠道聚合状态（桌面端 / 引擎 / 凭据 / 网关 + 积分快照） |
| GET | `/api/dumate/credits` | 积分余额 |
| GET/POST | `/api/dumate/config` | 配置读取（脱敏）/ 保存 / 轮换密钥 |
| POST | `/api/dumate/checkin` | 本机账号一键签到（status → claim 幂等） |
| GET | `/api/dumate/checkin-status` | 今日签到状态（loginBonusInfo 聚合） |

## 验证记录

**门禁结果（2026-10-01 全过）**：

| 门禁 | 结果 |
| --- | --- |
| `cargo check -p wb-switch-core -p wb-switch-server` | ✅ 通过 |
| `cargo test -p wb-switch-core dumate` | ✅ **29/29** |
| `cargo test -p wb-switch-core platform_checkin` | ✅ **16/16** |
| `cargo test -p wb-switch-core model_sync` | ✅ **18/18**（含 13 网关断言） |
| `cargo test -p wb-switch-core --lib`（全量） | ✅ **654/654** |
| server 单测 | ✅ **6/6** |
| `tsc --noEmit` | ✅ **0 错误** |
| 零自动触发审计（`tokio::spawn` / `fn spawn_` 扫 dumate 两模块） | ✅ **0 命中** |

**本机实测（2026-10-01，DuMate 桌面端运行中）**：

- 引擎发现：`127.0.0.1:52002`（进程环境 `DUMATE_HOST_URL`）；会话创建 + 消息提交
  **3.9s** 同步返回最终 assistant 消息；
- 签到状态：`hasIssued=true`（当日已领）；
- 积分：总额 **16020.00** / 已用 **55.73**。

## 已知边界（v1 明确不做）

1. **不拉起 / 不托管 DuMate 桌面端**——用户自行启动客户端；状态页只读报告
   （未运行时给人话指引「请打开 DuMate 客户端」）。
2. **无 HTTP_PROXY 透传代理兜底**——本机实测环境无代理变量、无此问题；仅状态里报告引擎健康。
3. **无多账号池**——凭据源是本机客户端单登录态，账号恒为 `dumate:local` 单账号。
4. **无 Anthropic `/v1/messages` 桥接**——仅 OpenAI 兼容协议，后续按需补。

## 使用指引（ZCode / 客户端接入）

| 项 | 值 |
| --- | --- |
| Base URL | `http://127.0.0.1:57890/dumate/v1` |
| API Key | `~/.wb-switch/dumate_proxy_config.json` 的 `api_key`（首次访问自动生成），**或**主网关密钥（`glm_proxy_config.json` 的 key，二选一皆放行） |
| 协议 | OpenAI 兼容：`GET /dumate/v1/models` + `POST /dumate/v1/chat/completions`（`stream:true` 支持） |

**模型名映射**（客户端友好，自动归一化）：

| 客户端槽位 | 实际模型 |
| --- | --- |
| `*haiku*`（如 `claude-haiku-*`） | `dumate-lite`（L0 极速） |
| `*sonnet*`（如 `claude-sonnet-*`） | `dumate-turbo`（L1 增强） |
| `*opus*`（如 `claude-opus-*`） | `dumate-ultra`（L2 专业） |
| `dumate-points` | 积分查询（不消耗模型积分） |

将任意 OpenAI 兼容客户端（含 ZCode 的 OpenAI 协议 provider）的 `base_url` 指向
`http://127.0.0.1:57890/dumate/v1`、API Key 填上述密钥即可；模型名填 `dumate-lite/turbo/ultra`
或 haiku/sonnet/opus 系列名均可。快速自检：

```bash
curl http://127.0.0.1:57890/dumate/v1/models \
  -H "Authorization: Bearer <api_key>"

curl http://127.0.0.1:57890/dumate/v1/chat/completions \
  -H "Authorization: Bearer <api_key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"dumate-lite","messages":[{"role":"user","content":"你好"}]}'
```

**前置条件**：DuMate 桌面端已启动并登录（引擎端口动态发现，无需手工配置）；
签到与积分功能还需客户端处于登录态（凭据来自 `%APPDATA%\qianfan-desktop-app\auth.json`，
失效时在 DuMate 客户端重新登录即可）。密钥可在 `/dumate` 页面查看与轮换。
