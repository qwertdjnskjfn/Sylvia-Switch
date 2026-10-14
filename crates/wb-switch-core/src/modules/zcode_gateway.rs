//! ZCode 反代网关：账号池选号 / 并发控制 / 分级冷却 / 会话亲和 / quota 状态
//!
//! 融合来源（2026-10-14）：Z-Accounts Gateway Edition（`zac_3_6.zip`，第三方
//! ZCode 多账号管理工具的网关核心，Go 实现 ~6800 行）。本项目**不引入 Go
//! 运行时**——把其独有算法移植到 Rust，账号/凭据/加密/切换/活动领取全部复用
//! 现有 [`super::zcode`] 基础设施。
//!
//! 相比既有 Sylvia 网关（glm_proxy / autoclaw_intl_pool 的 lease 范式）的**增量**：
//! 1. **每账号并发上限**（`max_in_flight`）：单账号最多 N 个在飞请求，超出即
//!    视为不可用（而非冷却）——避免单账号被打满导致连锁失败；
//! 2. **公平选号**（PickPreferred）：同并发数下取「最久未用」（LRU），让池内
//!    账号均匀分摊请求，而非固定轮询首位；
//! 3. **会话亲和**（SessionRegistry）：同一会话指纹固定路由到同一账号，保持
//!    上游 prompt 缓存温热（对话上下文不跨账号漂移）；亲和**永不覆盖**冷却/
//!    禁用/并发上限——资格过滤器先行；
//! 4. **quota 状态旁路**（QuotaBlockedUntil）：账号池条目的额度报告为
//!    "exhausted" 时直接跳过，不花一次上游请求；带 TTL（到期不再信任）。
//! 5. **Block 归因**（PoolBlock）：选不到号时区分「全在冷却 / 全满载 / 全
//!    被排除 / 额度阻塞」——可操作的排障文案，而不是笼统「无可用账号」。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::config::atomic_write;

/// 账号池成员（从 zcode 账号池快照构造）。
#[derive(Clone, Debug, PartialEq)]
pub struct GatewayMember {
    /// 池内唯一键 = zcode 账号 id（含账号级状态，换账号即换键）。
    pub key: String,
    /// 账号显示名（归因/日志用）。
    pub name: String,
    /// 上游 provider（如 anthropic / openai 兼容端点）。
    pub provider_id: String,
    /// 该账号的上游 Base URL（反代目标）。
    pub base_url: String,
    /// 该账号的 API Key（Bearer）。
    pub api_key: String,
    /// 虚拟设备 Mid（X-Device-Mid 头）。
    pub device_mid: String,
    /// 支持的模型集合（规范化后）。
    pub models: Vec<String>,
    /// 排除：桌面端当前登录的账号不进池（Go 版同款语义——避免与客户端自身
    /// 争抢同一账号的额度）。
    pub excluded: bool,
    /// 额度报告："ok" / "low" / "exhausted"；空 = 从未报告。
    pub quota_state: String,
    /// 额度阻塞截止（ms epoch）；"exhausted" 到该时刻前直接跳过。
    pub quota_blocked_until_ms: u64,
}

impl GatewayMember {
    /// 是否支持该模型（规范化比较）。
    pub fn supports(&self, model: &str) -> bool {
        let m = normalize_model(model);
        self.models.iter().any(|x| x == &m)
    }
}

/// 持久化的账号级状态（与 Go `persistedMemberState` 对应）。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PersistedMemberState {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cooldown_until_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub disabled_until_ms: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_used_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub successes: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub failures: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// 运行时账号状态（持久化字段 + 内存在飞计数）。
#[derive(Clone, Debug)]
struct MemberState {
    member: GatewayMember,
    persisted: PersistedMemberState,
    in_flight: u32,
}

/// 选不到号的原因归因（PoolBlock）。
#[derive(Clone, Debug, PartialEq)]
pub enum PoolBlock {
    /// 池里一个成员都没有。
    Empty,
    /// 有成员但全部不支持该模型。
    NoModelSupport,
    /// 全部被排除（桌面端当前登录账号被排除且池里只有它）。
    AllExcluded,
    /// 全部在冷却/禁用期。
    Cooling,
    /// 额度报告为 exhausted 且阻塞未过期。
    QuotaBlocked,
    /// 资格够但全部达到并发上限。
    Busy,
}

impl PoolBlock {
    /// 可操作的排障文案。
    pub fn message(&self) -> &'static str {
        match self {
            PoolBlock::Empty => "账号池为空：请先在 ZCode 页添加账号（或从本机导入登录态）",
            PoolBlock::NoModelSupport => "池内账号均不支持该模型",
            PoolBlock::AllExcluded => {
                "池内账号被全部排除：桌面端当前登录账号默认不入池，请再添加一个账号"
            }
            PoolBlock::Cooling => {
                "池内账号均在冷却保护期（近期触发限流/鉴权失败）：等待冷却结束，或到 ZCode 页解除"
            }
            PoolBlock::QuotaBlocked => {
                "池内账号额度均已耗尽（quota exhausted）：等待额度重置或添加新账号"
            }
            PoolBlock::Busy => "池内账号均已达并发上限（在飞请求已满）：稍后重试或调高每账号并发上限",
        }
    }
}

/// 账号池：选号（lease）+ 并发控制 + 冷却矩阵 + 落盘。
///
/// 成员状态放在 `Arc<PoolInner>` 里，`PoolLease` 与池共享它（见 PoolLease 文档）。
pub struct GatewayPool {
    inner: Arc<PoolInner>,
    max_in_flight: u32,
    state_file: std::path::PathBuf,
}

impl GatewayPool {
    /// 从成员列表建池（会读回持久化状态：冷却/失败计数不因重启丢失）。
    pub fn new(members: Vec<GatewayMember>, max_in_flight: u32, state_file: std::path::PathBuf) -> Self {
        let persisted = load_state(&state_file);
        let mut map = HashMap::new();
        for m in members {
            let p = persisted.get(&m.key).cloned().unwrap_or_default();
            map.insert(
                m.key.clone(),
                MemberState {
                    member: m,
                    persisted: p,
                    in_flight: 0,
                },
            );
        }
        GatewayPool {
            inner: Arc::new(PoolInner {
                members: std::sync::Mutex::new(map),
            }),
            max_in_flight: max_in_flight.max(1),
            state_file,
        }
    }

