//! 模型目录自动同步引擎（契约 docs/model-sync-contract.md §三，T1/T2/T3/T4/T7）。
//!
//! 12 行网关适配器（10 自动源 + 2 手动源）：每轮逐网关
//! 「拉最新 → 与上次快照 diff → 检测新增 → 上架动作 → 记录」。
//!
//! 落盘（全部在 `~/.wb-switch/` 下，损坏 JSON 一律视为空重建、不 panic）：
//! - `model_sync_state.json`    每网关上次清单快照（diff 基线）；
//! - `model_sync_registry.json` 上架注册表（grok 自动追加 / glmweb、qoder 手动补录）；
//! - `model_sync_log.json`      环形日志（近 50 轮全量结果）；
//! - `model_sync_config.json`   `{enabled, intervalMinutes}`（定时轮每轮现读，热更新）。
//!
//! 红线（契约 §六）：同步引擎不走网关请求路径（log_gateway_request 零新增 tag）；
//! 不写任何他应用配置文件；workbuddy_proxy::models_catalog / glm_proxy::resolve_route
//! 等既有语义零改动（本模块只读它们的既有 pub 拉取函数）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

#[cfg(test)]
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};

// ---------------------------------------------------------------------------
// 适配器表（契约 §3.1）
// ---------------------------------------------------------------------------

/// 单条同步模型（id 权威，name/倍率尽力而为——倍率目前仅 WorkBuddy ACP 目录提供）。
#[derive(Clone, Debug, PartialEq)]
pub struct SyncModel {
    pub id: String,
    pub name: Option<String>,
    pub multiplier: Option<f64>,
}

/// 源类型：Auto = 每轮自动拉取；Manual = 无上游清单端点，仅页面手动补录。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    Auto,
    Manual,
}

impl SourceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceMode::Auto => "auto",
            SourceMode::Manual => "manual",
        }
    }
}

/// 上架动作分型（契约 §3.3）：
/// - `ReportOnly`：清单已动态，新增仅记录报告（glm/kimi/qwen/loomy/miaoshou/trae/
///   workbuddy/workbuddy-pool/opencode）；
/// - `RegistrySync`：diff 出的 added 自动追加注册表并合并进 models_payload（grok）；
/// - `RegistryManual`：仅页面手动补录写入注册表（glmweb/qoder）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingAction {
    ReportOnly,
    RegistrySync,
    RegistryManual,
}

/// 拉取函数形态：统一装箱成 Future，便于静态适配器表持有函数指针。
pub type FetchFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<SyncModel>, String>> + Send>,
>;
pub type FetchFn = fn() -> FetchFuture;

/// 一行适配器：网关（与 log_gateway_request tag 口径一致）+ 模式 + 来源描述 + 拉取器。
pub struct SyncSource {
    pub gateway: &'static str,
    pub mode: SourceMode,
    /// 上架动作分型。
    pub action: ListingAction,
    /// 来源描述（status 端点展示用）。
    pub source: &'static str,
    /// 拉取器（Manual 行返回 Err("manual")）。
    pub fetch: FetchFn,
}

/// 13 行适配器表（11 自动源 + 2 手动源）。
pub const SYNC_SOURCES: &[SyncSource] = &[
    SyncSource {
        gateway: "glm",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "AutoClaw settings.json catalog（本机文件，AutoClaw 更新后下轮捕捉）",
        fetch: fetch_glm,
    },
    SyncSource {
        gateway: "kimi",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "www.kimi.com GetAvailableModels（5min 缓存）",
        fetch: fetch_kimi,
    },
    SyncSource {
        gateway: "grok",
        mode: SourceMode::Auto,
        action: ListingAction::RegistrySync,
        source: "x.ai GET /v1/models（Bearer = 账号池 build token）",
        fetch: fetch_grok,
    },
    SyncSource {
        gateway: "qwen",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "chat.qwen.ai/api/models（含静态兜底 7 条）",
        fetch: fetch_qwen,
    },
    SyncSource {
        gateway: "loomy",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "loomyad.xunfei.cn/api/v1/models",
        fetch: fetch_loomy,
    },
    SyncSource {
        gateway: "miaoshou",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "美团 ai 流域 model-types（含静态兜底）",
        fetch: fetch_miaoshou,
    },
    SyncSource {
        gateway: "trae",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "TRAE 客户端 SQLite state.vscdb（本地缓存）",
        fetch: fetch_trae,
    },
    SyncSource {
        gateway: "workbuddy",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "ACP session/new availableModels（native-first 目录）",
        fetch: fetch_workbuddy,
    },
    SyncSource {
        gateway: "workbuddy-pool",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "cloud /v3/config（cli agent 模型清单）",
        fetch: fetch_workbuddy_pool,
    },
    SyncSource {
        gateway: "opencode",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "本机 OpenCode server /provider（connected providers）",
        fetch: fetch_opencode,
    },
    SyncSource {
        gateway: "dumate",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "静态目录（dumate-lite/turbo/ultra/points，档位 L0/L1/L2）",
        fetch: fetch_dumate,
    },
    // MiniMax（2026-10-09）：模型表取桌面端 config.yaml 的真实目录（defaultModel
    // 置顶），回退硬编码表——与 /minimax/v1/models 同源，不做人工补录。
    SyncSource {
        gateway: "minimax",
        mode: SourceMode::Auto,
        action: ListingAction::ReportOnly,
        source: "桌面端 config.yaml 模型目录（defaultModel 置顶，回退硬编码表）",
        fetch: fetch_minimax,
    },
    SyncSource {
        gateway: "glmweb",
        mode: SourceMode::Manual,
        action: ListingAction::RegistryManual,
        source: "无清单端点（网页协议 model 不发上游）——手动补录",
        fetch: fetch_manual,
    },
    SyncSource {
        gateway: "qoder",
        mode: SourceMode::Manual,
        action: ListingAction::RegistryManual,
        source: "无清单概念（max/pro/lite 三档）——手动补录",
        fetch: fetch_manual,
    },
];

