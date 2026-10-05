# 「DuMate 多账号 + 统一账号管理」交付报告

> 2026-10-01 交付。唯一事实来源：`docs/unified-accounts-contract.md`（同日）。
> 版本：1.0.11（与同日早间「DuMate 4 档位模型并入 /opencode/v1」同属 1.0.11，见 CHANGELOG）。

## 1. 概述

本次交付围绕用户四需求，落地四件套：

| # | 交付 | 一句话说明 |
| --- | --- | --- |
| 1 | DuMate 多账号池 | 「导入本机登录态」或粘贴 cookies 添加池账号（`dumate:pool_N`），签到与积分 per 账号独立执行，全部进统一签到调度 |
| 2 | 一号一 IP | 池账号可配独立出站代理（http/socks5），签到/积分请求按账号走各自代理，切断 IP 维度风控关联；模型调用层仍走本机主账号 |
| 3 | 统一账号管理 | 新增 `GET /api/accounts/unified` 聚合全平台账号（登录态/积分/今日签到/代理/备注 + summary 统计）；账号管理页升级为总览条 + 统一账号表 + 平台筛选 + 跳转渠道页 |
| 4 | 无隐私遮罩 | 全面移除前端隐私遮罩（`privacyMask`/`maskIdentity`），名称一律显示完整全名（备注名优先）；上游已脱敏手机号无法还原，同样以备注优先展示 |

## 2. 设计说明

### 2.1 为什么 DuMate 模型调用层无法多账号

- 凭据源 = `%APPDATA%\qianfan-desktop-app\auth.json`：DuMate 桌面端是 **Electron 单客户端单登录态**，本机只有一份登录凭据；
- 模型引擎（opencode，`dumate-opencode.exe`）的 `INAPP_KEY` / `USER_ID` **绑定桌面端当前登录用户的进程环境**，模型流量的出站由客户端进程决定，网关侧只是转发；
- 因此**多账号只能在「签到/积分」（纯 cookie 接口）层落地**：这类接口只需要带账号自己的 cookies 发 HTTP 请求，与桌面端进程无关。模型调用层仍走本机主账号（契约 §0，UI 文案已在 AccountsPage / DuMatePage 说明）。

### 2.2 一号一 IP 的作用与可行性分层

- **作用**：同 IP 多账号是平台风控的关联性证据（AutoClaw 2026-09-28 批量封禁先例：4 账号同 deviceId + 突发大流量）。一账号一独立出站 IP 切断 IP 维度关联，显著降低连带封禁风险。
- **可行性分层**：
  - 签到/积分 = 纯 HTTP + cookie 请求 → `reqwest::Proxy::all(url)` per-request **完全可行**（`miaoshou.rs` 已有先例），本次在 `dumate.rs::bce_api(_with_cookies)` 加 `proxy: Option<&str>` 参数落地；`socks5://` 由 reqwest socks feature 支持；
  - 模型引擎层**不可行**：引擎进程的出站 IP 无法按账号切换（单进程单出口），不做。

### 2.3 全名无遮罩的两类来源

- 前端隐私遮罩（`privacyMask`/`maskIdentity`，AccountsPage 开关）→ **整体移除**，前端不再自行加星号；
- 上游 API 返回的手机号字段本身已脱敏（如 `180****2401`）→ 我方无从获取全名，**改为「备注名优先」**：有备注显示备注，无备注显示上游字段（用户可在备注里写全名）。
- API key/网关密钥的 `mask_secret`（dumate.rs/grok.rs）是密钥遮罩，安全必要，**保留**，与名称遮罩无关。

## 3. 实现清单

后端（wb-switch-core / wb-switch-server）：