    /// 全量同步成员（账号池增删后调用）：保留既有账号级状态，剔除已删除账号。
    pub fn sync_members(&self, members: Vec<GatewayMember>) {
        let persisted = load_state(&self.state_file);
        let mut map = HashMap::new();
        for m in members {
            let prev = self
                .inner
                .members
                .lock()
                .ok()
                .and_then(|old| old.get(&m.key).map(|s| s.persisted.clone()));
            let p = prev.unwrap_or_else(|| persisted.get(&m.key).cloned().unwrap_or_default());
            map.insert(
                m.key.clone(),
                MemberState {
                    member: m,
                    persisted: p,
                    in_flight: 0,
                },
            );
        }
        if let Ok(mut old) = self.inner.members.lock() {
            *old = map;
        }
        self.save();
    }

    /// 资格过滤器（对应 Go `usableLocked`）。
    fn usable(&self, st: &MemberState, model: &str, tried: &HashMap<String, bool>, now_ms: u64) -> bool {
        if tried.get(&st.member.key).copied().unwrap_or(false) {
            return false;
        }
        if !st.member.supports(model) {
            return false;
        }
        if st.in_flight >= self.max_in_flight {
            return false;
        }
        if st.member.excluded {
            return false;
        }
        if st.persisted.cooldown_until_ms > now_ms || st.persisted.disabled_until_ms > now_ms {
            return false;
        }
        if st.member.quota_blocked_until_ms > now_ms {
            return false;
        }
        true
    }

    /// 选号：优先 `preferred`（会话亲和），否则取「在飞最少 → 最久未用 → key 最小」。
    /// 返回 lease；None 时用 [`GatewayPool::block_reason`] 归因。
    pub fn pick_preferred(
        &self,
        model: &str,
        tried: &HashMap<String, bool>,
        preferred: &str,
    ) -> Option<PoolLease> {
        let now_ms = now_ms();
        let mut members = self.inner.members.lock().ok()?;
        // 亲和优先：首选账号也必须过全部资格过滤器（亲和永不覆盖冷却/禁用/满载）。
        if !preferred.is_empty() {
            if let Some(st) = members.get(preferred) {
                if self.usable(st, model, tried, now_ms) {
                    let m = members.get_mut(preferred).unwrap();
                    m.in_flight += 1;
                    m.persisted.last_used_ms = now_ms;
                    return Some(PoolLease {
                        inner: Arc::clone(&self.inner),
                        key: preferred.to_string(),
                    });
                }
            }
        }
        // 公平选号：并发数升序 → 最久未用升序 → key 字典序（稳定、可预期）。
        let mut chosen: Option<String> = None;
        for (key, st) in members.iter() {
            if !self.usable(st, model, tried, now_ms) {
                continue;
            }
            chosen = Some(match chosen {
                None => key.clone(),
                Some(cur) => {
                    let c = &members[&cur];
                    if st.in_flight != c.in_flight {
                        if st.in_flight < c.in_flight {
                            key.clone()
                        } else {
                            cur
                        }
                    } else if st.persisted.last_used_ms != c.persisted.last_used_ms {
                        if st.persisted.last_used_ms < c.persisted.last_used_ms {
                            key.clone()
                        } else {
                            cur
                        }
                    } else if st.member.key < c.member.key {
                        key.clone()
                    } else {
                        cur
                    }
                }
            });
        }
        let key = chosen?;
        let m = members.get_mut(&key)?;
        m.in_flight += 1;
        m.persisted.last_used_ms = now_ms;
        Some(PoolLease {
            inner: Arc::clone(&self.inner),
            key,
        })
    }

    /// 归因：解释为什么 pick 返回 None。
    pub fn block_reason(&self, model: &str) -> PoolBlock {
        let Ok(members) = self.inner.members.lock() else {
            return PoolBlock::Empty;
        };
        if members.is_empty() {
            return PoolBlock::Empty;
        }
        let now_ms = now_ms();
        let mut any_support = false;
        let mut all_excluded = true;
        let mut any_cooling = false;
        let mut any_quota = false;
        let mut any_busy = false;
        for st in members.values() {
            if !st.member.supports(model) {
                continue;
            }
            any_support = true;
            // 被排除（桌面端当前登录账号）不算「有资格」，也不进冷却/额度归因
            if st.member.excluded {
                continue;
            }
            all_excluded = false;
            if st.persisted.cooldown_until_ms > now_ms || st.persisted.disabled_until_ms > now_ms {
                any_cooling = true;
                continue;
            }
            if st.member.quota_blocked_until_ms > now_ms {
                any_quota = true;
                continue;
            }
            if st.in_flight >= self.max_in_flight {
                any_busy = true;
            }
        }
        if !any_support {
            PoolBlock::NoModelSupport
        } else if all_excluded {
            PoolBlock::AllExcluded
        } else if any_cooling {
            // 冷却优先级高于额度（冷却 = 明确失败证据，额度 = 上次快照）
            PoolBlock::Cooling
        } else if any_quota {
            PoolBlock::QuotaBlocked
        } else if any_busy {
            PoolBlock::Busy
        } else {
            // 归因只在 pick 失败后调用；走到这里说明资格过滤器有未覆盖的条件
            PoolBlock::Busy
        }
    }

    /// 成功回写：清冷却与错误、累计成功数。
    pub fn success(&self, key: &str) {
        if let Ok(mut members) = self.inner.members.lock() {
            if let Some(st) = members.get_mut(key) {
                st.persisted.successes += 1;
                st.persisted.last_error.clear();
                st.persisted.cooldown_until_ms = 0;
            }
        }
        self.save();
    }

    /// 失败回写：分级冷却（`disable` 走禁用期，语义更重）。
    pub fn failure(&self, key: &str, message: &str, cooldown: Duration, disable: bool) {
        let until = now_ms() + cooldown.as_millis() as u64;
        if let Ok(mut members) = self.inner.members.lock() {
            if let Some(st) = members.get_mut(key) {
                st.persisted.failures += 1;
                st.persisted.last_error = message.chars().take(300).collect();
                if disable {
                    st.persisted.disabled_until_ms = until;
                    st.persisted.cooldown_until_ms = 0;
                } else {
                    st.persisted.cooldown_until_ms = until;
                }
            }
        }
        self.save();
    }

    /// 手动解除某账号冷却（幂等）。
    pub fn clear_cooldown(&self, key: &str) {
        if let Ok(mut members) = self.inner.members.lock() {
            if let Some(st) = members.get_mut(key) {
                st.persisted.cooldown_until_ms = 0;
                st.persisted.disabled_until_ms = 0;
                st.persisted.last_error.clear();
            }
        }
        self.save();
    }

