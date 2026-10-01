# 局域网分享 OpenCode 网关（白名单反代）

> 用途：让**同一局域网内的另一台设备**（你自己的手机/笔记本，或朋友）调用本机的
> OpenCode 网关，而**不暴露**管理面（124 条 `/api/*`）与自用密钥。

## 架构（三层防护）

```
对方设备 ──► http://<你的LAN IP>:57900/opencode/v1  ──► 白名单反代 ──► 127.0.0.1:57890/opencode/v1
              Authorization: Bearer lan-xxxx                │
                                                            ├─ 白名单：只放行 /opencode/v1，其余 403
                                                            ├─ 鉴权：只认 lan-* 专用 key，注入真 key 转发
                                                            └─ 限流：每 IP 60 次/分钟
```

| 组件 | 位置 |
|---|---|
| 反代脚本 | `workbuddy-switch/tools/lan_share_proxy.cjs` |
| 启动器（静默） | `lan-share-on.vbs`（双击） |
| 停止器 | `lan-share-off.bat`（双击） |
| LAN 专用 key | `%USERPROFILE%\.wb-switch\lan_share_key.txt`（首次启动自动生成） |
| 访问日志 | `%USERPROFILE%\.wb-switch\lan_proxy.log` |
| PID 文件 | `%USERPROFILE%\.wb-switch\lan_proxy.pid` |

## 使用步骤

1. **双击 `lan-share-on.vbs`** → 反代在 `0.0.0.0:57900` 后台运行（无窗口）。
2. **确认端口在监听**：`netstat -ano | findstr :57900` 应看到 `0.0.0.0:57900 LISTENING`。
3. **从 `%USERPROFILE%\.wb-switch\lan_share_key.txt` 读出 lan-* key**，
   通过密码管理器/阅后即焚链接单独发给对方（**不要**和地址放同一条消息里）。
4. **用完双击 `lan-share-off.bat`** → 进程结束，分享立即终止。

## 发给对方的接入卡（复制这段，key 另行单独发）

```
服务：OpenCode 免费模型网关（局域网共享）
Base URL：http://<你的LAN IP>:57900/opencode/v1
API Key ：单独发送（形如 lan-xxxxxxxx，请勿转发/截图）
模型清单：GET {Base URL}/models
调用：
curl -X POST {Base URL}/chat/completions \
  -H "Authorization: Bearer <你的 lan key>" -H "Content-Type: application/json" \
  -d '{"model":"opencode/mimo-v2.6-flash-free","messages":[{"role":"user","content":"你好"}]}'
说明：模型限流/故障时网关自动切换可用模型，调用方无感
注意：仅局域网内可访问；发现泄露请通知我立即吊销
```

## 从对方设备验证

对方执行（把 IP 和 key 换成实际的）：

```bash
curl http://<LAN IP>:57900/opencode/v1/models -H "Authorization: Bearer <lan key>"
```
- 返回 43 个模型的 JSON = 通了；
- `Connection refused / timeout` = 见下方排查；
- `401` = key 不对；`403` = 路径写错了（只有 `/opencode/v1` 前缀可用）。

## 换 key / 吊销

| 动作 | 做法 |
|---|---|
| 换一把新 key | 删除 `lan_share_key.txt` → 重启反代（新 key 自动生成） |
| 立即终止所有分享 | 双击 `lan-share-off.bat` |
| 只查谁在用 | 看 `lan_proxy.log`（含来源 IP / 路径 / 状态码 / 耗时） |

## 注意事项

- 🔴 **只在需要时开，用完就关**：分享期间端口对同网段所有人可见（虽然没 key 进不来）。
- 🔴 **对方消耗的是你的免费池额度**（不花钱，但会挤占你可用性）；限流 60 次/分钟已内建，可用
  环境变量 `LAN_SHARE_RPM=30` 收紧。
- 🟡 **前提是「你在的网段」与「对方」互通**。若你的 Wi-Fi 是校园网/公共网络（如 `10.x.x.x`），
  设备间可能被 AP 隔离 → 对方连不上；这种情况改用手机热点组一个小网段，或改用隧道方案。
- 🟡 **防火墙**：多数情况下无需处理。若对方连不上而本机 `netstat` 显示已监听，用**管理员** PowerShell 执行：
  ```
  netsh advfirewall firewall add rule name="wb-switch LAN share 57900" dir=in action=allow protocol=TCP localport=57900 profile=private
  ```