| 文件 | 改动 |
| --- | --- |
| `crates/wb-switch-core/src/modules/dumate_accounts.rs` | 新建。DuMate 多账号池数据层+请求层：`~/.wb-switch/dumate_accounts.json` 原子写存储、`add_account`（cookies 逐条校验）/`import_from_client`（解密桌面端登录态，按 `bce-user-info` 去重指纹）/`update_account`/`remove_account`，以及 per 账号 `account_status/account_checkin/account_credits`（cookie 头用账号自己的 cookies，bce 请求走该账号 proxy；幂等/成功判定/签到日志与 `dumate:local` 同款） |
| `crates/wb-switch-core/src/modules/dumate.rs` | `bce_api`/`bce_api_with_cookies` 增加 `proxy: Option<&str>` 参数（`reqwest::Proxy::all`，解析失败中文报错；None/空 = 直连），本机账号路径行为不变 |
| `crates/wb-switch-core/src/modules/platform_checkin.rs` | `discover_dumate_accounts()` = 本机 `dumate:local`（客户端凭据存在时）+ 池全部条目合并去重，进统一签到调度；`get_platform_status`/`checkin_platform_account`/`get_platform_credits` 的 dumate arm 按 id 分发（local 走客户端凭据路径，`dumate:pool_*` 走 `dumate_accounts::account_*`）；池账号不计入「本机登录」 |
| `crates/wb-switch-core/src/modules/unified_accounts.rs` | 新建。`unified_accounts()` 全平台统一聚合：`all_checkin_accounts`（含 WorkBuddy，不跳过）+ Grok 账号池（无签到仅登录态）；per 账号 `loginState`（WorkBuddy 额外读 auth/refresh 链路 `needs_relogin`）、`credits`（`get_platform_credits` → `{available,total,used}`）、`todayCheckedIn`、`proxy`/`note`（仅 DuMate 池账号）、`gatewayPage` 平台→路由映射；输出 `summary {platforms, accounts, healthy, checkedInToday}` |
| `crates/wb-switch-core/src/modules/mod.rs` | 注册 `dumate_accounts` / `unified_accounts` 两个子模块 |
| `crates/wb-switch-server/src/api.rs` | 新增 6 条路由：`GET /api/accounts/unified`、`GET/POST /api/dumate/accounts`（list 含 `dumate:local` 行 + `isLocal` 标记，**不回传 cookie 明文**只回 `cookieCount`；add）、`/api/dumate/accounts/update|remove|import-client`、`/api/dumate/account/checkin`（local/pool 统一入口） |

前端：

| 文件 | 改动 |
| --- | --- |
| `src/lib/account-extras.ts` | 删除 `getPrivacyMask/setPrivacyMask/maskIdentity/maskEmail/PRIVACY_STORAGE_KEY`，仅保留备注名持久化；头注释声明「隐私脱敏已移除，全界面直显完整全名」 |
| `src/components/account-card.tsx` | 名称一律直显全名（备注名优先），移除 privacyMask 开关与 `maskIdentity` 分支 |
| `src/components/quick-switch-menu.tsx` | displayName 直显全名 |
| `src/pages/AccountsPage.tsx` | 重构为「统一账号管理」：顶部总览条（平台数/账号总数/登录态健康/今日已签 X/Y，来源 unified summary）+ 统一账号表（平台筛选 chips、登录态/积分/今日签到/代理徽标，操作：签到/编辑代理与备注/跳转渠道页；30s 静默轮询，失败保留旧数据）+ DuMate 多账号卡（导入本机登录态/折叠添加表单/单账号签到/行内代理编辑/删除）；旧后端加载失败时新区块整体隐藏，既有 WorkBuddy 卡与平台签到区保留不回退 |
| `src/pages/DuMatePage.tsx` | 新增多账号池卡：池列表（status/lastCheckinDay/cookieCount/proxy）+ 导入本机登录态 + 粘贴 cookies 添加（客户端先校验 JSON）+ 行内代理编辑 + 单账号签到；文案注明「多账号仅用于签到/积分，模型调用走本机主账号」 |
| `src/lib/api.ts` | ROUTES 新增 6 条（`get_unified_accounts` / `add|update|remove_dumate_account` / `import_dumate_client` / `dumate_account_checkin`）+ 对应封装函数 |
| `src/lib/types.ts` | 新增 `UnifiedAccount / UnifiedAccountCredits / UnifiedAccountsSummary / UnifiedAccountsPayload / DumatePoolAccount` 类型 |

