//! 平台账号一键切换：把账号池里的凭证写回本机客户端的登录态文件。
//!
//! - AutoClaw：`%APPDATA%/autoclaw/auth.json`，`token` / `refreshToken` 为
//!   Electron `enc:`（AES-256-GCM，密钥来自同目录 `Local State` 的 DPAPI 密文）。
//! - TRAE Work CN：`%APPDATA%/TRAE SOLO CN/User/globalStorage/storage.json`，
//!   登录态是 `iCubeAuthInfo://icube.cloudide` 的 byteCrypto 信封；
//!   设备私钥条目 `iCubeAuthInfo://icube-dc:<userId>` 需随账号一起换，
//!   否则下次续期会拿错私钥。
//!
//! 切换前会关闭对应客户端（避免客户端回写覆盖），切换后重新拉起。

use serde_json::{json, Value};

use crate::modules::config::{atomic_write, now_ms};
use crate::modules::vscode_cn_inject::encrypt_windows_gcm_v10;

/// 平台标识
pub const PLATFORM_AUTOCLAW: &str = "autoclaw";
pub const PLATFORM_TRAE: &str = "trae_work_cn";

// ---------------------------------------------------------------------------
// AutoClaw
// ---------------------------------------------------------------------------

/// 把账号池条目写回本机 `auth.json`。返回备份文件路径。
pub fn autoclaw_write_login(account: &Value) -> Result<Option<String>, String> {
    let dir = crate::modules::platform_checkin::autoclaw_data_dir()
        .ok_or_else(|| "无法定位 AutoClaw 数据目录".to_string())?;
    let path = dir.join("auth.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 AutoClaw auth.json 失败（请先安装并登录 AutoClaw）: {e}"))?;
    let mut auth: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 AutoClaw auth.json 失败: {e}"))?;

    let access = account
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "该账号缺少 accessToken，请先刷新再切换".to_string())?;
    let refresh = account
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "该账号缺少 refreshToken，请重新导入".to_string())?;

    // 池内 token 为明文（导入时已去 Bearer 前缀），写回时按客户端格式加密。
    let access_plain = access.strip_prefix("Bearer ").unwrap_or(access);
    let refresh_plain = refresh.strip_prefix("Bearer ").unwrap_or(refresh);

    let key = crate::modules::platform_checkin::autoclaw_encryption_key(&dir)?;
    let enc_access = encrypt_windows_gcm_v10(&key, access_plain.as_bytes())
        .map_err(|e| format!("加密 AutoClaw access token 失败: {e}"))?;
    let enc_refresh = encrypt_windows_gcm_v10(&key, refresh_plain.as_bytes())
        .map_err(|e| format!("加密 AutoClaw refresh token 失败: {e}"))?;

    let user_id = account_user_id(account);
    let enc_token = format!("enc:{}", base64_encode(&enc_access));
    let enc_refresh_token = format!("enc:{}", base64_encode(&enc_refresh));
    let now = now_ms();

    auth["token"] = json!(enc_token);
    auth["refreshToken"] = json!(enc_refresh_token);
    auth["updatedAt"] = json!(now);
    if let Some(device_id) = account.get("deviceId").and_then(Value::as_str) {
        if !device_id.is_empty() {
            auth["deviceId"] = json!(device_id);
        }
    }
    // userInfo 里的 user_id 决定客户端显示与本地发现 ID，必须一起换；
    // 手机号（user_phone / phone）是 `enc:` 密文，用同一把密钥重新加密。
    if let Some(uid) = &user_id {
        let mut user_info = auth
            .get("userInfo")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        user_info["user_id"] = json!(uid);
        // 池条目 label 形如 `180****2401`（脱敏）；无法反推明文，故只在
        // 已有 phone 密文可解时替换，否则保留原密文（不影响登录与切换）。
        if let Some(phone) = account
            .get("phone")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if let Ok(enc) = encrypt_windows_gcm_v10(&key, phone.as_bytes()) {
                let payload = format!("enc:{}", base64_encode(&enc));
                user_info["user_phone"] = json!(payload);
                user_info["phone"] = json!(payload);
            }
        }
        auth["userInfo"] = user_info;
    }

    let backup = backup_file(&path)?;
    let content = serde_json::to_string_pretty(&auth)
        .map_err(|e| format!("序列化 AutoClaw auth.json 失败: {e}"))?;
    atomic_write(&path, &content).map_err(|e| format!("写入 AutoClaw auth.json 失败: {e}"))?;

    // 客户端还有两份缓存也存着登录态，一起换掉，避免启动时用缓存覆盖回旧账号。
    let _ = write_token_cache(&dir, &enc_token, &enc_refresh_token, now);
    if let Some(uid) = &user_id {
        let _ = write_user_cache(&dir.join("user-cache.json"), uid);
    }

    Ok(backup)
}

