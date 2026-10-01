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