fn sync_model(id: String, name: Option<String>) -> SyncModel {
    SyncModel { id, name, multiplier: None }
}

/// 带倍率的构造（workbuddy ACP 目录等提供 x 倍率的源）。
fn sync_model_m(id: String, name: Option<String>, multiplier: Option<f64>) -> SyncModel {
    SyncModel { id, name, multiplier }
}

fn trim_opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// 从 OpenAI /v1/models 形状的 JSON（`data[]`）提取 (id, name)。
fn models_from_openai_list(value: &Value) -> Vec<SyncModel> {
    value
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(Value::as_str)?;
                    if id.is_empty() {
                        return None;
                    }
                    Some(sync_model_m(
                        id.to_string(),
                        m.get("name")
                            .or_else(|| m.get("display_name"))
                            .or_else(|| m.get("description"))
                            .and_then(Value::as_str)
                            .and_then(trim_opt),
                        m.get("multiplier").and_then(Value::as_f64),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

// --- 各网关拉取器（全部复用既有 pub 实现，零语义改动） ---

/// glm：`glm_proxy::model_routes()` 读 AutoClaw settings.json catalog（含静态兜底）。
fn fetch_glm() -> FetchFuture {
    Box::pin(async move {
        Ok(crate::modules::glm_proxy::model_routes()
            .into_iter()
            .map(|r| {
                sync_model(r.route_id.clone(), trim_opt(&r.alias).or(trim_opt(&r.body_model)))
            })
            .collect())
    })
}

/// kimi：`kimi::model_catalog()`（POST GetAvailableModels，5min TTL 缓存）。
fn fetch_kimi() -> FetchFuture {
    Box::pin(async move {
        let (specs, _default) = crate::modules::kimi::model_catalog().await?;
        Ok(specs
            .into_iter()
            .map(|s| sync_model(s.id, trim_opt(&s.display_name)))
            .collect())
    })
}

/// grok：新拉取器（grok.rs 实现，chat 路径零改动）。
fn fetch_grok() -> FetchFuture {
    Box::pin(async move {
        let list = crate::modules::grok::fetch_xai_models().await?;
        Ok(list
            .into_iter()
            .map(|(id, name)| sync_model(id, name))
            .collect())
    })
}

/// qwen：`qwen::fetch_desktop_models()`（动态 + 静态兜底 7 条）。
fn fetch_qwen() -> FetchFuture {
    Box::pin(async move {
        let models = crate::modules::qwen::fetch_desktop_models().await;
        Ok(models
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(Value::as_str)?;
                if id.is_empty() {
                    return None;
                }
                Some(sync_model(
                    id.to_string(),
                    m.get("name").and_then(Value::as_str).and_then(trim_opt),
                ))
            })
            .collect())
    })
}

/// loomy：`loomy_proxy::fetch_models()`（OpenAI list 形状）。
fn fetch_loomy() -> FetchFuture {
    Box::pin(async move {
        let value = crate::modules::loomy_proxy::fetch_models().await?;
        Ok(models_from_openai_list(&value))
    })
}

/// miaoshou：`miaoshou::fetch_models_payload_fresh()`（绕缓存直拉；None = 无账号/上游失败）。
fn fetch_miaoshou() -> FetchFuture {
    Box::pin(async move {
        let value = crate::modules::miaoshou::fetch_models_payload_fresh()
            .await
            .ok_or_else(|| "妙手模型目录拉取失败（无账号 accessToken 或上游不可达）".to_string())?;
        Ok(models_from_openai_list(&value))
    })
}

/// trae：`trae_proxy::models_catalog()`（客户端本地 state.vscdb，`{ok, models[]}`）。
fn fetch_trae() -> FetchFuture {
    Box::pin(async move {
        let catalog = crate::modules::trae_proxy::models_catalog().await;
        if catalog.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(catalog
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("TRAE 模型目录不可用")
                .to_string());
        }
        Ok(catalog
            .get("models")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id").and_then(Value::as_str)?;
                        if id.is_empty() {
                            return None;
                        }
                        Some(sync_model(
                            id.to_string(),
                            m.get("display_name")
                                .and_then(Value::as_str)
                                .and_then(trim_opt),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    })
}

/// workbuddy：`trae_acp::native_models_catalog_refresh()`（ACP session/new 目录）。
fn fetch_workbuddy() -> FetchFuture {
    Box::pin(async move {
        let catalog = crate::modules::trae_acp::native_models_catalog_refresh().await?;
        Ok(catalog
            .models
            .into_iter()
            .map(|m| sync_model_m(m.id, trim_opt(&m.name), m.multiplier))
            .collect())
    })
}

/// workbuddy-pool：`workbuddy_upstream::fetch_models(&PoolAccount)`（cloud /v3/config）。
fn fetch_workbuddy_pool() -> FetchFuture {
    Box::pin(async move {
        let acc = crate::modules::workbuddy_gateway::load_pool_accounts()
            .into_iter()
            .next()
            .ok_or_else(|| "WorkBuddy 账号池为空（accounts.json 无 WorkBuddy 账号）".to_string())?;
        let value = crate::modules::workbuddy_upstream::fetch_models(&acc).await?;
        Ok(models_from_openai_list(&value))
    })
}

/// opencode：`workbuddy_proxy::models_catalog()`（/provider，connected providers 展开）。
fn fetch_opencode() -> FetchFuture {
    Box::pin(async move {
        let catalog = crate::modules::workbuddy_proxy::models_catalog().await?;
        let mut out: Vec<SyncModel> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(providers) = catalog.get("providers").and_then(Value::as_array) {
            for provider in providers {
                if let Some(models) = provider.get("models").and_then(Value::as_array) {
                    for m in models {
                        let Some(id) = m.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        if id.is_empty() || !seen.insert(id.to_string()) {
                            continue;
                        }
                        out.push(sync_model(
                            id.to_string(),
                            m.get("name").and_then(Value::as_str).and_then(trim_opt),
                        ));
                    }
                }
            }
        }
        Ok(out)
    })
}

/// Manual 行：无上游清单端点，fetch 固定 Err("manual")（契约 §3.1）。
fn fetch_manual() -> FetchFuture {
    Box::pin(async move { Err("manual".to_string()) })
}

/// dumate：`dumate::models_payload()`（静态 4 模型目录，同步返回，永不失败——
/// 目录是编译期常量，不依赖桌面端是否在运行）。
fn fetch_dumate() -> FetchFuture {
    Box::pin(async move { Ok(models_from_openai_list(&crate::modules::dumate::models_payload())) })
}

/// MiniMax 模型清单（2026-10-09）：与 `/minimax/v1/models` 同源
/// （桌面端 `config.yaml` 真实目录，defaultModel 置顶，回退硬编码表）。
/// 纯本地读配置，无需网络，故 `Box::pin(async { … })` 即可。
fn fetch_minimax() -> FetchFuture {
    Box::pin(async move {
        let cfg = crate::modules::minimax::load_config();
        Ok(models_from_openai_list(
            &crate::modules::minimax::models_payload_filtered(&cfg, None),
        ))
    })
}

// ---------------------------------------------------------------------------
// diff 与基线（契约 §3.2，纯函数便于单测）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct DiffResult {
    /// true = 本轮为基线轮（首次成功建立快照），added 恒为空。
    pub baseline: bool,
    /// 新增模型 = remote − lastIds（保序、按 id 去重）。
    pub added: Vec<SyncModel>,
}

/// `last_ids = None`（或空，即从未成功建立过快照）→ 基线轮；
/// 否则 added = remote − lastIds，保序且不重复上报。
pub fn diff_against(last_ids: Option<&[String]>, remote: &[SyncModel]) -> DiffResult {
    let Some(prev) = last_ids else {
        return DiffResult { baseline: true, added: Vec::new() };
    };
    if prev.is_empty() {
        return DiffResult { baseline: true, added: Vec::new() };
    }
    let seen_prev: HashSet<&String> = prev.iter().collect();
    let mut emitted: HashSet<String> = HashSet::new();
    let added = remote
        .iter()
        .filter(|m| !seen_prev.contains(&m.id) && emitted.insert(m.id.clone()))
        .cloned()
        .collect();
    DiffResult { baseline: false, added }
}

// ---------------------------------------------------------------------------
// 落盘：state / registry / log / config（损坏 JSON 视为空重建，不 panic）
// ---------------------------------------------------------------------------

/// 单网关同步状态（契约 §3.2 形状：lastIds/lastSyncTs/lastStatus/lastError）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GatewayState {
    pub last_ids: Vec<String>,
    pub last_sync_ts: i64,
    pub last_status: String,
    pub last_error: Option<String>,
}

impl Default for GatewayState {
    fn default() -> Self {
        GatewayState {
            last_ids: Vec::new(),
            last_sync_ts: 0,
            last_status: String::new(),
            last_error: None,
        }
    }
}

pub type SyncStateMap = HashMap<String, GatewayState>;

fn read_json_file(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(not(test))]
fn state_path() -> PathBuf {
    store_dir().join("model_sync_state.json")
}

#[cfg(not(test))]
fn registry_path() -> PathBuf {
    store_dir().join("model_sync_registry.json")
}

#[cfg(not(test))]
fn log_path() -> PathBuf {
    store_dir().join("model_sync_log.json")
}

#[cfg(not(test))]
fn config_path() -> PathBuf {
    store_dir().join("model_sync_config.json")
}

fn write_json_file(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let content = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    atomic_write(path, &content).map_err(|e| e.to_string())
}

/// 读状态（路径注入版，供测试）。文件缺失/损坏 → 空表。
pub fn load_state_at(path: &Path) -> SyncStateMap {
    read_json_file(path)
        .and_then(|v| serde_json::from_value::<SyncStateMap>(v).ok())
        .unwrap_or_default()
}

pub fn load_state() -> SyncStateMap {
    load_state_at(&state_path())
}

pub fn save_state_at(path: &Path, state: &SyncStateMap) -> Result<(), String> {
    let value = serde_json::to_value(state).map_err(|e| e.to_string())?;
    write_json_file(path, &value)
}

pub fn save_state(state: &SyncStateMap) -> Result<(), String> {
    save_state_at(&state_path(), state)
}

/// 一条上架注册表条目（契约 §3.3 形状：id/name/addedTs/origin/note）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RegistryEntry {
    pub id: String,
    pub name: Option<String>,
    pub added_ts: i64,
    /// "sync"（引擎自动追加）| "manual"（页面补录）。
    pub origin: String,
    pub note: Option<String>,
}

