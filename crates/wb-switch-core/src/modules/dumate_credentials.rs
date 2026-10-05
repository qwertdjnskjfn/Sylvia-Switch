//! DuMate（百度搭子）qianfan 凭据解密（契约 docs/dumate-channel-contract.md §0/§2.1）。
//!
//! 凭据布局（2026-10-01 本机实测）：
//! - `%APPDATA%\qianfan-desktop-app\auth.json` 顶层
//!   `{cookies(base64), activeProviderType, accountProfiles, activeProfileId}`；
//! - 同目录 `.cookie-key` 为 32 字节**明文** AES-256-GCM 密钥（注意：不是 DPAPI
//!   保护文件，与 `vscode_cn_inject` 的 Chromium Safe Storage 路线不同，勿混用）；
//! - 解密：`base64decode(cookies)` → `iv=[0..12) tag=[12..28) cipher=[28..)`，
//!   AES-256-GCM（key=.cookie-key）→ UTF-8 JSON 数组，元素字段
//!   `{name,value,domain,hostOnly,path,secure,httpOnly,session,expirationDate,sameSite}`。
//!
//! 对外另提供 `bce_cookie_header`：按「域匹配 + 未过期 + 特殊字符值加引号」组装
//! console.bce.baidu.com 的 Cookie 头，并按实测约定从 `bce-user-info` cookie 的
//! 值取 `csrftoken` 请求头（不是名为 csrftoken 的 cookie）。

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose, Engine as _};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::modules::config::now_secs;

/// qianfan 凭据目录名（位于 dirs::config_dir()，Windows 即 %APPDATA% 下）。
pub const QIANFAN_DIR_NAME: &str = "qianfan-desktop-app";

const AUTH_JSON: &str = "auth.json";
const COOKIE_KEY_FILE: &str = ".cookie-key";
const GCM_IV_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

/// 解密后的单条 cookie（只保留组装请求头需要的字段）。
/// serde 形态与客户端 cookie 元素同构（name/value/domain/expirationDate），
/// 供 DuMate 账号池 `dumate_accounts.json` 落盘 round-trip。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DumateCookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    /// Unix 秒（Chrome cookie expirationDate 语义）；None = 会话 cookie（不过滤）。
    #[serde(default)]
    pub expiration_date: Option<f64>,
}

/// qianfan 凭据目录（`dirs::config_dir()/qianfan-desktop-app`）。
pub fn qianfan_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(QIANFAN_DIR_NAME))
}

/// 凭据是否齐备（auth.json + .cookie-key 都存在）。
pub fn credentials_present() -> bool {
    let Some(dir) = qianfan_dir() else {
        return false;
    };
    dir.join(AUTH_JSON).is_file() && dir.join(COOKIE_KEY_FILE).is_file()
}

/// 解密 qianfan cookies。失败给中文人话（含「请打开 DuMate 客户端重新登录」提示）。
pub fn decrypt_cookies() -> Result<Vec<DumateCookie>, String> {
    let dir = qianfan_dir().ok_or_else(|| {
        "未找到用户配置目录（%APPDATA%），无法读取 DuMate 凭据。请打开 DuMate 客户端重新登录".to_string()
    })?;
    decrypt_cookies_from_dir(&dir)
}

/// 从指定目录解密（`decrypt_cookies` 的可注入版本，供单测 round-trip）。
fn decrypt_cookies_from_dir(dir: &Path) -> Result<Vec<DumateCookie>, String> {
    let key_bytes = std::fs::read(dir.join(COOKIE_KEY_FILE)).map_err(|_| {
        "未找到 .cookie-key（qianfan 凭据缺失）。请打开 DuMate 客户端重新登录".to_string()
    })?;
    if key_bytes.len() != 32 {
        return Err(format!(
            ".cookie-key 长度异常（{} 字节，应为 32）。请打开 DuMate 客户端重新登录",
            key_bytes.len()
        ));
    }
    let auth_text = std::fs::read_to_string(dir.join(AUTH_JSON)).map_err(|_| {
        "未找到 auth.json（qianfan 凭据缺失）。请打开 DuMate 客户端重新登录".to_string()
    })?;
    let auth: Value = serde_json::from_str(&auth_text)
        .map_err(|e| format!("auth.json 解析失败: {e}。请打开 DuMate 客户端重新登录"))?;
    let cookies_b64 = auth
        .get("cookies")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if cookies_b64.is_empty() {
        return Err(
            "auth.json 中没有 cookies 字段。请打开 DuMate 客户端重新登录".to_string(),
        );
    }
    let raw = general_purpose::STANDARD
        .decode(cookies_b64.trim())
        .map_err(|_| {
            "auth.json 的 cookies 字段 base64 解码失败。请打开 DuMate 客户端重新登录".to_string()
        })?;
    let plain = decrypt_gcm(&key_bytes, &raw)?;
    parse_cookies_json(&plain)
}

