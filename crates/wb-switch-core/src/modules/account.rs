//! 账号存储：读取/写入 `~/.wb-switch/accounts.json`，与 Python 版共享数据目录。
//!
//! 对照 server.py `load_accounts` / `save_accounts` / `find_account` /
//! `account_display_name` / `account_meta`。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

use crate::modules::autoclaw_accounts::jwt_exp_from_access_token;
use crate::modules::config::{accounts_file, atomic_write, now_ms};

fn load_accounts_from_path(path: &Path) -> Vec<Value> {
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(Value::Array(accounts)) = serde_json::from_str::<Value>(&text) {
            return accounts;
        }
    }
    vec![]
}

fn save_accounts_to_path(path: &Path, accounts: &[Value]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(accounts).unwrap_or_default();
    atomic_write(path, &content)
}

fn find_account_in(accounts: &[Value], account_id: &str) -> Option<Value> {
    accounts
        .iter()
        .find(|account| {
            account.get("id").and_then(Value::as_str) == Some(account_id)
                || account.get("uid").and_then(Value::as_str) == Some(account_id)
        })
        .cloned()
}

fn delete_account_from_path(path: &Path, account_id: &str) -> Result<(), String> {
    let mut accounts = load_accounts_from_path(path);
    let before = accounts.len();
    accounts.retain(|account| account.get("id").and_then(Value::as_str) != Some(account_id));
    if accounts.len() == before {
        return Err("账号不存在".to_string());
    }
    save_accounts_to_path(path, &accounts).map_err(|error| error.to_string())
}

/// 读取账号库；文件缺失或损坏返回空列表。
pub fn load_accounts() -> Vec<Value> {
    load_accounts_from_path(&accounts_file())
}

/// 写回账号库（原子写），保持原 JSON 数组结构。
pub fn save_accounts(accounts: &[Value]) -> std::io::Result<()> {
    save_accounts_to_path(&accounts_file(), accounts)
}

/// 按 id 或 uid 查找账号。
pub fn find_account(account_id: &str) -> Option<Value> {
    find_account_in(&load_accounts(), account_id)
}

/// 账号展示名（email → nickname → uid → unknown）。
pub fn account_display_name(acc: &Value) -> String {
    get_str(acc, "email")
        .or_else(|| get_str(acc, "nickname"))
        .or_else(|| get_str(acc, "uid"))
        .unwrap_or_else(|| "unknown".to_string())
}

/// 账号的展示元数据（不泄露 token）。对照 server.py `account_meta`。
pub fn account_meta(acc: &Value) -> Value {
    json!({
        "id": acc.get("id"),
        "uid": acc.get("uid"),
        "email": acc.get("email"),
        "nickname": acc.get("nickname"),
        "enterpriseName": acc.get("enterpriseName"),
        "expiresAt": acc.get("expiresAt"),
        "refreshExpiresAt": acc.get("refreshExpiresAt"),
        "refreshedAt": acc.get("refreshedAt"),
        "createdAt": acc.get("createdAt"),
        "needsRelogin": acc.get("needs_relogin").and_then(|v| v.as_bool()) == Some(true),
        "needsReloginReason": acc.get("needs_relogin_reason"),
    })
}

/// 取非空字符串字段；空/缺失返回 None。
pub fn get_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 返回可用于 UID 缺失场景的真实邮箱。历史展示占位值不参与身份匹配。
fn identity_email(account: &Value) -> Option<String> {
    let email = get_str(account, "email")?;
    if !email.contains('@')
        || email.eq_ignore_ascii_case("unknown")
        || email == "手动添加"
        || get_str(account, "nickname").as_deref() == Some(email.as_str())
        || get_str(account, "uid").as_deref() == Some(email.as_str())
    {
        return None;
    }
    Some(email.to_ascii_lowercase())
}

