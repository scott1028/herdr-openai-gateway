//! End-to-end HTTP tests: axum Router + FakeHerdr + a temporary transcript root.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, Response, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use herdr_openai_gateway::config::Config;
use herdr_openai_gateway::herdr::fake::{FakeHerdr, ScriptedTurn};
use herdr_openai_gateway::server;
use herdr_openai_gateway::service::{QuestionHookEnv, Service, ServiceEnv};

struct TestApp {
    dir: PathBuf,
    fake: Arc<FakeHerdr>,
    router: Router,
}

impl TestApp {
    fn new(config: Config, fake: FakeHerdr) -> Self {
        Self::with_api_key(config, fake, None)
    }

    fn with_api_key(config: Config, fake: FakeHerdr, api_key: Option<String>) -> Self {
        Self::with_hook(config, fake, api_key, None)
    }

    fn with_question_hook(config: Config, fake: FakeHerdr) -> Self {
        Self::with_hook(
            config,
            fake,
            None,
            Some(QuestionHookEnv {
                url: "http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use".to_string(),
                token: "test-hook-token".to_string(),
            }),
        )
    }

    fn with_hook(
        config: Config,
        fake: FakeHerdr,
        api_key: Option<String>,
        question_hook: Option<QuestionHookEnv>,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-http-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let home = dir.join("home");
        let transcripts = dir.join("transcripts");
        let codex_sessions = dir.join("codex-sessions");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&transcripts).expect("create transcripts");
        std::fs::create_dir_all(&codex_sessions).expect("create codex sessions");
        let fake = Arc::new(
            fake.with_transcript_root(&transcripts)
                .with_codex_root(&codex_sessions),
        );
        let mut config = config;
        config.defaults.cwd = home.clone();
        config.defaults.allowed_cwd_roots = vec![home.clone()];
        let env = ServiceEnv {
            home: home.display().to_string(),
            transcript_roots: vec![transcripts],
            codex_sessions_root: Some(codex_sessions),
            variant_config_homes: std::collections::BTreeMap::new(),
            api_key,
            log_requests: false,
            question_hook,
            proc_root: None,
            server_addr: None,
        };
        let service = Service::new(config, Arc::clone(&fake), env);
        let router = server::router(service)
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 45_678))));
        Self { dir, fake, router }
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Response<Body> {
        let request = build_request(method, uri, body, headers);
        self.router.clone().oneshot(request).await.expect("oneshot")
    }

    async fn chat(&self, body: Value, headers: &[(&str, &str)]) -> Response<Body> {
        self.send(Method::POST, "/v1/chat/completions", Some(body), headers)
            .await
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn build_request(
    method: Method,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if !headers.iter().any(|(name, _)| *name == "host") {
        builder = builder.header("host", "localhost:8787");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(
            body.map(|value| Body::from(value.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .expect("request")
}

/// Short timeouts so scripted outcomes resolve fast.
fn test_config() -> Config {
    let mut config = Config::builtin();
    config.herdr.startup_timeout_ms = 3_001;
    config.herdr.turn_timeout_ms = 5_000;
    config.herdr.blocked_timeout_ms = 1_000;
    config.herdr.completion_grace_ms = 300;
    config
}

async fn json_body(response: &mut Response<Body>) -> Value {
    let bytes = response
        .body_mut()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("json body")
}

async fn text_body(response: &mut Response<Body>) -> String {
    let bytes = response
        .body_mut()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn prompt_texts(fake: &FakeHerdr) -> Vec<String> {
    fake.calls()
        .into_iter()
        .filter(|c| c.method == "agent.prompt")
        .filter_map(|c| c.param("text").map(String::from))
        .collect()
}

fn start_count(fake: &FakeHerdr) -> usize {
    fake.calls()
        .iter()
        .filter(|c| c.method == "agent.start")
        .count()
}

async fn wait_for_prompt(fake: &FakeHerdr) {
    for _ in 0..400 {
        if fake.calls().iter().any(|c| c.method == "agent.prompt") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("agent.prompt was never recorded");
}

fn sse_data_events(text: &str) -> Vec<String> {
    text.split("\n\n")
        .filter_map(|block| block.trim().strip_prefix("data: ").map(String::from))
        .collect()
}

#[tokio::test]
async fn models_lists_the_builtin_ids() {
    let app = TestApp::new(test_config(), FakeHerdr::new());
    let mut response = app.send(Method::GET, "/v1/models", None, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response).await;
    assert_eq!(body["object"], "list");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("array")
        .iter()
        .map(|model| model["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ids,
        [
            "claude+agent-tools",
            "claude+client-tools",
            "codex+agent-tools",
            "codex+client-tools"
        ]
    );
    // `created` is the server start time, not a placeholder.
    assert!(body["data"][0]["created"].as_u64().unwrap_or(0) > 0);
    assert_eq!(
        body["data"][0]["description"],
        "claude: the agent runs its own tools in a herdr tab; the client gets only the final text"
    );
    assert_eq!(
        body["data"][1]["description"],
        "claude: the client runs its own tools; the agent only replies with tool_calls"
    );

    let mut single = app
        .send(Method::GET, "/v1/models/claude+agent-tools", None, &[])
        .await;
    assert_eq!(single.status(), StatusCode::OK);
    assert_eq!(json_body(&mut single).await["id"], "claude+agent-tools");
    let mut bridge = app
        .send(Method::GET, "/v1/models/claude+client-tools", None, &[])
        .await;
    assert_eq!(bridge.status(), StatusCode::OK);
    let bridge_body = json_body(&mut bridge).await;
    assert_eq!(bridge_body["id"], "claude+client-tools");
    assert_eq!(
        bridge_body["description"],
        "claude: the client runs its own tools; the agent only replies with tool_calls"
    );
    let mut missing = app.send(Method::GET, "/v1/models/nope", None, &[]).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&mut missing).await["error"]["code"],
        "model_not_found"
    );
    app.cleanup();
}

#[tokio::test]
async fn first_request_spawns_with_session_id_and_returns_reply_and_headers() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("scripted reply".to_string())]),
    );
    let mut response = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response).await;
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "scripted reply");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["model"], "claude+agent-tools");
    assert!(body["id"].as_str().expect("id").starts_with("chatcmpl-"));
    assert_eq!(body["usage"]["total_tokens"], 0);

    let headers = response.headers();
    assert_eq!(headers["x-herdr-conversation"], "c0");
    let agent = headers["x-herdr-agent"]
        .to_str()
        .expect("ascii")
        .to_string();
    let tab = headers["x-herdr-tab"].to_str().expect("ascii").to_string();
    assert!(agent.starts_with("oa-claude-"), "agent header: {agent}");
    assert!(
        tab.starts_with("oa:claude+agent-tools:"),
        "tab header: {tab}"
    );

    let calls = app.fake.calls();
    let tab_creates: Vec<_> = calls
        .iter()
        .filter(|c| c.method == "tab.create")
        .filter(|c| c.param("label").is_some_and(|l| l.starts_with("oa:")))
        .collect();
    assert_eq!(tab_creates.len(), 1, "exactly one backing tab");
    let starts: Vec<_> = calls.iter().filter(|c| c.method == "agent.start").collect();
    assert_eq!(starts.len(), 1);
    let args: Vec<&str> = starts[0]
        .param("args")
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    assert_eq!(args.first().copied(), Some("--session-id"));
    assert_eq!(args.len(), 2);
    let session: Result<uuid::Uuid, _> = args[1].parse();
    assert!(
        session.is_ok(),
        "session id must be a uuid v4: {:?}",
        args[1]
    );
    app.cleanup();
}

