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
    GatewayEntry { id: "qwen", name: "Qwen 桌面版", page: "/qoder", base: "/qwen/v1" },
    GatewayEntry { id: "qwen-flash", name: "Qwen3.8-Flash", page: "/qoder", base: "/qwen-flash/v1" },
    GatewayEntry { id: "grok", name: "Grok", page: "/grok", base: "/grok/v1" },
    GatewayEntry { id: "loomy", name: "Loomy", page: "/loomy", base: "/loomy/v1" },
    // 妙手（美团智能体）：反代未通，base 先占位；账号数来自本机登录态发现。
    GatewayEntry { id: "miaoshou", name: "妙手", page: "/miaoshou", base: "—" },
    // ── 2026-09-22 三拆：原先 TRAE 与 WorkBuddy 共用一个「TRAE/WB 模型」页，
    //    现按上游来源拆成相互独立的网关（OpenCode 网关见 /opencode 页，其模块待建）。
    GatewayEntry {
        id: "trae-work-cn",
        name: "TRAE Work CN",
        page: "/trae-work-cn",
        base: "/trae/v1",
    },
    GatewayEntry {
        id: "workbuddy",
        name: "WorkBuddy",
        page: "/workbuddy",
        base: "/workbuddy/v1",
    },
    // OpenCode 网关：本机 OpenCode server 的**免费模型专用**入口（独立密钥）。
    GatewayEntry {
        id: "opencode",
        name: "OpenCode",
        page: "/opencode",
        base: "/opencode/v1",
    },
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
        "qwen" => crate::modules::qwen::models_payload().await,
        "qwen-flash" => crate::modules::qoder::qwen_flash_models_payload(),
        "grok" => crate::modules::grok::models_payload(),
        "glm" => crate::modules::glm_proxy::models_payload(),
        // ── 2026-09-22 三拆新增 ──
        "trae-work-cn" => crate::modules::trae_proxy::models_payload().await,
        // 2026-09-23：WorkBuddy **只暴露内置模型** —— 上游 provider 全量清单（原 591 条 / 14 渠道）
        // 按设计归 OpenCode 网关呈现（见 docs/gateway-split-20260922.md）。总览的 modelCount
        // 必须与 WorkBuddy 页一致，否则侧栏仍显示 591。OpenCode 侧继续用 models_payload()。
        "workbuddy" => crate::modules::workbuddy_proxy::builtin_models_payload(),
        // OpenCode 网关只暴露免费模型，故总览里的 modelCount 即为免费数
        "opencode" => crate::modules::opencode::models_payload()
            .await
            .unwrap_or_else(|_| json!({ "data": [] })),
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
        // qwen-flash 走 Qoder CN 专用池：账号视图只保留 🇨🇳 国内账号（2026-09-18 起与国际池隔离）
        "qwen-flash" => crate::modules::qoder::list_for_display()
            .into_iter()
            .filter(|a| a.get("region").and_then(Value::as_str) == Some("cn"))
            .collect(),
        "qwen" => {
            if crate::modules::qwen::extract_desktop_token().is_some() {
                vec![json!({ "name": "Qwen 桌面版", "status": "ok", "enabled": true })]
            } else {
                Vec::new()
            }
        }
        "grok" => crate::modules::grok::list_for_display(),
        "loomy" => crate::modules::loomy::list_for_display(),
        // 妙手（美团智能体）：账号 = 本机客户端登录态 + 已导入池账号（仅统计数量）。
        "miaoshou" => crate::modules::platform_checkin::discover_miaoshou_accounts()
            .into_iter()
            .map(|a| {
                json!({
                    "name": a.get("nickname").or_else(|| a.get("email")).cloned().unwrap_or(Value::Null),
                    "status": "ok",
                    "enabled": true,
                })
            })
            .collect(),
        // ── 2026-09-22 三拆新增 ──
        // TRAE Work CN：自有账号池（trae_accounts.json），与 GLM / 其它网关互不相干。
        "trae-work-cn" => crate::modules::trae_accounts::list_for_display(),
        // WorkBuddy：无自有账号池 —— 它的"账号"= 借道 OpenCode server 的已鉴权 provider 清单，
        // 而保护/保活对象是该 server 进程（见 workbuddy_proxy 的 watchdog/端口探测）。
        "workbuddy" => Vec::new(),
        _ => Vec::new(),
    }
}

/// 汇总所有网关的总览信息。
pub async fn overview() -> Value {
    let mut out = Vec::new();

    for gw in GATEWAYS {
        out.push(gateway_snapshot(gw.id).await);
    }

    json!({ "gateways": out })
}

/// 取网关的展示名与对外调用基址（OpenAI 兼容端点前缀）。
///
/// 供模型自测等模块复用 —— 避免各处重复维护一份网关路由表而导致漂移。
/// 未知 id 返回 `None`（由调用方给可行动的报错，不 panic）。
pub fn gateway_base(id: &str) -> Option<(&'static str, &'static str)> {
    GATEWAYS
        .iter()
        .find(|g| g.id == id)
        .map(|g| (g.name, g.base))
}

/// 单个网关的快照（总览循环体抽出，供渠道健康度等模块复用）。
///
/// 返回结构与 [`overview()`] 中的单条完全一致（`id/name/page/base/enabled/
/// health/error/accountCount/healthyAccounts/modelCount/capabilities/models`）；
/// 未知 id 返回降级快照（`enabled=false` / `health="error"`），不 panic。
pub async fn gateway_snapshot(id: &str) -> Value {
    let Some(gw) = GATEWAYS.iter().find(|g| g.id == id) else {
        return json!({
            "id": id,
            "name": id,
            "page": "",
            "base": "",
            "enabled": false,
            "health": "error",
            "error": "未知网关",
            "accountCount": 0,
            "healthyAccounts": 0,
            "modelCount": 0,
            "capabilities": [],
            "models": [],
        });
    };

    let accounts = fetch_accounts(gw.id);
    let healthy = accounts.iter().filter(|a| account_healthy(a)).count();

    let models = match fetch_models(gw.id).await {
        Ok(list) => list,
        Err(err) => {
            // 单网关失败不影响整体：健康态记为 error，模型为空
            return json!({
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
            });
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

    // 渠道数（去重 owned_by）：模型目录型网关（WorkBuddy / OpenCode）的模型数会被
    // 「同一模型被多个 provider 提供」放大，单看 modelCount 容易误读，故同时给出渠道数。
    let provider_count = {
        let mut set = std::collections::HashSet::new();
        for model in &models {
            if let Some(owner) = model.get("owned_by").and_then(Value::as_str) {
                if !owner.is_empty() {
                    set.insert(owner);
                }
            }
        }
        set.len()
    };

    json!({
        "id": gw.id,
        "name": gw.name,
        "page": gw.page,
        "base": gw.base,
        "enabled": enabled,
        "health": health,
        "accountCount": accounts.len(),
        "healthyAccounts": healthy,
        "modelCount": simplified.len(),
        "providerCount": provider_count,
        "capabilities": capabilities,
        "models": simplified,
    })
}