/// 按稳定身份将采集结果合并到账号列表，并返回最终持久化的账号。
///
/// 非空 UID 始终优先；仅当新账号没有 UID 时，才使用真实邮箱兜底。
/// 命中已有身份时保留本地 id，避免调用方持有的账号引用失效。
pub fn upsert_collected_account(accounts: &mut Vec<Value>, mut collected: Value) -> Value {
    let collected_uid = get_str(&collected, "uid");
    let collected_email = identity_email(&collected);
    let matches_identity = |existing: &Value| {
        if let Some(uid) = collected_uid.as_deref() {
            return get_str(existing, "uid").as_deref() == Some(uid);
        }
        collected_email
            .as_deref()
            .is_some_and(|email| identity_email(existing).as_deref() == Some(email))
    };

    let matching_indexes: Vec<usize> = accounts
        .iter()
        .enumerate()
        .filter_map(|(index, existing)| matches_identity(existing).then_some(index))
        .collect();

    if let Some(&first_index) = matching_indexes.first() {
        let existing = &accounts[first_index];
        if let Some(existing_id) = existing.get("id").cloned() {
            collected["id"] = existing_id;
        }
        if get_str(&collected, "uid").is_none() {
            if let Some(existing_uid) = existing.get("uid").cloned() {
                collected["uid"] = existing_uid;
            }
        }
        if let Some(created_at) = existing.get("createdAt").cloned() {
            collected["createdAt"] = created_at;
        }

        for index in matching_indexes.into_iter().rev() {
            accounts.remove(index);
        }
        accounts.insert(first_index.min(accounts.len()), collected.clone());
    } else {
        accounts.push(collected.clone());
    }

    collected
}

/// 使用统一身份规则保存采集到的账号。
pub fn save_collected_account(collected: Value) -> std::io::Result<Value> {
    let mut accounts = load_accounts();
    let saved = upsert_collected_account(&mut accounts, collected);
    save_accounts(&accounts)?;
    Ok(saved)
}