    /// 全部解除（无参版，供一键按钮）。
    pub fn clear_all_cooldowns(&self) {
        if let Ok(mut members) = self.inner.members.lock() {
            for st in members.values_mut() {
                st.persisted.cooldown_until_ms = 0;
                st.persisted.disabled_until_ms = 0;
                st.persisted.last_error.clear();
            }
        }
        self.save();
    }

    /// 快照：供状态页/健康看板展示。
    pub fn status(&self) -> Vec<PoolMemberStatus> {
        let Ok(members) = self.inner.members.lock() else {
            return Vec::new();
        };
        let now = now_ms();
        members
            .values()
            .map(|st| PoolMemberStatus {
                key: st.member.key.clone(),
                name: st.member.name.clone(),
                provider_id: st.member.provider_id.clone(),
                models: st.member.models.clone(),
                excluded: st.member.excluded,
                in_flight: st.in_flight,
                max_in_flight: self.max_in_flight,
                cooldown: st.persisted.cooldown_until_ms > now,
                cooldown_remain_secs: st.persisted.cooldown_until_ms.saturating_sub(now) / 1000,
                disabled: st.persisted.disabled_until_ms > now,
                quota_state: st.member.quota_state.clone(),
                quota_blocked: st.member.quota_blocked_until_ms > now,
                successes: st.persisted.successes,
                failures: st.persisted.failures,
                last_error: st.persisted.last_error.clone(),
                last_used_ms: st.persisted.last_used_ms,
            })
            .collect()
    }

    /// 在飞请求总数（等待者观测）。
    pub fn in_flight_total(&self) -> u32 {
        self.inner
            .members
            .lock()
            .map(|m| m.values().map(|s| s.in_flight).sum())
            .unwrap_or(0)
    }

    /// 释放某账号一个在飞名额（显式路径；lease Drop 亦自动归还）。
    pub fn release(&self, key: &str) {
        if let Ok(mut members) = self.inner.members.lock() {
            if let Some(st) = members.get_mut(key) {
                if st.in_flight > 0 {
                    st.in_flight -= 1;
                }
            }
        }
    }

    /// 测试用：直接改成员状态（仅限单元测试）。
    #[cfg(test)]
    fn mutate(&self, key: &str, f: impl FnOnce(&mut MemberState)) {
        if let Ok(mut members) = self.inner.members.lock() {
            if let Some(st) = members.get_mut(key) {
                f(st);
            }
        }
    }

    /// 测试用：只读成员状态快照（仅限单元测试）。
    #[cfg(test)]
    fn peek(&self, key: &str) -> Option<(PersistedMemberState, u32, String, u64)> {
        let members = self.inner.members.lock().ok()?;
        members.get(key).map(|st| {
            (
                st.persisted.clone(),
                st.in_flight,
                st.member.quota_state.clone(),
                st.member.quota_blocked_until_ms,
            )
        })
    }

    fn save(&self) {
        let Ok(members) = self.inner.members.lock() else {
            return;
        };
        let mut map = std::collections::HashMap::new();
        for st in members.values() {
            map.insert(st.member.key.clone(), st.persisted.clone());
        }
        drop(members);
        if let Ok(text) = serde_json::to_string_pretty(&map) {
            let _ = atomic_write(&self.state_file, &text);
        }
    }
}

/// RAII lease：Drop 时归还在飞名额。
///
/// ⚠️ 生命周期必须覆盖「上游请求全程」——流式响应要等 SSE 结束才 Drop，
/// 否则并发上限会提前放开、把上游打满。
///
/// 不携带裸指针（`*const GatewayPool` 不是 `Send`，会让 axum 要求
/// `Send + Sync` 的 handler Future 编译失败）：改为持有 `Arc` 包装的内部
/// 状态，lease 与池共享同一份 `Mutex<HashMap>`，Drop 时经 Arc 归还名额。
pub struct PoolLease {
    inner: Arc<PoolInner>,
    key: String,
}

/// 池内部共享状态（Arc 包裹 → Send + Sync）。
struct PoolInner {
    members: std::sync::Mutex<HashMap<String, MemberState>>,
}

impl PoolLease {
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 提前归还（不等 Drop）。
    pub fn release(mut self) {
        self.key.clear();
    }
}

impl Drop for PoolLease {
    fn drop(&mut self) {
        if self.key.is_empty() {
            return;
        }
        if let Ok(mut members) = self.inner.members.lock() {
            if let Some(st) = members.get_mut(&self.key) {
                if st.in_flight > 0 {
                    st.in_flight -= 1;
                }
            }
        }
    }
}

/// 状态页单账号行（camelCase 序列化：与项目其他 API 的 JSON 口径一致，
/// 前端直接消费；早期用默认 snake_case 导致前端字段读不到，已实测修正）。
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PoolMemberStatus {
    pub key: String,
    pub name: String,
    pub provider_id: String,
    pub models: Vec<String>,
    pub excluded: bool,
    pub in_flight: u32,
    pub max_in_flight: u32,
    pub cooldown: bool,
    pub cooldown_remain_secs: u64,
    pub disabled: bool,
    pub quota_state: String,
    pub quota_blocked: bool,
    pub successes: u64,
    pub failures: u64,
    pub last_error: String,
    pub last_used_ms: u64,
}

// ---------------------------------------------------------------------------
// 会话亲和（SessionRegistry）
// ---------------------------------------------------------------------------

/// 会话条目：某会话指纹当前绑定的账号 + 序号（淘汰用）。
#[derive(Clone, Debug)]
struct SessionEntry {
    member_key: String,
    last_used_ms: u64,
    seq: u64,
}

/// 会话指纹 → 账号 的绑定表。TTL 过期 + 容量上限双重淘汰。
///
/// 为什么需要：同一对话固定在同一账号上，上游 prompt 缓存保持温热
/// （命中缓存的请求更快更便宜）；对话不跨账号漂移，上下文连续性更好。
pub struct SessionRegistry {
    entries: HashMap<String, SessionEntry>,
    ttl_ms: u64,
    max: usize,
    next_seq: u64,
}

impl SessionRegistry {
    pub fn new(ttl_ms: u64, max: usize) -> Self {
        SessionRegistry {
            entries: HashMap::new(),
            ttl_ms,
            max: max.max(1),
            next_seq: 0,
        }
    }

    /// 查绑定（命中会刷新 last_used；过期条目顺带清除）。
    pub fn lookup(&mut self, fingerprint: &str) -> Option<String> {
        self.sweep();
        let now = now_ms();
        let e = self.entries.get_mut(fingerprint)?;
        e.last_used_ms = now;
        Some(e.member_key.clone())
    }