impl RegistryEntry {
    pub fn new(id: &str, name: Option<String>, origin: &str, note: Option<String>) -> Self {
        RegistryEntry {
            id: id.to_string(),
            name,
            added_ts: now_ms(),
            origin: origin.to_string(),
            note,
        }
    }
}

pub type RegistryMap = HashMap<String, Vec<RegistryEntry>>;

/// 读注册表（路径注入版）。文件缺失/损坏 → 空表。
pub fn load_registry_at(path: &Path) -> RegistryMap {
    read_json_file(path)
        .and_then(|v| serde_json::from_value::<RegistryMap>(v).ok())
        .unwrap_or_default()
}

pub fn load_registry() -> RegistryMap {
    load_registry_at(&registry_path())
}

pub fn save_registry_at(path: &Path, registry: &RegistryMap) -> Result<(), String> {
    let value = serde_json::to_value(registry).map_err(|e| e.to_string())?;
    write_json_file(path, &value)
}

pub fn save_registry(registry: &RegistryMap) -> Result<(), String> {
    save_registry_at(&registry_path(), registry)
}

/// 指定网关的注册表条目（models_payload 合并尾步用；每次实时读盘，
/// 手动补录写入后对应 /models 端点立即生效）。
pub fn registry_entries_for(gateway: &str) -> Vec<RegistryEntry> {
    load_registry().remove(gateway).unwrap_or_default()
}