/// 按 id 覆盖写入账号库（不存在则追加）。对照 server.py `_upsert_account`。
pub fn upsert_account(updated: &Value) -> std::io::Result<()> {
    let mut accounts = load_accounts();
    let id = updated.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let mut replaced = false;
    for a in accounts.iter_mut() {
        if a.get("id").and_then(|v| v.as_str()) == Some(id) {
            *a = updated.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        accounts.push(updated.clone());
    }
    save_accounts(&accounts)
}

/// 构造与官方对齐的请求头。对照 server.py `build_auth_headers`。
pub fn build_auth_headers(account: &Value) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!(
            "Bearer {}",
            get_str(account, "access_token").unwrap_or_default()
        ),
    );
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    if let Some(uid) = get_str(account, "uid") {
        headers.insert("X-User-Id".to_string(), uid);
    }
    if let Some(eid) =
        get_str(account, "enterpriseId").or_else(|| get_str(account, "enterprise_id"))
    {
        headers.insert("X-Enterprise-Id".to_string(), eid.clone());
        headers.insert("X-Tenant-Id".to_string(), eid);
    }
    if let Some(domain) = get_str(account, "domain") {
        headers.insert("X-Domain".to_string(), domain);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 每条用例独占临时文件（**不碰真实账号库**）。
    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "wb-account-edit-{tag}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn edit_patch_whitelist_rejects_protected_fields() {
        // 🔴 主键与上游原始态绝不可改：id 是删除/导出的锚点，auth_raw.uid 是池 uid 的来源。
        for field in ["id", "uid", "auth_raw", "profile_raw", "createdAt", "whatever"] {
            let err = validate_and_normalize_patch(&json!({ field: "x" }))
                .expect_err(&format!("`{field}` 必须被拒绝"));
            assert!(err.contains("不可编辑"), "报错须点名字段：{err}");
        }
        // 白名单内全部通过
        let ok = validate_and_normalize_patch(&json!({
            "nickname": "  新名字  ",
            "domain": "www.workbuddy.ai",
            "access_token": "at",
            "refresh_token": "rt",
            "expiresAt": 1790000000000i64,
            "refreshExpiresAt": 1790000000000i64,
        }))
        .expect("白名单字段应通过");
        assert_eq!(ok.len(), 6);
        // ⚠️ 不要按下标断言：`serde_json::Map` 默认是 BTreeMap，迭代按**键的字母序**
        //（access_token / domain / expiresAt / nickname / …），与插入顺序无关。
        // 按 key 查，顺序无关才稳。
        let get = |k: &str| {
            ok.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("缺少字段 {k}"))
        };
        // 字符串字段去首尾空白（避免「看起来一样实则不同」的脏数据）
        assert_eq!(get("nickname"), json!("新名字"));
        assert_eq!(get("access_token"), json!("at"));
        assert_eq!(get("expiresAt"), json!(1790000000000i64));
    }

    #[test]
    fn edit_patch_validation_is_all_or_nothing() {
        // 空 patch
        assert!(validate_and_normalize_patch(&json!({})).is_err());
        assert!(validate_and_normalize_patch(&json!("not-an-object")).is_err());
        // 类型错误
        assert!(validate_and_normalize_patch(&json!({ "expiresAt": "abc" })).is_err());
        assert!(validate_and_normalize_patch(&json!({ "expiresAt": 0 })).is_err());
        assert!(validate_and_normalize_patch(&json!({ "expiresAt": -1 })).is_err());
        assert!(validate_and_normalize_patch(&json!({ "nickname": 123 })).is_err());
        // 凭证 / domain 不许清空（清空等于弄死账号）
        for f in ["access_token", "refresh_token", "domain"] {
            assert!(
                validate_and_normalize_patch(&json!({ f.clone(): "   " })).is_err(),
                "`{f}` 清空必须被拒"
            );
        }
        // nickname 允许清空（展示回落 uid）
        assert!(validate_and_normalize_patch(&json!({ "nickname": "" })).is_ok());

        // ⭐ 关键：**混入一个非法字段 ⇒ 整体拒绝**，不能部分通过
        let mixed = json!({ "nickname": "ok", "id": "hack" });
        assert!(validate_and_normalize_patch(&mixed).is_err());
    }

    #[test]
    fn add_by_token_creates_and_dedups_by_token() {
        // 🔴 去重口径是**按 token**，不是按 uid/email —— 手动添加常缺这两者，
        // 按身份去重会让「重复点击」产生 N 个重复账号。
        let p = tmp_path("add-token");
        let token = "hdr.eyJleHAiOjE3OTAwMDAwMDB9.sig"; // payload = {"exp":1790000000}

        // ① 新建：region 由 domain 派生（.ai → intl）
        let a = add_account_by_token_at(&p, token, "RT", "www.workbuddy.ai", "国际号", "", 0)
            .expect("应添加成功");
        assert_eq!(a["domain"], "www.workbuddy.ai");
        assert_eq!(a["nickname"], "国际号");
        // expires_at 缺省 ⇒ 从 JWT exp 解析（秒 → 毫秒）
        assert_eq!(a["expiresAt"], 1790000000000i64);
        let id_first = a["id"].as_str().unwrap().to_string();
        assert!(!id_first.is_empty(), "必须生成 id（池 uid 的兜底来源）");

        // ② 同一段 token 再提交 ⇒ 原地覆盖，**不新增条目**，且保留原 id
        let b = add_account_by_token_at(&p, token, "RT2", "www.workbuddy.ai", "改名了", "", 0)
            .expect("重复提交应成功");
        assert_eq!(b["id"], json!(id_first), "必须沿用原 id");
        assert_eq!(b["nickname"], "改名了");
        assert_eq!(b["refresh_token"], "RT2");
        let list = load_accounts_from_path(&p);
        assert_eq!(list.len(), 1, "同 token 不得产生重复条目");

        // ③ 另一段 token ⇒ 新增
        add_account_by_token_at(&p, "other.token.here", "", "www.workbuddy.cn", "国内号", "", 0)
            .expect("应添加成功");
        assert_eq!(load_accounts_from_path(&p).len(), 2);

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn add_by_token_validates_required_fields() {
        let p = tmp_path("add-token-bad");
        // 空 token
        assert!(add_account_by_token_at(&p, "  ", "", "www.workbuddy.cn", "", "", 0).is_err());
        // 空 domain（它决定区域，不能省）
        assert!(add_account_by_token_at(&p, "some.token", "", "", "", "", 0).is_err());
        // 都不给
        assert!(add_account_by_token_at(&p, "", "", "", "", "", 0).is_err());
        // 失败不得创建文件
        assert!(!p.exists(), "校验失败时不得写盘");
    }

    #[test]
    fn edit_writes_only_given_fields_and_keeps_the_rest() {
        let p = tmp_path("partial");
        io_write(&p, &json!([
            { "id": "a1", "uid": "u1", "nickname": "旧名", "domain": "www.workbuddy.cn",
              "access_token": "AT-OLD", "refresh_token": "RT-OLD", "expiresAt": 111,
              "auth_raw": { "uid": "u1", "secret": "keep-me" }, "createdAt": 999 }
        ]));

        let updated = update_account_fields_at(&p, "a1", &json!({ "nickname": "新名" }))
            .expect("改昵称应成功");

        // 只改了 nickname
        assert_eq!(updated["nickname"], "新名");
        // 其余字段一字未动（PATCH 语义）
        assert_eq!(updated["access_token"], "AT-OLD");
        assert_eq!(updated["refresh_token"], "RT-OLD");
        assert_eq!(updated["expiresAt"], 111);
        assert_eq!(updated["domain"], "www.workbuddy.cn");
        assert_eq!(updated["auth_raw"]["secret"], "keep-me");
        assert_eq!(updated["createdAt"], 999);
        assert_eq!(updated["id"], "a1");

        // 落盘也要一致
        let on_disk = load_accounts_from_path(&p);
        assert_eq!(on_disk[0]["nickname"], "新名");
        assert_eq!(on_disk[0]["access_token"], "AT-OLD");

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn edit_rejects_unknown_account_without_touching_file() {
        let p = tmp_path("missing");
        io_write(&p, &json!([{ "id": "a1", "nickname": "x" }]));
        let before = std::fs::read_to_string(&p).unwrap();

        assert!(update_account_fields_at(&p, "nope", &json!({ "nickname": "y" })).is_err());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "找不到账号时不得改动文件"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 测试用写盘（不走原子写，够用且不引入额外依赖）。
    fn io_write(path: &std::path::Path, v: &Value) {
        std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
    }

    #[test]
    fn account_meta_strips_tokens() {
        let acc = json!({
            "id": "a1",
            "uid": "u1",
            "email": "x@y.z",
            "nickname": "小明",
            "enterpriseName": "某公司",
            "access_token": "SECRET_ACCESS",
            "refresh_token": "SECRET_REFRESH",
            "expiresAt": 123456,
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败",
        });
        let meta = account_meta(&acc);
        assert_eq!(meta["id"], "a1");
        assert_eq!(meta["needsRelogin"], true);
        assert_eq!(meta["needsReloginReason"], "刷新失败");
        assert!(meta.get("access_token").is_none(), "不得泄露 token");
        assert!(meta.get("refresh_token").is_none(), "不得泄露 token");
    }

    #[test]
    fn account_display_name_priority() {
        assert_eq!(
            account_display_name(&json!({"email": "a@b.c", "nickname": "n"})),
            "a@b.c"
        );
        assert_eq!(
            account_display_name(&json!({"nickname": "n", "uid": "u"})),
            "n"
        );
        assert_eq!(account_display_name(&json!({"uid": "u"})), "u");
        assert_eq!(account_display_name(&json!({})), "unknown");
    }

    #[test]
    fn get_str_trims_and_filters_empty() {
        assert_eq!(get_str(&json!({"k": "  v  "}), "k"), Some("v".to_string()));
        assert_eq!(get_str(&json!({"k": "  "}), "k"), None);
        assert_eq!(get_str(&json!({"k": 123}), "k"), None);
    }

    fn account(id: &str, uid: Option<&str>, nickname: &str, email: Option<&str>) -> Value {
        json!({
            "id": id,
            "uid": uid,
            "nickname": nickname,
            "email": email,
            "access_token": format!("token-{id}"),
            "createdAt": 1,
        })
    }

    #[test]
    fn same_nickname_with_different_uids_is_retained() {
        let mut accounts = vec![account("old", Some("uid-1"), "同名", Some("同名"))];
        let saved =
            upsert_collected_account(&mut accounts, account("new", Some("uid-2"), "同名", None));

        assert_eq!(accounts.len(), 2);
        assert_eq!(saved["id"], "new");
    }

    #[test]
    fn same_uid_refresh_preserves_local_id_and_removes_duplicates() {
        let mut accounts = vec![
            account("stable", Some("uid-1"), "旧名称", Some("old@example.com")),
            account("duplicate", Some("uid-1"), "重复记录", None),
        ];
        let saved = upsert_collected_account(
            &mut accounts,
            account("generated", Some("uid-1"), "新名称", None),
        );

        assert_eq!(accounts.len(), 1);
        assert_eq!(saved["id"], "stable");
        assert_eq!(saved["nickname"], "新名称");
        assert_eq!(saved["access_token"], "token-generated");
    }

    #[test]
    fn different_uids_with_same_real_email_are_retained() {
        let mut accounts = vec![account(
            "old",
            Some("uid-1"),
            "账号一",
            Some("shared@example.com"),
        )];
        upsert_collected_account(
            &mut accounts,
            account("new", Some("uid-2"), "账号二", Some("shared@example.com")),
        );

        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn real_email_is_fallback_only_when_collected_uid_is_missing() {
        let mut accounts = vec![account("stable", None, "旧名称", Some("user@example.com"))];
        let saved = upsert_collected_account(
            &mut accounts,
            account("generated", None, "新名称", Some("USER@example.com")),
        );

        assert_eq!(accounts.len(), 1);
        assert_eq!(saved["id"], "stable");
        assert_eq!(saved["nickname"], "新名称");
    }

    #[test]
    fn legacy_synthetic_email_does_not_merge_accounts() {
        let mut accounts = vec![account("old", None, "同名", Some("同名"))];
        upsert_collected_account(&mut accounts, account("new", None, "同名", Some("同名")));

        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn persisted_same_name_accounts_can_be_found_and_deleted_independently() {
        let test_dir = std::env::temp_dir().join(format!(
            "wb-switch-same-name-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = test_dir.join("accounts.json");
        let mut accounts = vec![];
        upsert_collected_account(
            &mut accounts,
            account("account-1", Some("uid-1"), "同名用户", None),
        );
        upsert_collected_account(
            &mut accounts,
            account("account-2", Some("uid-2"), "同名用户", None),
        );
        save_accounts_to_path(&path, &accounts).expect("same-name accounts should persist");

        let persisted = load_accounts_from_path(&path);
        assert_eq!(
            find_account_in(&persisted, "account-1").unwrap()["uid"],
            "uid-1"
        );
        assert_eq!(
            find_account_in(&persisted, "account-2").unwrap()["uid"],
            "uid-2"
        );

        delete_account_from_path(&path, "account-1").expect("first account should delete");
        let after_first_delete = load_accounts_from_path(&path);
        assert!(find_account_in(&after_first_delete, "account-1").is_none());
        assert_eq!(
            find_account_in(&after_first_delete, "account-2").unwrap()["uid"],
            "uid-2"
        );

        delete_account_from_path(&path, "account-2").expect("second account should delete");
        assert!(load_accounts_from_path(&path).is_empty());
        std::fs::remove_dir_all(&test_dir).expect("temporary account store should clean up");
    }
}

/// 删除账号（按 id）。
pub fn delete_account(account_id: &str) -> Result<(), String> {
    delete_account_from_path(&accounts_file(), account_id)
}

// ---------------------------------------------------------------------------
// 直接添加（token 方式，2026-10-13）
// ---------------------------------------------------------------------------

/// 直接用 **access_token** 添加账号（不经过文件导入 / OAuth）。
///
/// # 为什么需要它
///
/// 文件导入要求先有一份 JSON；OAuth 要求能走浏览器。而最常见的场景是
/// 「手上已经有一段 token」（从浏览器 devtools 抄的）——那时两者都不顺手。
/// 参照 MiniMax 通道的 `/accounts/add`（内联表单、必填仅 token）做成同款形态。
///
/// # 去重口径：**按 token**
///
/// `save_collected_account` 按 uid/email 去重，但手动添加时**往往没有 uid/email**
/// ⇒ 会每次新增一条，重复点击就得到 N 个重复账号。故这里先按 `access_token`
/// 精确匹配：命中则**原地覆盖**（沿用原 id/uid/createdAt），否则新建。
///
/// # 地域
///
/// 由 `domain` 后缀派生（与池的 `region` 判据同源：`.ai→intl` / 其余→cn`），
/// 调用方负责提示用户「选错域 = 进错区」。
pub fn add_account_by_token(
    access_token: &str,
    refresh_token: &str,
    domain: &str,
    nickname: &str,
    uid_hint: &str,
    expires_at: i64,
) -> Result<Value, String> {
    add_account_by_token_at(
        &accounts_file(),
        access_token,
        refresh_token,
        domain,
        nickname,
        uid_hint,
        expires_at,
    )
}

/// 写盘核心（**路径可注入**）：`cfg(test)` 下必须走临时目录，
/// 否则单测会改到用户的真实账号库（红线：写用户数据文件的函数必须做路径隔离）。
#[allow(clippy::too_many_arguments)]
fn add_account_by_token_at(
    path: &Path,
    access_token: &str,
    refresh_token: &str,
    domain: &str,
    nickname: &str,
    uid_hint: &str,
    expires_at: i64,
) -> Result<Value, String> {
    let token = access_token.trim();
    if token.is_empty() {
        return Err("access_token 不能为空".to_string());
    }
    let dom = domain.trim();
    if dom.is_empty() {
        return Err("domain 不能为空（它决定账号归属区域）".to_string());
    }

    // expires_at 缺省时从 JWT 的 exp 解析（复用既有工具；失败则留 0，
    // 由池按「未知过期时间」处理，不阻断添加）。
    let exp = if expires_at > 0 {
        expires_at
    } else {
        jwt_exp_from_access_token(token)
            .map(|sec| sec.saturating_mul(1000))
            .unwrap_or(0)
    };

    static ADD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ADD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut accounts = load_accounts_from_path(path);
    let same_token_idx = accounts
        .iter()
        .position(|a| get_str(a, "access_token").as_deref() == Some(token));

    let record = if let Some(idx) = same_token_idx {
        // 同一段 token ⇒ 原地覆盖，保留稳定字段（id / uid / createdAt）。
        //
        // 🔴 不能走 `upsert_collected_account`：它按 **uid/email** 去重，
        // 而手动添加往往两者都没有 ⇒ 会被判成「新账号」再 push 一条，
        // 重复点击就得到 N 个重复账号。
        // （单测 `add_by_token_creates_and_dedups_by_token` 守住这条。）
        let mut r = accounts[idx].clone();
        // 先不可变借用判一次，再拿可变借用（否则 E0502）
        let has_uid = get_str(&r, "uid").is_some();
        let want_uid = !uid_hint.trim().is_empty() && !has_uid;
        {
            let obj = r.as_object_mut().ok_or("账号记录不是对象")?;
            obj.insert("domain".to_string(), Value::from(dom));
            obj.insert("access_token".to_string(), Value::from(token));
            if !refresh_token.trim().is_empty() {
                obj.insert("refresh_token".to_string(), Value::from(refresh_token.trim()));
            }
            if exp > 0 {
                obj.insert("expiresAt".to_string(), Value::from(exp));
            }
            if !nickname.trim().is_empty() {
                obj.insert("nickname".to_string(), Value::from(nickname.trim()));
            }
            if want_uid {
                obj.insert("uid".to_string(), Value::from(uid_hint.trim()));
            }
        }
        accounts[idx] = r.clone();
        r
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let r = json!({
            "id": id,
            "domain": dom,
            "access_token": token,
            "refresh_token": refresh_token.trim(),
            "nickname": nickname.trim(),
            "uid": uid_hint.trim(),
            "expiresAt": exp,
            "createdAt": now_ms(),
        });
        accounts.push(r.clone());
        r
    };

    save_accounts_to_path(path, &accounts).map_err(|e| format!("保存账号库失败：{e}"))?;
    Ok(record)
}

// ---------------------------------------------------------------------------
// 账号编辑（白名单，2026-10-13）
// ---------------------------------------------------------------------------
/// **可编辑字段白名单**。不在此列的字段一律拒绝——尤其是这几类：
///
/// | 字段 | 为何不可改 |
/// | --- | --- |
/// | `id` | 删除 / 导出选择 / 列表 key 的主键，改了就对不上 |
/// | `uid` | 池选号与 token 回写的锚点（`account_uid`） |
/// | `auth_raw` | 上游返回的**原始态**（池的 uid 也取自 `auth_raw.uid`）|
/// | `profile_raw` | 同上 |
/// | `createdAt` | 审计信息，不应被改写 |
const EDITABLE_FIELDS: &[&str] = &[
    "nickname",
    "domain",
    "access_token",
    "refresh_token",
    "expiresAt",
    "refreshExpiresAt",
];

/// 按白名单更新一条账号记录，返回更新后的完整记录。
///
/// # 语义（写清以免误用）
///
/// - **PATCH 语义**：只改 `patch` 里出现的字段，其余原样保留。
/// - **全有或全无**：任一字段越界或校验不过 ⇒ **整体拒绝**，不做部分写入
///   （否则会出现「以为改了 A，结果 B 也悄悄变了」）。
/// - `domain` 可改，但它**决定区域归属**（`.ai→intl` / 其余→cn），
///   改动会让该账号换区；调用方须自行提示用户。
pub fn update_account_fields(account_id: &str, patch: &Value) -> Result<Value, String> {
    update_account_fields_at(&accounts_file(), account_id, patch)
}

/// 写盘核心（**路径可注入**）：`cfg(test)` 下必须走临时目录，
/// 否则单测会改到用户的真实账号库（红线：写用户数据文件的函数必须做路径隔离）。
fn update_account_fields_at(path: &Path, account_id: &str, patch: &Value) -> Result<Value, String> {
    let mut checked = validate_and_normalize_patch(patch)?;

    // 读-改-写（进程级锁：与导入共用同一把，避免并发读改写互相覆盖）
    static EDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = EDIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut accounts = load_accounts_from_path(path);
    let Some(acc) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(account_id))
    else {
        return Err(format!("账号不存在：{account_id}"));
    };
    let Some(map) = acc.as_object_mut() else {
        return Err("账号记录不是对象".to_string());
    };
    for (k, v) in checked.drain(..) {
        map.insert(k, v);
    }
    let updated = acc.clone();
    save_accounts_to_path(path, &accounts).map_err(|e| format!("保存账号库失败：{e}"))?;
    Ok(updated)
}

/// 纯函数：校验并归一化 patch（不读写盘，便于单测）。
///
/// 返回 `(字段名, 归一化后的值)` 列表。任一项不合法即整体 `Err`——
/// **全有或全无**，绝不部分写入（否则会出现「以为改了 A，结果 B 也悄悄变了」）。
fn validate_and_normalize_patch(patch: &Value) -> Result<Vec<(String, Value)>, String> {
    let Some(obj) = patch.as_object() else {
        return Err("patch 必须是 JSON 对象".to_string());
    };
    if obj.is_empty() {
        return Err("没有要修改的字段".to_string());
    }
    let mut checked: Vec<(String, Value)> = Vec::new();
    for (k, v) in obj {
        if !EDITABLE_FIELDS.contains(&k.as_str()) {
            return Err(format!(
                "字段 `{k}` 不可编辑；允许：{}",
                EDITABLE_FIELDS.join(" / ")
            ));
        }
        match k.as_str() {
            "expiresAt" | "refreshExpiresAt" => {
                let n = v
                    .as_i64()
                    .ok_or_else(|| format!("`{k}` 必须是整数（ms epoch）"))?;
                if n <= 0 {
                    return Err(format!("`{k}` 必须为正数"));
                }
                checked.push((k.clone(), Value::from(n)));
            }
            _ => {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("`{k}` 必须是字符串"))?
                    .trim()
                    .to_string();
                // 凭证与 domain 不许清空（清空等于弄死账号）；nickname 允许清空（回落 uid 展示）。
                if s.is_empty() && k != "nickname" {
                    return Err(format!("`{k}` 不能为空"));
                }
                checked.push((k.clone(), Value::from(s)));
            }
        }
    }
    Ok(checked)
}

/// 导入本机当前账号（从认证文件读取）。
///
/// 串行化：auth 文件加密时回退到进程内存扫描（可达两分钟），并发点击会在
/// 读改写账号库时竞争出重复条目，用互斥锁保证一次只跑一个导入。
pub fn import_local() -> Result<Value, String> {
    static IMPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = IMPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let acc = crate::modules::auth_file::import_from_auth_file()
        .ok_or("未读取到本地 WorkBuddy 登录信息")?;
    let saved = save_collected_account(acc).map_err(|e| e.to_string())?;
    Ok(account_meta(&saved))
}

// 手动添加账号（token 方式）已随 UI 入口「手动添加」一并下线；
// `identity_email` 中的 "手动添加" 占位过滤保留，用于兼容历史手动添加的旧账号。
