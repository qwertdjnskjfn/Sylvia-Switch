# 「注册机」内置说明文档 — Grok 融合网关

> 「注册机」是 Grok 融合网关所驱动的那个**专用 Chrome 实例**的称呼，术语由用户在
> 0.1.50 条目记入 CHANGELOG（用户原话：「没开注册机，Chrome 却总自动打开 grok.com
> 去注册」），**一律使用「注册机」，不得改名**。
>
> 本文档是该功能的完整版说明；界面「工作原理」折叠面板使用本文「注册机工作原理」
> 一节的全文，维护者视角的同源说明另见模块 doc 注释
> （`crates/wb-switch-core/src/modules/grok_register.rs`）。
>
> 实现契约（唯一事实来源）：`docs/grok-register-machine-contract.md`。

## 注册机工作原理

> 注册机是 Grok 融合网关专用的一个 Chrome 实例：独立 profile（`~/.wb-switch/grok-chrome-profile`，与你日常用的 Chrome 完全隔离）、固定调试端口 19233、非 headless 常驻窗口。
>
> 执行流程：
> 1. **手动启动**：你点击「打开注册机」——这是注册机唯一的启动方式，网关不会在开机、后台循环或任何计划任务中自动拉起它。
> 2. **打开注册页**：注册机以调试模式启动 Chrome 并打开 `https://grok.com`；未登录时该页面就是注册页。
> 3. **人工注册**：在注册机窗口里按 grok.com 的流程完成新账号注册（填邮箱、验证码等由你本人操作；注册机不代填、不自动提交）。
> 4. **抓取成果**：注册完成后点「抓取注册成果」，网关经 CDP 的 `Network.getCookies` 读取该实例的 sso / sso-rw 等 cookie（含 httpOnly），拼成账号凭证写入 Grok 账号池（账号 ID `grok_register`）。
> 5. **入池即用**：该账号随即参与 web 通道的轮询调度；之后每次同步/保活都会经同一实例续期，无需重复注册。
>
> 设计纪律：
> - **零自动触发**：模块不注册任何后台循环（无 30 分钟同步、无保活、无体检会启动它）；`status` 接口只读探测，不会启动浏览器。
> - **会话隔离**：专用 profile 不影响日常 Chrome；多个功能共用同一实例，手动「立即同步」会把你当前的登录态写回 autosync 账号。
> - **进程精准**：「关闭注册机」只结束带该 profile 目录命令行参数的 Chrome 进程，绝不误杀日常浏览器。

## 使用步骤

1. **打开注册机**：进入 Grok 融合网关页，找到「注册机 · Grok 账号注册专用浏览器」
   卡片（位于「SSO Cookie 自动同步」卡片之后），点击入口按钮「打开注册机（手动）」。
   这是注册机**唯一**的启动方式——入口点击前界面不请求任何接口，网关也不会在
   开机、后台循环或计划任务中自动拉起它。
2. **人工注册**：在弹出的专用 Chrome 窗口里按 grok.com 的流程完成新账号注册
   （填邮箱、验证码等由你本人操作；注册机不代填、不自动提交）。
3. **抓取成果**：注册完成后，回到卡片点「抓取注册成果」。网关经 CDP 的
   `Network.getCookies` 读取该实例的 sso / sso-rw 等 cookie（含 httpOnly），
   以固定账号 ID `grok_register` 做 upsert 写入 Grok 账号池。
4. **入池即用**：账号池列表随即出现该账号（账号名「注册机注册账号」），
   立即参与 web 通道的轮询调度；之后每次同步/保活都会经同一实例续期，
   无需重复注册。

**接口一览**（均注册于 `crates/wb-switch-server/src/api.rs`，handler 命名 `api_grok_register_*`）：

| 方法 | 路径 | 作用 | 是否启动浏览器 |
| --- | --- | --- | --- |
| GET | `/api/grok/register/status` | 只读状态探测 | 否（恒不） |
| POST | `/api/grok/register/open` | 手动打开注册机 | 是（仅此一处） |
| POST | `/api/grok/register/capture` | 抓取注册成果并入池 | 否（复用已开实例） |
| POST | `/api/grok/register/close` | 关闭注册机 | 否（只结束该实例） |