#[tokio::test]
async fn opencode_session_request_accepts_mock_connect_info() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("ok".to_string())]),
    );
    let mut response = app
        .chat(
            json!({
                "model": "claude+agent-tools",
                "messages": [{"role": "user", "content": "hello"}],
            }),
            &[("x-opencode-session-id", "opencode-test-session")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut response).await["choices"][0]["message"]["content"],
        "ok"
    );
    app.cleanup();
}

#[tokio::test]
async fn second_request_on_same_key_sends_only_the_new_user_text() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply("scripted reply".to_string()),
            ScriptedTurn::Reply("second reply".to_string()),
        ]),
    );
    let mut first = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = json_body(&mut first).await;
    let mut second = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "scripted reply"},
                {"role": "user", "content": "next question"}
            ]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut second).await["choices"][0]["message"]["content"],
        "second reply"
    );
    assert_eq!(start_count(&app.fake), 1, "no new spawn for a continuation");
    assert_eq!(
        prompt_texts(&app.fake),
        vec!["hello".to_string(), "next question".to_string()],
        "the second prompt is exactly the new user text"
    );
    app.cleanup();
}

#[tokio::test]
async fn codex_first_request_spawns_without_session_id_and_replies() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("codex reply".to_string())]),
    );
    let mut response = app
        .chat(
            json!({"model": "codex+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response).await;
    assert_eq!(body["choices"][0]["message"]["content"], "codex reply");
    assert_eq!(body["model"], "codex+agent-tools");
    let agent = response.headers()["x-herdr-agent"]
        .to_str()
        .expect("ascii")
        .to_string();
    assert!(agent.starts_with("oa-codex-"), "agent header: {agent}");

    let calls = app.fake.calls();
    let starts: Vec<_> = calls.iter().filter(|c| c.method == "agent.start").collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].param("kind"), Some("codex"));
    let args = starts[0].param("args").unwrap_or_default();
    assert!(
        !args.contains("--session-id"),
        "codex must not get --session-id: {args}"
    );
    app.cleanup();
}

#[tokio::test]
async fn codex_second_request_on_same_key_sends_only_the_new_user_text() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply("codex reply".to_string()),
            ScriptedTurn::Reply("codex second reply".to_string()),
        ]),
    );
    let mut first = app
        .chat(
            json!({"model": "codex+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = json_body(&mut first).await;
    let mut second = app
        .chat(
            json!({"model": "codex+agent-tools", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "codex reply"},
                {"role": "user", "content": "next question"}
            ]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut second).await["choices"][0]["message"]["content"],
        "codex second reply"
    );
    assert_eq!(start_count(&app.fake), 1, "no new spawn for a continuation");
    assert_eq!(
        prompt_texts(&app.fake),
        vec!["hello".to_string(), "next question".to_string()],
        "the second prompt is exactly the new user text"
    );
    app.cleanup();
}

#[tokio::test]
async fn two_keys_spawn_two_agents() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply("one".to_string()),
            ScriptedTurn::Reply("two".to_string()),
        ]),
    );
    for key in ["k1", "k2"] {
        let mut response = app
            .chat(
                json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
                &[("x-herdr-conversation", key)],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = json_body(&mut response).await;
    }
    assert_eq!(start_count(&app.fake), 2);
    app.cleanup();
}

#[tokio::test]
async fn identical_repeat_is_served_from_the_retry_cache() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply("scripted reply".to_string()),
            ScriptedTurn::Reply("second reply".to_string()),
        ]),
    );
    let mut first = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = json_body(&mut first).await;
    let history = json!({"model": "claude+agent-tools", "messages": [
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "scripted reply"},
        {"role": "user", "content": "next question"}
    ]});
    let mut second = app
        .chat(history.clone(), &[("x-herdr-conversation", "k1")])
        .await;
    let second_body = json_body(&mut second).await;
    let mut retry = app.chat(history, &[("x-herdr-conversation", "k1")]).await;
    let retry_body = json_body(&mut retry).await;
    assert_eq!(
        second_body["choices"][0]["message"]["content"],
        "second reply"
    );
    assert_eq!(
        retry_body["choices"], second_body["choices"],
        "retry replays the cached reply"
    );
    assert_eq!(
        prompt_texts(&app.fake),
        vec!["hello".to_string(), "next question".to_string()],
        "FakeHerdr saw the prompt once, not twice"
    );
    app.cleanup();
}

