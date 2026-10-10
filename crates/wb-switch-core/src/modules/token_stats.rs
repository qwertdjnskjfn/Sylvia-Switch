//! 本地 WorkBuddy / AutoClaw Token 统计。
//!
//! 这个模块是统计数据的唯一归属：日志只在这里解码、去重和按时间聚合，
//! Tauri 与 HTTP 层只负责转发结果。响应只包含聚合数字和脱敏标识，不返回
//! 消息正文、arguments 或认证信息。

use chrono::{Datelike, Local, Timelike};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::modules::stats::{units, window};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
    read: u64,
    write: u64,
}

#[derive(Clone, Debug, Default)]
struct Totals {
    usage: Usage,
    records: u64,
}

#[derive(Clone, Debug)]
struct SessionTotals {
    key: String,
    title: Option<String>,
    project: String,
    session_id: String,
    totals: Totals,
}

impl Totals {
    fn add(&mut self, usage: Usage) {
        self.usage.input = self.usage.input.saturating_add(usage.input);
        self.usage.output = self.usage.output.saturating_add(usage.output);
        self.usage.read = self.usage.read.saturating_add(usage.read);
        self.usage.write = self.usage.write.saturating_add(usage.write);
        self.records = self.records.saturating_add(1);
    }

    fn value(&self) -> Value {
        // 口径统一到 stats::units：部分平台（如 AutoClaw 某些接口）input 字段仅
        // 记录未缓存输入，此时 read > input。统一函数先规整再相除，保证
        // cacheHitRate 落在 0.0~1.0，且 uncachedInput 与总数统计准确。
        // 注意：总输入为 0 时本模块历史上输出 null（而非 0），此处保留该语义。
        let (input, uncached_input) =
            units::normalize_input_tokens_u64(self.usage.input, self.usage.read);
        let cache_hit_rate = (input > 0)
            .then(|| units::cache_hit_rate_u64(self.usage.input, self.usage.read));
        let total = units::total_tokens_u64(input, self.usage.output, self.usage.write);
        json!({
            "total": total,
            "input": input,
            "output": self.usage.output,
            "cacheRead": self.usage.read,
            "cacheWrite": self.usage.write,
            "uncachedInput": uncached_input,
            "records": self.records,
            "cacheHitRate": cache_hit_rate,
        })
    }
}

/// Read a non-negative integer from a JSON number or string.
fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| value.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64))
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| object.get(*key).and_then(number))
}

fn positive_field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(number)
            .filter(|value| *value > 0)
    })
}

fn cached_input_field(object: &Map<String, Value>) -> u64 {
    // Providers have emitted both flat aliases and OpenAI-compatible nested
    // details. Prefer a positive flat alias so a stale `cache_read...: 0`
    // field cannot hide a populated `prompt_cache_hit_tokens` value.
    positive_field(
        object,
        &[
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "cacheRead",
            "prompt_cache_hit_tokens",
            "cached_tokens",
        ],
    )
    .or_else(|| {
        object
            .get("prompt_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| positive_field(details, &["cached_tokens"]))
    })
    .or_else(|| {
        object
            .get("inputTokensDetails")
            .and_then(Value::as_array)
            .and_then(|details| {
                details.iter().find_map(|detail| {
                    detail
                        .as_object()
                        .and_then(|detail| positive_field(detail, &["cached_tokens"]))
                })
            })
    })
    .unwrap_or(0)
}

const CACHE_WRITE_KEYS: &[&str] = &[
    "cache_write_input_tokens",
    "cacheWriteInputTokens",
    "cacheWrite",
    "cache_creation_input_tokens",
    "prompt_cache_write_tokens",
];

fn usage_fields(object: &Map<String, Value>) -> Usage {
    Usage {
        input: field(
            object,
            &["input", "input_tokens", "inputTokens", "prompt_tokens"],
        )
        .unwrap_or(0),
        output: field(
            object,
            &[
                "output",
                "output_tokens",
                "outputTokens",
                "completion_tokens",
            ],
        )
        .unwrap_or(0),
        read: cached_input_field(object),
        write: positive_field(object, CACHE_WRITE_KEYS).unwrap_or(0),
    }
}

fn usage_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.as_object().filter(|object| {
        // Input is the required anchor for a usage record. It may legitimately
        // be zero (for example a provider reports output-only retries), so do
        // not use `input > 0` as the validity check.
        field(
            object,
            &["input", "input_tokens", "inputTokens", "prompt_tokens"],
        )
        .is_some()
    })
}