## 落点文件

| 文件 | 内容 | 说明 |
| --- | --- | --- |
| `~/.wb-switch/grok_register_state.json` | 注册机状态：`phase`（`idle`→`open`→`captured`）、`manualOpenCount`、`lastOpenedAt`、`lastCapturedAt`、`poolAccountId`、`updatedAt` | 经 `atomic_write` 落盘（字段 camelCase）；初始/关闭态为 `idle`，抓取入池后为 `captured` |
| `~/.wb-switch/grok_sso_token.json` | 专用实例抓到的 sso / sso-rw cookie | `status()` 的登录态判定来源：有 sso 或 sso-rw → `logged_in`；文件存在但无 sso → `not_logged_in`；无文件 → `unknown`（`crates/wb-switch-core/src/modules/grok_sso.rs:101`） |
| `~/.wb-switch/grok_accounts.json` | Grok 账号池 | 「抓取注册成果」以**固定账号 ID `grok_register`** upsert（存在则更新 token/status，不存在则 push）；账号名 `注册机注册账号`，provider `web`，tier `free`（`crates/wb-switch-core/src/modules/grok.rs:45`，upsert 写法参考 `crates/wb-switch-core/src/modules/grok_sso.rs:657`） |

## 排障

1. **「未找到 Chrome/Edge 可执行文件」**：注册机复用 `grok_sso` 的 Chrome 定位逻辑
   （`crates/wb-switch-core/src/modules/grok_sso.rs:125`）。在
   `~/.wb-switch/grok_sso_config.json` 里配置 `chrome_path` 指向 Chrome/Edge 可执行文件
   绝对路径（该文件由 `grok_sso` 读写，注册机与 SSO 登录共用同一配置，字段说明见
   `crates/wb-switch-core/src/modules/grok_sso.rs:47`）。
2. **CDP 19233 被占用**：`netstat -ano | findstr :19233` 查占用进程。若为上次残留的
   注册机实例（命令行带 `grok-chrome-profile`），先点「关闭注册机」或手动结束该进程，
   再重新打开。若为其它软件占用，改用它方或结束之。注意：启动时若探测到 19233 已有
   CDP 响应会**直接复用**该实例（`crates/wb-switch-core/src/modules/grok_sso.rs:169`），
   不会重复拉起窗口。
3. **关闭注册机只杀本实例，不误伤日常 Chrome**：「关闭注册机」按命令行匹配进程——
   Windows 经 `Get-CimInstance Win32_Process` 过滤 `Name='chrome.exe'` 且命令行含
   `grok-chrome-profile` 后 `Stop-Process -Force`；其它平台用
   `pkill -f grok-chrome-profile`。**严禁**按进程名无差别杀 chrome，日常浏览器
   不受影响。关闭后状态置回 `idle`（保留 `manualOpenCount` 历史）。
4. **`status` 只读语义**：`GET /api/grok/register/status`（及界面上的状态刷新）
   不会触发注册、不会启动浏览器——`status()` 不得调用 `ensure_browser`、不得启动
   任何进程，返回体 `autoTrigger` 恒为 `false`。唯一会启动浏览器的是
   `POST /api/grok/register/open`，且只由手动点击触发。
5. **与 SSO 同步/保活循环的关系**：注册机**自身没有任何循环**（不注册 30 分钟同步、
   保活、池体检，也无启动时自动触发）。但注册成果入池后，若 SSO 同步/保活循环处于
   开启状态（`grok_sso::spawn_sync_loop` / `spawn_keepalive_loop`，
   `crates/wb-switch-core/src/modules/grok_sso.rs:697` 起），后续续期会经**同一实例**
   （同一 profile / 同一 CDP 端口）完成，无需重复注册。不想让后台循环触碰该实例时，
   按 0.1.50 条目用运行时开关关闭（`grok_sso_config.json` 的 `enabled=false` 等），
   手动「立即同步」仍可用。