#[tokio::test]
async fn stream_request_emits_ordered_sse_events() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("streamed answer".to_string())]),
    );
    let mut response = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}],
                   "stream": true, "stream_options": {"include_usage": true}}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .starts_with("text/event-stream"));
    let text = text_body(&mut response).await;
    let events = sse_data_events(&text);
    assert_eq!(events.last().map(String::as_str), Some("[DONE]"));
    let payloads: Vec<Value> = events[..events.len() - 1]
        .iter()
        .map(|event| serde_json::from_str(event).expect("chunk json"))
        .collect();
    // 1. role chunk first.
    assert_eq!(payloads[0]["choices"][0]["delta"]["role"], "assistant");
    // 2. content piece(s).
    let contents: Vec<&str> = payloads
        .iter()
        .filter_map(|p| p["choices"][0]["delta"]["content"].as_str())
        .filter(|content| !content.is_empty())
        .collect();
    assert_eq!(contents, vec!["streamed answer"]);
    // 3. finish chunk.
    let finish = payloads
        .iter()
        .find(|p| p["choices"][0]["finish_reason"] != serde_json::Value::Null)
        .expect("finish chunk");
    assert_eq!(finish["choices"][0]["finish_reason"], "stop");
    // 4. usage chunk with empty choices.
    let usage = payloads
        .iter()
        .find(|p| p["choices"].as_array().is_some_and(Vec::is_empty))
        .expect("usage chunk");
    assert_eq!(usage["usage"]["total_tokens"], 0);
    // Order: role < first content < finish < usage.
    let index_of =
        |predicate: &dyn Fn(&Value) -> bool| payloads.iter().position(predicate).expect("event");
    let role_at = index_of(&|p| p["choices"][0]["delta"]["role"] == "assistant");
    let content_at = index_of(&|p| {
        p["choices"][0]["delta"]["content"]
            .as_str()
            .is_some_and(|c| c == "streamed answer")
    });
    let finish_at = index_of(&|p| p["choices"][0]["finish_reason"] == "stop");
    let usage_at = index_of(&|p| p["choices"].as_array().is_some_and(Vec::is_empty));
    assert!(role_at < content_at, "role chunk must come first");
    assert!(content_at < finish_at);
    assert!(finish_at < usage_at);
    app.cleanup();
}

#[tokio::test]
async fn blocked_forever_returns_an_explanatory_reply() {
    let mut config = test_config();
    config.herdr.blocked_timeout_ms = 50;
    let app = TestApp::new(
        config,
        FakeHerdr::new()
            .with_read_fails()
            .with_turns([ScriptedTurn::BlockedForever]),
    );
    let mut response = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response).await;
    let text = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("text");
    assert!(text.contains("waiting for approval"), "got {text}");
    assert!(
        text.contains("openai-api"),
        "mentions the workspace: {text}"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    app.cleanup();
}

#[tokio::test]
async fn concurrent_request_on_busy_conversation_gets_409() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
            delay_ms: 400,
            text: "slow reply".to_string(),
        }]),
    );
    let router = app.router.clone();
    let first = tokio::spawn(async move {
        router
            .oneshot(build_request(
                Method::POST,
                "/v1/chat/completions",
                Some(
                    json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
                ),
                &[("x-herdr-conversation", "k1")],
            ))
            .await
            .expect("first response")
    });
    wait_for_prompt(&app.fake).await;
    let mut second = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "user", "content": "while busy"}
            ]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(&mut second).await["error"]["code"],
        "conversation_busy"
    );
    let mut first = first.await.expect("task");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut first).await["choices"][0]["message"]["content"],
        "slow reply"
    );
    app.cleanup();
}

/// opencode's Esc abandons a streaming request. The next request on the same
/// conversation must take over instead of getting 409: it interrupts the agent
/// and waits out the turn it replaces.
#[tokio::test]
async fn client_hangup_supersedes_the_in_flight_turn() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::SlowReply {
                delay_ms: 300,
                text: "never read".to_string(),
            },
            ScriptedTurn::Reply("fresh answer".to_string()),
        ]),
    );
    let router = app.router.clone();
    let first = tokio::spawn(async move {
        router
            .oneshot(build_request(
                Method::POST,
                "/v1/chat/completions",
                Some(
                    json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}],
                           "stream": true}),
                ),
                &[("x-herdr-conversation", "k1")],
            ))
            .await
            .expect("first response")
    });
    wait_for_prompt(&app.fake).await;
    // Dropping the response drops the SSE body: what a client hangup looks like.
    drop(first.await.expect("first task"));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut second = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "user", "content": "new question"}
            ]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut second).await["choices"][0]["message"]["content"],
        "fresh answer"
    );
    let calls = app.fake.calls();
    let keys: Vec<&str> = calls
        .iter()
        .filter(|c| c.method == "agent.send_keys")
        .filter_map(|c| c.param("keys"))
        .collect();
    // Claude restores the interrupted prompt, so it also gets one ctrl+c to clear it.
    assert_eq!(
        keys,
        ["esc", "ctrl+c"],
        "the abandoned turn is interrupted once"
    );
    assert_eq!(prompt_texts(&app.fake), ["hello", "new question"]);
    app.cleanup();
}

#[tokio::test]
async fn side_request_on_existing_key_gets_409() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("reply".to_string())]),
    );
    let mut first = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = json_body(&mut first).await;
    let mut side = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [
                {"role": "user", "content": "generate a short title"}
            ]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(side.status(), StatusCode::CONFLICT);
    let body = json_body(&mut side).await;
    assert_eq!(body["error"]["code"], "conversation_side_request");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("small_model"),
        "message must hint at opencode small_model: {body}"
    );
    app.cleanup();
}

#[tokio::test]
async fn agent_exit_then_new_message_respawns_the_agent() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply("one".to_string()),
            ScriptedTurn::Exit,
            ScriptedTurn::Reply("two".to_string()),
        ]),
    );
    let mut first = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = json_body(&mut first).await;
    let mut during = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "one"},
                {"role": "user", "content": "during exit"}
            ]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(during.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        json_body(&mut during).await["error"]["code"],
        "backing_agent_exited"
    );
    let mut after = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "one"},
                {"role": "user", "content": "after exit"}
            ]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(after.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut after).await["choices"][0]["message"]["content"],
        "two"
    );
    assert_eq!(start_count(&app.fake), 2, "agent was respawned");
    app.cleanup();
}