/// Decode one record. Usage precedence is message.usage > providerData.usage >
/// top-level usage. Cache-write metadata may only exist on a non-selected
/// usage object or rawUsage, so those objects are consulted without counting
/// their input/output again.
fn usage(value: &Value) -> Option<Usage> {
    let provider = value.get("providerData");
    let candidates = [
        value.get("message").and_then(|message| message.get("usage")),
        provider.and_then(|data| data.get("usage")),
        value.get("usage"),
    ];
    let selected = candidates.iter().copied().find_map(usage_object)?;
    let mut result = usage_fields(selected);

    if result.write == 0 {
        result.write = candidates
            .iter()
            .copied()
            .filter_map(|candidate| candidate.and_then(Value::as_object))
            .chain(
                provider
                    .and_then(|data| data.get("rawUsage"))
                    .and_then(Value::as_object),
            )
            .find_map(|object| positive_field(object, CACHE_WRITE_KEYS))
            .unwrap_or(0);
    }

    // prompt_cache_miss_tokens is deliberately not a write alias: current
    // WorkBuddy logs use it for newly computed (uncached) input,
    // while their explicit cache-write fields may legitimately remain zero.

    Some(result)
}

fn timestamp(value: &Value) -> Option<i64> {
    let parse_raw = |raw: &Value| -> Option<i64> {
        if let Some(n) = raw.as_i64() {
            return Some(n);
        }
        if let Some(n) = raw.as_u64().and_then(|n| i64::try_from(n).ok()) {
            return Some(n);
        }
        if let Some(s) = raw.as_str() {
            let trimmed = s.trim();
            if let Ok(n) = trimmed.parse::<i64>() {
                return Some(n);
            }
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
                return Some(dt.timestamp_millis());
            }
        }
        None
    };

    value
        .get("message")
        .and_then(|m| m.get("timestamp").or_else(|| m.get("ts")))
        .and_then(parse_raw)
        .or_else(|| {
            value
                .get("timestamp")
                .or_else(|| value.get("ts"))
                .and_then(parse_raw)
        })
}

fn date(value: &Value) -> Option<String> {
    // 日归属统一走 stats::window（与其它统计模块同一套本地日定义）。
    window::local_day_key(timestamp(value)?)
}

fn hour(value: &Value) -> Option<String> {
    let timestamp = timestamp(value)?;
    chrono::DateTime::from_timestamp_millis(timestamp).map(|date| {
        let local = date.with_timezone(&Local);
        format!("{}-{}", local.weekday().num_days_from_monday(), local.hour())
    })
}

fn model(value: &Value) -> String {
    value
        .get("providerData")
        .and_then(|data| data.get("model"))
        .or_else(|| value.get("message").and_then(|m| m.get("model")))
        .or_else(|| value.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("未知模型")
        .to_string()
}

fn files(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Subagent logs duplicate parent-session context and are not part
            // of either product's primary usage accounting.
            if path.file_name().and_then(|name| name.to_str()) != Some("subagents") {
                files(&path, output);
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            output.push(path);
        }
    }
}

fn project_name(root: &Path, file: &Path) -> String {
    let name = file
        .strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .filter(|name| !name.is_empty() && !name.ends_with(".jsonl"));
    match name {
        // Product directories commonly encode the complete absolute path.
        // Returning that would leak a user name and parent directories.
        Some(name) if !name.starts_with("Users-") && !name.starts_with("home-") => {
            name.to_string()
        }
        _ => "未知项目".to_string(),
    }
}

fn record_project(value: &Value, fallback: &str) -> String {
    value
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 120)
        .unwrap_or(fallback)
        .to_string()
}

fn non_empty_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn groups(groups: HashMap<String, Totals>) -> Vec<Value> {
    let mut values: Vec<_> = groups
        .into_iter()
        .map(|(key, totals)| {
            let mut value = totals.value();
            value["key"] = json!(key);
            value
        })
        .collect();
    values.sort_by(|left, right| {
        total_value(right).cmp(&total_value(left))
    });
    values
}

fn session_groups(sessions: Vec<SessionTotals>) -> Vec<Value> {
    let mut values: Vec<_> = sessions
        .into_iter()
        .map(|session| {
            let mut value = session.totals.value();
            value["key"] = json!(session.key);
            value["title"] = json!(session.title);
            value["project"] = json!(session.project);
            value["sessionId"] = json!(session.session_id);
            value
        })
        .collect();
    values.sort_by(|left, right| {
        total_value(right).cmp(&total_value(left))
    });
    values
}

fn total_value(value: &Value) -> u64 {
    value
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            value
                .get("input")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(value.get("output").and_then(Value::as_u64).unwrap_or(0))
                .saturating_add(value.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0))
        })
}

#[derive(Default)]
struct SourceCollector {
    total: Totals,
    models: HashMap<String, Totals>,
    projects: HashMap<String, Totals>,
    sessions: Vec<SessionTotals>,
    daily: HashMap<String, Totals>,
    daily_by_model: HashMap<String, HashMap<String, Totals>>,
    hours: HashMap<String, Totals>,
    parse_errors: u64,
    coverage_start_at: Option<i64>,
    coverage_end_at: Option<i64>,
    session_key_counts: HashMap<String, usize>,
}