## 4. 验证记录（2026-10-01，门禁全过）

```bash
export PATH="/e/mingw64/bin:$HOME/.cargo/bin:$PATH"; export CARGO_TARGET_DIR="E:/wb-target"
cd "E:\WorkBuddy自动签到助手\workbuddy-switch"
```

| 门禁 | 结果 |
| --- | --- |
| `cargo check -p wb-switch-core -p wb-switch-server` | 通过（0 error） |
| `cargo test -p wb-switch-core dumate` | **37 passed / 0 failed** |
| `cargo test -p wb-switch-core platform_checkin` | **17 passed / 0 failed** |
| `cargo test -p wb-switch-core unified` | **8 passed / 0 failed** |
| `cargo test -p wb-switch-core`（全量） | **669 passed / 0 failed**（另 2 ignored） |
| `node node_modules/typescript/bin/tsc --noEmit` | **0 错误**（exit 0） |
| `grep -rn "maskIdentity\|getPrivacyMask" src/` | **0 命中**（account-extras.ts 定义亦已删除） |

## 5. 使用指引

1. **前置**：wb-switch 服务运行中；「导入本机登录态」需要 DuMate 桌面端在本机已登录。
2. **导入本机登录态**：账号管理页（`/accounts`）→「DuMate 多账号」卡 →「导入本机登录态」（DuMate 渠道页 `/dumate` 的多账号池卡同样提供）。后端解密桌面端 cookies，按 `bce-user-info` 去重——与既有账号重复时直接报「已是同一登录态」。
3. **手动添加账号**：「DuMate 多账号」卡 →「添加账号」→ 填名称 + 粘贴 cookies JSON（数组或 `{"cookies":[...]}`，逐条 `name`/`value` 非空）+ 可选出站代理。
4. **配置一号一 IP**：每行「编辑代理」填 `http://host:port` 或 `socks5://host:port`，空 = 直连。代理**只影响该账号的签到/积分出站流量**；模型调用不受影响（始终走本机主账号）。
5. **统一账号表**：账号管理页顶部总览条看四统计（平台数/账号总数/登录态健康/今日已签 X/Y）；统一账号表用平台 chips 筛选；操作列可单账号签到、编辑代理与备注、跳转对应渠道页；30s 自动刷新 + 手动刷新。
6. **自动签到**：池账号已进统一签到调度（`platform_checkin`），自动签到开启时与其它平台账号一起执行；UI 单账号「签到」按钮幂等（重复领取返回已领语义）。

## 6. 已知边界

- **cookies 明文存盘**：池账号 cookies 明文写 `~/.wb-switch/dumate_accounts.json`（atomic_write），与 trae/autoclaw 账号池同安全级别（文件在用户本机 home），v1 不加密；池列表接口不回传 cookie 明文（只回 `cookieCount`）。
- **上游手机号脱敏无法还原**：上游返回的 `180****2401` 这类字段我方无从获取全名，统一账号/账号卡按「备注名优先」展示，建议用户在备注中写全名；前端绝不自行加星号。
- **模型层单账号**：DuMate 模型调用（`/dumate/v1` 及 `/opencode/v1` 的 `dumate-*` 模型）始终走本机桌面端主账号（引擎 `INAPP_KEY`/`USER_ID` 绑定桌面端进程），多账号只覆盖签到/积分；引擎层一号一 IP 不可行（见 §2.1/§2.2）。
- **unified 的 proxy/note 仅 DuMate 池账号由后端下发**，其余账号的备注由前端 account-extras（localStorage）叠加显示。