/// 按契约布局解密：`iv=[0..12) tag=[12..28) cipher=[28..)`，AES-256-GCM。
fn decrypt_gcm(key: &[u8], raw: &[u8]) -> Result<String, String> {
    if raw.len() < GCM_IV_LEN + GCM_TAG_LEN {
        return Err(
            "cookies 密文过短（不足 iv+tag 长度）。请打开 DuMate 客户端重新登录".to_string(),
        );
    }
    let iv = &raw[..GCM_IV_LEN];
    let tag = &raw[GCM_IV_LEN..GCM_IV_LEN + GCM_TAG_LEN];
    let cipher = &raw[GCM_IV_LEN + GCM_TAG_LEN..];
    // aes_gcm 约定密文与 tag 连接（ciphertext || tag）。
    let mut msg = Vec::with_capacity(cipher.len() + GCM_TAG_LEN);
    msg.extend_from_slice(cipher);
    msg.extend_from_slice(tag);
    let c = Aes256Gcm::new(GenericArray::from_slice(key));
    let plain = c
        .decrypt(Nonce::from_slice(iv), msg.as_ref())
        .map_err(|_| {
            "cookies AES-256-GCM 解密失败（.cookie-key 与登录数据不匹配？）。请打开 DuMate 客户端重新登录"
                .to_string()
        })?;
    String::from_utf8(plain)
        .map_err(|_| "cookies 解密结果不是合法 UTF-8。请打开 DuMate 客户端重新登录".to_string())
}