- 🟢 反代**不启动时**，本机原有 `127.0.0.1:57890` 用法完全不受影响。

---

## 2026-10-01 加固：对外分享的三道护栏（反代层，不动网关内核）

**背景**：朋友用手机客户端调用时「一直超时」——服务端日志实锤：他选的是 `unorouter/*` 系模型，
实测 **360–647 秒**（6–11 分钟），任何手机客户端都会先超时；更糟的是多条长请求并行时，
**连 3.9 秒的 `dumate-lite` 都被挤到 120 秒并失败**（排队）。

因此在**分享层**（`tools/lan_share_proxy.cjs`）加三道护栏 —— 好处：**你自己内部使用完全不受影响**、
无需重编译部署：

| # | 护栏 | 行为 | 实测 |
| --- | --- | --- | --- |
| ① | **快模型白名单** | 只放行 `dumate-*` 与 `opencode/*`；慢模型直接 **400** + 人话指引（附允许清单与替代建议）。`GET /models` 也会**过滤后**再返回 | `unorouter/glm-4.5-flash:free`：**361 秒 → 0.006 秒 400** ✅ |
| ② | **超时护栏** | 上游超过 `LAN_SHARE_DEADLINE_MS`（默认 150s）仍未开始返回 → 本地 **504** + 人话；已在流式输出中的连接不打断 | 用 5s 护栏 + 89s 的 slow 模型：**5.03s 返回 504** ✅ |
| ③ | **健康路径本地直答** | `/global/health`、`/health`、`/healthz`、`/v1/health` 本地回 200（不打上游、不校验 key） | 客户端健康探测不再 403，避免误判 provider 不可用 ✅ |

**日志增强**：对话请求行现在带 `model=` 与 `models-filter N → M`，一眼能看出对方在用什么。

**可调（环境变量，一般不用动）**

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `LAN_SHARE_MODEL_FILTER` | `on` | `off` = 关闭模型白名单 |
| `LAN_SHARE_ALLOW_MODELS` | `dumate-,opencode/` | 允许的模型名前缀，逗号分隔 |
| `LAN_SHARE_DEADLINE_MS` | `150000` | 上游首字节超时（毫秒） |
| `LAN_SHARE_RPM` | `60` | 每 IP 每分钟请求上限 |

**生效方式**：反代代码变更需重启进程 —— 双击 `funnel-share-on.bat`（或 `lan-share-on.vbs`）。
（LAN key 变更**不需要**重启：key 走文件 mtime 热更新。）

**给朋友的正确用法**：只选 `dumate-lite`（3 秒级）→ 其次 `dumate-turbo` / `dumate-ultra`；开流式；别连点/并发。

---

## 2026-10-01 启动器加固：解决"双击看不到窗口 / 窗口一闪即逝"

**问题**：`funnel-share-on.bat` 曾在末尾加了用外部脚本宿主做的自检。若机器存在**命令黑名单**
（本机 `wmic` 即被拦），被拦程序可能**连带结束整个 cmd 进程** → 窗口一闪即逝，用户看不到任何输出。

**两条启动器（互补）**

| 启动器 | 特点 | 适用 |
| --- | --- | --- |
| **`start-funnel-proxy.vbs`**（推荐） | 纯 VBS 宿主 + `MSXML2.ServerXMLHTTP`，**不调用任何外部辅助程序**；隐藏启动 8443 反代 → 3s 后 HTTP 自检 → **弹 MsgBox 告知成功/失败** + 写一行 `VBS-LAUNCH ... ok=True/False` 到 `funnel-share-on.log` | 平时启动、需要明确反馈 |
| `funnel-share-on.bat` | **零外部程序**（纯 cmd）；自记录包装：无参运行时 `call "%~f0" --logged >> funnel-share-on.log 2>&1` 再 `pause`，窗口即使闪退也能从日志查因 | 需要看完整流程 / 重新下发 Funnel 配置 |

**约束（血泪）**
- .bat 必须 **纯 ASCII + CRLF**：含中文的 `echo` 在 cmd 里会乱码；LF 换行会让 `goto :label` 失效。
- .vbs 必须 **纯 ASCII**（宿主按 ANSI 读；CJK 路径与中文提示一律用 `ChrW` 构造）。
- **不要**在面向用户双击的脚本里引入任何可能被黑名单拦的外部程序。
- **代理进程必须由用户双击启动**：沙箱会回收自动化会话启动的一切进程（含独立派生进程），
  但**验证可完全自动化**（netstat + `/global/health` + 公网四项）。
