//! axum router: auth/Host/Origin guards, models/chat/healthz handlers, SSE streaming.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use sha2::{Digest as _, Sha256};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::herdr::HerdrApi;
use crate::openai::{
    self, ApiError, ChatCompletion, ChatCompletionChunk, ChatRequest, ChunkDelta,
    FunctionCallChunk, MessageContent, ModelObject, ToolCallChunk, Usage,
};
use crate::question_hook::HookInput;
use crate::service::Service;

/// Hard cap on request bodies.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// SSE comment cadence while a turn is running.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// Content pieces are at most this many bytes, split on char boundaries.
const CONTENT_CHUNK_BYTES: usize = 2048;

pub fn router<H: HerdrApi>(service: Arc<Service<H>>) -> Router {
    Router::new()
        .route("/healthz", get(healthz::<H>))
        .route("/v1/models", get(list_models::<H>))
        .route("/v1/models/{id}", get(get_model::<H>))
        .route("/v1/chat/completions", post(chat_completions::<H>))
        .route(
            "/internal/claude-hooks/pre-tool-use",
            post(claude_pre_tool_use::<H>),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(service)
}

/// Applied first in every handler: bearer auth when configured, and loopback-only
/// Host/Origin checks unless `allow_remote`.
fn guard<H: HerdrApi>(service: &Service<H>, headers: &HeaderMap) -> Result<(), ApiError> {
    if let Some(key) = service.api_key() {
        if !bearer_matches(headers, key) {
            return Err(ApiError::invalid_api_key("missing or invalid bearer token"));
        }
    }
    if !service.allow_remote() {
        let host = headers
            .get("host")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !host_is_loopback(host) {
            return Err(ApiError::origin_not_allowed(format!(
                "Host {host:?} is not a loopback name"
            )));
        }
        if headers.contains_key("origin") {
            return Err(ApiError::origin_not_allowed(
                "browsers must not call this API; Origin is not allowed",
            ));
        }
    }
    Ok(())
}

fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(got) = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
    else {
        return false;
    };
    constant_time_eq(got.as_bytes(), expected.as_bytes())
}

/// Hand-written constant-time comparison to avoid leaking the key through timing.
/// Both sides are hashed first, so the compared length is always 32 bytes and the
/// secret's length does not leak either.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let (a, b) = (Sha256::digest(a), Sha256::digest(b));
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn claude_pre_tool_use<H: HerdrApi>(
    State(service): State<Arc<Service<H>>>,
    headers: HeaderMap,
    Json(input): Json<HookInput>,
) -> Response {
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !host_is_loopback(host) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let token = headers
        .get("x-herdr-hook-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !service.hook_token_matches(token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match service.handle_question_hook(input).await {
        Some(response) => Json(response).into_response(),
        None => StatusCode::OK.into_response(),
    }
}

