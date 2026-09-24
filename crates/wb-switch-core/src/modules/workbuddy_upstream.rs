//! WorkBuddy cloud 上游 HTTP 客户端与出站预处理（网关重构 P1）。
//!
//! 接口按契约 §13 钉定（签名一字不改）：[`chat_base`] / [`prepare_body`] /
//! [`chat_stream`] / [`refresh_token`] / [`fetch_models`] / [`user_resource`]，
//! 账号类型引用 `workbuddy_gateway::PoolAccount`。
//!
//! 行为照抄参考实现 `_workbuddy-gateway-ref/momo0410-switch-gateway/crates/wb-switch-gateway/src/upstream/`：
//!
//! | 本文件 section | 参考文件 | 内容 |
//! |----------------|----------|------|
//! | 出站头族 | `headers.rs`（Go `headers.go`） | common/chat/billing/refresh 四类头 + models 专属 UA |
//! | [`prepare_body`] | `payload.rs`（Go `payload.go`） | 强制 stream、tool_choice/roles 归一、INTL 首条补 system |
//! | 脱敏 | `sanitize.rs`（Go `sanitize.go`） | 出站指纹清洗（开关可关） |
//! | [`chat_stream`] | `client.rs` chat 部分 | SSE 首字节超时 + 错误体读限 + classify |
//! | [`refresh_token`] | `client.rs` refresh 部分 | token 刷新（拿到即用） |
//! | [`fetch_models`] | `client.rs` models 部分 | `/v3/config` → OpenAI models 列表 |
//! | [`user_resource`] | `client.rs` billing 部分 | 积分余额与到期日 |
//!
//! 双区域（契约 §10）：`chat_base` cn→copilot.tencent.com / intl→www.workbuddy.ai；
//! billing 国服与 chat 分域（codebuddy.cn 系）、国际版同域；refresh 端点按参考口径
//! 走 chat_base（Go `client.go:673` / Rust `client.rs:186`）。
//!
//! 🔴 安全红线（契约 §10）：`X-Refresh-Token` 只允许出现在 refresh 端点
//! （[`apply_refresh_headers`]），绝不上 chat 请求——由测试
//! `chat_headers_never_carry_refresh_token` 固化。
//!
//! # 错误字符串约定（账号池/接线层据此分流；契约 §13 之外的内部约定）
//!
//! - 传输层失败（连接/发送/读流/超时，含 chat 首字节与流中空闲超时）：`transport: ...`
//!   —— 只换路不标失效（契约 §4：5xx/传输层指数退避重试 + 换号，不标 error）。
//! - 上游 HTTP >=400：`upstream <kind> (http <status>): <body 截断 200>`，
//!   `<kind>` = `workbuddy_errors::ErrKind::as_str()`（判定顺序见契约 §2-1，不得调换）。
//! - 业务 code != 0：`upstream <kind> (http <status>): code=<code> msg=<msg 截断 160>`；
//!   SessionDead（12153）据此标需重登，网络失败（无 `code=` 段）不标（契约 §4/P1）。
//! - 解析失败：`parse: ...`；刷新缺 token：`refresh_failed: ... — re-login required`。
//!
//! # 与参考实现的已知差异
//!
//! - reasoning_effort 按模型 supportedEfforts 降级：需要跨请求的 efforts 缓存，
//!   契约 §13 的 [`prepare_body`]/[`chat_stream`] 签名无该入口，故未实现；
//!   [`fetch_models`] 输出的 `efforts` 元数据可供后续接入。
//! - 脱敏的两个正则剥离层（billing 头 / cc_ 裸键值）：wb-switch-core 未引入 regex
//!   依赖，以等价手写扫描实现（语义逐条对齐，见 [`strip_billing_header`]/[`strip_cc_kv`]）。

use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::modules::config::now_ms;
use crate::modules::workbuddy_errors::{classify, ErrKind};
use crate::modules::workbuddy_gateway::PoolAccount;

// ---------------------------------------------------------------------------
// 常量（端点 / UA / 超时）
// ---------------------------------------------------------------------------

/// chat 基址·国服（契约 §10）。
const CHAT_BASE_CN: &str = "https://copilot.tencent.com";
/// chat 基址·国际版（契约 §10）。
const CHAT_BASE_INTL: &str = "https://www.workbuddy.ai";
/// billing 基址·国服：与 chat 分域，走 codebuddy.cn 系（契约 §10 / 参考 client.rs:20）。
const BILLING_BASE_CN: &str = "https://www.codebuddy.cn";
/// billing 基址·国际版：与 chat 同域（契约 §10）。
const BILLING_BASE_INTL: &str = "https://www.workbuddy.ai";

/// `X-Domain` 取值·国服。PoolAccount 不带 domain 字段（契约 §13），按区域推导；
/// 与账号库实测 domain 值一致（契约 §11：`www.workbuddy.cn/.ai`）。
const DOMAIN_CN: &str = "www.workbuddy.cn";
/// `X-Domain` 取值·国际版。
const DOMAIN_INTL: &str = "www.workbuddy.ai";

/// Origin/Referer·国服（参考 headers.rs:11）。
const ORIGIN_REFERER_CN: &str = "https://www.codebuddy.cn";
/// Origin/Referer·国际版（参考 headers.rs:12）。
const ORIGIN_REFERER_INTL: &str = "https://www.workbuddy.ai";

/// 官方 CLI UA 三段式（契约 §10 / 参考 headers.rs:10）。
const CLIENT_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
/// 拉模型配置用的 UA：**必须** WorkBuddy 前缀——实测 CLI 前缀会拿到另一套产品
/// （CodeBuddy）清单，其他 UA 直接 400（参考 client.rs:531-544）。
const MODELS_CONFIG_UA: &str = "WorkBuddy/5.5.2 WorkBuddy/5.5.2 CLI/2.137.1";
/// 模型配置接口路径（两区域均可用；参考 client.rs:546-552）。
const MODELS_CONFIG_PATH: &str = "/v3/config";
/// billing 积分查询路径（参考 client.rs:373）。
const USER_RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";
/// token 刷新路径（参考 client.rs:186 / Go client.go:673）。
const REFRESH_TOKEN_PATH: &str = "/v2/plugin/auth/token/refresh";

/// 短 RPC（refresh/models/user_resource）总时长上限（参考 client.rs:24 HTTP_TOTAL_TIMEOUT）。
const RPC_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
/// chat SSE 首字节（响应头）超时（对应 Go Transport.ResponseHeaderTimeout）。
pub const CHAT_HEADER_TIMEOUT: Duration = Duration::from_secs(15);
/// chat SSE 流中空闲超时：超过即断流返回 Err，调用方据此释放账号租约
/// （参考 sse.rs/idle.go 的「读块套 timeout」等价实现）。
pub const CHAT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// >=400 错误体读取上限 1MB（对应 Go io.LimitReader；参考 client.rs:240）。
const ERROR_BODY_LIMIT: usize = 1 << 20;

// ---------------------------------------------------------------------------
// 区域判定与端点
// ---------------------------------------------------------------------------

/// 区域判定：`region == "intl"`（大小写/首尾空白不敏感）为国际版，
/// 其余（含 "cn"/空/未知值）按国服。与参考 `Auth::is_intl`（domain `.ai` 后缀）
/// 同向——账号库 region 字段即由 domain 后缀派生（契约 §11）。
fn is_intl(region: &str) -> bool {
    region.trim().eq_ignore_ascii_case("intl")
}

/// 该账号所属区域的 chat 基址（契约 §13 钉定签名）。
pub fn chat_base(region: &str) -> &'static str {
    if is_intl(region) {
        CHAT_BASE_INTL
    } else {
        CHAT_BASE_CN
    }
}

/// 该账号所属区域的 billing 基址（按区域路由；国服与 chat 分域，国际版同域）。
fn billing_base(region: &str) -> &'static str {
    if is_intl(region) {
        BILLING_BASE_INTL
    } else {
        BILLING_BASE_CN
    }
}

/// `X-Domain` 取值（按区域推导，见 [`DOMAIN_CN`]/[`DOMAIN_INTL`]）。
fn domain_for(region: &str) -> &'static str {
    if is_intl(region) {
        DOMAIN_INTL
    } else {
        DOMAIN_CN
    }
}

/// Origin/Referer 基址（按区域路由；上游按 Origin 判定来源区域，跨区域发送会被拒）。
fn origin_referer_for(region: &str) -> &'static str {
    if is_intl(region) {
        ORIGIN_REFERER_INTL
    } else {
        ORIGIN_REFERER_CN
    }
}

/// chat 端点（契约 §10：`POST {base}/v2/chat/completions`）。
fn chat_url(region: &str) -> String {
    format!("{}/v2/chat/completions", chat_base(region))
}

/// token 刷新端点（参考口径走 chat_base，见模块文档）。
fn refresh_url(region: &str) -> String {
    format!("{}{}", chat_base(region), REFRESH_TOKEN_PATH)
}

/// 模型配置端点。
fn models_url(region: &str) -> String {
    format!("{}{}", chat_base(region), MODELS_CONFIG_PATH)
}