/// 追加/更新一条注册表条目（同 id upsert：更新 name/note/origin，不动 addedTs）。
/// 只增不删（删除走 [`remove_registry_entry`]）。
pub fn add_registry_entry(
    gateway: &str,
    id: &str,
    name: Option<String>,
    origin: &str,
    note: Option<String>,
) -> Result<(), String> {
    add_registry_entry_at(&registry_path(), gateway, id, name, origin, note)
}

pub fn add_registry_entry_at(
    path: &Path,
    gateway: &str,
    id: &str,
    name: Option<String>,
    origin: &str,
    note: Option<String>,
) -> Result<(), String> {
    let mut registry = load_registry_at(path);
    let entries = registry.entry(gateway.to_string()).or_default();
    if let Some(existing) = entries.iter_mut().find(|e| e.id == id) {
        if name.is_some() {
            existing.name = name;
        }
        if note.is_some() {
            existing.note = note;
        }
        existing.origin = origin.to_string();
    } else {
        entries.push(RegistryEntry::new(id, name, origin, note));
    }
    save_registry_at(path, &registry)
}

/// 删除一条注册表条目；返回是否真的删了。
pub fn remove_registry_entry(gateway: &str, id: &str) -> bool {
    remove_registry_entry_at(&registry_path(), gateway, id)
}

pub fn remove_registry_entry_at(path: &Path, gateway: &str, id: &str) -> bool {
    let mut registry = load_registry_at(path);
    let Some(entries) = registry.get_mut(gateway) else {
        return false;
    };
    let before = entries.len();
    entries.retain(|e| e.id != id);
    if entries.len() == before {
        return false;
    }
    save_registry_at(path, &registry).is_ok()
}

/// 环形日志容量（近 50 轮全量结果，契约 §3.4）。
pub const LOG_MAX_ROUNDS: usize = 50;

/// 读轮次日志（路径注入版，老 → 新序）。文件缺失/损坏 → 空数组。
pub fn load_log_at(path: &Path) -> Vec<Value> {
    read_json_file(path)
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

pub fn load_log() -> Vec<Value> {
    load_log_at(&log_path())
}

fn append_log_at(path: &Path, round: &Value) {
    let mut log = load_log_at(path);
    log.push(round.clone());
    if log.len() > LOG_MAX_ROUNDS {
        let drop = log.len() - LOG_MAX_ROUNDS;
        log.drain(..drop);
    }
    if let Ok(value) = serde_json::to_value(&log) {
        let _ = write_json_file(path, &value);
    }
}

fn append_log(round: &Value) {
    append_log_at(&log_path(), round)
}

/// 同步配置（契约 §四：`{enabled, intervalMinutes}`，默认启用 + 1440 分钟）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncConfig {
    pub enabled: bool,
    pub interval_minutes: i64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig { enabled: true, interval_minutes: 1440 }
    }
}

pub fn load_config() -> SyncConfig {
    load_config_at(&config_path())
}

pub fn load_config_at(path: &Path) -> SyncConfig {
    read_json_file(path)
        .and_then(|v| serde_json::from_value::<SyncConfig>(v).ok())
        .unwrap_or_default()
}

pub fn save_config(cfg: &SyncConfig) -> Result<(), String> {
    let value = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    write_json_file(&config_path(), &value)
}

// ---------------------------------------------------------------------------
// 测试路径隔离（契约 §六 5：state/log/registry 落盘路径测试中用注入隔离，防并行污染）
// ---------------------------------------------------------------------------

#[cfg(test)]
static STATE_PATH_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
#[cfg(test)]
static REGISTRY_PATH_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
#[cfg(test)]
static LOG_PATH_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
#[cfg(test)]
static CONFIG_PATH_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
/// 触碰 override 的测试必须整体串行（避免 A 测试 set 的路径被 B 测试改写）。
#[cfg(test)]
static TEST_PATH_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub fn with_test_store<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let _serial = TEST_PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    *STATE_PATH_OVERRIDE.lock().unwrap() = Some(dir.join("state.json"));
    *REGISTRY_PATH_OVERRIDE.lock().unwrap() = Some(dir.join("registry.json"));
    *LOG_PATH_OVERRIDE.lock().unwrap() = Some(dir.join("log.json"));
    *CONFIG_PATH_OVERRIDE.lock().unwrap() = Some(dir.join("config.json"));
    let out = f();
    *STATE_PATH_OVERRIDE.lock().unwrap() = None;
    *REGISTRY_PATH_OVERRIDE.lock().unwrap() = None;
    *LOG_PATH_OVERRIDE.lock().unwrap() = None;
    *CONFIG_PATH_OVERRIDE.lock().unwrap() = None;
    out
}