#[tokio::test]
async fn unknown_models_get_openai_404s() {
    let app = TestApp::new(test_config(), FakeHerdr::new());
    let mut response = app
        .chat(
            json!({"model": "nope", "messages": [{"role": "user", "content": "hi"}]}),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(&mut response).await;
    assert_eq!(body["error"]["code"], "model_not_found");
    assert_eq!(body["error"]["type"], "not_found_error");
    app.cleanup();
}

#[tokio::test]
async fn unreachable_herdr_gives_503_healthz_and_chat() {
    let app = TestApp::new(test_config(), FakeHerdr::new().with_unavailable());
    let mut health = app.send(Method::GET, "/healthz", None, &[]).await;
    assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(&mut health).await["herdr"]["reachable"], false);
    let mut chat = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(chat.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_body(&mut chat).await["error"]["code"],
        "herdr_unavailable"
    );
    app.cleanup();
}

#[tokio::test]
async fn api_key_host_origin_and_cwd_guards() {
    let app = TestApp::with_api_key(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("ok".to_string())]),
        Some("secret".to_string()),
    );
    let body =
        json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hi"}]});

    let mut missing = app.chat(body.clone(), &[]).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_body(&mut missing).await["error"]["code"],
        "invalid_api_key"
    );
    let wrong = app
        .chat(body.clone(), &[("authorization", "Bearer wrong")])
        .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let mut with_origin = app
        .chat(
            body.clone(),
            &[
                ("authorization", "Bearer secret"),
                ("origin", "http://evil.example"),
            ],
        )
        .await;
    assert_eq!(with_origin.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(&mut with_origin).await["error"]["code"],
        "origin_not_allowed"
    );

    let mut bad_host = app
        .chat(
            body.clone(),
            &[("authorization", "Bearer secret"), ("host", "example.com")],
        )
        .await;
    assert_eq!(bad_host.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(&mut bad_host).await["error"]["code"],
        "origin_not_allowed"
    );

    let mut outside_cwd = app
        .chat(
            body.clone(),
            &[("authorization", "Bearer secret"), ("x-herdr-cwd", "/")],
        )
        .await;
    assert_eq!(outside_cwd.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(&mut outside_cwd).await["error"]["code"],
        "cwd_not_allowed"
    );

    let mut ok = app.chat(body, &[("authorization", "Bearer secret")]).await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut ok).await["choices"][0]["message"]["content"],
        "ok"
    );
    app.cleanup();
}

fn tab_create_cwds(fake: &FakeHerdr) -> Vec<String> {
    fake.calls()
        .iter()
        .filter(|c| c.method == "tab.create")
        .filter_map(|c| c.param("cwd").map(String::from))
        .collect()
}

fn env_system_prompt(cwd: &std::path::Path) -> String {
    format!(
        "Here is some useful information about the environment you are running in:\n\
         <env>\n  Working directory: {}\n  Is directory a git repo: yes\n</env>",
        cwd.display()
    )
}

/// No `X-Herdr-Cwd`: the `Working directory` line of the client's system prompt picks
/// the backing agent's cwd, through the same allowed-roots check.
#[tokio::test]
async fn system_prompt_working_directory_becomes_the_backing_cwd() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("ok".to_string())]),
    );
    let project = app.dir.join("home").join("projects").join("repo");
    std::fs::create_dir_all(&project).expect("mkdir");
    let body = json!({
        "model": "claude+agent-tools",
        "messages": [
            {"role": "system", "content": env_system_prompt(&project)},
            {"role": "user", "content": "hi"},
        ],
    });
    let mut response = app.chat(body, &[("x-herdr-conversation", "k1")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut response).await["choices"][0]["message"]["content"],
        "ok"
    );
    let canonical = std::fs::canonicalize(&project).expect("canonical");
    assert_eq!(
        tab_create_cwds(&app.fake)
            .last()
            .expect("tab.create")
            .as_str(),
        canonical.to_str().expect("utf-8 path")
    );
    app.cleanup();
}

#[tokio::test]
async fn cwd_header_wins_over_the_system_prompt_working_directory() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("ok".to_string())]),
    );
    let from_prompt = app.dir.join("home").join("from-prompt");
    let from_header = app.dir.join("home").join("from-header");
    std::fs::create_dir_all(&from_prompt).expect("mkdir");
    std::fs::create_dir_all(&from_header).expect("mkdir");
    let body = json!({
        "model": "claude+agent-tools",
        "messages": [
            {"role": "system", "content": env_system_prompt(&from_prompt)},
            {"role": "user", "content": "hi"},
        ],
    });
    let header = from_header.display().to_string();
    let mut response = app
        .chat(
            body,
            &[
                ("x-herdr-conversation", "k1"),
                ("x-herdr-cwd", header.as_str()),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut response).await["choices"][0]["message"]["content"],
        "ok"
    );
    let canonical = std::fs::canonicalize(&from_header).expect("canonical");
    assert_eq!(
        tab_create_cwds(&app.fake)
            .last()
            .expect("tab.create")
            .as_str(),
        canonical.to_str().expect("utf-8 path")
    );
    app.cleanup();
}

#[tokio::test]
async fn system_prompt_cwd_outside_the_allowed_roots_is_403() {
    let app = TestApp::new(test_config(), FakeHerdr::new());
    // `app.dir` is the parent of the allowed root (`<dir>/home`), so it exists but
    // sits outside it.
    let body = json!({
        "model": "claude+agent-tools",
        "messages": [
            {"role": "system", "content": env_system_prompt(&app.dir)},
            {"role": "user", "content": "hi"},
        ],
    });
    let mut response = app.chat(body, &[("x-herdr-conversation", "k1")]).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = json_body(&mut response).await;
    assert_eq!(body["error"]["code"], "cwd_not_allowed");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("the client's Working directory"),
        "the message names the source, got {body}"
    );
    assert_eq!(start_count(&app.fake), 0, "no agent was started");
    app.cleanup();
}

fn read_tool() -> Value {
    json!({"type": "function", "function": {
        "name": "read",
        "description": "Read a file",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
    }})
}

fn assert_call_id(id: &str) {
    let hex = id.strip_prefix("call_").expect("call id prefix");
    assert_eq!(hex.len(), 24, "call id must be call_ + 24 hex: {id}");
    assert!(
        hex.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "call id hex must be lowercase: {id}"
    );
}