/// billing 积分查询端点。
fn user_resource_url(region: &str) -> String {
    format!("{}{}", billing_base(region), USER_RESOURCE_PATH)
}

// ---------------------------------------------------------------------------
// 出站头族（参考 headers.rs / Go headers.go）
// ---------------------------------------------------------------------------

/// 所有 API 共享的请求头。
fn apply_common(req: reqwest::RequestBuilder, region: &str) -> reqwest::RequestBuilder {
    let origin = origin_referer_for(region);
    req.header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Origin", origin)
        .header("Referer", format!("{origin}/"))
        .header("User-Agent", CLIENT_UA)
}

/// chat 专属账号头（common 之上）。缺省字段用 X-No-* 约定（与 CodeBuddy 官方 CLI 一致）。
///
/// 🔴 安全红线：绝不在此处携带 X-Refresh-Token——它只允许出现在 refresh 端点
/// （[`apply_refresh_headers`]）。上 chat 请求会成倍放大 refresh token 的暴露面。
fn apply_chat(req: reqwest::RequestBuilder, acc: &PoolAccount) -> reqwest::RequestBuilder {
    let req = apply_common(req, &acc.region);
    let req = if acc.access_token.is_empty() {
        req.header("X-No-Authorization", "1")
    } else {
        req.header("Authorization", format!("Bearer {}", acc.access_token))
    };
    let req = if acc.uid.is_empty() {
        req.header("X-No-User-Id", "1")
    } else {
        req.header("X-User-Id", &acc.uid)
    };
    // PoolAccount（契约 §13）不带 enterprise_id：恒以 X-No-* 占位。
    let req = req.header("X-No-Enterprise-Id", "1");
    // domain 由区域推导（账号库实测值与之一致），恒非空，不走 X-No-Department-Info。
    let req = req.header("X-Domain", domain_for(&acc.region));
    req.header("X-Product", "SaaS")
}

/// billing 接口请求头。
fn apply_billing(req: reqwest::RequestBuilder, acc: &PoolAccount) -> reqwest::RequestBuilder {
    let mut req = req
        .header("Authorization", format!("Bearer {}", acc.access_token))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    if !acc.uid.is_empty() {
        req = req.header("X-User-Id", &acc.uid);
    }
    // PoolAccount 无 enterprise_id：X-Enterprise-Id / X-Tenant-Id 恒缺（参考同结构）。
    req = req.header("X-Domain", domain_for(&acc.region));
    req
}

/// refresh 端点专属头——**X-Refresh-Token 只允许出现在这里**（契约 §10 安全红线）。
fn apply_refresh_headers(req: reqwest::RequestBuilder, acc: &PoolAccount) -> reqwest::RequestBuilder {
    let req = apply_common(req, &acc.region).header("X-Refresh-Token", &acc.refresh_token);
    // PoolAccount 无 enterprise_id：X-Enterprise-Id 分支恒不触发（参考同结构）。
    req.header("X-Auth-Refresh-Source", "workbuddy")
}

/// `/v3/config` 专属头：UA 必须 WorkBuddy 前缀（见 [`MODELS_CONFIG_UA`]）。
fn apply_models(req: reqwest::RequestBuilder, acc: &PoolAccount) -> reqwest::RequestBuilder {
    let origin = origin_referer_for(&acc.region);
    req.header("Authorization", format!("Bearer {}", acc.access_token))
        .header("Accept", "application/json")
        .header("Origin", origin)
        .header("Referer", format!("{origin}/"))
        .header("User-Agent", MODELS_CONFIG_UA)
}

// ---------------------------------------------------------------------------
// ChatOutcome 与 chat_stream（契约 §13）
// ---------------------------------------------------------------------------

/// chat 请求结果（契约 §13 钉定，签名一字不改）。
pub enum ChatOutcome {
    /// 2xx：SSE 响应流。流中空闲监控由调用方经 [`read_stream_chunk`] 施加
    /// （超过 [`CHAT_IDLE_TIMEOUT`] 未收到数据即断流 Err，调用方释放租约）。
    Stream(reqwest::Response),
    /// >=400：已读回响应体（上限 1MB）并已过 `workbuddy_errors::classify`，
    /// 供调用方按 §2-1 判定顺序分级换号。
    Error { status: u16, body: String },
}

/// 发 chat 请求并返回 SSE 响应流（契约 §13 钉定签名）。
///
/// - 出站体经 [`prepare_body`] 预处理（脱敏开关取网关配置 `sanitize_enabled`，
///   契约 §5：默认开、可关）；
/// - 首字节（响应头）超时 [`CHAT_HEADER_TIMEOUT`]（15s）；chat client 不设总超时
///   （流式响应可能很长），流中空闲由 [`read_stream_chunk`] 约束；
/// - >=400：读体（上限 1MB）并走 `workbuddy_errors::classify`，返回
///   [`ChatOutcome::Error`]；只有传输层失败才返回 Err（`transport: ...`）。
pub async fn chat_stream(acc: &PoolAccount, body: &Value) -> Result<ChatOutcome, String> {
    let url = chat_url(&acc.region);
    let prepared = prepare_body(body, &acc.region, gateway_sanitize_enabled());
    let payload = serde_json::to_vec(&prepared).map_err(|e| format!("parse: 序列化请求体失败: {e}"))?;
    let req = apply_chat(chat_http_client().post(&url).body(payload), acc);
    // 首字节阶段由 header 超时管（对应 Go Transport.ResponseHeaderTimeout）。
    let send = req.send();
    let resp = match tokio::time::timeout(CHAT_HEADER_TIMEOUT, send).await {
        Ok(r) => r.map_err(|e| format!("transport: {e}"))?,
        Err(_) => {
            return Err(format!(
                "transport: response header timeout after {:?}",
                CHAT_HEADER_TIMEOUT
            ))
        }
    };
    let status = resp.status().as_u16();
    if status >= 400 {
        let raw = read_body_limited(resp, ERROR_BODY_LIMIT).await?;
        // 走 workbuddy_errors::classify（契约 §2-1 判定顺序）出一行结构化日志；
        // 换号/冷却决策由调用方对同一 body 复判获得（kind 名与 ErrKind::as_str 一致）。
        let kind = classify(status, &raw);
        eprintln!(
            "[workbuddy-upstream] chat_stream uid={} status={} kind={} body={}",
            acc.uid,
            status,
            kind.as_str(),
            truncate(&raw, 200)
        );
        return Ok(ChatOutcome::Error { status, body: raw });
    }
    Ok(ChatOutcome::Stream(resp))
}

/// 读一块 SSE 数据（流中空闲监控由本函数承担）。
///
/// 对应参考 `Client::read_chunk` + `idle.go`：每次读块套 `idle` 超时，空闲即断流
/// 返回 Err——调用方（转发循环）据此**释放该账号租约**，避免把号卡死在一个永不
/// 产出的流上。`idle` 传 [`CHAT_IDLE_TIMEOUT`]（默认 60s）。
pub async fn read_stream_chunk(
    resp: &mut reqwest::Response,
    idle: Duration,
) -> Result<Option<Bytes>, String> {
    match tokio::time::timeout(idle, resp.chunk()).await {
        Ok(r) => r.map_err(|e| format!("transport: {e}")),
        Err(_) => Err(format!(
            "transport: stream idle timeout after {:?}",
            idle
        )),
    }
}

// ---------------------------------------------------------------------------
// 出站体预处理 prepare_body（契约 §13 / 参考 payload.rs）
// ---------------------------------------------------------------------------

/// 出站体预处理（契约 §13 钉定签名）：
/// 1. 强制 `stream:true`（上游拒绝非流式）；
/// 2. `tool_choice` 归一化（上游该字段是 string，对象形式会 400 code=11101）；
/// 3. roles 归一化（`developer` → `system`，上游 role 白名单不含 developer）；
/// 4. INTL 保证 messages 首条是 system（否则 400 code=11128）；
/// 5. `sanitize` 开时做基础指纹脱敏（身份句/反馈句/billing 头）。
///
/// 与参考 `prepare_body_for_region` 的差异：无 efforts 缓存入口，不做
/// reasoning_effort 降级（见模块文档「已知差异」）。
pub fn prepare_body(body: &Value, region: &str, sanitize: bool) -> Value {
    // 非对象（数组/标量）无字段可改写，原样返回（与参考一致）。
    let Some(obj) = body.as_object() else {
        return body.clone();
    };
    let mut obj = obj.clone();
    obj.insert("stream".to_string(), Value::Bool(true));
    normalize_tool_choice(&mut obj);
    normalize_roles(&mut obj);
    // 国际版适配放在 roles 归一化之后：developer 已被改写为 system，此时首条若
    // 已是 system 就不必补（否则会给 Codex 之类客户端多插一条）。
    if is_intl(region) {
        ensure_system_first(&mut obj);
    }
    if sanitize {
        if let Some(msgs) = obj.get_mut("messages").and_then(Value::as_array_mut) {
            sanitize_messages(msgs);
        }
    }
    Value::Object(obj)
}