impl SourceCollector {
    fn note_parse_error(&mut self) {
        self.parse_errors = self.parse_errors.saturating_add(1);
    }

    fn add(&mut self, usage: Usage, value: &Value, project: &str) {
        self.total.add(usage);
        let model_name = model(value);
        self.models
            .entry(model_name.clone())
            .or_default()
            .add(usage);
        self.projects
            .entry(project.to_string())
            .or_default()
            .add(usage);
        if let Some(day) = date(value) {
            self.daily.entry(day.clone()).or_default().add(usage);
            self.daily_by_model
                .entry(model_name)
                .or_default()
                .entry(day)
                .or_default()
                .add(usage);
        }
        if let Some(hour) = hour(value) {
            self.hours.entry(hour).or_default().add(usage);
        }
        if let Some(timestamp) = timestamp(value) {
            self.coverage_start_at = Some(
                self.coverage_start_at
                    .map_or(timestamp, |current| current.min(timestamp)),
            );
            self.coverage_end_at = Some(
                self.coverage_end_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }
    }

    fn push_session(
        &mut self,
        session_id: String,
        title: Option<String>,
        project: String,
        totals: Totals,
    ) {
        if totals.records == 0 {
            return;
        }
        let base_key = format!("{project} · {session_id}");
        let count = self.session_key_counts.entry(base_key.clone()).or_default();
        *count += 1;
        let key = if *count == 1 {
            base_key
        } else {
            format!("{base_key} · {}", *count)
        };
        self.sessions.push(SessionTotals {
            key,
            title,
            project,
            session_id,
            totals,
        });
    }

    fn into_value(self, name: &str, files_scanned: usize) -> Value {
        let daily_by_model = self
            .daily_by_model
            .into_iter()
            .map(|(model, points)| (model, Value::Array(groups(points))))
            .collect::<Map<String, Value>>();
        json!({
            "source": name,
            "summary": self.total.value(),
            "models": groups(self.models),
            "projects": groups(self.projects),
            "sessions": session_groups(self.sessions),
            "daily": groups(self.daily),
            "dailyByModel": daily_by_model,
            "hours": groups(self.hours),
            "filesScanned": files_scanned,
            "parseErrors": self.parse_errors,
            "coverageStartAt": self.coverage_start_at,
            "coverageEndAt": self.coverage_end_at,
        })
    }
}

fn extract_autoclaw_prompt(content: &str) -> Option<String> {
    const START_TAG: &str = "<<<AUTOCLAW_USER_AUTHORED_REQUEST_START>>>";
    const END_TAG: &str = "<<<AUTOCLAW_USER_AUTHORED_REQUEST_END>>>";
    if let Some(start_idx) = content.find(START_TAG) {
        let after_start = &content[start_idx + START_TAG.len()..];
        let prompt_body = if let Some(end_idx) = after_start.find(END_TAG) {
            &after_start[..end_idx]
        } else {
            after_start
        };
        let first_line = prompt_body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?;
        if !first_line.is_empty() {
            let mut title = first_line.to_string();
            if title.chars().count() > 60 {
                title = title.chars().take(60).collect::<String>() + "…";
            }
            return Some(title);
        }
    }
    None
}

fn source_from_roots(roots: Vec<PathBuf>, name: &str, cutoff: Option<i64>) -> Value {
    let mut root_and_paths = Vec::new();
    let mut seen_session_ids = std::collections::HashSet::new();

    for root in &roots {
        if !root.exists() {
            continue;
        }
        let mut paths = Vec::new();
        files(root, &mut paths);
        paths.sort();
        for path in paths {
            let session_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("")
                .to_string();
            if !session_id.is_empty() && !seen_session_ids.insert(session_id.clone()) {
                continue;
            }
            root_and_paths.push((root.clone(), path));
        }
    }

    let mut collector = SourceCollector::default();
    let files_scanned = root_and_paths.len();

    for (root, path) in root_and_paths {
        let session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("未知会话")
            .to_string();
        let fallback_project = project_name(&root, &path);
        let Ok(file) = std::fs::File::open(&path) else {
            collector.note_parse_error();
            continue;
        };

        let mut session_totals = Totals::default();
        let mut session_project: Option<String> = None;
        let mut ai_title: Option<String> = None;
        let mut summary: Option<String> = None;

        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                collector.note_parse_error();
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                collector.note_parse_error();
                continue;
            };

            // 如果文件首行包含 cwd，提取为会话所属项目
            if session_project.is_none() {
                if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                    let clean = Path::new(cwd)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(str::trim)
                        .filter(|n| !n.is_empty() && n.len() <= 120);
                    if let Some(proj) = clean {
                        session_project = Some(proj.to_string());
                    }
                }
            }