#[tokio::test]
async fn bridge_tool_calls_then_tool_results_round_trip() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply(
                "I will read it.\n<tool_calls>[{\"name\":\"read\",\"arguments\":{\"path\":\"a.txt\"}}]\
                 </tool_calls>"
                    .to_string(),
            ),
            ScriptedTurn::Reply("The file says hello.".to_string()),
        ]),
    );
    let tools = json!([read_tool()]);
    let mut first = app
        .chat(
            json!({"model": "claude+client-tools",
                   "messages": [
                       {"role": "system", "content": "You are helpful."},
                       {"role": "user", "content": "read a.txt"}
                   ],
                   "tools": tools}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let body = json_body(&mut first).await;
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["choices"][0]["message"]["content"], "I will read it.");
    let tool_calls = body["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("tool_calls array");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["type"], "function");
    assert_eq!(tool_calls[0]["function"]["name"], "read");
    assert_eq!(
        tool_calls[0]["function"]["arguments"],
        "{\"path\":\"a.txt\"}"
    );
    let call_id = tool_calls[0]["id"].as_str().expect("call id").to_string();
    assert_call_id(&call_id);

    let prompts = prompt_texts(&app.fake);
    let first_prompt = prompts.first().expect("first prompt");
    assert!(first_prompt.contains("[herdr-openai-gateway bridge protocol v1]"));
    assert!(first_prompt.contains("<client_system>\nYou are helpful.\n</client_system>"));
    assert!(first_prompt.contains("\"name\":\"read\""));
    assert!(first_prompt.ends_with("## Current message\nread a.txt"));

    let mut second = app
        .chat(
            json!({"model": "claude+client-tools",
                   "messages": [
                       {"role": "system", "content": "You are helpful."},
                       {"role": "user", "content": "read a.txt"},
                       {"role": "assistant", "content": "I will read it.", "tool_calls": tool_calls},
                       {"role": "tool", "tool_call_id": &call_id, "name": "read",
                        "content": "file body here"}
                   ],
                   "tools": tools}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = json_body(&mut second).await;
    assert_eq!(second_body["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        second_body["choices"][0]["message"]["content"],
        "The file says hello."
    );

    let prompts = prompt_texts(&app.fake);
    let next_prompt = prompts.last().expect("next prompt");
    assert!(next_prompt.contains("<tool_results>"));
    assert!(next_prompt.contains(&format!("<result id=\"{call_id}\" name=\"read\">")));
    assert!(next_prompt.contains("file body here"));
    assert!(
        next_prompt.ends_with("(Reminder: reply with plain text or a single <tool_calls> block.)")
    );
    assert!(
        !next_prompt.contains("<client_system>"),
        "the client system prompt must not be re-sent: {next_prompt}"
    );
    assert!(!next_prompt.contains("<updated_instructions>"));
    assert!(!next_prompt.contains("<updated_tools>"));
    assert_eq!(start_count(&app.fake), 1, "no new spawn for a continuation");
    app.cleanup();
}

#[tokio::test]
async fn bridge_updated_system_and_tools_are_announced_on_continue() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply("first answer".to_string()),
            ScriptedTurn::Reply("second answer".to_string()),
        ]),
    );
    let mut first = app
        .chat(
            json!({"model": "claude+client-tools",
                   "messages": [
                       {"role": "system", "content": "old instructions"},
                       {"role": "user", "content": "hello"}
                   ],
                   "tools": [read_tool()]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = json_body(&mut first).await;
    let mut second = app
        .chat(
            json!({"model": "claude+client-tools",
                   "messages": [
                       {"role": "system", "content": "new instructions"},
                       {"role": "user", "content": "hello"},
                       {"role": "assistant", "content": "first answer"},
                       {"role": "user", "content": "next"}
                   ],
                   "tools": [read_tool(), json!({"type": "function", "function": {"name": "edit"}})]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    let _ = json_body(&mut second).await;
    let prompts = prompt_texts(&app.fake);
    let next_prompt = prompts.last().expect("next prompt");
    assert!(next_prompt
        .starts_with("<updated_instructions>\nnew instructions\n</updated_instructions>"));
    assert!(next_prompt.contains("<updated_tools>"));
    assert!(next_prompt.contains("\"name\":\"edit\""));
    assert!(next_prompt.contains("next"));
    app.cleanup();
}

#[tokio::test]
async fn bridge_invalid_block_gets_exactly_one_repair_turn() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply("Sure:\n<tool_calls>not json at all</tool_calls>".to_string()),
            ScriptedTurn::Reply(
                "<tool_calls>[{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}]</tool_calls>"
                    .to_string(),
            ),
        ]),
    );
    let mut response = app
        .chat(
            json!({"model": "claude+client-tools",
                   "messages": [{"role": "user", "content": "read a"}],
                   "tools": [read_tool()]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response).await;
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    let prompts = prompt_texts(&app.fake);
    assert_eq!(prompts.len(), 2, "exactly one repair prompt");
    assert!(prompts[1].starts_with("Your previous reply contained an invalid <tool_calls> block:"));
    assert!(prompts[1].contains("Re-send the full reply following the protocol exactly."));
    app.cleanup();
}

#[tokio::test]
async fn bridge_invalid_twice_returns_raw_text_with_stop() {
    let raw_second = "still <tool_calls>also broken</tool_calls>";
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([
            ScriptedTurn::Reply("<tool_calls>not json</tool_calls>".to_string()),
            ScriptedTurn::Reply(raw_second.to_string()),
        ]),
    );
    let mut response = app
        .chat(
            json!({"model": "claude+client-tools",
                   "messages": [{"role": "user", "content": "read a"}],
                   "tools": [read_tool()]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response).await;
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        body["choices"][0]["message"]["content"], raw_second,
        "the raw repair reply becomes the content"
    );
    assert!(body["choices"][0]["message"].get("tool_calls").is_none());
    assert_eq!(prompt_texts(&app.fake).len(), 2);
    app.cleanup();
}

#[tokio::test]
async fn bridge_stream_emits_tool_call_chunks_before_finish() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply(
            "checking\n<tool_calls>[{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}]\
             </tool_calls>"
                .to_string(),
        )]),
    );
    let mut response = app
        .chat(
            json!({"model": "claude+client-tools",
                   "messages": [{"role": "user", "content": "read a"}],
                   "tools": [read_tool()],
                   "stream": true}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = text_body(&mut response).await;
    let events = sse_data_events(&text);
    assert_eq!(events.last().map(String::as_str), Some("[DONE]"));
    let payloads: Vec<Value> = events[..events.len() - 1]
        .iter()
        .map(|event| serde_json::from_str(event).expect("chunk json"))
        .collect();
    assert_eq!(payloads[0]["choices"][0]["delta"]["role"], "assistant");
    let tool_at = payloads
        .iter()
        .position(|p| p["choices"][0]["delta"]["tool_calls"] != serde_json::Value::Null)
        .expect("tool_calls chunk");
    let call = &payloads[tool_at]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(call["index"], 0);
    assert_eq!(call["type"], "function");
    assert_call_id(call["id"].as_str().expect("id"));
    assert_eq!(call["function"]["name"], "read");
    assert_eq!(call["function"]["arguments"], "{\"path\":\"a\"}");
    let finish_at = payloads
        .iter()
        .position(|p| p["choices"][0]["finish_reason"] == "tool_calls")
        .expect("finish chunk");
    assert!(
        tool_at < finish_at,
        "tool_call chunks precede the finish chunk"
    );
    app.cleanup();
}

#[tokio::test]
async fn codex_bridge_model_returns_tool_calls() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply(
            "<tool_calls>[{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}]</tool_calls>"
                .to_string(),
        )]),
    );
    let mut response = app
        .chat(
            json!({"model": "codex+client-tools",
                   "messages": [{"role": "user", "content": "read a"}],
                   "tools": [read_tool()]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response).await;
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    let agent = response.headers()["x-herdr-agent"]
        .to_str()
        .expect("ascii")
        .to_string();
    assert!(agent.starts_with("oa-codex"), "agent header: {agent}");
    app.cleanup();
}

#[tokio::test]
async fn self_exec_model_still_ignores_client_tools() {
    let app = TestApp::new(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::Reply("plain answer".to_string())]),
    );
    let mut response = app
        .chat(
            json!({"model": "claude+agent-tools",
                   "messages": [{"role": "user", "content": "read a"}],
                   "tools": [read_tool()]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response).await;
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["choices"][0]["message"]["content"], "plain answer");
    assert_eq!(
        prompt_texts(&app.fake),
        vec!["read a".to_string()],
        "self-exec prompts carry no bridge protocol"
    );
    app.cleanup();
}