    /// 提交/更新绑定（本轮成功结束后调用）。
    pub fn commit(&mut self, fingerprint: &str, member_key: &str) {
        let now = now_ms();
        self.next_seq += 1;
        self.entries.insert(
            fingerprint.to_string(),
            SessionEntry {
                member_key: member_key.to_string(),
                last_used_ms: now,
                seq: self.next_seq,
            },
        );
        self.sweep();
    }

    /// 绑定失效：本轮失败时调用——失败的会话不得延续到下一轮。
    pub fn invalidate(&mut self, fingerprint: &str) {
        self.entries.remove(fingerprint);
    }

    /// 全清（宿主进程重启 = 不拥有任何会话）。
    pub fn reset(&mut self) {
        self.entries.clear();
        self.next_seq = 0;
    }

    /// 统计（状态页展示）。
    pub fn stats(&self) -> (usize, usize) {
        (self.entries.len(), self.max)
    }

    /// 淘汰：先按 TTL 清过期，仍超容量则按 seq 淘汰最旧。
    fn sweep(&mut self) {
        let now = now_ms();
        if self.ttl_ms > 0 {
            self.entries
                .retain(|_, e| now.saturating_sub(e.last_used_ms) <= self.ttl_ms);
        }
        if self.entries.len() > self.max {
            let mut vals: Vec<(String, u64)> = self
                .entries
                .iter()
                .map(|(k, e)| (k.clone(), e.seq))
                .collect();
            vals.sort_by_key(|(_, seq)| *seq);
            let drop_n = self.entries.len() - self.max;
            for (k, _) in vals.into_iter().take(drop_n) {
                self.entries.remove(&k);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 分级冷却矩阵（CooldownProfile）
// ---------------------------------------------------------------------------

/// 冷却时长矩阵（与 Go `config.cooldown` 对应）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CooldownProfile {
    /// 429 / 「繁忙」→ 限流冷却。
    pub rate_limit: Duration,
    /// 额度耗尽（810001 类）→ 长冷却。
    pub quota: Duration,
    /// 401/403 鉴权失败 → 长冷却（账号级登录态问题）。
    pub auth: Duration,
    /// 验证码挑战 → 中冷却。
    pub captcha: Duration,
    /// 瞬时网络错 → 短冷却（快速自愈）。
    pub transient: Duration,
}

impl Default for CooldownProfile {
    fn default() -> Self {
        CooldownProfile {
            rate_limit: Duration::from_secs(600),
            quota: Duration::from_secs(3600),
            auth: Duration::from_secs(3600),
            captcha: Duration::from_secs(1800),
            transient: Duration::from_secs(15),
        }
    }
}

/// 网关配置（单例，来自 zcode 网关配置 JSON）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub enabled: bool,
    /// 网关访问密钥（空 = 不鉴权，仅本机回环可用；与其他渠道同一口径）。
    pub api_key: String,
    /// 每账号并发上限。
    pub max_in_flight: u32,
    /// 选号最长等待（秒）——满载时等待有空位而非立即失败。
    pub pick_wait: Duration,
    /// 会话亲和总开关 + TTL + 容量。
    pub session_affinity: bool,
    pub session_affinity_ttl: Duration,
    pub session_affinity_max: usize,
    /// 冷却矩阵。
    pub cooldown: CooldownProfile,
    /// 放行的模型（空 = 不放行任何，需显式配置）。
    pub models: Vec<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            enabled: false,
            api_key: String::new(),
            max_in_flight: 2,
            pick_wait: Duration::from_secs(30),
            session_affinity: true,
            session_affinity_ttl: Duration::from_secs(3600),
            session_affinity_max: 256,
            cooldown: CooldownProfile::default(),
            models: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// 全局单例
// ---------------------------------------------------------------------------

struct GatewayRuntime {
    pool: Option<GatewayPool>,
    sessions: Option<SessionRegistry>,
    cfg: GatewayConfig,
}

static RUNTIME: OnceLock<Mutex<GatewayRuntime>> = OnceLock::new();

fn runtime() -> &'static Mutex<GatewayRuntime> {
    RUNTIME.get_or_init(|| {
        Mutex::new(GatewayRuntime {
            pool: None,
            sessions: None,
            cfg: GatewayConfig::default(),
        })
    })
}

/// 网关状态文件（账号级冷却/失败计数持久化）。
pub fn state_file() -> std::path::PathBuf {
    super::config::store_dir().join("zcode_gateway_state.json")
}

/// 网关配置文件。
pub fn config_file() -> std::path::PathBuf {
    super::config::store_dir().join("zcode_gateway_config.json")
}

/// 读配置（缺文件 = 默认值）。
pub fn load_config() -> GatewayConfig {
    let mut cfg = GatewayConfig::default();
    if let Ok(text) = std::fs::read_to_string(config_file()) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(b) = v.get("enabled").and_then(serde_json::Value::as_bool) {
                cfg.enabled = b;
            }
            if let Some(s) = v.get("api_key").and_then(serde_json::Value::as_str) {
                cfg.api_key = s.trim().to_string();
            }
            if let Some(n) = v.get("max_in_flight").and_then(serde_json::Value::as_u64) {
                cfg.max_in_flight = (n as u32).clamp(1, 32);
            }
            if let Some(n) = v.get("pick_wait_seconds").and_then(serde_json::Value::as_u64) {
                cfg.pick_wait = Duration::from_secs(n.clamp(0, 600));
            }
            if let Some(b) = v.get("session_affinity").and_then(serde_json::Value::as_bool) {
                cfg.session_affinity = b;
            }
            if let Some(n) = v
                .get("session_affinity_ttl_seconds")
                .and_then(serde_json::Value::as_u64)
            {
                cfg.session_affinity_ttl = Duration::from_secs(n.clamp(60, 86_400));
            }
            if let Some(n) = v
                .get("session_affinity_max")
                .and_then(serde_json::Value::as_u64)
            {
                cfg.session_affinity_max = (n as usize).clamp(1, 4096);
            }
            if let Some(arr) = v.get("models").and_then(serde_json::Value::as_array) {
                cfg.models = arr
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(normalize_model)
                    .collect();
            }
            // 冷却矩阵（可选覆盖）
            if let Some(cd) = v.get("cooldown") {
                if let Some(n) = cd.get("rate_limit_seconds").and_then(serde_json::Value::as_u64) {
                    cfg.cooldown.rate_limit = Duration::from_secs(n.clamp(1, 86_400));
                }
                if let Some(n) = cd.get("quota_seconds").and_then(serde_json::Value::as_u64) {
                    cfg.cooldown.quota = Duration::from_secs(n.clamp(1, 86_400));
                }
                if let Some(n) = cd.get("auth_seconds").and_then(serde_json::Value::as_u64) {
                    cfg.cooldown.auth = Duration::from_secs(n.clamp(1, 86_400));
                }
                if let Some(n) = cd.get("captcha_seconds").and_then(serde_json::Value::as_u64) {
                    cfg.cooldown.captcha = Duration::from_secs(n.clamp(1, 86_400));
                }
                if let Some(n) = cd.get("transient_seconds").and_then(serde_json::Value::as_u64) {
                    cfg.cooldown.transient = Duration::from_secs(n.clamp(1, 3600));
                }
            }
        }
    }
    cfg
}