/// 保证 messages[0] 是 system（国际版协议要求）。
///
/// 仅在首条不是 system 时补一条**最小**的 system，不改动任何既有消息，也不合并——
/// 合并会改变模型看到的对话结构，风险大于收益。空 messages（或缺失）时不动：
/// 那种请求本就缺少上下文，交给上游报错更诚实。补的内容刻意保持中性，它只为满足
/// 协议前提，不承载业务语义。
fn ensure_system_first(obj: &mut Map<String, Value>) {
    let Some(Value::Array(msgs)) = obj.get_mut("messages") else {
        return;
    };
    if msgs.is_empty() {
        return;
    }
    let Some(first) = msgs[0].as_object() else {
        return;
    };
    if first
        .get("role")
        .and_then(Value::as_str)
        .map(|r| r.trim().eq_ignore_ascii_case("system"))
        .unwrap_or(false)
    {
        return;
    }
    let original_role = msgs[0].get("role").cloned().unwrap_or(Value::Null);
    msgs.insert(0, json!({"role": "system", "content": "You are a helpful assistant."}));
    eprintln!(
        "[workbuddy-upstream] intl payload: messages 首条非 system（role={original_role}），已补一条 system"
    );
}

/// 把 messages 里的 developer 角色归一为 system。
///
/// 上游对 role 做白名单校验，developer 不在白名单内，命中即 400 code=11128。
/// developer 是 OpenAI 新规范里 system 的别名（Codex / Cursor 等新客户端用它承载
/// system 级指令），改写不丢语义。此归一化是「协议兼容」而非「内容脱敏」，因此与
/// sanitize 开关解耦：即使 sanitize=false 也照常归一。只认 developer 一个值，
/// 其余 role 一律原样保留，不合并、不重排、不删除任何消息。
fn normalize_roles(obj: &mut Map<String, Value>) {
    let Some(Value::Array(msgs)) = obj.get_mut("messages") else {
        return;
    };
    for (i, m) in msgs.iter_mut().enumerate() {
        let Some(msg) = m.as_object_mut() else { continue };
        let Some(role) = msg.get("role").and_then(Value::as_str) else {
            continue;
        };
        if role.trim().eq_ignore_ascii_case("developer") {
            msg.insert("role".to_string(), Value::String("system".to_string()));
            eprintln!("[workbuddy-upstream] role normalized developer->system (idx={i})");
        }
    }
}

/// 按上游 Go struct（string 类型）改写 OpenAI tool_choice（参考 payload.rs:216-275）：
///   - "none" / {"type":"none"} → 删 tool_choice + 删 tools/functions
///   - {"type":"auto"/"required"} → 字符串 "auto"/"required"
///   - {"type":"function","function":{"name":"x"}} → 字符串 "x"（缺 name 回退 "auto"）
///   - 其他对象/非标量 → 删 tool_choice
fn normalize_tool_choice(obj: &mut Map<String, Value>) {
    fn suppress(obj: &mut Map<String, Value>) {
        obj.remove("tools");
        obj.remove("functions");
    }
    let Some(tc) = obj.get("tool_choice").cloned() else {
        return;
    };
    match tc {
        Value::String(v) => {
            if v.trim().eq_ignore_ascii_case("none") {
                obj.remove("tool_choice");
                suppress(obj);
            }
        }
        Value::Object(v) => {
            let typ = v
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_lowercase();
            match typ.as_str() {
                "none" => {
                    obj.remove("tool_choice");
                    suppress(obj);
                }
                "auto" | "required" => {
                    obj.insert("tool_choice".to_string(), Value::String(typ));
                }
                "function" => {
                    let mut name = v
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        name = v
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    let name = name.trim().to_string();
                    obj.insert(
                        "tool_choice".to_string(),
                        Value::String(if name.is_empty() { "auto".to_string() } else { name }),
                    );
                }
                _ => {
                    obj.remove("tool_choice");
                }
            }
        }
        _ => {
            obj.remove("tool_choice");
        }
    }
}

// ---------------------------------------------------------------------------
// 脱敏（参考 sanitize.rs / Go sanitize.go）
// ---------------------------------------------------------------------------

/// 出站请求体脱敏：剥离上游内容审核黑名单指纹。
///
/// 背景：客户端（Claude Code 类 CLI）在 system prompt 注入若干固定模板句，上游内容
/// 审核按逐字精确匹配拦截（非语义审核），一字改动即可绕过。策略：键值/header 型
/// 指纹整段剥离；承载语义的模板句最小改写（换一词），语义不变。
///
/// wb-switch-core 未引入 regex 依赖：参考的两个正则剥离层以等价手写扫描实现
/// （[`strip_billing_header`]/[`strip_cc_kv`]），语义逐条对齐；改写层本就是纯字符串
/// 替换，直接照抄。已知盲区与参考一致：`tool_calls.arguments` 不脱敏（契约 §2-9）。

/// 特征预检：任一命中才进入净化（contains 快速路径，普通请求全不中 → 原样返回）。
const SANITIZE_FEATURES: &[&str] = &[
    "x-anthropic-billing-header", // header 键值段键名
    "cc_entrypoint=",             // 尾随裸键值（截断前缀即可命中）
    "You are Claude Code",        // 身份句（截断前缀即可命中）
    "Main branch (",              // 注入指令句（截断前缀即可命中）
    "github.com/anthropics",      // 官方反馈链接（Claude Code 2.1.260+ 起出现在系统提示中）
    "led by OpenAI",              // Codex CLI instructions 的归属句
];

/// 改写层：全模板句逐字替换（每句只改一个词，语义不变）。
const SANITIZE_REWRITES: &[(&str, &str)] = &[
    (
        // 身份句有两个客户端变体，查找串不带结尾标点，两种形态都能命中：
        //   CLI 模式：You are Claude Code, Anthropic's official CLI for Claude.
        //   3P 模式：You are Claude Code, Anthropic's official CLI for Claude,
        //            running within the Claude Agent SDK.（claude-desktop-3p，2.1.260+）
        "You are Claude Code, Anthropic's official CLI for Claude",
        "You are Claude Code, Anthropic's official CLI tool for Claude",
    ),
    (
        "Main branch (you will usually use this for PRs)",
        "Default branch (you will usually use this for PRs)",
    ),
    (
        // Claude Code 2.1.260 的系统提示里带有指向 Anthropic 官方仓库的反馈链接，
        // 上游按「未批准渠道」指纹拦截（HTTP 400 code=11128）。
        // 只替换链接本身，句子结构与语义（如何提交反馈）不变。
        "https://github.com/anthropics/claude-code/issues",
        "https://github.com/user-feedback/issues",
    ),
    (
        // Codex CLI 的 instructions 首句声明 "an open source project led by OpenAI"，
        // 上游同样按「未批准渠道」指纹拦截（HTTP 400 code=11128）。
        // 只改归属表述，语义（开源项目）不变。
        "led by OpenAI",
        "led by the community",
    ),
];

/// 单段文本净化：预检不中 → 返回原串（零分配）。
fn sanitize_text(text: &str) -> String {
    if !has_fingerprint(text) {
        return text.to_string();
    }
    let mut t = text.to_string();
    for (from, to) in SANITIZE_REWRITES {
        t = t.replace(from, to);
    }
    t = strip_billing_header(&t);
    // 参考的快速路径守卫原样照抄（大小写敏感）：只有出现小写 "cc_" 才清裸键值。
    if t.contains("cc_") {
        // 清尾随裸 kv（cc_version=...; cc_entrypoint=...;），循环到不再变化
        loop {
            let prev = t.clone();
            t = strip_cc_kv(&t);
            if t == prev {
                break;
            }
        }
    }
    t.trim().to_string()
}

/// 特征预检：先走 contains 快速路径（零分配）；header 键名有大小写变体
/// （X-Anthropic-...），快速路径漏掉时再走大小写不敏感contains兜底
/// （对应参考的 `SANITIZE_HDR_RE.is_match`，`(?i)` 通道）。
fn has_fingerprint(text: &str) -> bool {
    SANITIZE_FEATURES.iter().any(|f| text.contains(f))
        || contains_ignore_ascii_case(text, "x-anthropic-billing-header:")
}