/// 更新 `token-cache.json`（结构与 auth.json 同款 `enc:` 凭证）。
fn write_token_cache(dir: &std::path::Path, token: &str, refresh: &str, now: i64) -> Result<(), String> {
    let path = dir.join("token-cache.json");
    let mut cache: Value = if let Ok(text) = std::fs::read_to_string(&path) {
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    cache["token"] = json!(token);
    cache["refreshToken"] = json!(refresh);
    cache["updatedAt"] = json!(now);
    let content = serde_json::to_string_pretty(&cache)
        .map_err(|e| format!("序列化 token-cache.json 失败: {e}"))?;
    atomic_write(&path, &content).map_err(|e| format!("写入 token-cache.json 失败: {e}"))
}

/// 更新 `user-cache.json` 的 userId（手机号字段是旧账号密文，保留不动）。
fn write_user_cache(path: &std::path::Path, user_id: &str) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Ok(()),
    };
    let mut cache: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    cache["userId"] = json!(user_id);
    cache["updatedAt"] = json!(now_ms());
    let content = serde_json::to_string_pretty(&cache)
        .map_err(|e| format!("序列化 user-cache.json 失败: {e}"))?;
    atomic_write(path, &content).map_err(|e| format!("写入 user-cache.json 失败: {e}"))
}