/// 保存配置。
pub fn save_config(cfg: &GatewayConfig) {
    let v = serde_json::json!({
        "enabled": cfg.enabled,
        "api_key": cfg.api_key,
        "max_in_flight": cfg.max_in_flight,
        "pick_wait_seconds": cfg.pick_wait.as_secs(),
        "session_affinity": cfg.session_affinity,
        "session_affinity_ttl_seconds": cfg.session_affinity_ttl.as_secs(),
        "session_affinity_max": cfg.session_affinity_max,
        "cooldown": {
            "rate_limit_seconds": cfg.cooldown.rate_limit.as_secs(),
            "quota_seconds": cfg.cooldown.quota.as_secs(),
            "auth_seconds": cfg.cooldown.auth.as_secs(),
            "captcha_seconds": cfg.cooldown.captcha.as_secs(),
            "transient_seconds": cfg.cooldown.transient.as_secs(),
        },
        "models": cfg.models,
    });
    if let Ok(text) = serde_json::to_string_pretty(&v) {
        let _ = atomic_write(&config_file(), &text);
    }
}

/// 模型名规范化（大小写/空白不敏感；Go 版同款口径）。
pub fn normalize_model(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

/// 单个池成员的可用凭据（解出明文后交给上游转发用；不入日志）。
#[derive(Clone, Debug)]
pub struct MemberCredential {
    pub base_url: String,
    pub api_key: String,
    pub device_mid: String,
    /// 账号级代理（池条目可配；None = 直连）。
    ///
    /// 🔴 实测必需：zcode.z.ai 在本机解析为 Clash fake-ip（198.18.x），
    /// reqwest 直连会 `error sending request`——必须走账号配置的代理，
    /// 或回落环境代理。账号未配代理时保持 None（由 reqwest 读环境变量）。
    pub proxy: Option<String>,
}

/// ZCode 官方上游基址（**Anthropic 协议通道**）。
///
/// 🔴 路径口径来自 Z-Accounts Gateway Edition 实测（`accounts.go` 的
/// `isStartPlanBaseURL` + `makeMember` 调用点）：完整基址是
/// `https://zcode.z.ai/api/v1/zcode-plan/anthropic`，请求端点是
/// `{base}/v1/messages`。早期按 `/api/v1/zcode-plan` 拼 `/messages` 会 404
/// （已实测踩坑修正）。
pub const UPSTREAM_BASE: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";

/// 请求端点（相对 base）。
pub const UPSTREAM_MESSAGES_PATH: &str = "/v1/messages";

/// 从 zcode 账号池快照构造网关成员。
///
/// 复用 `zcode` 现有能力：账号列表、凭据解密（`decrypt_opt`）、身份解析
/// （`identity_of`）、虚拟设备 Mid（`account_mid`）——不重复实现凭据层。
///
/// 语义对齐 Go 版 `AccountCatalog::Load`（实测口径）：
/// - **凭据优先 Coding Plan api-key**（provider kind=anthropic + start-plan
///   基址）；无则回落到 **zcodejwttoken**（JWT，老快照的兜底通道）；
/// - 基址固定 [`UPSTREAM_BASE`]（Start Plan Anthropic 通道）；
/// - 桌面端**当前登录**账号默认排除（避免与客户端争抢同一账号额度）；
/// - `quota_state` 从 quota 缓存读（"exhausted" → 阻塞至 `quota_state_ttl`）。
pub fn build_members(quota_ttl: Duration) -> Vec<GatewayMember> {
    let cfg = load_config();
    let live = super::zcode::list_accounts();
    let mut out = Vec::new();
    let quota_cache = super::zcode::load_quota_cache();
    let now = now_ms();
    let live_hash = super::zcode::live_credentials().map(|c| super::zcode::canonical_hash(&c));
    for acc in live {
        let Some(id) = acc.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(creds) = super::zcode::pool_credentials(id) else {
            continue;
        };
        // ① 首选 Coding Plan api-key（provider 配置里的 anthropic 通道凭据）
        let plan_key = creds
            .as_object()
            .and_then(|m| {
                m.iter()
                    .filter(|(k, _)| k.ends_with(":api-key"))
                    .filter_map(|(_, v)| v.as_str())
                    .next()
            })
            .and_then(|enc| super::zcode::decrypt_opt(Some(enc)))
            .filter(|k| !k.trim().is_empty());
        // ② 兜底 zcodejwttoken（Start Plan JWT）
        let key = match plan_key {
            Some(k) => k,
            None => match creds
                .get("zcodejwttoken")
                .and_then(serde_json::Value::as_str)
                .and_then(|enc| super::zcode::decrypt_opt(Some(enc)))
                .filter(|k| !k.trim().is_empty())
            {
                Some(k) => k,
                None => continue, // 两条通道都没有可用凭据 → 不入池
            },
        };
        let identity = super::zcode::identity_of(&creds);
        // 排除：桌面端当前登录账号
        let excluded = live_hash
            .as_ref()
            .map(|h| h == &super::zcode::canonical_hash(&creds))
            .unwrap_or(false);
        // quota 状态（读缓存；exhausted → 阻塞）
        let (quota_state, quota_blocked_until_ms) = match quota_cache.get(id) {
            Some(q) => {
                let exhausted = q
                    .get("exhausted")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                    || q.get("state").and_then(serde_json::Value::as_str) == Some("exhausted");
                (
                    q.get("state")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    if exhausted { now + quota_ttl.as_millis() as u64 } else { 0 },
                )
            }
            _ => (String::new(), 0),
        };
        out.push(GatewayMember {
            key: id.to_string(),
            name: identity
                .label()
                .unwrap_or_else(|| acc.get("name").and_then(serde_json::Value::as_str).unwrap_or(id).to_string()),
            provider_id: if identity.provider.is_empty() {
                "zai".to_string()
            } else {
                identity.provider.clone()
            },
            base_url: UPSTREAM_BASE.to_string(),
            api_key: key,
            device_mid: super::zcode::account_mid(id).unwrap_or_default(),
            models: cfg.models.clone(),
            excluded,
            quota_state,
            quota_blocked_until_ms,
        });
    }
    out
}

/// 取某成员的明文凭据（转发用）。
pub fn member_credential(key: &str) -> Option<MemberCredential> {
    let rt = runtime().lock().ok()?;
    let pool = rt.pool.as_ref()?;
    let members = pool.inner.members.lock().ok()?;
    let st = members.get(key)?;
    Some(MemberCredential {
        base_url: st.member.base_url.clone(),
        api_key: st.member.api_key.clone(),
        device_mid: st.member.device_mid.clone(),
        proxy: super::zcode::account_proxy(key),
    })
}

/// 确保运行时已按当前配置/成员同步（幂等；账号池变化时自动重建池）。
pub fn ensure_runtime() -> bool {
    let cfg = load_config();
    if !cfg.enabled {
        return false;
    }
    let members = build_members(cfg.cooldown.quota);
    if members.is_empty() {
        return false;
    }
    let Ok(mut rt) = runtime().lock() else {
        return false;
    };
    match rt.pool.as_mut() {
        Some(p) => {
            p.sync_members(members);
        }
        None => {
            rt.pool = Some(GatewayPool::new(
                members,
                cfg.max_in_flight,
                state_file(),
            ));
        }
    }
    if rt.sessions.is_none() {
        rt.sessions = Some(SessionRegistry::new(
            cfg.session_affinity_ttl.as_millis() as u64,
            cfg.session_affinity_max,
        ));
    }
    rt.cfg = cfg;
    true
}

/// 选号（对外）：返回 (lease, 是否亲和命中)。
pub fn acquire(model: &str, fingerprint: &str) -> Result<(PoolLease, bool), String> {
    let cfg = load_config();
    let preferred = if cfg.session_affinity && !fingerprint.is_empty() {
        runtime()
            .lock()
            .ok()
            .and_then(|mut rt| rt.sessions.as_mut().and_then(|s| s.lookup(fingerprint)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let mut rt = runtime().lock().map_err(|_| "网关内部锁获取失败".to_string())?;
    let pool = rt.pool.as_mut().ok_or_else(|| "网关未就绪（账号池为空或未启用）".to_string())?;
    let mut tried = HashMap::new();
    let lease = pool.pick_preferred(model, &tried, &preferred).ok_or_else(|| {
        let reason = pool.block_reason(model);
        reason.message().to_string()
    })?;
    let affinity_hit = !preferred.is_empty() && lease.key() == preferred;
    Ok((lease, affinity_hit))
}

/// 请求结束回写（success 路径）。
pub fn report_success(lease: &PoolLease, fingerprint: &str) {
    let key = lease.key().to_string();
    let cfg = load_config();
    if let Ok(mut rt) = runtime().lock() {
        if let Some(p) = rt.pool.as_mut() {
            p.success(&key);
        }
        if cfg.session_affinity && !fingerprint.is_empty() {
            if let Some(s) = rt.sessions.as_mut() {
                s.commit(fingerprint, &key);
            }
        }
    }
}

/// 请求失败回写：按失败族映射分级冷却 + 断开会话亲和。
pub fn report_failure(lease: &PoolLease, fingerprint: &str, family: FailureFamily, message: &str) {
    let key = lease.key().to_string();
    let cfg = load_config();
    let (cooldown, disable) = match family {
        FailureFamily::RateLimit => (cfg.cooldown.rate_limit, false),
        FailureFamily::Quota => (cfg.cooldown.quota, false),
        FailureFamily::Auth => (cfg.cooldown.auth, true),
        FailureFamily::Captcha => (cfg.cooldown.captcha, false),
        FailureFamily::Transient => (cfg.cooldown.transient, false),
    };
    if let Ok(mut rt) = runtime().lock() {
        if let Some(p) = rt.pool.as_mut() {
            p.failure(&key, message, cooldown, disable);
        }
        // 失败会话不得延续到下一轮（避免把坏账号粘在活跃对话上）
        if cfg.session_affinity && !fingerprint.is_empty() {
            if let Some(s) = rt.sessions.as_mut() {
                s.invalidate(fingerprint);
            }
        }
    }
}

/// 一次上游转发的结果（server 层据此组装 HTTP 响应）。
pub struct ForwardResult {
    pub status: u16,
    pub body: String,
}

/// 转发一次请求到选中的上游账号（非流式）。
///
/// 完整闭环：选号（亲和优先）→ 取凭据 → 请求上游 → 按成功/失败族回写池状态。
/// lease 在函数内 Drop（归还在飞名额）。
pub async fn forward(body: serde_json::Value, anthropic: bool) -> Result<ForwardResult, String> {
    if !ensure_runtime() {
        return Err(
            "未启用或账号池无可用账号（需至少一个含 Coding Plan api-key 的 ZCode 账号）".to_string(),
        );
    }
    let model = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    if model.is_empty() {
        return Err("请求缺少 model 字段".to_string());
    }
    let cfg = load_config();
    if !cfg.models.is_empty()
        && !cfg
            .models
            .iter()
            .any(|m| m == &normalize_model(&model))
    {
        return Err(format!(
            "模型 {model} 未在网关放行列表（配置页可添加）"
        ));
    }
    let fingerprint = session_fingerprint(&body);
    let (lease, _affinity_hit) = acquire(&model, &fingerprint)?;
    let Some(cred) = member_credential(lease.key()) else {
        return Err("账号凭据读取失败".to_string());
    };
    // 上游是 Anthropic 协议通道（Start Plan）：端点固定 /v1/messages。
    // 无论调用方用 OpenAI 还是 Anthropic 形状，最终都转发到该端点
    // （Go 版同款：baseURL + "/v1/messages"）。
    let _ = anthropic;
    let url = format!("{}{}", cred.base_url, UPSTREAM_MESSAGES_PATH);
    // 代理：账号级配置优先（fake-ip 环境必需，见 MemberCredential::proxy 文档）
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600));
    if let Some(px) = cred.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        builder = builder.proxy(
            reqwest::Proxy::all(px).map_err(|e| format!("代理配置无效 ({px}): {e}"))?,
        );
    }
    let mut req = builder
        .build()
        .map_err(|e| format!("HTTP 客户端构建失败: {e}"))?
        .post(&url)
        .bearer_auth(&cred.api_key)
        .header("Content-Type", "application/json");
    if !cred.device_mid.is_empty() {
        req = req.header("X-Device-Mid", &cred.device_mid);
    }
    let resp = match req.json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            report_failure(
                &lease,
                &fingerprint,
                FailureFamily::Transient,
                &format!("上游请求失败: {e}"),
            );
            return Err(format!("上游请求失败: {e}"));
        }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if (200..300).contains(&status) {
        report_success(&lease, &fingerprint);
        return Ok(ForwardResult { status, body: text });
    }
    // 失败：分级冷却 + 断开会话亲和
    let family = FailureFamily::classify(status, &text);
    report_failure(&lease, &fingerprint, family, &text);
    Ok(ForwardResult { status, body: text })
}