#[cfg(test)]
fn override_slot(which: WhichPath) -> &'static Mutex<Option<PathBuf>> {
    match which {
        WhichPath::State => &STATE_PATH_OVERRIDE,
        WhichPath::Registry => &REGISTRY_PATH_OVERRIDE,
        WhichPath::Log => &LOG_PATH_OVERRIDE,
        WhichPath::Config => &CONFIG_PATH_OVERRIDE,
    }
}

#[cfg(test)]
fn test_override_path(which: WhichPath) -> PathBuf {
    let real = match which {
        WhichPath::State => store_dir().join("model_sync_state.json"),
        WhichPath::Registry => store_dir().join("model_sync_registry.json"),
        WhichPath::Log => store_dir().join("model_sync_log.json"),
        WhichPath::Config => store_dir().join("model_sync_config.json"),
    };
    override_slot(which)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or(real)
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum WhichPath {
    State,
    Registry,
    Log,
    Config,
}

#[cfg(test)]
fn config_path() -> PathBuf {
    test_override_path(WhichPath::Config)
}

#[cfg(test)]
fn state_path() -> PathBuf {
    test_override_path(WhichPath::State)
}

#[cfg(test)]
fn registry_path() -> PathBuf {
    test_override_path(WhichPath::Registry)
}

#[cfg(test)]
fn log_path() -> PathBuf {
    test_override_path(WhichPath::Log)
}

// ---------------------------------------------------------------------------
// 同步主流程（契约 §3.2/§3.3/§3.4）
// ---------------------------------------------------------------------------

static SYNC_BUSY: AtomicBool = AtomicBool::new(false);

/// panic 安全的 busy 防护：Drop 在 unwind 路径也会复位标志位，避免
/// run_sync_inner 一旦 panic 后所有后续触发（定时轮+手动）永远 busy（复核 finding #4）。
struct BusyGuard;
impl Drop for BusyGuard {
    fn drop(&mut self) {
        SYNC_BUSY.store(false, Ordering::SeqCst);
    }
}

/// 跑一轮同步。并发防护：进行中再触发直接返回 `{busy: true}`（不排队）。
/// `filter` 指定本轮只跑哪些网关（None = 全部 12 行）。
pub async fn run_sync(trigger: &str, filter: Option<&[String]>) -> Value {
    if SYNC_BUSY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return json!({
            "ok": false,
            "busy": true,
            "error": "同步已在进行中，请稍后再试",
        });
    }
    let _guard = BusyGuard;
    run_sync_inner(trigger, filter).await
}

async fn run_sync_inner(trigger: &str, filter: Option<&[String]>) -> Value {
    let start = Instant::now();
    let ts = now_ms();
    let mut state = load_state();
    let mut results: Vec<Value> = Vec::new();
    let mut added_summary: Vec<String> = Vec::new();

    for src in SYNC_SOURCES {
        if let Some(list) = filter {
            if !list.iter().any(|g| g == src.gateway) {
                continue;
            }
        }
        let result = run_one_source(src, &state).await;
        // 成功轮更新 state 快照（error 轮 run_one_source 已保留 lastIds 只改状态字段）。
        if let Some(new_state) = result.get("newState") {
            if let Ok(parsed) =
                serde_json::from_value::<GatewayState>(new_state.clone())
            {
                state.insert(src.gateway.to_string(), parsed);
            }
        }
        if let Some(added) = result.get("added").and_then(Value::as_array) {
            if !added.is_empty() {
                added_summary.push(format!("{} +{}", src.gateway, added.len()));
            }
        }
        results.push(result);
    }

    if let Err(e) = save_state(&state) {
        eprintln!("[model-sync] state 落盘失败: {e}");
    }
    // 剥离行内 newState（快照已由 save_state 持久化；留在 results 会随日志
    // 落盘 50 轮 × 12 网关 × 全量 id 清单，膨胀且超出契约 §3.4 行形状——复核 finding #2）。
    for result in results.iter_mut() {
        if let Some(obj) = result.as_object_mut() {
            obj.remove("newState");
        }
    }
    let round = json!({
        "ts": ts,
        "trigger": trigger,
        "durationMs": start.elapsed().as_millis() as u64,
        "results": results,
    });
    append_log(&round);
    // T7：非基线新增 → 一条汇总通知（notify 内部 10 分钟节流防刷屏）。
    if !added_summary.is_empty() {
        crate::modules::notify::notify(
            "model-sync",
            "模型同步",
            &format!("检测到新增模型：{}", added_summary.join("、")),
        );
    }
    round
}

