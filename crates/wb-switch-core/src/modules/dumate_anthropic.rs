//! DuMate 通道的 Anthropic Messages 协议兼容层（2026-10-01，1.0.13）。
//!
//! 背景：ZCode 等客户端的 provider **统一走 `anthropic-messages` 协议**（实测其 11 个
//! provider 全是该类型），而 DuMate 通道只提供 OpenAI 风格 `/dumate/v1/chat/completions`。
//! 本模块把 Anthropic Messages 请求转换为 OpenAI 载荷、复用 dumate 上游链路，
//! 响应（含 SSE 流）再转回 Anthropic 事件序列 —— 转换逻辑复用 `glm_anthropic` 的通用实现。
//!
//! 对外：`messages()`；路由：`POST /dumate/v1/messages`。

use bytes::Bytes;
use serde_json::{json, Value};

use super::glm_anthropic::{from_openai_response, to_openai_payload, AnthropicSseTransformer};
use super::workbuddy_proxy::WorkbuddyUpstreamStream;

/// 统一返回：Anthropic 形状的 JSON，或 Anthropic SSE 事件流。
pub enum DumateAnthropicReply {
    Json(Value),
    Stream(WorkbuddyUpstreamStream),
}

/// Anthropic Messages 入口：支持流式与非流式。
pub async fn messages(body: Value) -> Result<DumateAnthropicReply, String> {
    let (openai_body, model, is_stream) = to_openai_payload(&body)?;
    if is_stream {
        let (upstream, _actual) = super::dumate::chat_completions_stream(&model, &openai_body)
            .await
            .map_err(|e| e.message)?;
        Ok(DumateAnthropicReply::Stream(pump_stream(upstream).await))
    } else {
        let json_resp = super::dumate::chat_completions_json(&model, &openai_body)
            .await
            .map_err(|e| e.message)?;
        Ok(DumateAnthropicReply::Json(from_openai_response(
            &json_resp,
            &model,
        )))
    }
}

/// 把 dumate 的 OpenAI SSE 字节流转换为 Anthropic SSE 事件流。
async fn pump_stream(mut upstream: WorkbuddyUpstreamStream) -> WorkbuddyUpstreamStream {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);

    tokio::spawn(async move {
        let mut transformer = AnthropicSseTransformer::new();
        // 跨 chunk 的 UTF-8 半字符尾部：严格 from_utf8 失败会整块丢弃，
        // 导致大帧（中文）被误判「空流」。统一走 decode_stream_chunk。
        let mut carry: Vec<u8> = Vec::new();

        while let Some(item) = upstream.rx.recv().await {
            match item {
                Ok(chunk) => {
                    // 保活注释帧（以 : 开头）原样透传，保持连接活性
                    if chunk.starts_with(b":") {
                        if tx.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    let text = super::glm_anthropic::decode_stream_chunk(&mut carry, &chunk);
                    for event in transformer.push(&text) {
                        if tx.send(Ok(Bytes::from(event))).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    // 用 overloaded_error（Anthropic 语义=瞬时故障，客户端应重试），
                    // 而非 api_error（客户端判定不可重试而直接放弃）。
                    let err_event = format!(
                        "event: error\ndata: {}\n\n",
                        json!({
                            "type": "error",
                            "error": {
                                "type": "overloaded_error",
                                "message": format!("DuMate 上游流异常中断：{e}")
                            }
                        })
                    );
                    let _ = tx.send(Ok(Bytes::from(err_event))).await;
                    return;
                }
            }
        }
        if !carry.is_empty() {
            let tail = String::from_utf8_lossy(&carry).into_owned();
            for event in transformer.push(&tail) {
                let _ = tx.send(Ok(Bytes::from(event))).await;
            }
        }
        for event in transformer.finish() {
            if tx.send(Ok(Bytes::from(event))).await.is_err() {
                return;
            }
        }
    });

    WorkbuddyUpstreamStream { rx }
}