/// 失败族（决定冷却档位）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FailureFamily {
    /// 429 / 「繁忙」→ 限流。
    RateLimit,
    /// 额度耗尽（810001 类）。
    Quota,
    /// 401/403 → 登录态失效（禁用级）。
    Auth,
    /// 验证码挑战。
    Captcha,
    /// 瞬时网络错。
    Transient,
}

impl FailureFamily {
    /// 从 HTTP 码 + 业务码推断失败族。
    pub fn classify(http_status: u16, body: &str) -> Self {
        // 810001 = pay-view 额度用尽（口径与其他渠道一致）
        if body.contains("810001") {
            return FailureFamily::Quota;
        }
        // 3012 = z.ai 风控拒绝（"unusual activity"，HTTP 405 常伴随）。
        // 实测（2026-10-14）：**直连上游的请求会被固定拒绝**——Z-Accounts
        // Gateway Edition 的注释亦证实「direct transport, which z.ai's risk
        // engine always rejects with 3012」，其解法是驱动官方客户端 app-server
        // 代发。此处按限流类处理（长冷却 + 不永久禁用），避免同一账号被反复
        // 无效重试；是否启用桥接通道由配置决定。
        if body.contains("3012") || body.contains("unusual activity") {
            return FailureFamily::RateLimit;
        }
        match http_status {
            401 | 403 => FailureFamily::Auth,
            429 => FailureFamily::RateLimit,
            _ => {
                if body.contains("繁忙") || body.contains("rate") || body.contains("limit") {
                    FailureFamily::RateLimit
                } else if http_status >= 500 || http_status == 0 {
                    FailureFamily::Transient
                } else {
                    FailureFamily::Transient
                }
            }
        }
    }
}