/// 池条目对应的 AutoClaw user_id（优先显式字段，其次从 accessToken 载荷解析）。
pub fn account_user_id(account: &Value) -> Option<String> {
    for pointer in ["/userId", "/user_id", "/userInfo/user_id"] {
        if let Some(id) = account.pointer(pointer).and_then(Value::as_str) {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    // 池条目 id 形如 `autoclaw:<user_id>`。
    if let Some(id) = account.get("id").and_then(Value::as_str) {
        if let Some(rest) = id.strip_prefix("autoclaw:") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    let token = account.get("accessToken").and_then(Value::as_str)?;
    let payload = token.split('.').nth(1)?;
    let decoded = base64_decode(payload)?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get("user_id")
        .and_then(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
}

// ---------------------------------------------------------------------------
// TRAE Work CN
// ---------------------------------------------------------------------------

/// 把账号池条目写回 TRAE `storage.json`。返回备份文件路径。
pub fn trae_write_login(account: &Value) -> Result<Option<String>, String> {
    // `TraeSession`（解密上下文）是 platform_checkin 的私有结构，加解密必须
    // 在模块内部完成，因此这里转调 `trae_replace_login`：
    // 它复用续期时的同一套 tables / table_start，替换登录态与设备私钥条目后写盘。
    crate::modules::platform_checkin::trae_replace_login(account)
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 备份原文件（同名 `.bak-<时间戳>`），返回备份路径。
fn backup_file(path: &std::path::Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = path.with_extension(format!("json.bak-{stamp}"));
    std::fs::copy(path, &backup).map_err(|e| format!("备份 {} 失败: {e}", path.display()))?;
    Ok(Some(backup.to_string_lossy().to_string()))
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let padded = match text.len() % 4 {
        2 => format!("{text}=="),
        3 => format!("{text}="),
        _ => text.to_string(),
    };
    base64::engine::general_purpose::STANDARD
        .decode(padded.trim())
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(text.trim())
                .ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn autoclaw_account(id_suffix: &str) -> Value {
        json!({
            "id": format!("autoclaw:{id_suffix}"),
            "label": "测试",
            "accessToken": "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VyX2lkIjo1NjAxMjh9.sig",
            "refreshToken": "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VyX2lkIjo1NjAxMjh9.sig2",
            "deviceId": "dev-64-chars",
            "status": "ok",
        })
    }

    #[test]
    fn user_id_falls_back_to_pool_id_and_jwt() {
        // 显式 userId 字段优先
        let mut a = autoclaw_account("abc");
        a["userId"] = json!("explicit");
        assert_eq!(account_user_id(&a), Some("explicit".to_string()));
        // 其次从池 id 解析
        assert_eq!(account_user_id(&autoclaw_account("from-id")), Some("from-id".to_string()));
        // 最后解析 access token 载荷（数字 user_id）
        let jwt_only = json!({
            "accessToken": "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VyX2lkIjo1NjAxMjh9.sig"
        });
        assert_eq!(account_user_id(&jwt_only), Some("560128".to_string()));
    }

    #[test]
    fn base64_helpers_roundtrip() {
        let original = b"hello-wb-switch";
        let encoded = base64_encode(original);
        assert_eq!(base64_decode(&encoded).as_deref(), Some(original.as_slice()));
        // URL-safe 无填充也能解
        assert_eq!(base64_decode("aGVsbG8").as_deref(), Some(b"hello".as_slice()));
    }

    #[test]
    fn client_image_names_cover_supported_platforms() {
        // tasklist 的 IMAGENAME 过滤器必须带 .exe
        assert_eq!(client_image_name(PLATFORM_AUTOCLAW), Some("AutoClaw.exe"));
        assert_eq!(client_image_name(PLATFORM_TRAE), Some("TRAE SOLO CN.exe"));
        assert_eq!(client_image_name("unknown"), None);
    }

    /// 回归：曾因「拼接 id 字符串比较」把任意池条目都判成已激活。
    #[test]
    fn already_local_only_matches_same_uid() {
        let local = "autoclaw:1daf28d6620e82fa5d7af0a04630b834";
        let other = "autoclaw:eb5b1479e05f209f5bdfb421968787e2";
        // 命中：池 id 的 uid 与本机 uid 相同
        assert!(autoclaw_is_local(local, &json!({"id": local})));
        // 不命中：不同 uid（即便二者形态一致）
        assert!(!autoclaw_is_local(local, &json!({"id": other})));
        // 显式 localDiscoveryId 标记优先
        assert!(autoclaw_is_local(
            local,
            &json!({"id": other, "localDiscoveryId": local})
        ));
        assert!(!autoclaw_is_local(
            local,
            &json!({"id": other, "localDiscoveryId": "autoclaw:someone-else"})
        ));
    }

    /// 抽出 AutoClaw 判定逻辑以便单测（不依赖真实 auth.json）。
    fn autoclaw_is_local(local_id: &str, account: &Value) -> bool {
        if let Some(marked) = account.get("localDiscoveryId").and_then(Value::as_str) {
            return marked == local_id;
        }
        let Some(local_uid) = local_id.strip_prefix("autoclaw:") else {
            return false;
        };
        let id = account.get("id").and_then(Value::as_str).unwrap_or("");
        if local_uid == id.strip_prefix("autoclaw:").unwrap_or(id) {
            return true;
        }
        account_user_id(account).is_some_and(|uid| uid == local_uid)
    }
}

// ---------------------------------------------------------------------------
// 切换编排：关客户端 → 写登录态 → 重启客户端
// ---------------------------------------------------------------------------

/// 一键切换入口：`platform` = `autoclaw` / `trae_work_cn`。
///
/// 流程：找到池条目 → （可选）关闭客户端 → 写登录态 → （可选）重启客户端。
/// 返回给前端的摘要包含备份路径与是否关闭了客户端进程。
pub async fn switch_account(platform: &str, id: &str, restart: bool) -> Result<Value, String> {
    let account = resolve_account(platform, id)?;
    let source = account
        .get("_source")
        .and_then(Value::as_str)
        .unwrap_or("pool")
        .to_string();

    // 目标就是本机当前登录账号时无需切换（幂等返回，避免白关一次客户端）。
    // 判定不只看 `_source`：前端点池卡片时 id 是池条目，但账号可能恰好就是
    // 本机已登录那个（例如刚切过去），此时按「池条目的 localDiscoveryId 命中」
    // 或「uid 与实时登录 uid 相同」也判为已激活。
    if source == "local" || is_already_local(platform, id, &account) {
        return Ok(json!({
            "ok": true,
            "platform": platform,
            "id": id,
            "alreadyActive": true,
            "closedProcesses": 0,
            "launched": false,
            "note": "该账号已是本机客户端当前登录账号，无需切换",
        }));
    }

    let closed = match close_client(platform).await {
        Ok(count) => count,
        Err(e) if e.contains("仅支持 Windows") => 0,
        Err(e) => return Err(format!("关闭客户端失败: {e}")),
    };

    let backup = match platform {
        PLATFORM_AUTOCLAW => autoclaw_write_login(&account)?,
        PLATFORM_TRAE => trae_write_login(&account)?,
        _ => return Err(format!("不支持的平台: {platform}")),
    };

    let launched = if restart {
        match launch_client(platform) {
            Ok(()) => true,
            // 客户端没装在常见路径不算失败：登录态已写好，用户手动打开即可。
            Err(e) => {
                mark_local_discovery(platform, id);
                return Ok(json!({
                    "ok": true,
                    "platform": platform,
                    "id": id,
                    "closedProcesses": closed,
                    "backup": backup,
                    "launched": false,
                    "note": e,
                }));
            }
        }
    } else {
        false
    };

    // 回写「本机当前登录」标记，使后续切换能正确识别为已激活。
    mark_local_discovery(platform, id);

    Ok(json!({
        "ok": true,
        "platform": platform,
        "id": id,
        "label": account.get("label").cloned().unwrap_or(Value::Null),
        "closedProcesses": closed,
        "backup": backup,
        "launched": launched,
    }))
}

/// 切换成功后把池条目的 `localDiscoveryId` 指向新账号，并清除旧账号的该字段。
fn mark_local_discovery(platform: &str, id: &str) {
    let path = match platform {
        PLATFORM_AUTOCLAW => crate::modules::autoclaw_accounts::accounts_path(),
        PLATFORM_TRAE => crate::modules::trae_accounts::accounts_path(),
        _ => return,
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(accounts) = root.get_mut("accounts").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in accounts.iter_mut() {
        let entry_id = entry.get("id").and_then(Value::as_str).unwrap_or("");
        if entry_id == id {
            entry["localDiscoveryId"] = json!(id);
        } else if entry.get("localDiscoveryId").is_some() {
            if let Some(obj) = entry.as_object_mut() {
                obj.remove("localDiscoveryId");
            }
        }
    }
    if let Ok(content) = serde_json::to_string_pretty(&root) {
        let _ = atomic_write(&path, &content);
    }
}

/// 池条目是否就是本机当前登录账号（判定「无需切换」）。
///
/// 关键：**只比较账号 uid**，不能比较拼接后的 id 字符串——池 id 与本地
/// 发现 id 形态都是 `autoclaw:<uid>`，字符串比较会把任意池条目都误判成命中。
fn is_already_local(platform: &str, id: &str, account: &Value) -> bool {
    match platform {
        PLATFORM_AUTOCLAW => {
            let Some(local_id) = crate::modules::autoclaw_accounts::local_discovery_id() else {
                return false;
            };
            // 池条目里带 `localDiscoveryId` 是导入本机快照时写的，最可靠。
            if let Some(marked) = account.get("localDiscoveryId").and_then(Value::as_str) {
                return marked == local_id;
            }
            // 否则比较 uid：本机 uid 来自 `autoclaw:<uid>`，池条目 uid 取
            // `account_user_id`（显式字段 → 池 id 后缀 → JWT 载荷）。
            let Some(local_uid) = local_id.strip_prefix("autoclaw:") else {
                return false;
            };
            if local_uid == id.strip_prefix("autoclaw:").unwrap_or(id) {
                return true;
            }
            account_user_id(account).is_some_and(|uid| uid == local_uid)
        }
        PLATFORM_TRAE => {
            let Some(local_uid) = crate::modules::platform_checkin::trae_local_user_id() else {
                return false;
            };
            if id == "trae_work_cn:local" {
                return true;
            }
            let target_uid = account
                .get("userId")
                .and_then(Value::as_str)
                .or_else(|| id.strip_prefix("trae_work_cn:").filter(|rest| *rest != "local"));
            target_uid.is_some_and(|uid| uid == local_uid)
        }
        _ => false,
    }
}

/// 把前端传来的各类 id 解析成账号池条目：
/// - 池条目 id（`autoclaw:<uid>` / `trae_work_cn:<userId>`）→ 直接取池条目；
/// - 本机实时登录 id（`autoclaw:<uid>` 无池条目、`trae_work_cn:local`）→
///   现场从客户端登录态生成一份同结构快照（标记 `_source=local`，用于幂等短路）。
fn resolve_account(platform: &str, id: &str) -> Result<Value, String> {
    let pool_lookup = match platform {
        PLATFORM_AUTOCLAW => crate::modules::autoclaw_accounts::find_stored(id),
        PLATFORM_TRAE => crate::modules::trae_accounts::find_stored(id),
        other => return Err(format!("不支持的平台: {other}")),
    };
    if let Ok(mut account) = pool_lookup {
        if let Some(obj) = account.as_object_mut() {
            obj.insert("_source".to_string(), json!("pool"));
        }
        return Ok(account);
    }

    // 兜底：尝试「本机实时账号」——先刷新其凭证，再包装成池条目同结构。
    match platform {
        PLATFORM_AUTOCLAW => {
            let local_id = crate::modules::autoclaw_accounts::local_discovery_id();
            if local_id.as_deref() != Some(id) {
                return Err(format!("账号不存在: {id}"));
            }
            let auth_path = crate::modules::platform_checkin::autoclaw_data_dir()
                .ok_or_else(|| "无法定位 AutoClaw 数据目录".to_string())?
                .join("auth.json");
            let auth: Value = serde_json::from_str(
                &std::fs::read_to_string(&auth_path)
                    .map_err(|e| format!("读取 AutoClaw auth.json 失败: {e}"))?,
            )
            .map_err(|e| format!("解析 AutoClaw auth.json 失败: {e}"))?;
            let token = crate::modules::platform_checkin::read_autoclaw_token()
                .map_err(|e| format!("本机 AutoClaw 登录态不可用: {e}"))?;
            let refresh_enc = auth
                .get("refreshToken")
                .and_then(Value::as_str)
                .ok_or_else(|| "auth.json 缺少 refreshToken".to_string())?;
            let refresh = crate::modules::platform_checkin::autoclaw_decrypt_token(refresh_enc)?;
            Ok(json!({
                "id": id,
                "label": auth.pointer("/userInfo/user_phone").cloned().unwrap_or(Value::Null),
                "accessToken": token.trim_start_matches("Bearer "),
                "refreshToken": refresh.trim_start_matches("Bearer "),
                "deviceId": auth.get("deviceId").cloned().unwrap_or(Value::Null),
                "_source": "local",
            }))
        }
        PLATFORM_TRAE => {
            let local_id = crate::modules::platform_checkin::trae_local_pool_id();
            if id != "trae_work_cn:local" && local_id.as_deref() != Some(id) {
                return Err(format!("账号不存在: {id}"));
            }
            let snapshot = crate::modules::platform_checkin::trae_local_snapshot()
                .map_err(|e| format!("读取本机 TRAE 登录态失败: {e}"))?;
            let mut account = snapshot;
            if let Some(obj) = account.as_object_mut() {
                obj.insert("id".to_string(), json!(id));
                obj.insert("_source".to_string(), json!("local"));
            }
            Ok(account)
        }
        other => Err(format!("不支持的平台: {other}")),
    }
}

/// 客户端进程映像名（tasklist 的 IMAGENAME 过滤器需要带 `.exe`）。
fn client_image_name(platform: &str) -> Option<&'static str> {
    match platform {
        PLATFORM_AUTOCLAW => Some("AutoClaw.exe"),
        // TRAE Work CN 客户端的实际映像名（国外版/Code 版是别的名字）
        PLATFORM_TRAE => Some("TRAE SOLO CN.exe"),
        _ => None,
    }
}

/// 关闭客户端并等待退出（Windows：tasklist + taskkill，先温和后强制）。
pub async fn close_client(platform: &str) -> Result<usize, String> {
    let image = client_image_name(platform)
        .ok_or_else(|| format!("不支持的平台: {platform}"))?;
    #[cfg(target_os = "windows")]
    {
        let rows = crate::modules::process::windows_tasklist_image_rows(image);
        let pids: Vec<u32> = rows.iter().map(|row| row.pid).collect();
        if pids.is_empty() {
            return Ok(0);
        }
        for pid in &pids {
            let pid_s = pid.to_string();
            let _ = crate::modules::process::run_cmd_timeout(
                "taskkill",
                &["/PID", &pid_s, "/T"],
                10,
            );
        }
        if !crate::modules::process::wait_windows_pids_gone(&pids, std::time::Duration::from_secs(8))
            .is_empty()
        {
            for pid in &pids {
                let pid_s = pid.to_string();
                let _ = crate::modules::process::run_cmd_timeout(
                    "taskkill",
                    &["/PID", &pid_s, "/T", "/F"],
                    10,
                );
            }
            let _ = crate::modules::process::wait_windows_pids_gone(
                &pids,
                std::time::Duration::from_secs(4),
            );
        }
        Ok(pids.len())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = image;
        Err("平台账号切换目前仅支持 Windows".to_string())
    }
}

/// 启动客户端（如果可定位到可执行文件）。
pub fn launch_client(platform: &str) -> Result<(), String> {
    let (folder, exe_name) = match platform {
        PLATFORM_AUTOCLAW => ("autoclaw", "AutoClaw.exe"),
        PLATFORM_TRAE => ("TRAE SOLO CN", "Trae CN.exe"),
        _ => return Err(format!("不支持的平台: {platform}")),
    };
    let label = if platform == PLATFORM_AUTOCLAW { "AutoClaw" } else { "TRAE Work CN" };
    match find_client_exe(folder, exe_name, label) {
        Some(path) => launch_detached(&path).map_err(|e| {
            format!("启动 {label} 失败: {e}（路径: {}）", path.display())
        }),
        None => Err(format!(
            "已写入登录态，但未能定位 {label} 可执行文件，请手动打开客户端"
        )),
    }
}

/// 启动客户端。优先 `ShellExecute`（带 elevate 权限语义，能处理要求 UAC
/// 提权的应用，manifest 里 `requestedExecutionLevel=requireAdministrator`
/// 的 exe 直接 `Command::spawn` 会报 os error 740）。
#[cfg(target_os = "windows")]
fn launch_detached(exe: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt as _;

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> isize;
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let file = wide(exe.as_os_str());
    // SEE_MASK_NOASYNC(0x100) | SEE_MASK_FLAG_NO_UI(0x400)：异步不弹 UI；
    // 返回值 > 32 表示成功。
    let result = unsafe {
        ShellExecuteW(
            0,
            wide(std::ffi::OsStr::new("open")).as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    if result <= 32 {
        return Err(format!("ShellExecuteW 返回 {result}（0=内存不足 2=文件未找到 5=拒绝访问）"));
    }
    Ok(())
}

/// 非 Windows 平台的直接启动。
#[cfg(not(target_os = "windows"))]
fn launch_detached(exe: &std::path::Path) -> Result<(), String> {
    use std::process::Stdio;
    crate::modules::process::cmd_builder(exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 定位客户端可执行文件：注册表 Uninstall 的 DisplayIcon → 常见目录 → App Paths。
/// 实测两个客户端都可能装在非默认盘（如 `E:\AutoClaw智谱龙虾\`），
/// 所以注册表优先，目录猜测只作兜底。
fn find_client_exe(folder: &str, exe_name: &str, label: &str) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    if let Some(path) = find_client_exe_from_registry(label, exe_name) {
        return Some(path);
    }

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for var in ["LOCALAPPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)", "APPDATA"] {
        if let Ok(base) = std::env::var(var) {
            let root = std::path::PathBuf::from(base);
            candidates.push(root.join("Programs").join(folder).join(exe_name));
            candidates.push(root.join(folder).join(exe_name));
        }
    }
    if let Some(found) = candidates.into_iter().find(|path| path.exists()) {
        return Some(found);
    }

    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "(Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe_name}' -ErrorAction SilentlyContinue).'(default)'"
        );
        if let Some(out) = crate::modules::process::ps_output(&script, 8) {
            let path = out.trim().trim_matches('"');
            if !path.is_empty() {
                let candidate = std::path::PathBuf::from(path);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// 从卸载注册表读取安装位置（`DisplayIcon` 形如 `E:\app\App.exe,0`）。
///
/// 直接用 winreg 读注册表：不产生子进程，也没有 `-Command` 经系统代码页
/// 传中文路径（如 `E:\AutoClaw智谱龙虾\`）乱码的问题。
#[cfg(target_os = "windows")]
fn find_client_exe_from_registry(label: &str, exe_name: &str) -> Option<std::path::PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let needle = if label.starts_with("AutoClaw") { "autoclaw" } else { "trae" };
    let roots = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE];
    let bases = [
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ];
    for root in roots {
        for base in bases {
            let Ok(key) = RegKey::predef(root).open_subkey(base) else {
                continue;
            };
            for sub in key.enum_keys().flatten() {
                let Ok(entry) = key.open_subkey(&sub) else {
                    continue;
                };
                let display_name = entry
                    .get_value::<String, _>("DisplayName")
                    .unwrap_or_default()
                    .to_lowercase();
                // TRAE Work CN 的安装名是 `TraeWork CN (User)`，用 `trae` 前缀匹配；
                // 但本机装了 3 个 Trae 变体，只认 Work/SOLO 那个。
                let matched = if needle == "autoclaw" {
                    display_name.contains("autoclaw")
                } else {
                    (display_name.contains("trae")
                        && (display_name.contains("work") || display_name.contains("solo")))
                };
                if !matched {
                    continue;
                }
                let Ok(icon) = entry.get_value::<String, _>("DisplayIcon") else {
                    continue;
                };
                for part in icon.split(',').rev() {
                    // DisplayIcon 可能带图标索引后缀；从尾部找最像 exe 的段。
                    let candidate_text = part.trim().trim_matches('"');
                    if candidate_text.is_empty() {
                        continue;
                    }
                    let candidate = std::path::PathBuf::from(candidate_text);
                    if candidate.is_file()
                        && candidate
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.eq_ignore_ascii_case(exe_name))
                    {
                        return Some(candidate);
                    }
                }
                // DisplayIcon 指向别的 exe 时，用 InstallLocation 拼目标 exe。
                if let Ok(dir) = entry.get_value::<String, _>("InstallLocation") {
                    let sibling = std::path::PathBuf::from(dir.trim().trim_matches('"'))
                        .join(exe_name);
                    if sibling.is_file() {
                        return Some(sibling);
                    }
                }
                if let Some(parent) = entry
                    .get_value::<String, _>("DisplayIcon")
                    .ok()
                    .and_then(|icon| {
                        std::path::PathBuf::from(icon.split(',').next().unwrap_or(&icon))
                            .parent()
                            .map(std::path::Path::to_path_buf)
                    })
                {
                    let sibling = parent.join(exe_name);
                    if sibling.is_file() {
                        return Some(sibling);
                    }
                }
            }
        }
    }
    None
}