/// Loopback Host names in any port form: `localhost`, `127.0.0.1`, `[::1]`.
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim();
    if host == "::1" {
        return true;
    }
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.find(']') {
            Some(index) => &rest[..index],
            None => rest,
        }
    } else if let Some((name, port)) = host.rsplit_once(':') {
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            name
        } else {
            host
        }
    } else {
        host
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

async fn healthz<H: HerdrApi>(
    State(service): State<Arc<Service<H>>>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = guard(&service, &headers) {
        return error_response(err);
    }
    let body = service.health().await;
    let reachable = body["herdr"]["reachable"].as_bool().unwrap_or(false);
    let status = if reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

async fn list_models<H: HerdrApi>(
    State(service): State<Arc<Service<H>>>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = guard(&service, &headers) {
        return error_response(err);
    }
    let models = service
        .models()
        .iter()
        .map(|model| (model.id.clone(), model.description()));
    Json(openai::model_list(models, service.created())).into_response()
}

async fn get_model<H: HerdrApi>(
    State(service): State<Arc<Service<H>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = guard(&service, &headers) {
        return error_response(err);
    }
    let Some(model) = service.models().iter().find(|model| model.id == id) else {
        return error_response(ApiError::model_not_found(format!("model not found: {id}")));
    };
    Json(ModelObject {
        id,
        description: model.description(),
        object: "model",
        created: service.created(),
        owned_by: "herdr-openai-server",
    })
    .into_response()
}

async fn chat_completions<H: HerdrApi>(
    State(service): State<Arc<Service<H>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(err) = guard(&service, &headers) {
        return error_response(err);
    }
    let request: ChatRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return error_response(ApiError::new(
                400,
                "invalid_request_error",
                "invalid_json",
                err.to_string(),
            ));
        }
    };
    if service.log_requests() {
        log_request_shape(&headers, &request);
    }
    let mut normalized = match openai::normalize(request, |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }) {
        Ok(normalized) => normalized,
        Err(err) => return error_response(err),
    };
    normalized.client_peer = Some(peer);
    if normalized.stream {
        return stream_response(service, normalized);
    }
    let model = normalized.model.clone();
    match service.chat(normalized).await {
        Ok(outcome) => {
            let completion = ChatCompletion::completion(
                &completion_id(),
                &model,
                unix_seconds(),
                &outcome.reply,
            );
            let mut response = Json(completion).into_response();
            let response_headers = response.headers_mut();
            response_headers.insert(
                "x-herdr-conversation",
                HeaderValue::from_str(&outcome.conversation.to_string())
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            response_headers.insert(
                "x-herdr-agent",
                HeaderValue::from_str(&outcome.agent_name)
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            response_headers.insert(
                "x-herdr-tab",
                HeaderValue::from_str(&outcome.tab_label)
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            response
        }
        Err(err) => error_response(err),
    }
}

/// Render an `ApiError` with its status and the OpenAI-shaped body.
fn error_response(err: ApiError) -> Response {
    let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(err.body())).into_response()
}

/// The id format for both non-stream and stream responses.
fn completion_id() -> String {
    format!("chatcmpl-{}", Uuid::new_v4().simple())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// SSE: role chunk immediately, keepalive comments while the turn runs, content in
/// <= 2 KiB pieces, finish chunk, optional usage chunk, then `[DONE]`. Errors after
/// the stream started arrive as an error event followed by `[DONE]`.
fn stream_response<H: HerdrApi>(
    service: Arc<Service<H>>,
    request: crate::openai::NormalizedRequest,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    let id = completion_id();
    let created = unix_seconds();
    let model = request.model.clone();
    let include_usage = request.include_usage;
    tokio::spawn(async move {
        let role_chunk = ChatCompletionChunk::chunk(
            &id,
            &model,
            created,
            ChunkDelta {
                role: Some("assistant".to_string()),
                content: Some(String::new()),
                ..ChunkDelta::default()
            },
        );
        if tx.send(data_event(&role_chunk)).await.is_err() {
            return;
        }
        // Stop awaiting the turn as soon as the client hangs up: dropping the
        // receiver here is what marks the running turn as orphaned.
        let outcome = tokio::select! {
            result = service.chat(request) => result,
            _ = tx.closed() => return,
        };
        match outcome {
            Ok(outcome) => {
                if let Some(text) = outcome.reply.text.as_deref() {
                    for piece in split_content(text) {
                        let chunk = ChatCompletionChunk::chunk(
                            &id,
                            &model,
                            created,
                            ChunkDelta {
                                content: Some(piece),
                                ..ChunkDelta::default()
                            },
                        );
                        if tx.send(data_event(&chunk)).await.is_err() {
                            return;
                        }
                    }
                }
                // One chunk per tool call, before the finish chunk.
                for (index, call) in outcome.reply.tool_calls.iter().enumerate() {
                    let chunk = ChatCompletionChunk::chunk(
                        &id,
                        &model,
                        created,
                        ChunkDelta {
                            tool_calls: Some(vec![ToolCallChunk {
                                index: index as u32,
                                id: Some(call.id.clone()),
                                call_type: Some("function".to_string()),
                                function: Some(FunctionCallChunk {
                                    name: Some(call.function.name.clone()),
                                    arguments: Some(call.function.arguments.clone()),
                                }),
                            }]),
                            ..ChunkDelta::default()
                        },
                    );
                    if tx.send(data_event(&chunk)).await.is_err() {
                        return;
                    }
                }
                let finish =
                    ChatCompletionChunk::finish(&id, &model, created, outcome.reply.finish_reason);
                let _ = tx.send(data_event(&finish)).await;
                if include_usage {
                    let usage_chunk = ChatCompletionChunk {
                        id: id.clone(),
                        object: "chat.completion.chunk",
                        created,
                        model: model.clone(),
                        choices: Vec::new(),
                        usage: Some(Usage::default()),
                    };
                    let _ = tx.send(data_event(&usage_chunk)).await;
                }
            }
            Err(err) => {
                let _ = tx
                    .send(Ok(Event::default().data(err.body().to_string())))
                    .await;
            }
        }
        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
    });
    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(KEEPALIVE_INTERVAL))
        .into_response()
}

fn data_event(chunk: &impl serde::Serialize) -> Result<Event, Infallible> {
    Ok(Event::default().data(serde_json::to_string(chunk).unwrap_or_default()))
}

/// Split into pieces of at most `CONTENT_CHUNK_BYTES`, never inside a char.
fn split_content(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > CONTENT_CHUNK_BYTES {
            pieces.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Credential headers whose values `--log-requests` never prints.
const REDACTED_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-api-key",
    "api-key",
    "x-herdr-hook-token",
];

/// `name: value` per header, with credential headers redacted.
fn format_header_text(headers: &HeaderMap) -> Vec<String> {
    headers
        .iter()
        .map(|(name, value)| {
            if REDACTED_HEADERS.contains(&name.as_str()) {
                format!("{name}: [redacted]")
            } else {
                format!("{name}: {}", value.to_str().unwrap_or("<non-utf8>"))
            }
        })
        .collect()
}

/// `--log-requests`: headers (credentials redacted) and body shape only; the
/// message text is never logged.
fn log_request_shape(headers: &HeaderMap, request: &ChatRequest) {
    let header_text = format_header_text(headers);
    let message_shapes: Vec<String> = request
        .messages
        .iter()
        .map(|message| {
            let length = match message.content.as_ref() {
                Some(MessageContent::Text(text)) => text.len(),
                Some(MessageContent::Parts(parts)) => parts
                    .iter()
                    .map(|part| part.text.as_ref().map_or(0, |text| text.len()))
                    .sum(),
                None => 0,
            };
            format!("{}({length})", message.role)
        })
        .collect();
    let tool_names: Vec<&str> = request
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool.function.name.as_str())
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        model = %request.model,
        stream = request.stream,
        messages = %message_shapes.join(", "),
        tools = %tool_names.join(", "),
        headers = %header_text.join(" | "),
        "request shape",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(header: &str) -> bool {
        host_is_loopback(header)
    }

    #[test]
    fn loopback_hosts_are_accepted() {
        assert!(host("localhost"));
        assert!(host("localhost:8787"));
        assert!(host("127.0.0.1"));
        assert!(host("127.0.0.1:8787"));
        assert!(host("[::1]"));
        assert!(host("[::1]:8787"));
        assert!(host("::1"));
    }

    #[test]
    fn remote_or_rebinding_hosts_are_rejected() {
        assert!(!host("example.com"));
        assert!(!host("example.com:8787"));
        assert!(!host("10.0.0.5:8787"));
        assert!(!host("0.0.0.0:8787"));
        assert!(!host("127.0.0.2:8787"));
        assert!(!host(""));
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"key", b"key"));
        assert!(!constant_time_eq(b"key", b"kez"));
        assert!(!constant_time_eq(b"key", b"keys"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"key"));
        let long = "k".repeat(1000);
        assert!(constant_time_eq(long.as_bytes(), long.as_bytes()));
        assert!(!constant_time_eq(long.as_bytes(), b"k"));
    }

    #[test]
    fn credential_headers_are_redacted_in_request_logs() {
        let mut headers = HeaderMap::new();
        for name in REDACTED_HEADERS {
            headers.insert(name, HeaderValue::from_static("secret-value"));
        }
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let text = format_header_text(&headers);
        assert!(
            !text.iter().any(|line| line.contains("secret-value")),
            "{text:?}"
        );
        for name in REDACTED_HEADERS {
            assert!(text.contains(&format!("{name}: [redacted]")), "{text:?}");
        }
        assert!(text.contains(&"content-type: application/json".to_string()));
    }

    #[test]
    fn content_pieces_respect_the_limit_and_char_boundaries() {
        let text = "x".repeat(5000);
        let pieces = split_content(&text);
        assert!(pieces
            .iter()
            .all(|piece| piece.len() <= CONTENT_CHUNK_BYTES));
        assert_eq!(pieces.concat(), text);

        let cjk = "你好世界".repeat(1000);
        let pieces = split_content(&cjk);
        assert!(pieces
            .iter()
            .all(|piece| piece.len() <= CONTENT_CHUNK_BYTES));
        assert_eq!(pieces.concat(), cjk);

        assert_eq!(split_content("short"), vec!["short".to_string()]);
        assert!(split_content("").is_empty());
    }
}