/// A discovered variant model is launched by typing its command into the tab
/// shell (`pane.send_input`), never through `agent.start`; the base `claude`
/// model keeps the managed `agent.start` path.
#[tokio::test]
async fn variant_model_launches_by_typing_command_base_stays_managed() {
    let mut config = test_config();
    // Unmanaged agents need herdr's 3 s settle before the rename.
    config.herdr.startup_timeout_ms = 10_000;
    let variants = vec![herdr_openai_gateway::discovery::Variant {
        name: "claude-personal".to_string(),
        kind: herdr_openai_gateway::config::AgentKind::Claude,
        config_home: None,
    }];
    config.models = config.effective_models(&variants);
    let app = TestApp::new(
        config,
        FakeHerdr::new()
            .with_variant_command("claude-personal", "claude")
            .with_turns([
                ScriptedTurn::Reply("variant reply".to_string()),
                ScriptedTurn::Reply("base reply".to_string()),
            ]),
    );

    let mut response = app
        .chat(
            json!({"model": "claude-personal+agent-tools",
                   "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k1")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut response).await["choices"][0]["message"]["content"],
        "variant reply"
    );

    let calls = app.fake.calls();
    assert!(
        !calls.iter().any(|c| c.method == "agent.start"),
        "the variant must not use agent.start"
    );
    let sends: Vec<_> = calls
        .iter()
        .filter(|c| c.method == "pane.send_input")
        .collect();
    assert_eq!(sends.len(), 1, "exactly one typed command");
    let text = sends[0].param("text").unwrap_or_default();
    assert!(
        text.starts_with("claude-personal --session-id "),
        "got {text}"
    );
    let session: Result<uuid::Uuid, _> = text.split_whitespace().last().unwrap_or_default().parse();
    assert!(session.is_ok(), "typed session id must be a uuid: {text}");
    assert_eq!(sends[0].param("keys"), Some("Enter"));
    let renames: Vec<_> = calls
        .iter()
        .filter(|c| c.method == "agent.rename")
        .collect();
    assert_eq!(renames.len(), 1, "the detected agent gets our name");
    let agent = response.headers()["x-herdr-agent"]
        .to_str()
        .expect("ascii")
        .to_string();
    assert!(
        agent.starts_with("oa-claude-personal-"),
        "agent header: {agent}"
    );

    // The base `claude` model still goes through the managed `agent.start`.
    let mut base = app
        .chat(
            json!({"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}),
            &[("x-herdr-conversation", "k2")],
        )
        .await;
    assert_eq!(base.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut base).await["choices"][0]["message"]["content"],
        "base reply"
    );
    assert_eq!(start_count(&app.fake), 1, "base claude uses agent.start");
    app.cleanup();
}

#[tokio::test]
async fn claude_question_hook_relays_and_returns_answers() {
    let app = TestApp::with_question_hook(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
            delay_ms: 500,
            text: "I have the answer".to_string(),
        }]),
    );
    let chat_router = app.router.clone();
    let first_request = build_request(
        Method::POST,
        "/v1/chat/completions",
        Some(json!({
            "model": "claude+agent-tools",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "question",
                    "parameters": {"type": "object", "properties": {"questions": {"type": "array"}}}
                }
            }]
        })),
        &[("x-herdr-conversation", "k-question")],
    );
    let first_task = tokio::spawn(async move {
        chat_router
            .oneshot(first_request)
            .await
            .expect("first chat")
    });
    for _ in 0..200 {
        if app
            .fake
            .calls()
            .iter()
            .any(|call| call.method == "agent.prompt")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let start = app
        .fake
        .calls()
        .into_iter()
        .find(|call| call.method == "agent.start")
        .expect("agent.start");
    let session_id = start
        .param("args")
        .and_then(|args| args.split_once("--session-id "))
        .map(|(_, id)| id.split_whitespace().next().unwrap_or_default())
        .expect("session id")
        .to_string();
    let hook_router = app.router.clone();
    let hook_request = build_request(
        Method::POST,
        "/internal/claude-hooks/pre-tool-use",
        Some(json!({
            "session_id": session_id,
            "tool_name": "AskUserQuestion",
            "tool_use_id": "toolu_123",
            "tool_input": {
                "questions": [{
                    "question": "Which environments?",
                    "header": "Environment",
                    "options": [
                        {"label": "Development", "description": "Local setup"},
                        {"label": "Production", "description": "Live service"}
                    ],
                    "multiSelect": true
                }]
            }
        })),
        &[
            ("content-type", "application/json"),
            ("x-herdr-hook-token", "test-hook-token"),
        ],
    );
    let hook_task = tokio::spawn(async move {
        hook_router
            .oneshot(hook_request)
            .await
            .expect("hook request")
    });

    let mut first_response = first_task.await.expect("first task");
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = json_body(&mut first_response).await;
    let question_call = &first_body["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(question_call["function"]["name"], "question");
    let arguments: Value = serde_json::from_str(
        question_call["function"]["arguments"]
            .as_str()
            .expect("arguments"),
    )
    .expect("question arguments");
    assert_eq!(arguments["questions"][0]["multiple"], true);
    assert_eq!(
        arguments["questions"][0]["options"][0]["description"],
        "Local setup"
    );

    let call_id = question_call["id"].as_str().expect("call id");
    let mut answer_response = app
        .chat(
            json!({
                "model": "claude+agent-tools",
                "messages": [
                    {"role": "user", "content": "hello"},
                    first_body["choices"][0]["message"].clone(),
                    {
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": "User has answered your questions: \"Which environments?\"=\"Development, Production\". You can now continue with the user's answers in mind."
                    }
                ]
            }),
            &[("x-herdr-conversation", "k-question")],
        )
        .await;
    assert_eq!(answer_response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut answer_response).await["choices"][0]["message"]["content"],
        "I have the answer"
    );
    let mut hook_response = hook_task.await.expect("hook task");
    assert_eq!(hook_response.status(), StatusCode::OK);
    let hook_body = json_body(&mut hook_response).await;
    assert_eq!(
        hook_body["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert_eq!(
        hook_body["hookSpecificOutput"]["updatedInput"]["answers"]["Which environments?"],
        "Development, Production"
    );
    assert!(
        !app.fake
            .calls()
            .iter()
            .any(|call| call.method == "agent.send_keys"),
        "the hook answer must not press TUI keys"
    );
    app.cleanup();
}

#[tokio::test]
async fn claude_hook_relay_keeps_all_questions_after_unmatched_text() {
    let app = TestApp::with_question_hook(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
            delay_ms: 1_000,
            text: "answered both questions".to_string(),
        }]),
    );
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "question",
            "parameters": {"type": "object", "properties": {"questions": {"type": "array"}}}
        }
    }]);
    let chat_router = app.router.clone();
    let first_request = build_request(
        Method::POST,
        "/v1/chat/completions",
        Some(json!({
            "model": "claude+agent-tools",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": tools
        })),
        &[("x-herdr-conversation", "k-hook-re-relay")],
    );
    let first_task = tokio::spawn(async move {
        chat_router
            .oneshot(first_request)
            .await
            .expect("first chat")
    });
    wait_for_prompt(&app.fake).await;
    let start = app
        .fake
        .calls()
        .into_iter()
        .find(|call| call.method == "agent.start")
        .expect("agent.start");
    let session_id = start
        .param("args")
        .and_then(|args| args.split_once("--session-id "))
        .map(|(_, id)| id.split_whitespace().next().unwrap_or_default())
        .expect("session id")
        .to_string();
    let hook_router = app.router.clone();
    let hook_request = build_request(
        Method::POST,
        "/internal/claude-hooks/pre-tool-use",
        Some(json!({
            "session_id": session_id,
            "tool_name": "AskUserQuestion",
            "tool_use_id": "toolu_multi",
            "tool_input": {
                "questions": [
                    {
                        "question": "Q1",
                        "header": "First",
                        "options": [{"label": "A", "description": "First answer"}],
                        "multiSelect": false
                    },
                    {
                        "question": "Q2",
                        "header": "Second",
                        "options": [{"label": "B", "description": "Second answer"}],
                        "multiSelect": false
                    }
                ]
            }
        })),
        &[
            ("content-type", "application/json"),
            ("x-herdr-hook-token", "test-hook-token"),
        ],
    );
    let hook_task = tokio::spawn(async move {
        hook_router
            .oneshot(hook_request)
            .await
            .expect("hook request")
    });
    let mut first_response = first_task.await.expect("first task");
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = json_body(&mut first_response).await;
    let first_message = first_body["choices"][0]["message"].clone();

    let mut relayed = app
        .chat(
            json!({
                "model": "claude+agent-tools",
                "messages": [
                    {"role": "user", "content": "hello"},
                    first_message.clone(),
                    {"role": "user", "content": "I need to check both first."}
                ],
                "tools": tools
            }),
            &[("x-herdr-conversation", "k-hook-re-relay")],
        )
        .await;
    assert_eq!(relayed.status(), StatusCode::OK);
    let relayed_body = json_body(&mut relayed).await;
    let relayed_message = relayed_body["choices"][0]["message"].clone();
    let relayed_call = &relayed_message["tool_calls"][0];
    let relayed_call_id = relayed_call["id"].as_str().expect("relayed call id");
    let arguments: Value = serde_json::from_str(
        relayed_call["function"]["arguments"]
            .as_str()
            .expect("relayed arguments"),
    )
    .expect("question arguments");
    assert_eq!(arguments["questions"].as_array().unwrap().len(), 2);
    assert_eq!(
        arguments["questions"][0]["options"][0]["description"],
        "First answer"
    );
    assert_eq!(
        arguments["questions"][1]["options"][0]["description"],
        "Second answer"
    );

    let mut answer = app
        .chat(
            json!({
                "model": "claude+agent-tools",
                "messages": [
                    {"role": "user", "content": "hello"},
                    first_message,
                    {"role": "user", "content": "I need to check both first."},
                    relayed_message,
                    {
                        "role": "tool",
                        "tool_call_id": relayed_call_id,
                        "content": "User has answered your questions: \"Q1\"=\"A\", \"Q2\"=\"B\". You can now continue with the user's answers in mind."
                    }
                ],
                "tools": tools
            }),
            &[("x-herdr-conversation", "k-hook-re-relay")],
        )
        .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut answer).await["choices"][0]["message"]["content"],
        "answered both questions"
    );
    let mut hook_response = hook_task.await.expect("hook task");
    assert_eq!(hook_response.status(), StatusCode::OK);
    let hook_body = json_body(&mut hook_response).await;
    assert_eq!(
        hook_body["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert_eq!(
        hook_body["hookSpecificOutput"]["updatedInput"]["answers"]["Q1"],
        "A"
    );
    assert_eq!(
        hook_body["hookSpecificOutput"]["updatedInput"]["answers"]["Q2"],
        "B"
    );
    app.cleanup();
}