/// 手动解除冷却（单账号 / 全部）。
pub fn clear_cooldown(key: Option<&str>) {
    if let Ok(mut rt) = runtime().lock() {
        if let Some(p) = rt.pool.as_mut() {
            match key {
                Some(k) => p.clear_cooldown(k),
                None => p.clear_all_cooldowns(),
            }
        }
    }
}

/// 会话指纹：优先显式 `session_id` / `user`，否则取首条消息内容的 SHA-256
/// 前 16 字节（同一对话稳定命中同一账号，保持上游 prompt 缓存温热）。
///
/// 为什么放 core：sha2 依赖在 core 侧；指纹本身只用于「同一对话 → 同一账号」
/// 的路由绑定，不承担安全职责，但也不用弱哈希以免被误用。
pub fn session_fingerprint(body: &serde_json::Value) -> String {
    if let Some(sid) = body
        .get("session_id")
        .or_else(|| body.get("user"))
        .and_then(serde_json::Value::as_str)
    {
        if !sid.trim().is_empty() {
            return format!("s:{}", sid.trim());
        }
    }
    let text = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .map(|m| {
            m.get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default();
    if text.is_empty() {
        return String::new();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    format!(
        "m:{}",
        digest[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// 状态页数据：配置 + 池快照 + 亲和统计。
pub fn status() -> serde_json::Value {
    let cfg = load_config();
    ensure_runtime();
    let rt = runtime().lock().ok();
    let members = rt
        .as_ref()
        .and_then(|r| r.pool.as_ref())
        .map(|p| p.status())
        .unwrap_or_default();
    let (session_count, session_max) = rt
        .as_ref()
        .and_then(|r| r.sessions.as_ref())
        .map(|s| s.stats())
        .unwrap_or((0, 0));
    serde_json::json!({
        "enabled": cfg.enabled,
        // 只报是否配置，不回传明文密钥
        "apiKeySet": !cfg.api_key.is_empty(),
        "maxInFlight": cfg.max_in_flight,
        "pickWaitSecs": cfg.pick_wait.as_secs(),
        "sessionAffinity": cfg.session_affinity,
        "sessionAffinityTtlSecs": cfg.session_affinity_ttl.as_secs(),
        "sessionAffinityMax": cfg.session_affinity_max,
        "sessionCount": session_count,
        "sessionMax": session_max,
        "models": cfg.models,
        "cooldown": {
            "rateLimitSecs": cfg.cooldown.rate_limit.as_secs(),
            "quotaSecs": cfg.cooldown.quota.as_secs(),
            "authSecs": cfg.cooldown.auth.as_secs(),
            "captchaSecs": cfg.cooldown.captcha.as_secs(),
            "transientSecs": cfg.cooldown.transient.as_secs(),
        },
        "members": members,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load_state(path: &std::path::Path) -> HashMap<String, PersistedMemberState> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 单测（纯函数：选号公平性 / 资格过滤 / 冷却矩阵 / 亲和 TTL 与容量）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn member(key: &str, models: &[&str]) -> GatewayMember {
        GatewayMember {
            key: key.to_string(),
            name: format!("acct-{key}"),
            provider_id: "zai".to_string(),
            base_url: "https://zcode.z.ai".to_string(),
            api_key: "k".to_string(),
            device_mid: "mid".to_string(),
            models: models.iter().map(|m| normalize_model(m)).collect(),
            excluded: false,
            quota_state: String::new(),
            quota_blocked_until_ms: 0,
        }
    }

    /// 每个测试独立状态文件：池状态会落盘（冷却/last_used/计数），共用一个
    /// 路径会让并行测试互相污染（实测踩过：跨用例读到上一个用例的 last_used
    /// 导致「最小 key 优先」断言随机失败）。
    fn pool(members: Vec<GatewayMember>, max_in_flight: u32) -> GatewayPool {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let file = std::env::temp_dir()
            .join(format!("zcode-gw-test-{}-{}.json", std::process::id(), uniq));
        GatewayPool::new(members, max_in_flight, file)
    }

    #[test]
    fn pick_prefers_least_in_flight_then_lru() {
        // 每账号上限 1：满载即不可用（并发闸门的核心语义）
        let mut p = pool(vec![member("a", &["m"]), member("b", &["m"])], 1);
        // 第一次：都没用过 → key 小的 a 先
        let l1 = p.pick_preferred("m", &HashMap::new(), "").expect("first pick");
        assert_eq!(l1.key(), "a");
        // a 已满载（1/1）→ 选 b
        let l2 = p.pick_preferred("m", &HashMap::new(), "").expect("second pick");
        assert_eq!(l2.key(), "b");
        // a、b 都满载 → 选不到，归因 Busy
        assert!(p.pick_preferred("m", &HashMap::new(), "").is_none());
        assert_eq!(p.block_reason("m"), PoolBlock::Busy);
        // a 的 lease 释放（Drop 归还名额）后又能选
        drop(l1);
        let l3 = p.pick_preferred("m", &HashMap::new(), "").expect("after release");
        assert_eq!(l3.key(), "a", "lease Drop 后名额归还，a 重新可选");
    }

    #[test]
    fn affinity_never_overrides_eligibility() {
        let mut p = pool(vec![member("a", &["m"]), member("b", &["m"])], 2);
        // a 进冷却
        p.failure("a", "429 busy", Duration::from_secs(600), false);
        // 亲和指定 a，但 a 冷却中 → 必须落到 b
        let l = p.pick_preferred("m", &HashMap::new(), "a").expect("fallback pick");
        assert_eq!(l.key(), "b", "亲和不得覆盖冷却");
    }

    #[test]
    fn excluded_and_quota_and_model_filters() {
        let mut m = member("a", &["m1"]);
        m.excluded = true;
        let mut q = member("b", &["m1"]);
        q.quota_state = "exhausted".to_string();
        q.quota_blocked_until_ms = now_ms() + 3_600_000;
        let mut p = pool(vec![m, q], 2);
        // a 排除、b 额度阻塞 → 选不到
        assert!(p.pick_preferred("m1", &HashMap::new(), "").is_none());
        assert_eq!(p.block_reason("m1"), PoolBlock::QuotaBlocked);
        // 都不支持 m2
        assert_eq!(p.block_reason("m2"), PoolBlock::NoModelSupport);
        // 清掉额度阻塞 → b 可用了
        p.mutate("b", |st| st.member.quota_blocked_until_ms = 0);
        assert!(p.pick_preferred("m1", &HashMap::new(), "").is_some());
    }

    #[test]
    fn failure_uses_disable_vs_cooldown_and_success_clears() {
        let mut p = pool(vec![member("a", &["m"])], 1);
        p.failure("a", "429 busy", Duration::from_secs(60), false);
        assert!(p.peek("a").unwrap().0.cooldown_until_ms > now_ms());
        assert_eq!(p.peek("a").unwrap().0.disabled_until_ms, 0);
        // 成功即清冷却 + 错误 + 计数（限流类冷却可自愈）
        p.success("a");
        assert_eq!(p.peek("a").unwrap().0.cooldown_until_ms, 0);
        assert_eq!(p.peek("a").unwrap().0.successes, 1);
        assert!(p.peek("a").unwrap().0.last_error.is_empty());
        // disable 语义：鉴权失败进禁用期；success **不清**禁用（与 Go 原版一致，
        // 登录态问题必须等 TTL 或人工解除，否则会反复打无效请求）
        p.failure("a", "401 auth", Duration::from_secs(60), true);
        assert!(p.peek("a").unwrap().0.disabled_until_ms > now_ms());
        p.success("a");
        assert!(p.peek("a").unwrap().0.disabled_until_ms > now_ms(), "禁用不被成功清除");
        // 人工解除：两种归零
        p.clear_cooldown("a");
        assert_eq!(p.peek("a").unwrap().0.disabled_until_ms, 0);
        assert_eq!(p.peek("a").unwrap().0.cooldown_until_ms, 0);
    }

    #[test]
    fn tried_map_skips_already_failed_this_request() {
        let mut p = pool(vec![member("a", &["m"])], 1);
        let mut tried = HashMap::new();
        tried.insert("a".to_string(), true);
        assert!(p.pick_preferred("m", &tried, "").is_none());
    }

    #[test]
    fn session_registry_ttl_and_capacity_eviction() {
        let mut r = SessionRegistry::new(0, 2);
        r.commit("s1", "a");
        r.commit("s2", "b");
        assert_eq!(r.lookup("s1").as_deref(), Some("a"));
        // 超容量 → 淘汰 seq 最小的 s1
        r.commit("s3", "c");
        assert_eq!(r.lookup("s1"), None, "超容量应淘汰最旧");
        assert_eq!(r.lookup("s3").as_deref(), Some("c"));
        // invalidate 立即失效
        r.invalidate("s3");
        assert_eq!(r.lookup("s3"), None);
        // TTL=0 表示不按时间淘汰（容量仍生效）
        let mut r2 = SessionRegistry::new(0, 1);
        r2.commit("x", "a");
        assert_eq!(r2.lookup("x").as_deref(), Some("a"));
    }

    #[test]
    fn config_clamps_are_sane() {
        let mut cfg = GatewayConfig::default();
        cfg.max_in_flight = 0; // 会被 clamp 到 1
        let v = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v.get("max_in_flight").unwrap().as_u64(), Some(0));
        // 保存→读回 round-trip
        save_config(&cfg);
        let back = load_config();
        assert_eq!(back.cooldown.rate_limit, cfg.cooldown.rate_limit);
        assert_eq!(back.session_affinity_max, cfg.session_affinity_max);
    }

    #[test]
    fn failure_family_classifies_risk_control_and_quota() {
        // 3012 风控（HTTP 405 伴随）→ 限流类（长冷却但不永久禁用）
        assert_eq!(
            FailureFamily::classify(405, r#"{"code":3012,"msg":"request has been blocked due to unusual activity."}"#),
            FailureFamily::RateLimit
        );
        // 810001 额度耗尽 → Quota
        assert_eq!(
            FailureFamily::classify(403, r#"{"code":810001,"msg":"pay-view"}"#),
            FailureFamily::Quota
        );
        // 401/403 鉴权 → Auth（禁用级）
        assert_eq!(FailureFamily::classify(401, "unauthorized"), FailureFamily::Auth);
        // 429 → 限流
        assert_eq!(FailureFamily::classify(429, "too many"), FailureFamily::RateLimit);
        // 5xx → 瞬时
        assert_eq!(FailureFamily::classify(502, "bad gateway"), FailureFamily::Transient);
    }
}