/// 兼容字符串与多模态数组；只动 text part，image 等 part 不动。
/// 就地改写，返回是否发生变化。
fn sanitize_content(v: &mut Value) -> bool {
    match v {
        Value::String(s) => {
            let s_new = sanitize_text(s);
            if s_new != *s {
                *v = Value::String(s_new);
                true
            } else {
                false
            }
        }
        Value::Array(parts) => {
            let mut changed = false;
            for p in parts.iter_mut() {
                let Some(m) = p.as_object_mut() else { continue };
                let Some(text) = m.get("text").and_then(Value::as_str) else {
                    continue;
                };
                let s = sanitize_text(text);
                if s != text {
                    m.insert("text".to_string(), Value::String(s));
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}

/// 净化 messages 中的 content；任一命中返回 true。
fn sanitize_messages(messages: &mut [Value]) -> bool {
    let mut changed = false;
    for msg in messages.iter_mut() {
        let Some(m) = msg.as_object_mut() else { continue };
        let Some(content) = m.get_mut("content") else {
            continue;
        };
        if sanitize_content(content) {
            changed = true;
        }
    }
    changed
}

/// 剥离 `x-anthropic-billing-header:<值>;` 整段（大小写不敏感）。
///
/// 对应参考 `sanitize.rs` 的 `SANITIZE_HDR_RE`（`(?i)x-anthropic-billing-header:[^;\n]*;?\s*`）：
/// 值取到首个 `;`/换行前（均不含），随后吞掉可选的一个 `;` 与后续空白（含换行）。
fn strip_billing_header(text: &str) -> String {
    const KEY: &str = "x-anthropic-billing-header:";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = find_ignore_ascii_case(rest, KEY) {
        out.push_str(&rest[..idx]);
        let mut pos = idx + KEY.len();
        // [^;\n]*：值到首个 ';' 或换行（不含）。UTF-8 多字节序列内不会出现这两个
        // ASCII 字节，按字节推进与按字符推进等价。
        while let Some(c) = rest.get(pos..).and_then(|t| t.chars().next()) {
            if c == ';' || c == '\n' {
                break;
            }
            pos += c.len_utf8();
        }
        // ;?：吞掉可选的一个分号。
        if rest.get(pos..).is_some_and(|t| t.starts_with(';')) {
            pos += 1;
        }
        // \s*：吞掉后续空白（Unicode，含换行）。
        while let Some(c) = rest.get(pos..).and_then(|t| t.chars().next()) {
            if !c.is_whitespace() {
                break;
            }
            pos += c.len_utf8();
        }
        rest = &rest[pos..];
    }
    out.push_str(rest);
    out
}

/// 剥离尾随裸键值 `cc_<key>=<value>[;]`（大小写不敏感，词边界前置）。
///
/// 对应参考 `sanitize.rs` 的 `SANITIZE_KV_RE`（`(?i)\bcc_[a-z0-9_]+=[^;\n]*;?\s*`）：
/// `\b` 按 regex crate 的 Unicode 词边界口径实现——前一字符是单词字符
/// （字母/数字/下划线）时不命中（`acc_foo=1;` 里的 `cc_` 不算）。单次扫描即移除
/// 全部不重叠匹配，等价于参考的 `replace_all` + 循环到稳定。
fn strip_cc_kv(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        match match_cc_kv_at(text, i) {
            // 命中整段：跳过不拷贝（相当于 replace_all 移除）。
            Some(end) => i = end,
            // 未命中：拷贝当前字符，前进一个字符。
            None => {
                let c = text[i..].chars().next().unwrap_or(' ');
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// 若 `text` 的字节偏移 `i`（须为字符边界）处是完整的 `cc_<key>=<value>[;]` 段，
/// 返回该段的结束偏移；否则 None。
fn match_cc_kv_at(text: &str, i: usize) -> Option<usize> {
    if !text.is_char_boundary(i) {
        return None;
    }
    let rest = &text[i..];
    let mut it = rest.char_indices();
    let (_, c0) = it.next()?;
    let (_, c1) = it.next()?;
    let (off2, c2) = it.next()?;
    if !(c0.eq_ignore_ascii_case(&'c') && c1.eq_ignore_ascii_case(&'c') && c2 == '_') {
        return None;
    }
    // \b：前一字符是单词字符（字母/数字/下划线）则不是词边界。
    if i > 0 {
        if let Some(prev) = text.get(..i).and_then(|s| s.chars().next_back()) {
            if prev.is_alphanumeric() || prev == '_' {
                return None;
            }
        }
    }
    let key_start = i + off2 + c2.len_utf8();
    let b = text.as_bytes();
    // [a-z0-9_]+（(?i)）：键名其余部分，至少一个字符后必须接到 '='。
    let mut j = key_start;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    if j == key_start || j >= b.len() || b[j] != b'=' {
        return None;
    }
    j += 1;
    // [^;\n]*：值到首个 ';' 或换行（不含）。
    while j < b.len() && b[j] != b';' && b[j] != b'\n' {
        j += 1;
    }
    // ;?：吞掉可选的一个分号。
    if j < b.len() && b[j] == b';' {
        j += 1;
    }
    // \s*：吞掉后续空白（Unicode，含换行）。
    while let Some(c) = text.get(j..).and_then(|t| t.chars().next()) {
        if !c.is_whitespace() {
            break;
        }
        j += c.len_utf8();
    }
    Some(j)
}

/// 大小写不敏感的子串查找，返回字节偏移（对应参考正则的 `(?i)` 通道）。
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// 大小写不敏感的 contains。
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    find_ignore_ascii_case(haystack, needle).is_some()
}

// ---------------------------------------------------------------------------
// 短 RPC：refresh_token / fetch_models / user_resource（契约 §13）
// ---------------------------------------------------------------------------

/// 惰性刷新 access token（契约 §13 钉定签名；调用方在 `expires_at < now + 5min` 时调用）。
///
/// 端点 `{chat_base}/v2/plugin/auth/token/refresh`（参考口径走 chat_base，见模块文档），
/// 头族走 [`apply_refresh_headers`]（X-Refresh-Token 只出现在这里）。返回
/// `(新 access_token, 新 expires_at ms)`；**一次性消费语义：拿到即用**——调用方应
/// 立即将新 token 用于后续请求并回写账号库（`writeback_tokens`），不要继续发旧 token。
///
/// `expiresIn` 缺失（<=0）时保留账号旧过期时间（参考 preserveExpiry 口径：避免把
/// 过期时间清零引发刷新风暴）。响应里的轮换 refreshToken / domain 无法透过钉定签名
/// 回传（`writeback_tokens` 亦无该参数），按参考行为丢弃。
pub async fn refresh_token(acc: &PoolAccount) -> Result<(String, i64), String> {
    if acc.refresh_token.trim().is_empty() {
        return Err("refresh_failed: no refreshToken — re-login required".to_string());
    }
    let url = refresh_url(&acc.region);
    let req = apply_refresh_headers(rpc_http_client().post(&url), acc);
    let data = rpc_do_json(req).await?;
    let tok: RefreshResp = serde_json::from_value(data)
        .map_err(|e| format!("parse: refresh 响应解析失败: {e}"))?;
    if tok.access_token.is_empty() {
        return Err(
            "refresh_failed: no accessToken in response — re-login required".to_string(),
        );
    }
    let expires_at = if tok.expires_in > 0 {
        now_ms() + tok.expires_in * 1000
    } else {
        // preserveExpiry：响应缺 expiresIn 时保留旧过期时间，避免刷新风暴。
        acc.expires_at
    };
    Ok((tok.access_token, expires_at))
}

/// 模型目录（契约 §13 钉定签名）：`data.agents[name=="cli"].models` + `data.models`
/// 元数据关联（contextWindow/maxOutputTokens/efforts），转 OpenAI models 列表 JSON。
///
/// 数据来源是 cli agent 的可用清单（**不是** `data.models`——后者是产品全量池，
/// 含图片/视频生成、lite 辅助模型等）。`disabled` 的模型不下发（模型级停用开关，
/// 可能仍留在白名单里）；cli 清单里有、models 池里没有的仍返回（元数据缺失）。
/// 上游没给 cli agent（或清单为空）时兜底回退全量池（宁可多不可少）。
pub async fn fetch_models(acc: &PoolAccount) -> Result<Value, String> {
    let url = models_url(&acc.region);
    let req = apply_models(rpc_http_client().get(&url), acc);
    let resp = req.send().await.map_err(|e| format!("transport: {e}"))?;
    let status = resp.status().as_u16();
    let raw = read_body_limited(resp, ERROR_BODY_LIMIT).await?;
    if status != 200 {
        return Err(format!("models api status {status}: {}", truncate(&raw, 120)));
    }
    let env: ModelsEnv =
        serde_json::from_str(&raw).map_err(|e| format!("models parse: {e}"))?;
    if env.code != 0 {
        return Err(format!("models api code={}", env.code));
    }
    let data = build_models_list(&env);
    if data.is_empty() {
        return Err("models api returned empty list".to_string());
    }
    Ok(json!({"object": "list", "data": data}))
}

/// 纯函数：`agents[cli].models` ∩ `models` 元数据 → OpenAI models 列表的 data 数组。
/// 从 [`fetch_models`] 抽出以便无网络矩阵测试。
fn build_models_list(env: &ModelsEnv) -> Vec<Value> {
    // 先按 id 建索引，便于给 cli 清单补元数据。
    let mut meta: HashMap<String, ModelMeta> = HashMap::with_capacity(env.data.models.len());
    let mut disabled: HashSet<String> = HashSet::with_capacity(env.data.models.len());
    for m in &env.data.models {
        if m.id.is_empty() || meta.contains_key(&m.id) {
            continue;
        }
        meta.insert(
            m.id.clone(),
            ModelMeta {
                context_window: m.max_input_tokens,
                max_tokens: m.max_output_tokens,
                efforts: m.reasoning.supported_efforts.clone(),
            },
        );
        if m.disabled {
            disabled.insert(m.id.clone());
        }
    }

    // 取 cli agent 的可用清单（这是客户端真正能选的模型）。
    let cli_models = env
        .data
        .agents
        .iter()
        .find(|ag| ag.name == "cli")
        .map(|ag| ag.models.clone())
        .unwrap_or_default();

    let mut data: Vec<Value> = Vec::with_capacity(cli_models.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(cli_models.len());
    for id in &cli_models {
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        // 上游显式标了 disabled 的不下发；cli 清单里有、models 池里没有的仍返回
        // （它确实可用，只是元数据未知）。
        if disabled.contains(id) {
            continue;
        }
        data.push(model_entry(id, meta.get(id)));
    }

    // 兜底：上游没给 cli agent 时退回全量池（宁可多不可少，保持旧行为）。
    if data.is_empty() {
        for m in &env.data.models {
            if m.disabled || m.id.is_empty() || !seen.insert(m.id.clone()) {
                continue;
            }
            data.push(model_entry(&m.id, meta.get(&m.id)));
        }
    }
    data
}

/// 组装一条 OpenAI models 列表条目；元数据缺失时只带基础字段（缺失=未知，不谎报 0）。
fn model_entry(id: &str, meta: Option<&ModelMeta>) -> Value {
    let mut entry = Map::new();
    entry.insert("id".to_string(), json!(id));
    entry.insert("object".to_string(), json!("model"));
    entry.insert("created".to_string(), json!(0));
    entry.insert("owned_by".to_string(), json!("workbuddy"));
    if let Some(m) = meta {
        entry.insert("context_length".to_string(), json!(m.context_window));
        entry.insert("max_output_tokens".to_string(), json!(m.max_tokens));
        entry.insert("efforts".to_string(), json!(m.efforts));
    }
    Value::Object(entry)
}

/// 余额与到期日（契约 §13 钉定签名；驱动分层选号，先烧快过期的额度）。
///
/// 返回 `(credits, credit_expires_at ms)`；`credit_expires_at = 0` 表示未知。
/// credits = 所有套餐可花费积分之和（Cycle* 优先，负值钳 0）；到期日只统计
/// **还有剩余**的套餐中最早的时刻（已用尽的套餐到期日再早也无意义）。
pub async fn user_resource(acc: &PoolAccount) -> Result<(i64, i64), String> {
    let url = user_resource_url(&acc.region);
    let now = chrono::Local::now();
    let fmt = "%Y-%m-%d %H:%M:%S";
    let body = json!({
        "PageNumber": 1,
        "PageSize": 100,
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "PackageEndTimeRangeBegin": now.format(fmt).to_string(),
        "PackageEndTimeRangeEnd": (now + chrono::Duration::days(365 * 101)).format(fmt).to_string(),
    });
    let req = apply_billing(rpc_http_client().post(&url).json(&body), acc);
    let data = rpc_do_json(req).await?;
    let resp: ResourceResp = if data.is_null() {
        ResourceResp::default()
    } else {
        serde_json::from_value(data).map_err(|e| format!("parse: resource parse: {e}"))?
    };
    let (credits, soonest_expire_at) = aggregate_credits(&resp.response.data.accounts);
    // 契约 §13：credit_expires_at 为 ms epoch；内部聚合用秒，这里换 ms。
    Ok((credits, soonest_expire_at * 1000))
}

/// 纯函数：套餐列表 → (credits, soonest_expire_at **秒**；0=未知)。
fn aggregate_credits(accounts: &[ResourcePackage]) -> (i64, i64) {
    let mut credits = 0i64;
    let mut soonest = 0i64;
    for acct in accounts {
        let r = acct.remain();
        credits += r;
        // 只有「还有剩余」的套餐才代表真实到期压力；已用尽的套餐到期日再早也无意义。
        if r <= 0 {
            continue;
        }
        let at = acct.expiry_unix();
        if at > 0 && (soonest == 0 || at < soonest) {
            soonest = at;
        }
    }
    (credits, soonest)
}

// ---------------------------------------------------------------------------
// HTTP client 与公共读取工具
// ---------------------------------------------------------------------------

/// 短 RPC client（refresh/models/user_resource）：显式总超时 120s
/// （参考 client.rs:24）。上游是公网 host，正常 client 即可——代理走 reqwest 默认的
/// 环境变量探测（国际版在国内直连不稳定时靠它；参考 client.rs:52-53 同口径）。
fn rpc_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(RPC_TOTAL_TIMEOUT)
            .build()
            .expect("构建 workbuddy 上游 RPC client 失败")
    })
}

/// chat SSE client：不设总超时（流式响应可能很长），首字节由 [`CHAT_HEADER_TIMEOUT`]
/// 约束、流中空闲由 [`read_stream_chunk`] 约束（参考 client.rs:29-30 同口径）。
fn chat_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::new())
}

/// 出站脱敏开关：取网关配置 `sanitize_enabled`（契约 §5，默认开、可关；
/// 配置缺失时由 `load_config` 的默认值兜底）。
fn gateway_sanitize_enabled() -> bool {
    crate::modules::workbuddy_gateway::load_config().sanitize_enabled
}

/// 发短 RPC 并解统一信封（对应参考 `Client::do_json`）：
/// HTTP 非 2xx 或业务 code != 0 → Err（文案含分类，见模块文档错误字符串约定）。
async fn rpc_do_json(req: reqwest::RequestBuilder) -> Result<Value, String> {
    let resp = req.send().await.map_err(|e| format!("transport: {e}"))?;
    let status = resp.status().as_u16();
    let raw = read_body_limited(resp, ERROR_BODY_LIMIT).await?;
    if status >= 400 {
        let kind = classify(status, &raw);
        return Err(format!(
            "upstream {} (http {status}): {}",
            kind.as_str(),
            truncate(&raw, 200)
        ));
    }
    let env: ApiEnvelope = serde_json::from_str(&raw)
        .map_err(|e| format!("parse: parse failed: {e} (body: {})", truncate(&raw, 120)))?;
    if env.code != 0 {
        // 业务错误同样过 classify：SessionDead（12153）等需要调用方从文案外拿到 kind
        // （账号池据此标需重登；网络失败无 code= 段，不标——契约 §4/P1）。
        let kind = classify(status, &env.msg);
        let kind = if kind == ErrKind::None { ErrKind::Client } else { kind };
        return Err(format!(
            "upstream {} (http {status}): code={} msg={}",
            kind.as_str(),
            env.code,
            truncate(&env.msg, 160)
        ));
    }
    Ok(env.data)
}

/// 读响应体，上限 `limit` 字节（对应 Go io.LimitReader；超限截断）。
async fn read_body_limited(mut resp: reqwest::Response, limit: usize) -> Result<String, String> {
    let mut raw: Vec<u8> = Vec::new();
    while raw.len() < limit {
        match resp.chunk().await.map_err(|e| format!("transport: {e}"))? {
            Some(chunk) => {
                let remain = limit - raw.len();
                if chunk.len() > remain {
                    raw.extend_from_slice(&chunk[..remain]);
                    break;
                }
                raw.extend_from_slice(&chunk);
            }
            None => break,
        }
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// 按字符数截断（展示用；与 workbuddy_proxy::truncate 同口径，不做字节切片）。
fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// 上游响应结构体（参考 client.rs）
// ---------------------------------------------------------------------------

/// 上游统一信封。
#[derive(Debug, Default, Deserialize)]
struct ApiEnvelope {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Value,
}

/// token 刷新响应（data 段）。
#[derive(Debug, Default, Deserialize)]
struct RefreshResp {
    #[serde(default, rename = "accessToken")]
    access_token: String,
    #[serde(default, rename = "expiresIn")]
    expires_in: i64,
}

/// `/v3/config` 响应信封。
#[derive(Debug, Default, Deserialize)]
struct ModelsEnv {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: ModelsData,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsData {
    #[serde(default, rename = "models")]
    models: Vec<ModelsModel>,
    #[serde(default, rename = "agents")]
    agents: Vec<ModelsAgent>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsModel {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "maxInputTokens")]
    max_input_tokens: i64,
    #[serde(default, rename = "maxOutputTokens")]
    max_output_tokens: i64,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    reasoning: ModelsReasoning,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsReasoning {
    #[serde(default, rename = "supportedEfforts")]
    supported_efforts: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsAgent {
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: Vec<String>,
}

/// cli 清单条目的元数据（从 `data.models` 按 id 关联补齐）。
#[derive(Debug, Default, Clone)]
struct ModelMeta {
    /// = maxInputTokens
    context_window: i64,
    /// = maxOutputTokens
    max_tokens: i64,
    /// reasoning.supportedEfforts（空=未知/固定档）
    efforts: Vec<String>,
}

/// billing get-user-resource 返回的单个套餐（字段名与上游一致，PascalCase）。
///
/// 上游还返回 CapacitySize / CapacityUsed，但聚合口径（参考 `ResourcePackage`）
/// 不读它们，故不声明；serde 对未知字段默认忽略，解析不受影响。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ResourcePackage {
    #[serde(rename = "CapacityRemain")]
    capacity_remain: i64,
    #[serde(rename = "CycleCapacitySize")]
    cycle_capacity_size: i64,
    #[serde(rename = "CycleCapacityRemain")]
    cycle_capacity_remain: i64,
    #[serde(rename = "CycleCapacityUsed")]
    cycle_capacity_used: i64,
    /// 到期字段（实测国服与国际版均返回）：
    /// - DeductionEndTime 抵扣截止（epoch 毫秒）—— 额度真正失效的时刻，优先采用
    /// - ExpiredTime / CycleEndTime 兼容回退（实测可能是 "2006-01-02 15:04:05" 字符串）
    ///
    /// 声明为 Value：同一字段在不同区域/套餐上出现过数字与字符串两种形态。
    #[serde(rename = "DeductionEndTime")]
    deduction_end_time: Value,
    #[serde(rename = "ExpiredTime")]
    expired_time: Value,
    #[serde(rename = "CycleEndTime")]
    cycle_end_time: Value,
}

impl ResourcePackage {
    /// 该套餐可花费积分（与原聚合口径逐字一致：Cycle* 优先，负值钳 0）。
    fn remain(&self) -> i64 {
        let r = if self.cycle_capacity_size > 0 {
            self.cycle_capacity_remain
        } else if self.cycle_capacity_remain > 0 || self.cycle_capacity_used > 0 {
            self.cycle_capacity_remain
        } else {
            self.capacity_remain
        };
        r.max(0)
    }

    /// 该套餐的到期时刻（Unix 秒）；0 = 未知。
    fn expiry_unix(&self) -> i64 {
        for v in [&self.deduction_end_time, &self.expired_time, &self.cycle_end_time] {
            let at = parse_expiry_unix(v);
            if at > 0 {
                return at;
            }
        }
        0
    }
}

/// 把秒/毫秒 epoch 统一成秒（上游混用两种精度）。
fn normalize_epoch(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    if n > 1_000_000_000_000 {
        return n / 1000;
    }
    n
}

/// 解析上游到期字段，兼容 epoch 秒/毫秒与常见日期字符串（参考 client.rs:707-752）。
fn parse_expiry_unix(v: &Value) -> i64 {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                normalize_epoch(i)
            } else if let Some(f) = n.as_f64() {
                normalize_epoch(f as i64)
            } else {
                0
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return 0;
            }
            if let Ok(n) = s.parse::<i64>() {
                return normalize_epoch(n);
            }
            for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S%.f"] {
                if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
                    // chrono 0.4.45 的 and_local_timezone 返回 LocalResult（无 Ok 变体），
                    // 按本项目既有口径取 .single()（checkin.rs:329 / credits.rs:80-93）：
                    // 仅「唯一映射」时采用，Ambiguous/None 跳过——与参考内层 local.single()
                    // 的意图一致（参考 client.rs:728 的 if let Ok(local) 在其锁定的同一
                    // chrono 版本下无法编译，此处按可编译口径修正）。
                    if let Some(ts) = naive.and_local_timezone(chrono::Local).single() {
                        return ts.timestamp();
                    }
                }
            }
            // RFC3339（带时区偏移）
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
                return ts.timestamp();
            }
            // 仅日期：按当日 23:59:59 计（额度一般用到当天结束）。
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                let naive = d.and_hms_opt(23, 59, 59).unwrap();
                // 同上方 NaiveDateTime 分支：LocalResult 取 .single()。
                if let Some(ts) = naive.and_local_timezone(chrono::Local).single() {
                    return ts.timestamp();
                }
            }
            0
        }
        _ => 0,
    }
}

