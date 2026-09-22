//! 官方流水（`ledgers_std`）读取：分页抓取与「模型消耗」筛选。
//!
//! 这是**唯一**的流水网络读取实现 —— 原先模型用量（对账）与积分统计（AutoClaw 窗口）
//! 各拉一次流水，既重复请求又容易口径漂移。解析与匹配规则在
//! [`super::super::reconcile`]，此处只管「把数据取回来」。

use serde_json::Value;

use crate::modules::autoclaw_accounts::PoolAccount;
use crate::modules::config::{http_request, now_ms};
use crate::modules::platform_checkin::autoclaw_headers;
use crate::modules::stats::reconcile::{self, LedgerEntry};

/// 流水单页条数。实测接口按 `page_size` 如实返回（该账号全量仅 190 条也能一页拿全），
/// 而默认 100 条在高频使用日远远不够 —— 待对账的调用经常落在第 100 条之外。
pub const LEDGER_PAGE_SIZE: usize = 500;

/// 最多翻页数：500 × 4 = 2000 条，足够覆盖重度使用日的待对账窗口。
pub const LEDGER_MAX_PAGES: usize = 4;

/// 按账号 token 查询官方流水 debit 条目，自动跟随 `next_page_token` 游标。
///
/// ⚠️ **必须用该账号自己的 token**（见 `model_usage::reconcile_with_ledger` 的说明）：
/// 扣费记在各账号自己的账本里，用别人的 token 查会漏且可能错配。
///
/// `oldest_needed_ms`：不需要比这更早的条目，覆盖到即提前停止，避免给官方接口
/// 做无意义翻页。实测游标到底后会**重复返回最后一页**，因此必须校验游标前进。
pub async fn fetch_entries(
    token: &str,
    oldest_needed_ms: i64,
) -> Result<Vec<LedgerEntry>, String> {
    // 流水记在账号所属区域的账本里：按 token 路由网关。
    let host = crate::modules::autoclaw_accounts::region_for_token(token).api_host();
    let mut candidates: Vec<LedgerEntry> = Vec::new();
    let mut page_token: Option<String> = None;
    let mut prev_oldest: Option<i64> = None;
    for _ in 0..LEDGER_MAX_PAGES {
        let mut url = format!(
            "{host}/agent-assetmgr/api/v1/ledgers_std?asset_type=point&wallet_type=all&page_size={LEDGER_PAGE_SIZE}"
        );
        if let Some(token) = &page_token {
            url.push_str("&page_token=");
            url.push_str(token);
        }
        let resp = http_request(&url, "GET", None, Some(&autoclaw_headers(Some(token)))).await;
        if resp.get("code").and_then(Value::as_i64) != Some(0) {
            return Err("查询 AutoClaw 流水失败".to_string());
        }
        let entries = resp
            .pointer("/data/entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if entries.is_empty() {
            break;
        }
        let mut page_oldest = i64::MAX;
        for entry in &entries {
            let Some(parsed) = reconcile::parse_entry(entry) else {
                continue;
            };
            page_oldest = page_oldest.min(parsed.ts);
            candidates.push(parsed);
        }
        // 已覆盖到需要的时间下限 → 不必继续翻页。
        if page_oldest <= oldest_needed_ms {
            break;
        }
        // 游标没有前进（重复返回同一批最旧条目）→ 防死循环，立即停止。
        if prev_oldest.is_some_and(|previous| page_oldest >= previous) {
            break;
        }
        prev_oldest = Some(page_oldest);
        let next = resp
            .pointer("/data/next_page_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|token| !token.is_empty());
        match next {
            Some(token) if page_token.as_deref() != Some(token.as_str()) => {
                page_token = Some(token)
            }
            _ => break,
        }
    }
    Ok(candidates)
}

/// 窗口内「模型消耗积分」流水（按账号 token 查官方流水账）。
///
/// 只统计真实的模型调用扣费，排除积分过期、签到加赠等其他余额变动 —— 供积分统计页
/// 核算 AutoClaw 账号的真实消耗（余额差会把过期清零误判为消耗）。
///
/// 返回完整 [`LedgerEntry`]（含 `id` 与 `model`），调用方可据此做归因/去重。
pub async fn model_consumption(account: &PoolAccount, since_ms: i64) -> Vec<LedgerEntry> {
    let candidates = match fetch_entries(&account.bearer(), since_ms).await {
        Ok(candidates) => candidates,
        Err(_) => return Vec::new(),
    };
    let now = now_ms();
    candidates
        .into_iter()
        .filter(|entry| entry.ts >= since_ms && entry.ts <= now)
        .collect()
}