            // Title metadata belongs to the whole JSONL session file. Read it
            // before applying the usage cutoff so an older title can still
            // label usage that falls inside the selected range. aiTitle has
            // precedence over summary regardless of event order.
            if let Some(title) = non_empty_text(value.get("aiTitle")) {
                ai_title = Some(title);
            }
            if let Some(value) = non_empty_text(value.get("summary")) {
                summary = Some(value);
            }
            // AutoClaw 用户提问提取
            if ai_title.is_none() && summary.is_none() {
                if let Some(msg) = value.get("message") {
                    if msg.get("role").and_then(Value::as_str) == Some("user") {
                        if let Some(content) = msg.get("content").and_then(Value::as_str) {
                            if let Some(extracted) = extract_autoclaw_prompt(content) {
                                ai_title = Some(extracted);
                            }
                        }
                    }
                }
            }

            // Records with a missing timestamp are excluded from a bounded
            // range rather than guessed from file mtime or browser time.
            if cutoff.is_some_and(|minimum| timestamp(&value).is_none_or(|ts| ts < minimum)) {
                continue;
            }
            let Some(usage) = usage(&value) else {
                continue;
            };
            let project = record_project(
                &value,
                session_project.as_deref().unwrap_or(&fallback_project),
            );
            if session_project.is_none() {
                session_project = Some(project.clone());
            }
            session_totals.add(usage);
            collector.add(usage, &value, &project);
        }

        collector.push_session(
            session_id,
            ai_title.or(summary),
            session_project.unwrap_or(fallback_project),
            session_totals,
        );
    }

    collector.into_value(name, files_scanned)
}

fn source(root: PathBuf, name: &str, cutoff: Option<i64>) -> Value {
    source_from_roots(vec![root], name, cutoff)
}

fn autoclaw_source(home: &Path, cutoff: Option<i64>) -> Value {
    let roots = vec![
        home.join(".openclaw-autoclaw/agents"),
        home.join(".openclaw/agents"),
    ];
    source_from_roots(roots, "autoclaw", cutoff)
}