/// 单网关处理：拉取 → diff → 上架动作 → 结果行。
/// 返回值内嵌 `newState`（下轮快照；error 轮保留 lastIds）。
async fn run_one_source(src: &SyncSource, state: &SyncStateMap) -> Value {
    let prev_state = state.get(src.gateway);
    let last_ids = prev_state.map(|s| s.last_ids.clone());

    let fetched = (src.fetch)().await;
    match fetched {
        Err(e) if e == "manual" && src.mode == SourceMode::Manual => {
            // 手动网关：不拉取、不 diff。total = 当前注册表条目数（手动补录即上架）。
            let total = registry_entries_for(src.gateway).len();
            return json!({
                "gateway": src.gateway,
                "mode": src.mode.as_str(),
                "ok": true,
                "total": total,
                "added": [],
                "baseline": false,
                "note": "manual",
            });
        }
        Err(e) => {
            // 单网关失败：记 error + lastError，不中断整轮；lastIds 保留不回滚。
            let new_state = GatewayState {
                last_ids: prev_state.map(|s| s.last_ids.clone()).unwrap_or_default(),
                last_sync_ts: now_ms(),
                last_status: "error".to_string(),
                last_error: Some(e.clone()),
            };
            return json!({
                "gateway": src.gateway,
                "mode": src.mode.as_str(),
                "ok": false,
                "total": new_state.last_ids.len(),
                "added": [],
                "baseline": false,
                "error": e,
                "newState": serde_json::to_value(new_state).unwrap_or(Value::Null),
            });
        }
        Ok(models) => {
            let total = models.len();
            let diff = diff_against(last_ids.as_deref(), &models);
            // 上架动作分型：RegistrySync 型把 added 自动追加注册表（origin=sync）；
            // 基线轮 added 恒空，不动注册表（连续失败不回滚已注册条目）。
            if src.action == ListingAction::RegistrySync && !diff.baseline && !diff.added.is_empty()
            {
                for m in &diff.added {
                    if let Err(e) =
                        add_registry_entry(src.gateway, &m.id, m.name.clone(), "sync", None)
                    {
                        eprintln!("[model-sync] {} 注册表追加失败: {e}", src.gateway);
                    }
                }
            }
            let note = if diff.baseline { "baseline established" } else { "" };
            let new_state = GatewayState {
                last_ids: models.iter().map(|m| m.id.clone()).collect(),
                last_sync_ts: now_ms(),
                last_status: "ok".to_string(),
                last_error: None,
            };
            let mut result = json!({
                "gateway": src.gateway,
                "mode": src.mode.as_str(),
                "ok": true,
                "total": total,
                "added": diff.added.iter().map(|m| json!({
                    "id": m.id,
                    "name": m.name,
                    "multiplier": m.multiplier,
                })).collect::<Vec<_>>(),
                "baseline": diff.baseline,
                "newState": serde_json::to_value(new_state).unwrap_or(Value::Null),
            });
            if !note.is_empty() {
                result["note"] = json!(note);
            }
            result
        }
    }
}

// ---------------------------------------------------------------------------
// status 聚合（契约 §四：读 state，秒开不触发拉取）
// ---------------------------------------------------------------------------

/// GET /api/model-sync/status 的数据面：12 行适配器元数据 + state 快照 + 最近轮新增数。
pub fn status_payload() -> Value {
    let cfg = load_config();
    let state = load_state();
    let registry = load_registry();
    let log = load_log();
    let last_round = log.last().cloned().unwrap_or(Value::Null);
    let gateways: Vec<Value> = SYNC_SOURCES
        .iter()
        .map(|src| {
            let st = state.get(src.gateway);
            // manual 网关（glmweb/qoder）不参与同步轮：total=注册表条目数、
            // lastSyncTs=最近补录时刻，否则页面永远显示「—/未同步」低估目录
            // （复核 finding #3）。
            let (status, total, last_sync_ts, last_error) = if src.mode == SourceMode::Manual {
                let entries = registry.get(src.gateway);
                let n = entries.map(|v| v.len()).unwrap_or(0);
                let ts = entries
                    .and_then(|v| v.iter().map(|e| e.added_ts).max())
                    .unwrap_or(0);
                ("ok", n, ts, None)
            } else {
                let status = match st {
                    None => "never",
                    Some(s) if s.last_status == "ok" => "ok",
                    Some(_) => "error",
                };
                (
                    status,
                    st.map(|s| s.last_ids.len()).unwrap_or(0),
                    st.map(|s| s.last_sync_ts).unwrap_or(0),
                    st.and_then(|s| s.last_error.clone()),
                )
            };
            // 最近一轮该网关的新增数（log 无记录 → 0）。
            let added_count = last_round
                .get("results")
                .and_then(Value::as_array)
                .and_then(|rs| {
                    rs.iter().find(|r| {
                        r.get("gateway").and_then(Value::as_str) == Some(src.gateway)
                    })
                })
                .and_then(|r| r.get("added").and_then(Value::as_array))
                .map(|a| a.len())
                .unwrap_or(0);
            json!({
                "gateway": src.gateway,
                "mode": src.mode.as_str(),
                "source": src.source,
                "status": status,
                "ok": status == "ok",
                "lastSyncTs": last_sync_ts,
                "total": total,
                "registryCount": registry.get(src.gateway).map(|v| v.len()).unwrap_or(0),
                "addedCount": added_count,
                "lastError": last_error,
            })
        })
        .collect();
    json!({
        "enabled": cfg.enabled,
        "intervalMinutes": cfg.interval_minutes,
        "nextRunHint": next_run_hint(&cfg, &log),
        "gateways": gateways,
    })
}

