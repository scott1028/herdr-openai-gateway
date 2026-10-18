//! TDD guardrails for Batch 4: conversations survive a gateway restart.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use herdr_openai_gateway::config::Config;
use herdr_openai_gateway::herdr::fake::{FakeHerdr, ScriptedTurn};
use herdr_openai_gateway::server;
use herdr_openai_gateway::service::{QuestionHookEnv, Service, ServiceEnv};
use herdr_openai_gateway::state::{SavedState, StateFile};

const MODEL: &str = "claude+agent-tools";
const THREAD: &str = "thread-1";

/// One temp dir shared by every gateway "run" of a test, like one machine.
struct Machine {
    dir: PathBuf,
    fake: Arc<FakeHerdr>,
}

impl Machine {
    fn new(turns: impl IntoIterator<Item = ScriptedTurn>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-restart-tdd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        for sub in ["home", "transcripts", "codex-sessions"] {
            std::fs::create_dir_all(dir.join(sub)).expect("create temp dir");
        }
        let fake = Arc::new(
            FakeHerdr::new()
                .with_turns(turns)
                .with_transcript_root(dir.join("transcripts"))
                .with_codex_root(dir.join("codex-sessions")),
        );
        Self { dir, fake }
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state").join("state.json")
    }

    /// Start one gateway run on this machine, the way `serve` does.
    fn start(&self) -> Gateway {
        let state = StateFile::open(&self.state_path()).expect("open state file");
        let saved = state.load();
        let hook_token = saved
            .as_ref()
            .and_then(|saved| saved.hook_token.clone())
            .unwrap_or_else(|| "hook-token-1".to_string());
        let home = self.dir.join("home");
        let mut config = Config::builtin();
        config.herdr.startup_timeout_ms = 3_001;
        config.herdr.turn_timeout_ms = 5_000;
        config.herdr.blocked_timeout_ms = 1_000;
        config.herdr.completion_grace_ms = 300;
        config.defaults.cwd = home.clone();
        config.defaults.allowed_cwd_roots = vec![home.clone()];
        let env = ServiceEnv {
            home: home.display().to_string(),
            transcript_roots: vec![self.dir.join("transcripts")],
            codex_sessions_root: Some(self.dir.join("codex-sessions")),
            variant_config_homes: std::collections::BTreeMap::new(),
            api_key: None,
            log_requests: false,
            question_hook: Some(QuestionHookEnv {
                url: "http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use".to_string(),
                token: hook_token,
            }),
            proc_root: None,
            server_addr: None,
        };
        let service = Service::with_state(config, Arc::clone(&self.fake), env, state, saved);
        let router = server::router(Arc::clone(&service))
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 45_678))));
        Gateway { service, router }
    }

    fn saved(&self) -> SavedState {
        read_state(&self.state_path())
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Gateway {
    service: Arc<Service<FakeHerdr>>,
    router: Router,
}

impl Gateway {
    async fn chat(&self, messages: Value) -> (StatusCode, Value) {
        let body = json!({"model": MODEL, "messages": messages});
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("host", "localhost:8787")
            .header("content-type", "application/json")
            .header("x-session-id", THREAD)
            .body(Body::from(body.to_string()))
            .expect("request");
        let response = self.router.clone().oneshot(request).await.expect("oneshot");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn read_state(path: &Path) -> SavedState {
    let text = std::fs::read_to_string(path).expect("state file exists");
    serde_json::from_str(&text).expect("state file parses")
}

fn reply_text(body: &Value) -> &str {
    body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
}

#[tokio::test]
async fn a_finished_turn_is_saved_with_its_thread_and_tab() {
    let machine = Machine::new([ScriptedTurn::Reply("pong".to_string())]);
    let gateway = machine.start();

    let (status, body) = gateway
        .chat(json!([{"role": "user", "content": "ping"}]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(reply_text(&body), "pong");

    let saved = machine.saved();
    assert!(!saved.owner_id.is_empty());
    assert_eq!(saved.hook_token.as_deref(), Some("hook-token-1"));
    assert_eq!(saved.conversations.len(), 1, "{saved:?}");
    let conversation = &saved.conversations[0];
    assert_eq!(conversation.key.as_deref(), Some(THREAD));
    assert_eq!(conversation.model_id, MODEL);
    assert_eq!(conversation.cwd, machine.dir.join("home"));
    // The user message plus our reply.
    assert_eq!(conversation.consumed, 2);
    assert_eq!(conversation.user_digests.len(), 1);
    let agent = conversation.agent.as_ref().expect("the agent is saved");
    assert!(
        agent.tab_label.starts_with(&format!("oa:{MODEL}:")),
        "{agent:?}"
    );
    assert!(agent.session_id.is_some(), "{agent:?}");
}

#[test]
fn the_state_file_defaults_to_the_xdg_state_dir() {
    use herdr_openai_gateway::config::default_state_path;
    let path = default_state_path(|key| match key {
        "XDG_STATE_HOME" => Some("/xdg-state".to_string()),
        "HOME" => Some("/home/u".to_string()),
        _ => None,
    });
    assert_eq!(
        path,
        PathBuf::from("/xdg-state/herdr-openai-gateway/state.json")
    );
    let path = default_state_path(|key| match key {
        "HOME" => Some("/home/u".to_string()),
        _ => None,
    });
    assert_eq!(
        path,
        PathBuf::from("/home/u/.local/state/herdr-openai-gateway/state.json")
    );
}

#[test]
fn a_second_gateway_cannot_share_the_state_file() {
    let machine = Machine::new([]);
    let _first = StateFile::open(&machine.state_path()).expect("first open");
    let second = StateFile::open(&machine.state_path());
    assert!(second.is_err(), "the lock must refuse a second gateway");
}

fn agent_starts(fake: &FakeHerdr) -> usize {
    fake.calls()
        .iter()
        .filter(|call| call.method == "agent.start")
        .count()
}

fn last_prompt(fake: &FakeHerdr) -> String {
    fake.calls()
        .iter()
        .rev()
        .find(|call| call.method == "agent.prompt")
        .and_then(|call| call.param("text").map(str::to_string))
        .unwrap_or_default()
}

fn second_turn() -> Value {
    json!([
        {"role": "user", "content": "first question"},
        {"role": "assistant", "content": "first answer"},
        {"role": "user", "content": "second question"}
    ])
}

/// Run one turn on a fresh gateway, stop it gracefully, and return the saved tab.
async fn first_run(machine: &Machine) -> herdr_openai_gateway::state::SavedAgent {
    let gateway = machine.start();
    let (status, body) = gateway
        .chat(json!([{"role": "user", "content": "first question"}]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    gateway.service.shutdown().await;
    machine.saved().conversations[0]
        .agent
        .clone()
        .expect("the agent is saved")
}

#[tokio::test]
async fn a_restarted_gateway_keeps_talking_to_the_same_tab() {
    let machine = Machine::new([
        ScriptedTurn::Reply("first answer".to_string()),
        ScriptedTurn::Reply("second answer".to_string()),
    ]);
    let saved_agent = first_run(&machine).await;
    // A graceful stop leaves the tab for the next run.
    assert!(
        machine
            .fake
            .tab_labels(&saved_agent.workspace_id)
            .contains(&saved_agent.tab_label),
        "the tab must survive the shutdown"
    );

    let gateway = machine.start();
    gateway.service.startup_sweep().await;
    let (status, body) = gateway.chat(second_turn()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(reply_text(&body), "second answer");
    assert_eq!(agent_starts(&machine.fake), 1, "no second CLI is started");
    let prompt = last_prompt(&machine.fake);
    assert!(prompt.contains("second question"), "{prompt}");
    assert!(
        !prompt.contains("first question"),
        "only the delta is sent: {prompt}"
    );
}

#[tokio::test]
async fn a_tab_that_lost_its_tokens_in_a_herdr_restart_is_still_adopted() {
    let machine = Machine::new([
        ScriptedTurn::Reply("first answer".to_string()),
        ScriptedTurn::Reply("second answer".to_string()),
    ]);
    let saved_agent = first_run(&machine).await;
    // herdr restores tabs and labels but never pane tokens.
    machine.fake.clear_pane_tokens(&saved_agent.pane_id);

    let gateway = machine.start();
    gateway.service.startup_sweep().await;
    assert!(
        machine
            .fake
            .tab_labels(&saved_agent.workspace_id)
            .contains(&saved_agent.tab_label),
        "the startup sweep must not close an adopted tab"
    );
    let (status, body) = gateway.chat(second_turn()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(agent_starts(&machine.fake), 1, "no second CLI is started");
}

#[tokio::test]
async fn leftover_tabs_the_state_does_not_name_are_still_swept() {
    let machine = Machine::new([ScriptedTurn::Reply("first answer".to_string())]);
    let saved_agent = first_run(&machine).await;
    machine.fake.clear_pane_tokens(&saved_agent.pane_id);
    std::fs::remove_file(machine.state_path()).expect("drop the state file");

    let gateway = machine.start();
    gateway.service.startup_sweep().await;
    assert!(
        !machine
            .fake
            .tab_labels(&saved_agent.workspace_id)
            .contains(&saved_agent.tab_label),
        "an untagged tab nobody remembers is an orphan"
    );
}

#[tokio::test]
async fn a_saved_tab_nobody_takes_over_is_closed() {
    let machine = Machine::new([ScriptedTurn::Reply("first answer".to_string())]);
    let saved_agent = first_run(&machine).await;
    // The model was dropped from the config between the runs.
    let mut saved = machine.saved();
    saved.conversations[0].model_id = "retired+agent-tools".to_string();
    std::fs::write(
        machine.state_path(),
        serde_json::to_string(&saved).expect("serialize"),
    )
    .expect("rewrite the state file");

    let gateway = machine.start();
    gateway.service.startup_sweep().await;
    assert!(
        !machine
            .fake
            .tab_labels(&saved_agent.workspace_id)
            .contains(&saved_agent.tab_label),
        "a tab still carrying our token must not outlive its conversation"
    );
}

#[tokio::test]
async fn a_conversation_whose_tab_is_gone_resumes_its_session_in_a_new_tab() {
    use herdr_openai_gateway::herdr::HerdrApi as _;
    let machine = Machine::new([
        ScriptedTurn::Reply("first answer".to_string()),
        ScriptedTurn::Reply("second answer".to_string()),
    ]);
    let saved_agent = first_run(&machine).await;
    let session = saved_agent.session_id.clone().expect("claude session id");
    // Someone closed the tab while the gateway was down.
    machine
        .fake
        .tab_close(&saved_agent.tab_id)
        .await
        .expect("close the tab");

    let gateway = machine.start();
    gateway.service.startup_sweep().await;
    let (status, body) = gateway.chat(second_turn()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(reply_text(&body), "second answer");
    let typed: Vec<String> = machine
        .fake
        .calls()
        .iter()
        .filter(|call| call.method == "pane.send_input")
        .filter_map(|call| call.param("text").map(str::to_string))
        .collect();
    assert!(
        typed.iter().any(
            |line| line.starts_with("claude ") && line.contains(&format!("--resume {session}"))
        ),
        "the new tab resumes the saved session: {typed:?}"
    );
    assert_eq!(agent_starts(&machine.fake), 1, "no fresh CLI is started");
    let prompt = last_prompt(&machine.fake);
    assert!(prompt.contains("second question"), "{prompt}");
    assert!(
        !prompt.contains("first question"),
        "only the delta is sent: {prompt}"
    );
}

#[tokio::test]
async fn a_conversation_resumes_even_when_its_whole_workspace_is_gone() {
    use herdr_openai_gateway::herdr::HerdrApi as _;
    let machine = Machine::new([
        ScriptedTurn::Reply("first answer".to_string()),
        ScriptedTurn::Reply("second answer".to_string()),
    ]);
    let saved_agent = first_run(&machine).await;
    let session = saved_agent.session_id.clone().expect("claude session id");
    // Closing every tab closes the workspace, like a herdr reset would.
    for tab in machine
        .fake
        .tab_list(&saved_agent.workspace_id)
        .await
        .expect("list tabs")
    {
        machine
            .fake
            .tab_close(&tab.tab_id)
            .await
            .expect("close tab");
    }

    let gateway = machine.start();
    gateway.service.startup_sweep().await;
    let (status, body) = gateway.chat(second_turn()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let resumed = machine.fake.calls().iter().any(|call| {
        call.method == "pane.send_input"
            && call
                .param("text")
                .is_some_and(|line| line.contains(&format!("--resume {session}")))
    });
    assert!(resumed, "the saved session is resumed in a new workspace");
    let prompt = last_prompt(&machine.fake);
    assert!(
        !prompt.contains("first question"),
        "only the delta is sent: {prompt}"
    );
}