#[derive(Debug, Default, Deserialize)]
struct ResourceResp {
    #[serde(default)]
    response: ResourceResponse,
}

#[derive(Debug, Default, Deserialize)]
struct ResourceResponse {
    #[serde(default)]
    data: ResourceData,
}

#[derive(Debug, Default, Deserialize)]
struct ResourceData {
    #[serde(default, rename = "Accounts")]
    accounts: Vec<ResourcePackage>,
}

// ---------------------------------------------------------------------------
// 测试（纯函数矩阵；网络调用不测——E2E 在 workflow 外）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试账号（字段与契约 §13 的 PoolAccount 逐字对应，防漂移）。
    fn acc(region: &str) -> PoolAccount {
        PoolAccount {
            uid: "uid-1".to_string(),
            nickname: "n".to_string(),
            region: region.to_string(),
            access_token: "tok".to_string(),
            refresh_token: "rt".to_string(),
            expires_at: 0,
            refresh_expires_at: 0,
            credits: 0,
            credit_expires_at: 0,
        }
    }

    /// 从 RequestBuilder 上读最终请求头。
    ///
    /// 注意 reqwest 0.12.28 的 API 归属（async_impl/request.rs）：`headers(&self) -> &HeaderMap`
    /// getter 在 **Request** 上（:81），RequestBuilder 上的 `headers(self, HeaderMap)` 是 setter（:239）；
    /// 参考实现 headers.rs 测试里的 `builder.headers()` 在其锁定的同一 reqwest 版本下同样编译不过
    /// （继 chrono 之后第二个参考代码问题），故先 try_clone + build 再取头。
    fn hdr(req: &reqwest::RequestBuilder, name: &str) -> Option<String> {
        req.try_clone()?
            .build()
            .ok()?
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    }

    fn chat_req(a: &PoolAccount) -> reqwest::RequestBuilder {
        apply_chat(
            reqwest::Client::new().post("https://example.invalid/v2/chat/completions"),
            a,
        )
    }

    fn models_env(v: Value) -> ModelsEnv {
        serde_json::from_value(v).unwrap()
    }

    /// cycle 套餐构造helper（remain 走 CycleCapacityRemain）。
    fn pkg(remain: i64, expiry: Value) -> ResourcePackage {
        ResourcePackage {
            cycle_capacity_size: 1,
            cycle_capacity_remain: remain,
            deduction_end_time: expiry,
            ..Default::default()
        }
    }

    // ---- 区域判定与端点 ----

    #[test]
    fn chat_base_routes_by_region() {
        assert_eq!(chat_base("cn"), "https://copilot.tencent.com");
        assert_eq!(chat_base("intl"), "https://www.workbuddy.ai");
        // 大小写/首尾空白不敏感；未知区域按国服（与参考 is_intl 的 .ai 后缀判定同向）
        assert_eq!(chat_base("CN"), "https://copilot.tencent.com");
        assert_eq!(chat_base(" intl "), "https://www.workbuddy.ai");
        assert_eq!(chat_base(""), "https://copilot.tencent.com");
        assert_eq!(chat_base("us"), "https://copilot.tencent.com");
    }

    #[test]
    fn endpoint_urls_per_region() {
        assert_eq!(
            chat_url("cn"),
            "https://copilot.tencent.com/v2/chat/completions"
        );
        assert_eq!(
            chat_url("intl"),
            "https://www.workbuddy.ai/v2/chat/completions"
        );
        // refresh 端点按参考口径走 chat_base（client.rs:186 / Go client.go:673）
        assert_eq!(
            refresh_url("cn"),
            "https://copilot.tencent.com/v2/plugin/auth/token/refresh"
        );
        assert_eq!(
            refresh_url("intl"),
            "https://www.workbuddy.ai/v2/plugin/auth/token/refresh"
        );
        assert_eq!(models_url("cn"), "https://copilot.tencent.com/v3/config");
        assert_eq!(models_url("intl"), "https://www.workbuddy.ai/v3/config");
        // billing 国服分域（codebuddy.cn 系），国际版同域（契约 §10）
        assert_eq!(
            user_resource_url("cn"),
            "https://www.codebuddy.cn/v2/billing/meter/get-user-resource"
        );
        assert_eq!(
            user_resource_url("intl"),
            "https://www.workbuddy.ai/v2/billing/meter/get-user-resource"
        );
    }

    // ---- 出站头族 ----

    #[test]
    fn chat_headers_region_routing_and_account_headers() {
        let cn = chat_req(&acc("cn"));
        assert_eq!(hdr(&cn, "origin"), Some("https://www.codebuddy.cn".into()));
        assert_eq!(
            hdr(&cn, "referer"),
            Some("https://www.codebuddy.cn/".into())
        );
        let intl = chat_req(&acc("intl"));
        assert_eq!(
            hdr(&intl, "origin"),
            Some("https://www.workbuddy.ai".into())
        );
        assert_eq!(
            hdr(&intl, "referer"),
            Some("https://www.workbuddy.ai/".into())
        );

        let h = chat_req(&acc("cn")).build().unwrap().headers().clone();
        assert_eq!(h.get("authorization").unwrap(), "Bearer tok");
        assert_eq!(h.get("x-user-id").unwrap(), "uid-1");
        assert_eq!(h.get("x-product").unwrap(), "SaaS");
        assert_eq!(h.get("user-agent").unwrap(), CLIENT_UA);
        // X-Domain 按区域推导（与账号库实测 domain 值一致）
        assert_eq!(h.get("x-domain").unwrap(), "www.workbuddy.cn");
        // PoolAccount 无 enterprise_id：恒 X-No-* 占位
        assert_eq!(h.get("x-no-enterprise-id").unwrap(), "1");
        assert!(h.get("x-enterprise-id").is_none());
        assert_eq!(h.get("accept").unwrap(), "application/json, text/plain, */*");
        assert_eq!(h.get("x-requested-with").unwrap(), "XMLHttpRequest");

        let h = chat_req(&acc("intl")).build().unwrap().headers().clone();
        assert_eq!(h.get("x-domain").unwrap(), "www.workbuddy.ai");
    }

    #[test]
    fn chat_headers_placeholders_when_missing() {
        let a = PoolAccount {
            access_token: String::new(),
            uid: String::new(),
            ..acc("cn")
        };
        let h = chat_req(&a).build().unwrap().headers().clone();
        assert_eq!(h.get("x-no-authorization").unwrap(), "1");
        assert!(h.get("authorization").is_none(), "缺 token 时不得携带 Authorization");
        assert_eq!(h.get("x-no-user-id").unwrap(), "1");
        assert!(h.get("x-user-id").is_none());
    }

    #[test]
    fn chat_headers_never_carry_refresh_token() {
        // 🔴 安全红线（契约 §10）：X-Refresh-Token 绝不上 chat 请求。
        let h = chat_req(&acc("cn")).build().unwrap().headers().clone();
        assert!(h.get("x-refresh-token").is_none());
        assert!(h.get("x-auth-refresh-source").is_none());
    }

    #[test]
    fn billing_headers_shape() {
        let req = apply_billing(
            reqwest::Client::new().post("https://example.invalid/billing"),
            &acc("cn"),
        );
        let h = req.build().unwrap().headers().clone();
        assert_eq!(h.get("authorization").unwrap(), "Bearer tok");
        assert_eq!(h.get("accept").unwrap(), "application/json");
        assert_eq!(h.get("content-type").unwrap(), "application/json");
        assert_eq!(h.get("x-user-id").unwrap(), "uid-1");
        assert_eq!(h.get("x-domain").unwrap(), "www.workbuddy.cn");
        // 无 enterprise_id：不带租户头
        assert!(h.get("x-tenant-id").is_none());
    }

    #[test]
    fn refresh_headers_carry_refresh_token() {
        let req = apply_refresh_headers(
            reqwest::Client::new().post("https://example.invalid/refresh"),
            &acc("intl"),
        );
        let h = req.build().unwrap().headers().clone();
        // X-Refresh-Token 只允许出现在 refresh 端点（契约 §10 安全红线）
        assert_eq!(h.get("x-refresh-token").unwrap(), "rt");
        assert_eq!(h.get("x-auth-refresh-source").unwrap(), "workbuddy");
        assert_eq!(h.get("origin").unwrap(), "https://www.workbuddy.ai");
    }

    #[test]
    fn models_headers_use_workbuddy_ua() {
        let req = apply_models(
            reqwest::Client::new().get("https://example.invalid/v3/config"),
            &acc("cn"),
        );
        let h = req.build().unwrap().headers().clone();
        assert_eq!(h.get("user-agent").unwrap(), MODELS_CONFIG_UA);
        assert!(h
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("WorkBuddy/"));
        assert_eq!(h.get("authorization").unwrap(), "Bearer tok");
    }

    // ---- prepare_body ----

    #[test]
    fn prepare_body_forces_stream_true() {
        let out = prepare_body(&json!({"model": "m", "stream": false}), "cn", false);
        assert_eq!(out["stream"], json!(true));
        let out = prepare_body(&json!({"model": "m"}), "intl", false);
        assert_eq!(out["stream"], json!(true));
    }

    #[test]
    fn prepare_body_non_object_passthrough() {
        assert_eq!(prepare_body(&json!([1, 2]), "cn", true), json!([1, 2]));
        assert_eq!(prepare_body(&json!("x"), "intl", true), json!("x"));
        assert_eq!(prepare_body(&json!(null), "cn", true), json!(null));
    }

    #[test]
    fn prepare_body_intl_system_first() {
        let src = json!({"messages": [{"role": "user", "content": "hi"}]});
        // 国服无「首条必须 system」要求，不动
        let cn = prepare_body(&src, "cn", false);
        assert_eq!(cn["messages"].as_array().unwrap().len(), 1);
        // 国际版补一条最小 system（防 400 code=11128）
        let intl = prepare_body(&src, "intl", false);
        let msgs = intl["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], json!("system"));
        assert_eq!(msgs[0]["content"], json!("You are a helpful assistant."));
        assert_eq!(msgs[1]["role"], json!("user"));
        assert_eq!(msgs[1]["content"], json!("hi"));
        // stream 仍被强制
        assert_eq!(intl["stream"], json!(true));
    }

    #[test]
    fn prepare_body_intl_skips_existing_system_and_empty() {
        // 首条已是 system（大小写/空白不敏感）不补
        let src = json!({"messages": [{"role": " SYSTEM ", "content": "x"}]});
        let out = prepare_body(&src, "intl", false);
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        // 空 messages 不动（交给上游报错更诚实）
        let src = json!({"messages": []});
        let out = prepare_body(&src, "intl", false);
        assert_eq!(out["messages"].as_array().unwrap().len(), 0);
        // 缺 messages 字段不动
        let out = prepare_body(&json!({"model": "m"}), "intl", false);
        assert!(out.get("messages").is_none());
        assert_eq!(out["stream"], json!(true));
    }

    #[test]
    fn prepare_body_developer_role_normalized() {
        let src = json!({"messages": [{"role": "developer", "content": "be nice"}, {"role": "user", "content": "hi"}]});
        // 协议兼容归一与 sanitize 解耦：sanitize=false 也照常归一
        let out = prepare_body(&src, "cn", false);
        assert_eq!(out["messages"][0]["role"], json!("system"));
        assert_eq!(out["messages"][1]["role"], json!("user"));
        // intl：developer 已归一为 system，不必再插一条（否则 Codex 客户端会多一条 system）
        let out = prepare_body(&src, "intl", false);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], json!("system"));
        assert_eq!(msgs[0]["content"], json!("be nice"));
    }

    #[test]
    fn prepare_body_tool_choice_matrix() {
        let out = prepare_body(&json!({"tool_choice": {"type": "auto"}}), "cn", false);
        assert_eq!(out["tool_choice"], json!("auto"));
        let out = prepare_body(&json!({"tool_choice": {"type": "required"}}), "cn", false);
        assert_eq!(out["tool_choice"], json!("required"));
        let out = prepare_body(
            &json!({"tool_choice": {"type": "function", "function": {"name": "get_weather"}}}),
            "cn",
            false,
        );
        assert_eq!(out["tool_choice"], json!("get_weather"));
        // name 取 tool_choice 对象内的 name（参考 normalizeToolChoice 的 v["name"] 口径）；
        // 顶层的 "name" 不是 tool_choice 的字段，不参与归一化。
        let out = prepare_body(
            &json!({"tool_choice": {"type": "function", "name": "n"}}),
            "cn",
            false,
        );
        assert_eq!(out["tool_choice"], json!("n"));
        let out = prepare_body(&json!({"tool_choice": {"type": "function"}}), "cn", false);
        assert_eq!(out["tool_choice"], json!("auto"));
        let out = prepare_body(&json!({"tool_choice": {"type": "bogus"}}), "cn", false);
        assert!(out.get("tool_choice").is_none());
        // "none" 连 tools/functions 一起删
        let out = prepare_body(
            &json!({"tool_choice": "none", "tools": [1], "functions": {}}),
            "cn",
            false,
        );
        assert!(out.get("tool_choice").is_none());
        assert!(out.get("tools").is_none());
        assert!(out.get("functions").is_none());
        let out = prepare_body(&json!({"tool_choice": {"type": "none"}, "tools": [1]}), "cn", false);
        assert!(out.get("tools").is_none());
        // 大小写不敏感
        let out = prepare_body(&json!({"tool_choice": "NONE"}), "cn", false);
        assert!(out.get("tool_choice").is_none());
    }

    #[test]
    fn prepare_body_sanitize_matrix() {
        // 身份句：sanitize=true 改写，false 原样
        let identity = json!({"messages": [{"role": "system", "content": "You are Claude Code, Anthropic's official CLI for Claude."}]});
        let out = prepare_body(&identity, "cn", true);
        assert_eq!(
            out["messages"][0]["content"],
            json!("You are Claude Code, Anthropic's official CLI tool for Claude.")
        );
        let out = prepare_body(&identity, "cn", false);
        assert_eq!(
            out["messages"][0]["content"],
            json!("You are Claude Code, Anthropic's official CLI for Claude.")
        );

        // billing 头：true 剥、false 留
        let billing = json!({"messages": [{"role": "user", "content": "x-anthropic-billing-header: cc_version=1.0; cc_entrypoint=cli;\nrest"}]});
        let out = prepare_body(&billing, "cn", true);
        let content = out["messages"][0]["content"].as_str().unwrap();
        assert!(!content.to_lowercase().contains("x-anthropic-billing-header"));
        assert!(!content.contains("cc_entrypoint"));
        assert!(content.contains("rest"));
        let out = prepare_body(&billing, "cn", false);
        assert!(out["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("x-anthropic-billing-header"));

        // 多模态数组：只动 text part，image 等 part 不动
        let mm = json!({"messages": [{"role": "system", "content": [{"type": "text", "text": "led by OpenAI"}, {"type": "image", "source": {"data": "led by OpenAI"}}]}]});
        let out = prepare_body(&mm, "cn", true);
        assert_eq!(
            out["messages"][0]["content"][0]["text"],
            json!("led by the community")
        );
        assert_eq!(
            out["messages"][0]["content"][1]["source"]["data"],
            json!("led by OpenAI")
        );

        // 反馈链接 / 分支句改写
        let links = json!({"messages": [{"role": "system", "content": "Main branch (you will usually use this for PRs); see https://github.com/anthropics/claude-code/issues"}]});
        let out = prepare_body(&links, "cn", true);
        let content = out["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("Default branch ("));
        assert!(content.contains("https://github.com/user-feedback/issues"));
        assert!(!content.contains("github.com/anthropics"));

        // 脱敏与区域、stream 强制互不干扰
        let out = prepare_body(&identity, "intl", true);
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["messages"][0]["role"], json!("system"));
        assert!(out["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("CLI tool"));
    }

    // ---- 脱敏单元 ----

    #[test]
    fn sanitize_text_rewrites_identity_both_variants() {
        let cli = "You are Claude Code, Anthropic's official CLI for Claude.";
        assert_eq!(
            sanitize_text(cli),
            "You are Claude Code, Anthropic's official CLI tool for Claude."
        );
        let sdk = "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.";
        assert!(sanitize_text(sdk).contains("official CLI tool for Claude, running within"));
    }

    #[test]
    fn sanitize_text_rewrites_branch_feedback_and_attribution() {
        assert_eq!(
            sanitize_text("Main branch (you will usually use this for PRs)"),
            "Default branch (you will usually use this for PRs)"
        );
        assert_eq!(
            sanitize_text("see https://github.com/anthropics/claude-code/issues for feedback"),
            "see https://github.com/user-feedback/issues for feedback"
        );
        assert_eq!(
            sanitize_text("an open source project led by OpenAI"),
            "an open source project led by the community"
        );
    }

    #[test]
    fn sanitize_text_strips_billing_header_and_cc_kv() {
        let src = "x-anthropic-billing-header: cc_version=1.0; cc_entrypoint=cli;\nrest";
        let out = sanitize_text(src);
        assert!(!out.to_lowercase().contains("x-anthropic-billing-header"));
        assert!(out.contains("rest"));
        // 大小写变体走 (?i) 兜底通道
        let upper = sanitize_text("X-Anthropic-Billing-Header: foo;");
        assert!(!upper.to_lowercase().contains("billing-header"));
        let kv = sanitize_text("prefix cc_version=2.63.2; cc_entrypoint=cli; suffix");
        assert!(!kv.contains("cc_version"));
        assert!(!kv.contains("cc_entrypoint"));
        assert!(kv.contains("prefix"));
        assert!(kv.contains("suffix"));
        // 词边界：前缀是单词字符时不算裸键值（对应正则 \b）
        assert_eq!(sanitize_text("xcc_version=1;"), "xcc_version=1;");
    }

    #[test]
    fn sanitize_text_plain_untouched() {
        assert_eq!(sanitize_text("你好，世界"), "你好，世界");
        assert_eq!(sanitize_text(""), "");
        assert!(!has_fingerprint("ordinary conversation"));
        assert!(has_fingerprint("You are Claude Code"));
        assert!(has_fingerprint("X-Anthropic-Billing-Header: foo;"));
    }

    // ---- fetch_models 纯函数矩阵 ----

    #[test]
    fn build_models_list_joins_cli_whitelist_with_metadata() {
        let env = models_env(json!({
            "code": 0,
            "data": {
                "models": [
                    {"id": "m1", "name": "M1", "maxInputTokens": 1000, "maxOutputTokens": 100, "reasoning": {"supportedEfforts": ["low", "high"]}},
                    {"id": "m2", "name": "M2", "maxInputTokens": 2000, "maxOutputTokens": 200, "disabled": true}
                ],
                "agents": [{"name": "cli", "models": ["m1", "m2", "m3"]}]
            }
        }));
        let list = build_models_list(&env);
        let ids: Vec<&str> = list.iter().filter_map(|m| m["id"].as_str()).collect();
        // m2 被上游标 disabled：即便在 cli 白名单里也不下发
        assert_eq!(ids, vec!["m1", "m3"]);
        // m1：data.models 元数据关联补齐（contextWindow/maxOutputTokens/efforts）
        assert_eq!(list[0]["object"], json!("model"));
        assert_eq!(list[0]["owned_by"], json!("workbuddy"));
        assert_eq!(list[0]["context_length"], json!(1000));
        assert_eq!(list[0]["max_output_tokens"], json!(100));
        assert_eq!(list[0]["efforts"], json!(["low", "high"]));
        // m3：cli 清单里有、models 池里没有——仍返回，但元数据缺失（不谎报 0）
        assert!(list[1].get("context_length").is_none());
        assert!(list[1].get("max_output_tokens").is_none());
        assert!(list[1].get("efforts").is_none());
    }

    #[test]
    fn build_models_list_falls_back_to_full_pool_without_cli_agent() {
        let env = models_env(json!({
            "code": 0,
            "data": {
                "models": [
                    {"id": "a", "maxInputTokens": 10, "maxOutputTokens": 1},
                    {"id": "b", "disabled": true}
                ],
                "agents": [{"name": "web", "models": ["a"]}]
            }
        }));
        let list = build_models_list(&env);
        let ids: Vec<&str> = list.iter().filter_map(|m| m["id"].as_str()).collect();
        // cli agent 缺失 → 兜底全量池；b disabled 剔除
        assert_eq!(ids, vec!["a"]);
        assert_eq!(list[0]["context_length"], json!(10));
        assert_eq!(list[0]["max_output_tokens"], json!(1));
    }

    #[test]
    fn build_models_list_dedups_and_empty() {
        let env = models_env(json!({
            "code": 0,
            "data": {"models": [], "agents": [{"name": "cli", "models": ["x", "x"]}]}
        }));
        let list = build_models_list(&env);
        assert_eq!(list.len(), 1);
        // cli 清单里有、models 池里没有：仍返回，但不带元数据字段
        assert!(list[0].get("context_length").is_none());
        assert!(list[0].get("efforts").is_none());

        let empty = models_env(json!({"code": 0, "data": {"models": [], "agents": []}}));
        assert!(build_models_list(&empty).is_empty());
    }

    // ---- user_resource 纯函数矩阵 ----

    #[test]
    fn aggregate_credits_sums_and_tracks_soonest() {
        let pkgs = vec![
            pkg(100, json!(1_800_000_000_000i64)), // 剩余 100，到期（epoch 毫秒）
            pkg(50, json!(1_700_000_000)),      // 剩余 50，到期（epoch 秒）更早
            pkg(0, json!(1_600_000_000)),       // 已用尽：到期再早也不算压力
        ];
        let (credits, soonest) = aggregate_credits(&pkgs);
        assert_eq!(credits, 150);
        assert_eq!(soonest, 1_700_000_000);
    }

    #[test]
    fn aggregate_credits_clamps_negative_and_unknown_expiry() {
        let (credits, soonest) = aggregate_credits(&[pkg(-5, Value::Null)]);
        assert_eq!(credits, 0, "负值钳 0");
        assert_eq!(soonest, 0, "无到期字段 = 未知");
        let (credits, soonest) = aggregate_credits(&[pkg(7, Value::Null)]);
        assert_eq!(credits, 7);
        assert_eq!(soonest, 0);
    }

    #[test]
    fn parse_expiry_unix_normalizes_precision() {
        assert_eq!(parse_expiry_unix(&json!(1_800_000_000_000i64)), 1_800_000_000);
        assert_eq!(parse_expiry_unix(&json!(1_800_000_000)), 1_800_000_000);
        assert_eq!(parse_expiry_unix(&json!("1800000000")), 1_800_000_000);
        assert_eq!(parse_expiry_unix(&json!(0)), 0);
        assert_eq!(parse_expiry_unix(&Value::Null), 0);
        assert_eq!(parse_expiry_unix(&json!("")), 0);
        assert_eq!(parse_expiry_unix(&json!("garbage")), 0);
        // 日期字符串按本地时区解释（参考口径），只断言能解出正时刻。
        // 取年中正午与 23:59:59——均不落在任何现实时区的 DST 缝隙时段内，
        // 避免测试机时区差异导致 .single() 落到 Ambiguous/None。
        assert!(parse_expiry_unix(&json!("2099-06-15 12:34:56")) > 0);
        assert!(parse_expiry_unix(&json!("2099-06-15")) > 0);
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("积分不足", 2), "积分");
        assert_eq!(truncate("积分不足", 10), "积分不足");
    }
}
