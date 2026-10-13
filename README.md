# Sylvia Switch

多渠道 AI 账号管理与统一网关工具：把散落在十几个 AI 客户端里的登录态、账号池、积分与签到，
收敛到一套本地服务里统一管理，并对外提供 OpenAI / Anthropic 兼容的反向代理接口。

> **[在线演示](https://qwertdjnskjfn.github.io/Sylvia-Switch/)**（只读；账号、积分与请求记录均为虚构数据，所有业务操作已禁用）

<p align="center">
  <img src="public/icon-transparent.png" alt="Sylvia Switch 图标" width="128" />
</p>

## 这是什么

桌面 AI 客户端（WorkBuddy、AutoClaw、TRAE、Loomy、Qwen、Qoder、MiniMax、Kimi、Grok、DuMate、
妙手、ChatGLM、ZCode …）每家一套登录态、一套积分、一套签到。本项目把这些能力收进一个本地服务：

- **统一账号池**：一个渠道多个账号，支持固定账号集合、轮询、区域隔离、熔断冷却与自动换号；
- **统一签到**：所有渠道的每日签到聚合到一个入口，支持自动补签与批量领取；
- **统一网关**：把账号池包装成 OpenAI / Anthropic 兼容接口，供 ZCode、OpenCode、Cursor 等
  客户端当作一个供应商使用；
- **健康与观测**：渠道健康看板、模型目录自动同步、模型实测对话、用量与工具调用监控。

后端是 Rust（axum + tokio），前端是 React + shadcn/ui，由 RustEmbed 编译期嵌入二进制，
部署时**只需一个可执行文件**（Windows 需系统自带 `curl.exe`，见下方「运行前提」）。

## 功能

| 模块 | 说明 |
| --- | --- |
| 账号管理 | 扫码登录、本机导入、手动添加凭据；账号池支持固定集合与多账号互备 |
| 统一签到 | 聚合全部渠道的每日签到：启动即核验、周期补签、一键全量、签到日志与积分快照 |
| 反向代理 | `/glm-proxy`、`/glm-plan`、`/minimax`、`/qwen`、`/kimi`、`/grok`、`/loomy`、`/workbuddy`、`/opencode` 等渠道端点，OpenAI / Anthropic 双协议 |
| 账号池治理 | 区域隔离、选号策略（轮询 / 本机优先 / 仅本机 / 固定集合）、熔断指数退避、失败自动换号 |
| 渠道健康 | 加权健康评分、阈值预警、时间范围对比、横向对比 |
| 模型目录同步 | 定期拉取上游模型目录 → 差异对账 → 自动上架，带变更日志 |
| 模型实测 | 聊天式模型连通性测试，实时延迟与流式响应 |
| 用量统计 | 积分到期监控、请求用量统计、Token 统计、工具调用监控 |
| 自动更新 | GitHub Releases 源检查新版本 |

## 安装与使用

### 方式一：npm

```bash
npm i -g workbuddy-switch
workbuddy-switch                    # 启动本地服务并自动打开浏览器
workbuddy-switch status             # 终端查看当前账号
workbuddy-switch serve --port 57900 # 换端口启动
```

服务默认监听 `http://127.0.0.1:57890`。端口被占用时用 `--port` 换一个即可。

### 运行前提

服务本身是单文件，但**功能可用范围取决于本机环境**：

| 前提 | 影响 |
| --- | --- |
| 系统 `curl`（Windows 10/11 自带） | MiniMax 渠道的对话请求依赖它（该上游证书链需系统 TLS 栈） |
| 各 AI 桌面客户端已安装并登录 | 「从本机导入账号」读取客户端登录态；未安装的渠道需手动粘贴凭据 |
| 网络可达对应上游 | 国际渠道（如 WorkBuddy 国际版、AutoClaw 国际版）需要可用的国际出口 |

没有客户端也不影响使用：可在各渠道页面手动添加账号凭据，或用 OAuth 扫码登录（WorkBuddy 等支持）。

### 方式二：从源码构建

前置：Rust 1.80+、Node.js 20+。

```bash
git clone https://github.com/changexbc/workbuddy-switch.git
cd workbuddy-switch

# 前端先构建（RustEmbed 会把它嵌进二进制，顺序不能反）
npm install
npm run build

# 再构建后端
cargo build --release -p wb-switch-server
# 产物：target/release/wb-switch
```

从源码构建时建议把 `CARGO_TARGET_DIR` 指向一个**纯 ASCII 路径**
（例如 `E:\wb-target`），否则路径含中文时部分构建脚本会失败：

```bash
CARGO_TARGET_DIR=E:/wb-target cargo build --release -p wb-switch-server
```

### 方式三：桌面安装包

前往 [GitHub Releases](https://github.com/changexbc/workbuddy-switch/releases/latest) 下载对应平台安装包。

## 配置说明

项目运行数据统一存放在 `~/.wb-switch/`（Windows 为 `%USERPROFILE%\.wb-switch\`），
按渠道拆分为独立 JSON 文件，全部在首次运行时自动生成，**无需手工创建**：

| 文件 | 用途 |
| --- | --- |
| `accounts.json` | WorkBuddy 主账号库（凭据为 `enc:v1` 加密，密钥派生自本机信息） |
| `autoclaw_accounts.json` | AutoClaw 账号池 |
| `glm_proxy_config.json` | GLM 代理配置与账号池 |
| `minimax_config.json` | MiniMax 渠道账号池与选号策略 |
| `zcode_accounts.json` | ZCode 账号池 |
| `auto_checkin_logs.json` | 统一签到日志 |
| `gateway_api_keys.json` | 网关访问密钥 |

在 Web 界面的「设置」页可以调整签到参数、保活阈值、冷却基数与上限等。
各渠道的账号导入、凭据与代理配置都在对应渠道页面内完成。

## 安全说明

- 所有上游凭据在本地以 `enc:v1`（AES-256-GCM）加密存储，密钥由本机信息派生，不落盘明文；
- 服务默认只监听 `127.0.0.1`，不对外网暴露；
- 网关访问需要密钥鉴权（`Authorization: Bearer <key>`）；
- 本仓库不包含任何真实账号凭据、令牌或个人身份信息。

## 贡献

欢迎提交 Issue 与 PR。新增渠道时请一并补充：后端模块 + 单元测试 + 路由注册 +
统一网关三处注册（`gateway_overview` / `model_sync` / `channel_health`）+ 前端页面 +
统一签到接入（`platform_checkin.rs`）+ CHANGELOG 条目。

## 致谢

感谢 [Linux.do](https://linux.do) 社区。

## 许可

[MIT](./LICENSE)