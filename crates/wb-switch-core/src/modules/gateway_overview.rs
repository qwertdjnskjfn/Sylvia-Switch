//! 统一网关总览：把所有模型网关（原生 Rust 版）聚合成一份清单，供前端下拉菜单浏览与切换。
//!
//! 设计要点：
//! - 只做**聚合**，不改动任何网关模块的既有逻辑；
//! - 每个网关独立取数，单个网关失败不影响其他网关（降级为空模型 + error 健康态）；
//! - 返回的字段结构统一，前端无需关心各网关差异。

use serde_json::{json, Value};

/// 单个网关在总览里的展示信息。
struct GatewayEntry {
    id: &'static str,
    name: &'static str,
    /// 前端页面路由（用于「前往管理」跳转）
    page: &'static str,
    /// 对外调用基址（OpenAI 兼容端点前缀）
    base: &'static str,
}

const GATEWAYS: &[GatewayEntry] = &[
    GatewayEntry { id: "glm", name: "GLM 代理", page: "/glm-proxy", base: "/v1" },
    GatewayEntry { id: "kimi", name: "Kimi", page: "/kimi", base: "/kimi/v1" },
    GatewayEntry { id: "qoder", name: "Qoder", page: "/qoder", base: "/qoder/v1" },
    GatewayEntry { id: "grok", name: "Grok", page: "/grok", base: "/grok/v1" },
    GatewayEntry { id: "loomy", name: "Loomy", page: "/loomy", base: "/loomy/v1" },
];

/// 账号是否处于健康可用状态（用于统计）。
fn account_healthy(account: &Value) -> bool {
    if account.get("enabled").and_then(Value::as_bool) == Some(false) {
        return false;
    }
    match account.get("status").and_then(Value::as_str) {
        None => true,
        Some("ok") | Some("active") | Some("healthy") => true,
        Some(_) => false,
    }
}

/// 从模型条目推断能力标签（各网关字段名不同，统一归一）。
fn model_capabilities(model: &Value) -> Vec<&'static str> {
    let mut caps: Vec<&'static str> = Vec::new();
    let id = model
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    // Loomy 等网关把能力放在嵌套的 capabilities 对象里（含 input_modalities）
    let caps_obj = model.get("capabilities");
    let read_bool = |key: &str| caps_obj.and_then(|c| c.get(key)).and_then(Value::as_bool).unwrap_or(false);
    let has_input = |kind: &str| caps_obj
        .and_then(|c| c.get("input_modalities"))
        .and_then(Value::as_array)
        .is_some_and(|list| list.iter().any(|v| v.as_str() == Some(kind)));

    // 明确的能力字段优先（顶层与嵌套对象都读）
    let flag = |key: &str| {
        model.get(key).and_then(Value::as_bool).unwrap_or(false) || read_bool(key)
    };
    if flag("thinking") || flag("reasoning") {
        caps.push("reasoning");
    }
    if flag("web_search") || flag("search") {
        caps.push("search");
    }
    if flag("vision") || flag("image") || has_input("image") {
        caps.push("image");
    }
    if flag("video") || has_input("video") {
        caps.push("video");
    }
    if flag("voice") || flag("audio") {
        caps.push("voice");
    }
    if flag("tools") || flag("function_calling") {
        caps.push("tools");
    }

    // 退化到模型名关键字
    if !caps.iter().any(|c| *c == "image") && (id.contains("image") || id.contains("imagine")) {
        caps.push("image");
    }
    if !caps.iter().any(|c| *c == "video") && id.contains("video") {
        caps.push("video");
    }
    if !caps.iter().any(|c| *c == "voice")
        && (id.contains("voice") || id.contains("tts") || id.contains("stt"))
    {
        caps.push("voice");
    }
    if !caps.iter().any(|c| *c == "search") && id.contains("search") {
        caps.push("search");
    }
    if !caps.iter().any(|c| *c == "reasoning") && (id.contains("thinking") || id.contains("reasoning")) {
        caps.push("reasoning");
    }

    if caps.is_empty() {
        caps.push("text");
    } else if !caps.iter().any(|c| *c == "text") {
        // 多模态模型同时也具备文本能力
        caps.insert(0, "text");
    }
    caps
}

/// 取某网关的模型列表；失败时返回 Err，由调用方降级。
///
/// 注意：各模块的 `models_payload` 签名并不统一（kimi / loomy 为异步 Result，
/// qoder / grok / glm 为同步 Value），这里逐一适配，不改动它们的实现。
async fn fetch_models(gateway: &str) -> Result<Vec<Value>, String> {
    let payload: Value = match gateway {
        "kimi" => crate::modules::kimi::models_payload().await?,
        "loomy" => crate::modules::loomy_proxy::models_payload().await?,
        "qoder" => crate::modules::qoder::models_payload(),
        "grok" => crate::modules::grok::models_payload(),
        "glm" => crate::modules::glm_proxy::models_payload(),
        _ => return Ok(Vec::new()),
    };
    let list = payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(list)
}

/// 取某网关的账号列表（统一走 `list_for_display`，各模块均已返回 `Vec<Value>`）。
fn fetch_accounts(gateway: &str) -> Vec<Value> {
    match gateway {
        "kimi" => crate::modules::kimi::list_for_display(),
        "qoder" => crate::modules::qoder::list_for_display(),
        "grok" => crate::modules::grok::list_for_display(),
        "loomy" => crate::modules::loomy::list_for_display(),
        _ => Vec::new(),
    }
}

/// 汇总所有网关的总览信息。
pub async fn overview() -> Value {
    let mut out = Vec::new();

    for gw in GATEWAYS {
        let accounts = fetch_accounts(gw.id);
        let healthy = accounts.iter().filter(|a| account_healthy(a)).count();

        let models = match fetch_models(gw.id).await {
            Ok(list) => list,
            Err(err) => {
                // 单网关失败不影响整体：健康态记为 error，模型为空
                out.push(json!({
                    "id": gw.id,
                    "name": gw.name,
                    "page": gw.page,
                    "base": gw.base,
                    "enabled": !accounts.is_empty() || gw.id == "glm",
                    "health": "error",
                    "error": err,
                    "accountCount": accounts.len(),
                    "healthyAccounts": healthy,
                    "modelCount": 0,
                    "capabilities": [],
                    "models": [],
                }));
                continue;
            }
        };

        let mut capabilities: Vec<&'static str> = Vec::new();
        let mut simplified: Vec<Value> = Vec::new();
        for model in &models {
            let caps = model_capabilities(model);
            for cap in &caps {
                if !capabilities.contains(cap) {
                    capabilities.push(cap);
                }
            }
            simplified.push(json!({
                "id": model.get("id").cloned().unwrap_or(Value::Null),
                "name": model
                    .get("display_name")
                    .or_else(|| model.get("name"))
                    .cloned()
                    .unwrap_or_else(|| model.get("id").cloned().unwrap_or(Value::Null)),
                "capabilities": caps,
            }));
        }

        // 有模型目录的网关（无「账号」概念）也视为可用：只要有模型就算启用
        let enabled = !accounts.is_empty() || !models.is_empty();
        let health = if !enabled {
            "disabled"
        } else if healthy == 0 && !accounts.is_empty() {
            "cooling"
        } else {
            "ok"
        };

        out.push(json!({
            "id": gw.id,
            "name": gw.name,
            "page": gw.page,
            "base": gw.base,
            "enabled": enabled,
            "health": health,
            "accountCount": accounts.len(),
            "healthyAccounts": healthy,
            "modelCount": simplified.len(),
            "capabilities": capabilities,
            "models": simplified,
        }));
    }

    json!({ "gateways": out })
}