/// MiniMax 来源（2026-10-10）。
///
/// ⚠️ **与 workbuddy/autoclaw 的取数方式不同**：MiniMax 桌面端**不产出**
/// jsonl 会话日志 —— 实测 `~/.minimax` 下只有 `v2/observability/events/*.jsonl`
/// （遥测事件）与 `AppData/Roaming/MiniMax/observability-outbox.jsonl`，
/// **均不含 token 用量**，故**不能复用 [`source_from_roots`] 扫描器**。
///
/// 改为读取**网关自己的计量记录**（`glm_usage_records.json` 中 `channel="minimax"`
/// 的条目）。该数据由 `minimax::call_metrics` 在每次调用后落库，Token 直接取自
/// 上游 `usage`（含缓存字段），口径比日志解析更准。
///
/// 输出形状与 [`source_from_roots`] **逐字段同构**，前端无需任何改动：
/// `source / coverageStartAt / coverageEndAt / filesScanned / parseErrors /
///  summary / daily / dailyByModel / hours / models / projects / sessions`。
///
/// 字段映射（两套口径的对齐）：
/// | token_stats | model_usage 记录 |
/// | --- | --- |
/// | `input` | `promptTokens`（**含**缓存读取，与 workbuddy 口径一致）|
/// | `output` | `completionTokens` |
/// | `cacheRead` | `cachedTokens` |
/// | `cacheWrite` | `0`（上游该项实测恒为 0；Anthropic 的 `cache_creation_input_tokens` 为 0）|
/// | `uncachedInput` | `uncachedPromptTokens` |
/// | `records` | 记录条数 |
///
/// `projects` 用**账号标签**（MiniMax 无「项目」概念，账号是更有意义的维度）；
/// `sessions` 同样按账号聚合（无会话概念，用 `sessionId` 承载账号 id）。
fn minimax_source(cutoff: Option<i64>) -> Value {
    use crate::modules::model_usage;

    // 只取本渠道、时间戳有效、且在范围内的记录。
    let recs: Vec<Value> = model_usage::load_records()
        .into_iter()
        .filter(|r| model_usage::record_matches_provider(r, Some("minimax")))
        .filter(|r| {
            let ts = r.get("ts").and_then(Value::as_i64).unwrap_or(0);
            ts > 0 && cutoff.is_none_or(|minimum| ts >= minimum)
        })
        .collect();

    let num = |r: &Value, key: &str| -> u64 {
        r.get(key)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v as u64)
            .unwrap_or(0)
    };
    let usage_of = |r: &Value| Usage {
        // input 取 promptTokens（含缓存读取），与 workbuddy 侧「input 含 cacheRead」同口径。
        input: num(r, "promptTokens"),
        output: num(r, "completionTokens"),
        read: num(r, "cachedTokens"),
        // 上游未提供缓存写入（Anthropic 的 cache_creation_input_tokens 实测为 0）。
        write: 0,
    };
    let bucket_key = |r: &Value, kind: &str| -> String {
        let ts = r.get("ts").and_then(Value::as_i64).unwrap_or(0);
        match kind {
            "hour" => crate::modules::stats::window::floor_to_bucket(
                ts,
                3_600_000,
            )
            .to_string(),
            _ => {
                // 本地日 YYYY-MM-DD（与 workbuddy 侧一致：按本机时区切日）。
                let day_start = crate::modules::stats::window::local_day_start_ms(ts);
                chrono::DateTime::from_timestamp_millis(day_start)
                    .map(|d| d.with_timezone(&Local).format("%Y-%m-%d").to_string())
                    .unwrap_or_default()
            }
        }
    };

    // ---- 汇总 ----
    let mut total = Totals::default();
    // ---- 各维度分组：day / hour / model / account ----
    let mut by_day: std::collections::BTreeMap<String, Totals> = Default::default();
    let mut by_hour: std::collections::BTreeMap<String, Totals> = Default::default();
    let mut by_model: std::collections::BTreeMap<String, Totals> = Default::default();
    let mut by_account: std::collections::BTreeMap<String, Totals> = Default::default();
    // dailyByModel: model -> day -> Totals
    let mut by_model_day: std::collections::BTreeMap<String, std::collections::BTreeMap<String, Totals>> =
        Default::default();
    let mut min_ts = i64::MAX;
    let mut max_ts = i64::MIN;

    for r in &recs {
        let u = usage_of(r);
        let ts = r.get("ts").and_then(Value::as_i64).unwrap_or(0);
        min_ts = min_ts.min(ts);
        max_ts = max_ts.max(ts);
        let model = r
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .unwrap_or("未知模型")
            .to_string();
        let account = r
            .get("accountLabel")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
            .unwrap_or("MiniMax")
            .to_string();
        let day = bucket_key(r, "day");
        let hour = bucket_key(r, "hour");

        total.add(u);
        by_day.entry(day.clone()).or_default().add(u);
        by_hour.entry(hour).or_default().add(u);
        by_model.entry(model.clone()).or_default().add(u);
        by_account.entry(account).or_default().add(u);
        by_model_day
            .entry(model)
            .or_default()
            .entry(day)
            .or_default()
            .add(u);
    }

    // 排序：数值降序（与扫描器各维度的排序口径保持一致观感）。
    let mut models: Vec<Value> = by_model
        .into_iter()
        .map(|(k, t)| {
            let mut v = t.value();
            v["key"] = json!(k);
            v
        })
        .collect();
    models.sort_by(|a, b| {
        b.get("total").and_then(Value::as_u64).unwrap_or(0)
            .cmp(&a.get("total").and_then(Value::as_u64).unwrap_or(0))
    });

    let to_list = |m: std::collections::BTreeMap<String, Totals>| -> Vec<Value> {
        m.into_iter()
            .map(|(k, t)| {
                let mut v = t.value();
                v["key"] = json!(k);
                v
            })
            .collect()
    };
    let daily = to_list(by_day);
    let hours = to_list(by_hour);
    let accounts = to_list(by_account);

    let daily_by_model: Map<String, Value> = by_model_day
        .into_iter()
        .map(|(model, days)| {
            let list: Vec<Value> = days
                .into_iter()
                .map(|(day, t)| {
                    let mut v = t.value();
                    v["key"] = json!(day);
                    v
                })
                .collect();
            (model, json!(list))
        })
        .collect();

    // sessions：MiniMax 无「会话」概念，用**账号**承载（sessionId 放账号 id），
    // title 放账号标签 —— 这样前端会话列表仍有意义，而不是空表。
    let sessions: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let key = a.get("key").cloned().unwrap_or(json!("MiniMax"));
            json!({
                "key": key,
                "sessionId": key,
                "title": key,
                "project": "MiniMax",
                "total": a.get("total").cloned().unwrap_or(json!(0)),
                "input": a.get("input").cloned().unwrap_or(json!(0)),
                "output": a.get("output").cloned().unwrap_or(json!(0)),
                "cacheRead": a.get("cacheRead").cloned().unwrap_or(json!(0)),
                "cacheWrite": a.get("cacheWrite").cloned().unwrap_or(json!(0)),
                "uncachedInput": a.get("uncachedInput").cloned().unwrap_or(json!(0)),
                "records": a.get("records").cloned().unwrap_or(json!(0)),
                "cacheHitRate": a.get("cacheHitRate").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    // projects：同样按账号维度（见函数文档）。
    let projects = accounts.clone();

    json!({
        "source": "minimax",
        "coverageStartAt": if min_ts == i64::MAX { Value::Null } else { json!(min_ts) },
        "coverageEndAt": if max_ts == i64::MIN { Value::Null } else { json!(max_ts) },
        // 无日志文件可扫：用记录数代替，保持字段存在（前端用于「已扫描 N 个文件」提示）。
        "filesScanned": recs.len(),
        "parseErrors": 0,
        "summary": total.value(),
        "daily": daily,
        "dailyByModel": Value::Object(daily_by_model),
        "hours": hours,
        "models": models,
        "projects": projects,
        "sessions": sessions,
    })
}

/// Return WorkBuddy and AutoClaw aggregates.
/// `days` is interpreted in Rust using the same millisecond clock for every source.
pub fn get_statistics(days: Option<i64>) -> Value {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let generated_at = crate::modules::config::now_ms();
    let range_days = match days {
        Some(7) => Some(7),
        Some(30) => Some(30),
        Some(90) => Some(90),
        _ => None,
    };
    let cutoff = range_days.map(|value| generated_at - value * 86_400_000);
    json!({
        "generatedAt": generated_at,
        "rangeDays": range_days,
        "sources": [
            source(home.join(".workbuddy/projects"), "workbuddy", cutoff),
            // AutoClaw（OpenClaw 内核）的 agent 会话：
            // 优先扫描 ~/.openclaw-autoclaw/agents，并兼容 ~/.openclaw/agents
            autoclaw_source(&home, cutoff),
            // MiniMax：无本地会话日志，改读网关计量记录（详见 minimax_source 文档）
            minimax_source(cutoff),
        ],
    })
}

/// 测试专用：用给定 usage 走一遍真实的 [`Totals`] 口径并返回其 JSON 输出。
///
/// 供 `stats::units` 的契约测试比对 —— 一旦有人改动本模块或统一函数任一侧的
/// 边界处理，该测试会立即失败，避免两处口径再次漂移。
#[cfg(test)]
pub(crate) fn totals_from_usage_for_test(
    input: u64,
    cached: u64,
    output: u64,
    write: u64,
) -> Value {
    let mut totals = Totals::default();
    totals.add(Usage {
        input,
        output,
        read: cached,
        write,
    });
    totals.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- MiniMax source（2026-10-10）----
    // 说明：`minimax_source` 读的是**真实计量文件**（glm_usage_records.json），
    // 故此处只断言「输出形状与扫描器同构」这类不依赖具体数据的约束；
    // 数值正确性由 E2E 实测覆盖（见 CHANGELOG 2.0.38）。

    #[test]
    fn minimax_source_shape_matches_file_scanner_contract() {
        // 无论有无数据，顶层字段必须与 source_from_roots 一致 ——
        // 少一个字段前端就会渲染异常（前端按同一契约消费）。
        let v = minimax_source(None);
        for key in [
            "source", "coverageStartAt", "coverageEndAt", "filesScanned",
            "parseErrors", "summary", "daily", "dailyByModel", "hours",
            "models", "projects", "sessions",
        ] {
            assert!(v.get(key).is_some(), "缺字段：{key}");
        }
        assert_eq!(v["source"], json!("minimax"), "source 必须是 minimax");
        assert_eq!(v["parseErrors"], json!(0), "本来源无文件解析，恒为 0");
    }

    #[test]
    fn minimax_summary_exposes_the_same_totals_fields() {
        // summary 与扫描器的 Totals::value() 同构：少字段会让前端 NaN。
        let v = minimax_source(None);
        let s = &v["summary"];
        for key in [
            "total", "input", "output", "cacheRead", "cacheWrite",
            "uncachedInput", "records", "cacheHitRate",
        ] {
            assert!(s.get(key).is_some(), "summary 缺字段：{key}");
        }
    }

    #[test]
    fn minimax_groups_carry_key_for_frontend_listing() {
        // models / projects / sessions / daily / hours 里每一项都必须带 `key`，
        // 否则前端列表无法渲染（既有契约）。
        let v = minimax_source(None);
        for group in ["models", "projects", "sessions", "daily", "hours"] {
            if let Some(list) = v[group].as_array() {
                for (i, item) in list.iter().enumerate() {
                    assert!(item.get("key").is_some(), "{group}[{i}] 缺 key");
                    assert!(item.get("total").is_some(), "{group}[{i}] 缺 total");
                }
            }
        }
    }

    #[test]
    fn cutoff_in_the_past_still_returns_a_valid_document() {
        // 时间范围过滤：极早的 cutoff 不应 panic，且形状依旧完整。
        let v = minimax_source(Some(0));
        assert_eq!(v["source"], json!("minimax"));
        assert!(v.get("summary").is_some());
        // 未来 cutoff ⇒ 无记录命中，summary 应为全 0（而不是缺字段）
        let v2 = minimax_source(Some(i64::MAX / 2));
        assert_eq!(v2["summary"]["records"], json!(0));
        assert_eq!(v2["summary"]["input"], json!(0));
    }

    use super::*;
    use std::fs;

    #[test]
    fn usage_priority_aliases_and_raw_cache_write() {
        let value = json!({
            "providerData": {
                "usage": { "inputTokens": 99, "outputTokens": 22 },
                "rawUsage": { "prompt_cache_write_tokens": 2 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(usage(&value), Some(Usage { input: 10, output: 3, read: 4, write: 2 }));
    }

    #[test]
    fn cache_write_uses_explicit_aliases_but_never_cache_miss() {
        let provider_usage_write = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 22,
                    "cache_write_input_tokens": 0,
                    "cache_creation_input_tokens": 7
                },
                "rawUsage": {
                    "prompt_cache_miss_tokens": 91,
                    "prompt_cache_write_tokens": 0
                }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(
            usage(&provider_usage_write),
            Some(Usage {
                input: 10,
                output: 3,
                read: 4,
                write: 7,
            })
        );

        let cache_miss_only = json!({
            "providerData": {
                "rawUsage": { "prompt_cache_miss_tokens": 91 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3
            }}
        });
        assert_eq!(
            usage(&cache_miss_only),
            Some(Usage {
                input: 10,
                output: 3,
                read: 0,
                write: 0,
            })
        );
    }

    #[test]
    fn cache_read_accepts_nested_provider_details() {
        let value = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 3,
                    "inputTokensDetails": [{ "cached_tokens": 7 }]
                }
            }
        });
        assert_eq!(
            usage(&value),
            Some(Usage {
                input: 99,
                output: 3,
                read: 7,
                write: 0,
            })
        );

        let raw = json!({
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 2,
                "cache_read_input_tokens": 0,
                "prompt_cache_hit_tokens": 12
            }
        });
        assert_eq!(
            usage(&raw),
            Some(Usage {
                input: 20,
                output: 2,
                read: 12,
                write: 0,
            })
        );

    }

    #[test]
    fn source_excludes_subagents_and_counts_each_record_once() {
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-{}-{}",
            std::process::id(),
            crate::modules::config::now_ms()
        ));
        let project = root.join("fixture-project");
        let ignored = project.join("subagents");
        fs::create_dir_all(&ignored).expect("create fixture dirs");
        let record = json!({
            "timestamp": crate::modules::config::now_ms(),
            "providerData": {
                "model": "fixture-model",
                "usage": { "inputTokens": 20, "outputTokens": 5 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        fs::write(project.join("session.jsonl"), format!("{}\nnot-json\n", record))
            .expect("write fixture");
        fs::write(ignored.join("agent.jsonl"), format!("{}\n", record)).expect("write ignored fixture");

        let result = source(root.clone(), "fixture", None);
        assert_eq!(result["filesScanned"], 1);
        assert_eq!(result["parseErrors"], 1);
        assert_eq!(result["summary"]["input"], 10);
        assert_eq!(result["summary"]["output"], 3);
        assert_eq!(result["summary"]["cacheRead"], 4);
        assert_eq!(result["summary"]["total"], 13);
        assert_eq!(result["summary"]["records"], 1);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(result["dailyByModel"]["fixture-model"][0]["key"], today);
        assert_eq!(result["projects"][0]["key"], "fixture-project");
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn bounded_source_excludes_records_before_cutoff() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-range-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |timestamp| {
            json!({
                "timestamp": timestamp,
                "cwd": "/fixture/example-project",
                "message": { "usage": { "input_tokens": 10, "output_tokens": 2 } }
            })
        };
        fs::write(
            project.join("session.jsonl"),
            format!("{}\n{}\n", record(now - 10_000), record(now - 100_000)),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", Some(now - 50_000));
        assert_eq!(result["summary"]["records"], 1);
        assert_eq!(result["summary"]["input"], 10);
        assert_eq!(result["projects"][0]["key"], "example-project");
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn session_titles_are_file_scoped_and_independent_from_usage_cutoff() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-titles-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let usage_record = |input| {
            json!({
                "timestamp": now,
                "cwd": "/private/example-project",
                "message": { "usage": { "input_tokens": input, "output_tokens": 2 } }
            })
        };

        fs::write(
            project.join("session-a.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n",
                json!({ "type": "summary", "summary": "摘要不应覆盖 AI 标题" }),
                json!({ "type": "ai-title", "aiTitle": "旧标题" }),
                usage_record(10),
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
            ),
        )
        .expect("write ai title fixture");
        fs::write(
            project.join("session-b.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "type": "ai-title",
                    "timestamp": now - 100_000,
                    "aiTitle": "范围外保留标题"
                }),
                usage_record(20),
            ),
        )
        .expect("write cutoff title fixture");
        fs::write(
            project.join("session-c.jsonl"),
            format!(
                "{}\n{}\n",
                usage_record(30),
                json!({ "type": "summary", "summary": "摘要回退标题" }),
            ),
        )
        .expect("write summary fixture");
        fs::write(
            project.join("session-d.jsonl"),
            format!("{}\n", usage_record(40)),
        )
        .expect("write untitled fixture");
        fs::write(
            project.join("session-e.jsonl"),
            format!(
                "{}\n{}\n",
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
                usage_record(50),
            ),
        )
        .expect("write duplicate title fixture");

        let result = source(root.clone(), "fixture", Some(now - 50_000));
        let sessions = result["sessions"].as_array().expect("session groups");
        let by_id = |session_id: &str| {
            sessions
                .iter()
                .find(|session| session["sessionId"] == session_id)
                .expect("session group by id")
        };

        assert_eq!(result["summary"]["input"], 150);
        assert_eq!(result["summary"]["records"], 5);
        assert_eq!(sessions.len(), 5);
        assert_eq!(by_id("session-a")["title"], "最新 AI 标题");
        assert_eq!(by_id("session-b")["title"], "范围外保留标题");
        assert_eq!(by_id("session-c")["title"], "摘要回退标题");
        assert!(by_id("session-d")["title"].is_null());
        assert_eq!(by_id("session-a")["project"], "example-project");
        assert_ne!(by_id("session-a")["key"], by_id("session-e")["key"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn get_statistics_returns_workbuddy_autoclaw_and_minimax_sources() {
        let value = get_statistics(None);
        let sources = value["sources"].as_array().expect("sources");
        let names: Vec<_> = sources
            .iter()
            .map(|source| source["source"].as_str().unwrap_or_default())
            .collect();
        // 2026-10-10：新增 `minimax` 来源（本地无 jsonl 会话日志，
        // 改读网关计量记录，详见 `minimax_source` 文档）。
        // 顺序固定 —— 前端 tab 与默认选中依赖它稳定。
        assert_eq!(
            names,
            ["workbuddy", "autoclaw", "minimax"]
        );
    }

    #[test]
    fn autoclaw_session_parsing_and_prompt_extraction() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-autoclaw-{}-{now}",
            std::process::id()
        ));
        let session_dir = root.join("agents").join("main").join("sessions");
        fs::create_dir_all(&session_dir).expect("create fixture dirs");

        let session_header = json!({
            "type": "session",
            "version": 3,
            "id": "e3d875e9-b33d-4b47-a36c-cb69417f0863",
            "timestamp": "2026-08-30T14:38:43.406Z",
            "cwd": "C:\\Users\\test\\test-workspace"
        });

        let user_message = json!({
            "type": "message",
            "id": "user-msg-1",
            "timestamp": "2026-08-30T14:38:43.938Z",
            "message": {
                "role": "user",
                "content": "<system-reminder>\nsome rules\n</system-reminder>\n\n<<<AUTOCLAW_USER_AUTHORED_REQUEST_START>>>\n这是测试项目分析提问\n<<<AUTOCLAW_USER_AUTHORED_REQUEST_END>>>\n"
            }
        });

        let assistant_message = json!({
            "type": "message",
            "id": "asst-msg-1",
            "timestamp": "2026-08-30T14:39:22.520Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "回复"}],
                "model": "zai_glm-5.3-flash",
                "usage": {
                    "input": 27791,
                    "output": 522,
                    "cacheRead": 6976,
                    "cacheWrite": 0,
                    "reasoningTokens": 412,
                    "totalTokens": 35289
                },
                "timestamp": now
            }
        });

        let session_file = session_dir.join("e3d875e9-b33d-4b47-a36c-cb69417f0863.jsonl");
        fs::write(
            &session_file,
            format!("{}\n{}\n{}\n", session_header, user_message, assistant_message),
        )
        .expect("write fixture");

        let result = source_from_roots(vec![root.join("agents")], "autoclaw", None);
        assert_eq!(result["source"], "autoclaw");
        assert_eq!(result["filesScanned"], 1);
        assert_eq!(result["summary"]["records"], 1);
        assert_eq!(result["summary"]["input"], 27791);
        assert_eq!(result["summary"]["output"], 522);
        assert_eq!(result["summary"]["cacheRead"], 6976);
        assert_eq!(result["summary"]["total"], 28313);
        assert_eq!(result["models"][0]["key"], "zai_glm-5.3-flash");
        assert_eq!(result["projects"][0]["key"], "test-workspace");
        assert_eq!(result["sessions"][0]["sessionId"], "e3d875e9-b33d-4b47-a36c-cb69417f0863");
        assert_eq!(result["sessions"][0]["title"], "这是测试项目分析提问");

        fs::remove_dir_all(root).expect("remove fixture");
    }
}