/// 下轮提示：最近一条 auto 轮时刻 + 间隔（未启用/从未跑过 → null）。
fn next_run_hint(cfg: &SyncConfig, log: &[Value]) -> Value {
    if !cfg.enabled || cfg.interval_minutes <= 0 {
        return Value::Null;
    }
    let last_auto_ts = log
        .iter()
        .rev()
        .find(|r| r.get("trigger").and_then(Value::as_str) == Some("auto"))
        .and_then(|r| r.get("ts"))
        .and_then(Value::as_i64);
    match last_auto_ts {
        Some(ts) => json!(ts + cfg.interval_minutes * 60_000),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// 定时轮（契约 §3.5，仿 workbuddy_gateway::spawn_balance_patrol_loop）
// ---------------------------------------------------------------------------

/// 首轮延迟（分钟级周期的任务不抢启动窗口）。
const FIRST_DELAY_SECS: u64 = 5 * 60;
/// 0 = 关闭时的空转轮询间隔（保持循环存活，配置热改回即恢复）。
const IDLE_POLL_SECS: u64 = 60;

/// 定时同步循环：启动 5 分钟后首轮，之后每 `intervalMinutes` 分钟一轮；
/// 配置每轮现读（0 = 关闭但循环空转，热更新语义对齐 balance_patrol）。
pub fn spawn_model_sync_loop() {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(FIRST_DELAY_SECS)).await;
        loop {
            let cfg = load_config();
            if cfg.enabled && cfg.interval_minutes > 0 {
                let round = run_sync("auto", None).await;
                let gateways = round
                    .get("results")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                eprintln!("[model-sync] 自动轮完成：{gateways} 个网关已处理");
                tokio::time::sleep(std::time::Duration::from_secs(
                    cfg.interval_minutes as u64 * 60,
                ))
                .await;
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(IDLE_POLL_SECS)).await;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 单测（契约 G1：diff/基线、读写容错、status 聚合）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, name: Option<&str>) -> SyncModel {
        SyncModel { id: id.to_string(), name: name.map(String::from), multiplier: None }
    }

    fn ids(models: &[SyncModel]) -> Vec<String> {
        models.iter().map(|m| m.id.clone()).collect()
    }

    // --- diff 与基线 ---

    #[test]
    fn diff_none_is_baseline() {
        let remote = vec![m("a", None), m("b", Some("B"))];
        let d = diff_against(None, &remote);
        assert!(d.baseline, "无历史快照 = 基线轮");
        assert!(d.added.is_empty(), "基线轮不报新增");
    }

    #[test]
    fn diff_empty_history_is_baseline() {
        // 曾失败过（条目存在但 lastIds 空）→ 仍是基线轮。
        let d = diff_against(Some(&[]), &[m("a", None)]);
        assert!(d.baseline);
        assert!(d.added.is_empty());
    }

    #[test]
    fn diff_detects_new_models_in_order() {
        let last = vec!["a".to_string(), "b".to_string()];
        let remote = vec![m("a", None), m("x", Some("X")), m("b", None), m("y", None)];
        let d = diff_against(Some(&last), &remote);
        assert!(!d.baseline);
        assert_eq!(ids(&d.added), vec!["x".to_string(), "y".to_string()], "保序");
        assert_eq!(d.added[0].name.as_deref(), Some("X"), "name 随条目透传");
    }

    #[test]
    fn diff_no_repeat_when_nothing_new() {
        let last = vec!["a".to_string(), "b".to_string()];
        let remote = vec![m("b", None), m("a", None)];
        let d = diff_against(Some(&last), &remote);
        assert!(!d.baseline);
        assert!(d.added.is_empty(), "二轮无重复上报（G5）");
    }

    #[test]
    fn diff_dedupes_remote_duplicates() {
        let last = vec!["a".to_string()];
        let remote = vec![m("n", None), m("n", Some("dup"))];
        let d = diff_against(Some(&last), &remote);
        assert_eq!(d.added.len(), 1, "remote 内重复 id 只报一次");
    }

    #[test]
    fn diff_removed_models_do_not_appear() {
        // 上游下架不回填 added（只增不删是安全默认，契约 R3）。
        let last = vec!["a".to_string(), "gone".to_string()];
        let remote = vec![m("a", None)];
        let d = diff_against(Some(&last), &remote);
        assert!(d.added.is_empty());
    }

    // --- state 读写容错（tempdir 路径注入，防并行污染） ---

    fn temp_store() -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("wb-model-sync-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn state_roundtrip_and_corruption_tolerance() {
        let dir = temp_store();
        let path = dir.join("state.json");

        // 缺失文件 → 空。
        assert!(load_state_at(&path).is_empty());

        // 写入 → 读回一致。
        let mut st = SyncStateMap::new();
        st.insert(
            "kimi".to_string(),
            GatewayState {
                last_ids: vec!["k1".into(), "k2".into()],
                last_sync_ts: 1234,
                last_status: "ok".into(),
                last_error: None,
            },
        );
        save_state_at(&path, &st).expect("save state");
        let back = load_state_at(&path);
        assert_eq!(back["kimi"].last_ids, vec!["k1".to_string(), "k2".to_string()]);
        assert_eq!(back["kimi"].last_sync_ts, 1234);

        // 损坏 JSON → 视为空重建，不 panic。
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(load_state_at(&path).is_empty());
        save_state_at(&path, &st).expect("重建成功");
        assert_eq!(load_state_at(&path)["kimi"].last_sync_ts, 1234);

        // JSON 类型不对（数组而非对象）→ 空。
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert!(load_state_at(&path).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_camel_case_shape_matches_contract() {
        // 契约 §3.2 钉定字段名：lastIds/lastSyncTs/lastStatus/lastError。
        let dir = temp_store();
        let path = dir.join("state.json");
        let mut st = SyncStateMap::new();
        st.insert(
            "grok".to_string(),
            GatewayState {
                last_ids: vec!["g1".into()],
                last_sync_ts: 42,
                last_status: "error".into(),
                last_error: Some("boom".into()),
            },
        );
        save_state_at(&path, &st).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"lastIds\""), "camelCase: {text}");
        assert!(text.contains("\"lastSyncTs\""), "{text}");
        assert!(text.contains("\"lastError\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_roundtrip_upsert_and_remove() {
        let dir = temp_store();
        let path = dir.join("registry.json");
        assert!(load_registry_at(&path).is_empty(), "缺失 → 空");

        add_registry_entry_at(&path, "glmweb", "glm-9", Some("GLM 9".into()), "manual", None)
            .unwrap();
        add_registry_entry_at(&path, "glmweb", "glm-9-x", None, "manual", Some("备注".into()))
            .unwrap();
        let reg = load_registry_at(&path);
        assert_eq!(reg["glmweb"].len(), 2);
        assert_eq!(reg["glmweb"][0].id, "glm-9");
        assert_eq!(reg["glmweb"][0].origin, "manual");

        // upsert 同 id：更新 name，不追加。
        add_registry_entry_at(&path, "glmweb", "glm-9", Some("GLM 9 Pro".into()), "manual", None)
            .unwrap();
        let reg = load_registry_at(&path);
        assert_eq!(reg["glmweb"].len(), 2);
        assert_eq!(reg["glmweb"][0].name.as_deref(), Some("GLM 9 Pro"));

        // remove：删掉后恢复。
        assert!(remove_registry_entry_at(&path, "glmweb", "glm-9"));
        assert!(!remove_registry_entry_at(&path, "glmweb", "glm-9"), "再删=不存在");
        assert_eq!(load_registry_at(&path)["glmweb"].len(), 1);

        // 损坏 JSON → 空（不 panic）。
        std::fs::write(&path, "]]]").unwrap();
        assert!(load_registry_at(&path).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_camel_case_shape_matches_contract() {
        let dir = temp_store();
        let path = dir.join("registry.json");
        add_registry_entry_at(&path, "grok", "grok-9", None, "sync", None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"addedTs\""), "契约 §3.3 addedTs: {text}");
        assert!(text.contains("\"origin\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_ring_buffer_caps_at_50() {
        let dir = temp_store();
        let path = dir.join("log.json");

        assert!(load_log_at(&path).is_empty(), "缺失 → 空");
        for i in 0..(LOG_MAX_ROUNDS + 10) {
            append_log_at(&path, &json!({"ts": i, "trigger": "auto", "results": []}));
        }
        let log = load_log_at(&path);
        assert_eq!(log.len(), LOG_MAX_ROUNDS, "环形截断到 50");
        assert_eq!(log[0]["ts"], 10, "最老的 10 轮被挤出");
        assert_eq!(log.last().unwrap()["ts"], LOG_MAX_ROUNDS as i64 + 9, "新轮在尾部");

        // 损坏 JSON → 空（append 从空重建）。
        std::fs::write(&path, "{{{").unwrap();
        assert!(load_log_at(&path).is_empty());
        append_log_at(&path, &json!({"ts": 1}));
        assert_eq!(load_log_at(&path).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_roundtrip_and_defaults() {
        let dir = temp_store();
        let path = dir.join("config.json");
        assert_eq!(load_config_at(&path), SyncConfig::default(), "缺失 → 默认 1440/启用");

        std::fs::write(&path, r#"{"enabled": false, "intervalMinutes": 30}"#).unwrap();
        let cfg = load_config_at(&path);
        assert!(!cfg.enabled);
        assert_eq!(cfg.interval_minutes, 30);

        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(load_config_at(&path), SyncConfig::default(), "损坏 → 默认");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- status 聚合与 nextRunHint ---

    #[test]
    fn status_payload_lists_all_gateways() {
        let dir = temp_store();
        with_test_store(&dir, || {
            let payload = status_payload();
            let gateways = payload.get("gateways").and_then(Value::as_array).unwrap();
            assert_eq!(gateways.len(), 14, "契约 G3：14 网关行齐全（含 minimax）");
            let names: Vec<&str> = gateways
                .iter()
                .map(|g| g.get("gateway").and_then(Value::as_str).unwrap())
                .collect();
            for expected in [
                "glm", "kimi", "grok", "qwen", "loomy", "miaoshou", "trae", "workbuddy",
                "workbuddy-pool", "opencode", "dumate", "glmweb", "qoder", "minimax",
            ] {
                assert!(names.contains(&expected), "缺 {expected}: {names:?}");
            }
            let manual = gateways.iter().find(|g| g["gateway"] == "glmweb").unwrap();
            assert_eq!(manual["mode"], "manual");
            let auto = gateways.iter().find(|g| g["gateway"] == "kimi").unwrap();
            assert_eq!(auto["mode"], "auto");
            assert_eq!(auto["status"], "never", "未同步网关状态");
            assert_eq!(payload["intervalMinutes"], 1440, "默认间隔");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_run_hint_follows_last_auto_round() {
        let cfg = SyncConfig { enabled: true, interval_minutes: 60 };
        let log = vec![
            json!({"ts": 1000, "trigger": "manual"}),
            json!({"ts": 2000, "trigger": "auto"}),
        ];
        assert_eq!(next_run_hint(&cfg, &log), json!(2000 + 60 * 60_000));
        let cfg_off = SyncConfig { enabled: true, interval_minutes: 0 };
        assert_eq!(next_run_hint(&cfg_off, &log), Value::Null, "0 = 关闭");
        assert_eq!(next_run_hint(&cfg, &[]), Value::Null, "从未跑过 auto → null");
    }
}