#[tokio::test]
async fn claude_question_hook_rejects_bad_tokens_and_ignores_unknown_sessions() {
    let app = TestApp::with_question_hook(test_config(), FakeHerdr::new());
    let payload = json!({
        "session_id": "unknown-session",
        "tool_name": "AskUserQuestion",
        "tool_use_id": "toolu_123",
        "tool_input": {"questions": []}
    });
    let unauthorized = app
        .send(
            Method::POST,
            "/internal/claude-hooks/pre-tool-use",
            Some(payload.clone()),
            &[
                ("content-type", "application/json"),
                ("x-herdr-hook-token", "wrong-token"),
            ],
        )
        .await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let mut unknown = app
        .send(
            Method::POST,
            "/internal/claude-hooks/pre-tool-use",
            Some(payload),
            &[
                ("content-type", "application/json"),
                ("x-herdr-hook-token", "test-hook-token"),
            ],
        )
        .await;
    assert_eq!(unknown.status(), StatusCode::OK);
    assert!(text_body(&mut unknown).await.is_empty());
    app.cleanup();
}

#[tokio::test]
async fn claude_question_hook_falls_back_when_question_tool_was_not_offered() {
    let app = TestApp::with_question_hook(
        test_config(),
        FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
            delay_ms: 400,
            text: "finished".to_string(),
        }]),
    );
    let chat_router = app.router.clone();
    let chat_request = build_request(
        Method::POST,
        "/v1/chat/completions",
        Some(json!({
            "model": "claude+agent-tools",
            "messages": [{"role": "user", "content": "hello"}]
        })),
        &[],
    );
    let chat_task = tokio::spawn(async move {
        chat_router
            .oneshot(chat_request)
            .await
            .expect("chat request")
    });
    wait_for_prompt(&app.fake).await;
    let start = app
        .fake
        .calls()
        .into_iter()
        .find(|call| call.method == "agent.start")
        .expect("agent.start");
    let session_id = start
        .param("args")
        .and_then(|args| args.split_once("--session-id "))
        .map(|(_, id)| id.split_whitespace().next().unwrap_or_default())
        .expect("session id");
    let mut hook = app
        .send(
            Method::POST,
            "/internal/claude-hooks/pre-tool-use",
            Some(json!({
                "session_id": session_id,
                "tool_name": "AskUserQuestion",
                "tool_use_id": "toolu_456",
                "tool_input": {"questions": [{"question": "Continue?"}]}
            })),
            &[
                ("content-type", "application/json"),
                ("x-herdr-hook-token", "test-hook-token"),
            ],
        )
        .await;
    assert_eq!(hook.status(), StatusCode::OK);
    assert!(text_body(&mut hook).await.is_empty());
    let response = chat_task.await.expect("chat task");
    assert_eq!(response.status(), StatusCode::OK);
    app.cleanup();
}