/// 解析解密后的 JSON 数组为 `DumateCookie` 列表（容错：单条损坏跳过整包报错）。
fn parse_cookies_json(plain: &str) -> Result<Vec<DumateCookie>, String> {
    let arr: Vec<Value> = serde_json::from_str(plain).map_err(|e| {
        format!("cookies 解密结果不是 JSON 数组: {e}。请打开 DuMate 客户端重新登录")
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for item in &arr {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let value = item
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let domain = item
            .get("domain")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let expiration_date = item
            .get("expirationDate")
            .and_then(Value::as_f64);
        if name.is_empty() {
            continue;
        }
        out.push(DumateCookie {
            name,
            value,
            domain,
            expiration_date,
        });
    }
    if out.is_empty() {
        return Err(
            "cookies 解密结果为空。请打开 DuMate 客户端重新登录".to_string(),
        );
    }
    Ok(out)
}

/// 组装 console.bce.baidu.com 请求头：`(Cookie 头, csrftoken 头)`。
///
/// 过滤规则（契约 §0 实测）：域匹配 + 未过期；值含特殊字符（cookie-octet 之外）
/// 时加引号。`csrftoken` 取 `bce-user-info` cookie 的值（实测约定，非名为
/// csrftoken 的 cookie）；取不到为空串，调用方应视为登录态异常。
pub fn bce_cookie_header(cookies: &[DumateCookie], host: &str) -> (String, String) {
    let now = now_secs() as f64;
    let mut parts: Vec<String> = Vec::new();
    let mut csrf = String::new();
    for c in cookies {
        if c.name.is_empty() || c.value.is_empty() {
            continue;
        }
        if !domain_matches(&c.domain, host) {
            continue;
        }
        if let Some(exp) = c.expiration_date {
            if exp <= now {
                continue;
            }
        }
        if c.name == "bce-user-info" && csrf.is_empty() {
            csrf = c.value.clone();
        }
        parts.push(format!("{}={}", c.name, quote_cookie_value(&c.value)));
    }
    (parts.join("; "), csrf)
}

/// cookie 域匹配：带前导点的域 cookie 匹配宿主及其子域；无点（hostOnly）只精确匹配。
fn domain_matches(domain: &str, host: &str) -> bool {
    let d = domain.trim().to_ascii_lowercase();
    let h = host.trim().to_ascii_lowercase();
    if d.is_empty() || h.is_empty() {
        return false;
    }
    match d.strip_prefix('.') {
        Some(bare) => h == bare || h.ends_with(&format!(".{bare}")),
        None => h == d,
    }
}

/// RFC 6265 cookie-octet：空格/逗号/分号/双引号/反斜杠等特殊字符出现时值需加引号。
fn quote_cookie_value(value: &str) -> String {
    let needs_quote = value.is_empty() || value.bytes().any(|b| !is_cookie_octet(b));
    if needs_quote {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

fn is_cookie_octet(b: u8) -> bool {
    matches!(b, 0x21 | 0x23..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E)
}

// ---------------------------------------------------------------------------
// 单测：自加密样本 round-trip + 请求头组装断言（不依赖网络/真实进程）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 按契约布局加密（iv || tag || cipher → base64），仅测试使用。
    fn encrypt_gcm_for_test(key: &[u8], iv: &[u8; 12], plain: &str) -> Result<Vec<u8>, String> {
        let c = Aes256Gcm::new(GenericArray::from_slice(key));
        let enc = c
            .encrypt(Nonce::from_slice(iv), plain.as_bytes())
            .map_err(|e| e.to_string())?;
        let (cipher, tag) = enc.split_at(enc.len() - 16);
        let mut out = Vec::with_capacity(GCM_IV_LEN + GCM_TAG_LEN + cipher.len());
        out.extend_from_slice(iv);
        out.extend_from_slice(tag);
        out.extend_from_slice(cipher);
        Ok(out)
    }

    /// 构造临时 qianfan 目录：auth.json + .cookie-key。
    fn make_sample_dir(key: &[u8], cookies_json: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-dumate-creds-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let iv = [0x37u8; 12];
        let raw = encrypt_gcm_for_test(key, &iv, cookies_json).unwrap();
        std::fs::write(
            dir.join(AUTH_JSON),
            format!(
                r#"{{"cookies":"{}","activeProviderType":1,"accountProfiles":[],"activeProfileId":"p1"}}"#,
                general_purpose::STANDARD.encode(raw)
            ),
        )
        .unwrap();
        std::fs::write(dir.join(COOKIE_KEY_FILE), key).unwrap();
        dir
    }

    const SAMPLE_COOKIES: &str = r#"[
        {"name":"BDUSS","value":"abcdef123456","domain":".baidu.com","hostOnly":false,"path":"/","secure":true,"httpOnly":true,"session":false,"expirationDate":1893456000,"sameSite":"no_restriction"},
        {"name":"bce-user-info","value":"csrf-token-xyz%3D","domain":".bce.baidu.com","hostOnly":false,"path":"/","secure":true,"httpOnly":false,"session":false,"expirationDate":1893456000,"sameSite":"no_restriction"},
        {"name":"SESSION_COOKIE","value":"no-exp","domain":"console.bce.baidu.com","hostOnly":true,"path":"/","secure":true,"httpOnly":true,"session":true,"sameSite":"unspecified"}
    ]"#;

    #[test]
    fn decrypt_round_trip_with_self_encrypted_sample() {
        let key: Vec<u8> = (0u8..32).collect();
        let dir = make_sample_dir(&key, SAMPLE_COOKIES);
        let cookies = decrypt_cookies_from_dir(&dir).expect("round-trip 解密成功");
        assert_eq!(cookies.len(), 3);
        assert_eq!(cookies[0].name, "BDUSS");
        assert_eq!(cookies[0].value, "abcdef123456");
        assert_eq!(cookies[0].domain, ".baidu.com");
        assert_eq!(cookies[0].expiration_date, Some(1893456000.0));
        // csrftoken cookie 值原样保留（含 URL 编码字符）。
        assert_eq!(cookies[1].value, "csrf-token-xyz%3D");
        // 会话 cookie：无 expirationDate → None。
        assert_eq!(cookies[2].name, "SESSION_COOKIE");
        assert_eq!(cookies[2].expiration_date, None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decrypt_missing_files_gives_human_error() {
        let dir = std::env::temp_dir().join(format!(
            "wb-dumate-creds-empty-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let err = decrypt_cookies_from_dir(&dir).unwrap_err();
        assert!(err.contains("重新登录"), "人话提示缺失: {err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decrypt_rejects_short_key() {
        // 样本用合法 32 字节密钥加密，再把 .cookie-key 换成 16 字节 → 长度校验拦截。
        let dir = make_sample_dir(&(0u8..32).collect::<Vec<u8>>(), SAMPLE_COOKIES);
        std::fs::write(dir.join(COOKIE_KEY_FILE), [7u8; 16]).unwrap();
        let err = decrypt_cookies_from_dir(&dir).unwrap_err();
        assert!(err.contains("32"), "应提示密钥长度: {err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decrypt_rejects_mismatched_key() {
        // 密文用 [1u8; 32] 加密，随后把 .cookie-key 换成另一把钥匙 → GCM 校验失败。
        let dir = make_sample_dir(&[1u8; 32], SAMPLE_COOKIES);
        let wrong: Vec<u8> = (2u8..34).collect();
        std::fs::write(dir.join(COOKIE_KEY_FILE), &wrong).unwrap();
        let err = decrypt_cookies_from_dir(&dir).unwrap_err();
        assert!(err.contains("AES-256-GCM"), "应提示 GCM 解密失败: {err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decrypt_rejects_corrupt_base64() {
        let key: Vec<u8> = (0u8..32).collect();
        let dir = make_sample_dir(&key, SAMPLE_COOKIES);
        std::fs::write(
            dir.join(AUTH_JSON),
            r#"{"cookies":"%%%not-base64%%%","activeProviderType":1}"#,
        )
        .unwrap();
        let err = decrypt_cookies_from_dir(&dir).unwrap_err();
        assert!(err.contains("base64"), "应提示 base64 解码失败: {err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bce_cookie_header_filters_domain_and_expiry_and_takes_csrf_from_bce_user_info() {
        let now = now_secs() as f64;
        let cookies = vec![
            DumateCookie {
                name: "BDUSS".into(),
                value: "abcdef123456".into(),
                domain: ".baidu.com".into(),
                expiration_date: Some(now + 3600.0),
            },
            DumateCookie {
                name: "bce-user-info".into(),
                value: "csrf-token-xyz%3D".into(),
                domain: ".bce.baidu.com".into(),
                expiration_date: Some(now + 3600.0),
            },
            // 已过期 → 剔除。
            DumateCookie {
                name: "EXPIRED".into(),
                value: "old".into(),
                domain: ".bce.baidu.com".into(),
                expiration_date: Some(now - 10.0),
            },
            // 无关域 → 剔除。
            DumateCookie {
                name: "FOREIGN".into(),
                value: "x".into(),
                domain: ".example.com".into(),
                expiration_date: Some(now + 3600.0),
            },
            // hostOnly 精确匹配 → 保留。
            DumateCookie {
                name: "HOST_ONLY".into(),
                value: "hostval".into(),
                domain: "console.bce.baidu.com".into(),
                expiration_date: None,
            },
        ];
        let (header, csrf) = bce_cookie_header(&cookies, "console.bce.baidu.com");
        assert!(header.contains("BDUSS=abcdef123456"), "{header}");
        assert!(header.contains("HOST_ONLY=hostval"), "{header}");
        assert!(!header.contains("EXPIRED"), "{header}");
        assert!(!header.contains("FOREIGN"), "{header}");
        // csrftoken 取 bce-user-info cookie 的值（契约实测约定）。
        assert_eq!(csrf, "csrf-token-xyz%3D");
    }

    #[test]
    fn bce_cookie_header_quotes_special_char_values() {
        let cookies = vec![DumateCookie {
            name: "BAIDUID".into(),
            value: "A1B2C3 token with space".into(),
            domain: ".baidu.com".into(),
            expiration_date: None,
        }];
        let (header, csrf) = bce_cookie_header(&cookies, "console.bce.baidu.com");
        assert!(
            header.contains("BAIDUID=\"A1B2C3 token with space\""),
            "特殊字符值应加引号: {header}"
        );
        assert!(csrf.is_empty(), "无 bce-user-info 时 csrftoken 为空");
    }

    #[test]
    fn domain_matches_rules() {
        assert!(domain_matches(".bce.baidu.com", "console.bce.baidu.com"));
        assert!(domain_matches(".bce.baidu.com", "bce.baidu.com"));
        assert!(domain_matches(".baidu.com", "console.bce.baidu.com"));
        assert!(!domain_matches("console.bce.baidu.com", "bce.baidu.com"));
        assert!(domain_matches("console.bce.baidu.com", "console.bce.baidu.com"));
        assert!(!domain_matches("", "console.bce.baidu.com"));
        assert!(!domain_matches(".bce.baidu.com", ""));
    }
}