#[tokio::test]
async fn claude_question_hook_is_denied_when_its_turn_finishes() {
    let mut config = test_config();
    config.herdr.blocked_timeout_ms = 3_000;
    let app = TestApp::with_question_hook(
        config,
        FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
            delay_ms: 400,
            text: "finished without the question".to_string(),
        }]),
    );
    let chat_router = app.router.clone();
    let chat_request = build_request(
        Method::POST,
        "/v1/chat/completions",
        Some(json!({
            "model": "claude+agent-tools",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "question",
                    "parameters": {"type": "object", "properties": {"questions": {"type": "array"}}}
                }
            }]
        })),
        &[],
    );
    let chat_task = tokio::spawn(async move {
        chat_router
            .oneshot(chat_request)
            .await
            .expect("chat request")
    });
    wait_for_prompt(&app.fake).await;
    let start = app
        .fake
        .calls()
        .into_iter()
        .find(|call| call.method == "agent.start")
        .expect("agent.start");
    let session_id = start
        .param("args")
        .and_then(|args| args.split_once("--session-id "))
        .map(|(_, id)| id.split_whitespace().next().unwrap_or_default())
        .expect("session id")
        .to_string();
    let hook_router = app.router.clone();
    let hook_request = build_request(
        Method::POST,
        "/internal/claude-hooks/pre-tool-use",
        Some(json!({
            "session_id": session_id,
            "tool_name": "AskUserQuestion",
            "tool_use_id": "toolu_pending",
            "tool_input": {"questions": [{"question": "Continue?", "header": "Continue"}]}
        })),
        &[
            ("content-type", "application/json"),
            ("x-herdr-hook-token", "test-hook-token"),
        ],
    );
    let hook_task = tokio::spawn(async move {
        hook_router
            .oneshot(hook_request)
            .await
            .expect("hook request")
    });
    let mut chat_response = chat_task.await.expect("chat task");
    assert_eq!(chat_response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut chat_response).await["choices"][0]["message"]["tool_calls"][0]["function"]
            ["name"],
        "question"
    );
    let mut hook_response = tokio::time::timeout(Duration::from_secs(1), hook_task)
        .await
        .expect("hook should wake before blocked_timeout_ms")
        .expect("hook task");
    assert_eq!(hook_response.status(), StatusCode::OK);
    let hook_body = json_body(&mut hook_response).await;
    assert_eq!(
        hook_body["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    assert_eq!(
        hook_body["hookSpecificOutput"]["permissionDecisionReason"],
        "No answer from the user in time"
    );
    app.cleanup();
}
