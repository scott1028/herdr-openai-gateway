//! Coordination layer: classify -> spawn/turn -> commit, in-flight join, caps, reaper.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use crate::agents::lifecycle::{self, AgentHandle, LaunchExtras, SpawnError};
use crate::agents::turn::{self, BlockedHook, TranscriptRoots, TurnLimits, TurnOutcome};
use crate::client_process::{self, ClientProcess};
use crate::config::{AgentKind, Config, ModelMode, ModelSpec};
use crate::conversation::{
    self, Classification, ConversationId, HookQuestion, InflightSnapshot, PendingDialog, Registry,
};
use crate::dialog::{self, Dialog};
use crate::herdr::{AgentInfo, AgentStatus, HerdrApi, HerdrError, ReadSource, WaitSpec};
use crate::openai::{
    ApiError, AssistantReply, FinishReason, FunctionCall, NormalizedMessage, NormalizedRequest,
    ToolCall,
};
use crate::prompt;
use crate::prompt::bridge::{self, BridgeChanges};
use crate::question_hook::{self, HookAnswer, HookInput};
use crate::toolcall;
use crate::transcript::{codex, StopKind};

/// Result shared between the original requester and any joiners.
type TurnResult = Result<ChatOutcome, ApiError>;

/// What an in-flight turn has published on its watch channel so far. A dialog
/// relay returns to waiting clients immediately; the turn keeps running.
#[derive(Debug, Clone)]
enum TurnState {
    Running,
    /// A dialog relayed to the client; `seq` orders relays per conversation.
    Dialog {
        reply: AssistantReply,
        seq: u64,
    },
    Done(TurnResult),
}

/// Serializes turns on one conversation: a superseding turn waits here until the
/// turn it replaced has finished touching the agent.
type TurnLock = tokio::sync::Mutex<()>;

/// Tracks hook wait time excluded from the turn timeout.
#[derive(Default)]
struct HookPause(Mutex<HookPauseState>);

#[derive(Default)]
struct HookPauseState {
    pending_since: Option<Instant>,
    accumulated: Duration,
}

impl HookPause {
    fn start(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.pending_since = Some(Instant::now());
    }

    fn finish(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let pending_since = state.pending_since.take();
        if let Some(pending_since) = pending_since {
            state.accumulated += pending_since.elapsed();
        }
    }

    fn is_active(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_since
            .is_some()
    }

    fn paused(&self) -> Duration {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.accumulated
            + state
                .pending_since
                .map(|started| started.elapsed())
                .unwrap_or(Duration::ZERO)
    }
}

/// Dialog answer key timing: the agent TUI re-renders slowly, so every key
/// press is confirmed on screen before the next one is sent.
const DIALOG_SETTLE_GAP: Duration = Duration::from_millis(150);
const DIALOG_SETTLE_TIMEOUT: Duration = Duration::from_millis(2_000);
const DIALOG_MOVE_POLL: Duration = Duration::from_millis(100);
const DIALOG_MOVE_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Outcome of waiting for the dialog screen to settle before any key is sent.
enum Settle {
    Settled(Dialog),
    Unreadable,
    Unsettled,
}

/// Outcome of polling one screen update after a single cursor key.
enum CursorMove {
    Moved(Dialog),
    Unreadable,
    Changed,
    TimedOut,
}

/// Outcome of the two-read confirmation that the cursor rests on the answer.
enum CursorConfirm {
    Confirmed,
    Unreadable,
    Changed,
    Unconfirmed,
}

/// Poll cadence while waiting for a turn to finish (config carries the timeouts).
const TURN_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Poll cadence while waiting for the client's answer to a startup dialog.
const STARTUP_DIALOG_POLL: Duration = Duration::from_millis(200);

/// How long the agent must be missing from its pane to count as exited.
const AGENT_EXIT_CONFIRM: Duration = Duration::from_secs(3);

/// How long the agent must stay `idle`/`done` after a dialog was answered
/// before it counts as ready; matches herdr's `AGENT_START_SETTLE_DELAY`.
const POST_DIALOG_SETTLE: Duration = Duration::from_secs(3);

/// How long a superseding turn waits for the agent to come back idle after Esc.
const ABORT_WAIT_MS: u64 = 10_000;
const CLAUDE_DISABLE_AGENT_VIEW_ENV: &str = "CLAUDE_CODE_DISABLE_AGENT_VIEW";

/// A `New` request that carries a conversation key but no tools is opencode's
/// title-generation shape; it waits this long before reserving so the
/// tool-carrying main request (arriving milliseconds later) wins the race.
const SIDE_REQUEST_COALESCE: Duration = Duration::from_millis(500);

/// Injectable environment so tests never touch the process environment.
#[derive(Debug, Clone)]
pub struct ServiceEnv {
    pub home: String,
    pub transcript_roots: Vec<PathBuf>,
    /// Overrides the Codex sessions root (tests); production computes it per model.
    pub codex_sessions_root: Option<PathBuf>,
    /// Config dir per model id for discovered variants, used to locate their
    /// transcripts (Claude config dir / Codex home).
    pub variant_config_homes: std::collections::BTreeMap<String, PathBuf>,
    /// Value of the configured API key env var, resolved once at startup.
    pub api_key: Option<String>,
    pub log_requests: bool,
    /// Claude question hook endpoint and per-process authentication token.
    pub question_hook: Option<QuestionHookEnv>,
    /// Linux procfs root used to identify loopback OpenCode clients.
    pub proc_root: Option<PathBuf>,
    /// Actual listener address used to match the client's TCP socket.
    pub server_addr: Option<SocketAddr>,
}

/// Settings used to configure Claude's PreToolUse HTTP hook.
#[derive(Debug, Clone)]
pub struct QuestionHookEnv {
    /// Loopback URL of the hook endpoint.
    pub url: String,
    /// Random token accepted by the endpoint.
    pub token: String,
}

/// A completed turn plus the herdr placement the client can use for debugging.
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub reply: AssistantReply,
    pub conversation: ConversationId,
    pub agent_name: String,
    pub tab_label: String,
}

/// What the detached turn task must do before prompting the agent.
enum TurnJob {
    /// New or Fork: the backing agent does not exist yet; the task spawns it.
    SpawnNew,
    /// Continue: an existing handle; awaited to ready when `ready == false`.
    /// Boxed so the handle does not inflate the turn enums.
    RunExisting { agent: Box<AgentHandle> },
}

/// A conversation reserved under the registry lock; the turn task owns it now.
struct StartPlan {
    id: ConversationId,
    prompt_text: String,
    job: TurnJob,
    /// True when the record was created by this reservation: a failed launch
    /// drops it. A model switch keeps the record so the conversation id survives.
    new_record: bool,
    tx: watch::Sender<TurnState>,
    /// The requester's join handle, created in the same critical section as the
    /// in-flight marker so `receiver_count` is never 0 before the requester
    /// waits; `spawn_turn` hands it over.
    rx: Option<watch::Receiver<TurnState>>,
    /// Held for the whole turn so a superseding turn cannot touch the agent early.
    turn_lock: Arc<TurnLock>,
    /// Registry generation at reserve time; a newer generation means this turn
    /// was superseded and its result must not be committed.
    generation: u64,
    /// True when this turn replaces an abandoned one: interrupt the agent first.
    interrupt: bool,
    /// Forked or evicted agents, closed after the locks are released.
    close: Vec<AgentHandle>,
}

/// The in-flight turn of a conversation: its result channel plus the lock that
/// serializes it against a superseding turn.
#[derive(Clone)]
struct InflightTurn {
    tx: watch::Sender<TurnState>,
    turn_lock: Arc<TurnLock>,
    question_offered: bool,
    hook_pause: Arc<HookPause>,
}

/// Outcome of the atomic classify/reserve critical section.
enum Reserved {
    Finished(Result<ChatOutcome, ApiError>),
    Join {
        id: ConversationId,
        rx: watch::Receiver<TurnState>,
    },
    /// The request answers a relayed dialog: `chat` presses the option into the
    /// agent's TUI and joins the continuing turn. Boxed so the handle and the
    /// pending entry do not inflate the reserve enums.
    Answer {
        id: ConversationId,
        pending: Box<PendingDialog>,
        /// The in-flight snapshot `answer_dialog` replaced; a failed apply puts
        /// both back so an identical retry of the answer is applied again.
        snapshot: Box<InflightSnapshot>,
        answer: DialogAnswer,
        rx: watch::Receiver<TurnState>,
        agent: Box<AgentHandle>,
    },
    Start(StartPlan),
}

enum DialogAnswer {
    Tui(usize),
    Hook(HookAnswer),
}

/// Spawn failure split: a blocked agent keeps its tab and its handle.
enum LaunchError {
    Blocked { handle: AgentHandle },
    Failed(ApiError),
}

/// Digests of the System text and tools JSON last SENT to a bridge agent, used
/// to decide whether a Continue prompt needs `<updated_instructions>`/`<updated_tools>`.
#[derive(Debug, Clone)]
struct BridgeSent {
    system_digest: String,
    tools_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedClient {
    peer: SocketAddr,
    process: ClientProcess,
}

pub struct Service<H: HerdrApi> {
    config: Config,
    herdr: Arc<H>,
    env: ServiceEnv,
    turn_limits: TurnLimits,
    /// Server start time in unix seconds, reported as `created` by `/v1/models`.
    created: u32,
    /// Transcript locations per model id, by kind.
    transcript_roots: BTreeMap<String, TranscriptRoots>,
    registry: Mutex<Registry>,
    agents: Mutex<BTreeMap<ConversationId, AgentHandle>>,
    bridge_sent: Mutex<BTreeMap<ConversationId, BridgeSent>>,
    inflight: Mutex<BTreeMap<ConversationId, InflightTurn>>,
    clients: Mutex<BTreeMap<ConversationId, TrackedClient>>,
    workspace_id: Mutex<Option<String>>,
    /// This instance's `oa_owner` pane token value.
    owner_id: String,
    /// `oa:` tabs the previous periodic sweep saw without an owner token.
    untagged_tabs: Mutex<BTreeSet<String>>,
}

struct HookRequestGuard<'a, H: HerdrApi> {
    service: &'a Service<H>,
    id: ConversationId,
    hook: Arc<Mutex<Option<HookQuestion>>>,
    pause: Arc<HookPause>,
    finished: bool,
}

impl<H: HerdrApi> HookRequestGuard<'_, H> {
    fn finish(&mut self) {
        if !self.finished {
            self.pause.finish();
            self.finished = true;
        }
    }
}

impl<H: HerdrApi> Drop for HookRequestGuard<'_, H> {
    fn drop(&mut self) {
        if !self.finished {
            self.service.expire_hook_question(self.id, &self.hook, true);
            self.pause.finish();
            self.finished = true;
        }
    }
}

impl<H: HerdrApi> Service<H> {
    pub fn new(config: Config, herdr: Arc<H>, env: ServiceEnv) -> Arc<Self> {
        let turn_limits = TurnLimits {
            turn_timeout_ms: config.herdr.turn_timeout_ms,
            blocked_timeout_ms: config.herdr.blocked_timeout_ms,
            completion_grace_ms: config.herdr.completion_grace_ms,
            poll_interval: TURN_POLL_INTERVAL,
        };
        let transcript_roots = config
            .models
            .iter()
            .map(|model| {
                let variant_home = env.variant_config_homes.get(&model.id);
                let roots = match model.kind {
                    AgentKind::Claude => {
                        let mut roots = Vec::new();
                        if let Some(dir) = variant_home {
                            roots.push(dir.clone());
                        }
                        if let Some(dir) =
                            model.env.get("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty())
                        {
                            let dir = PathBuf::from(dir);
                            if !roots.contains(&dir) {
                                roots.push(dir);
                            }
                        }
                        for root in &env.transcript_roots {
                            if !roots.contains(root) {
                                roots.push(root.clone());
                            }
                        }
                        TranscriptRoots::Claude(roots)
                    }
                    AgentKind::Codex => {
                        let codex_home = codex::codex_home(
                            &model.env,
                            variant_home.map(PathBuf::as_path),
                            |key| std::env::var(key).ok(),
                        );
                        let sessions_root = env
                            .codex_sessions_root
                            .clone()
                            .unwrap_or_else(|| codex_home.join("sessions"));
                        TranscriptRoots::Codex {
                            sessions_root,
                            home_display: crate::config::tilde_display(&codex_home, &env.home),
                        }
                    }
                };
                (model.id.clone(), roots)
            })
            .collect();
        Arc::new(Self {
            config,
            herdr,
            env,
            turn_limits,
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs() as u32)
                .unwrap_or(0),
            transcript_roots,
            registry: Mutex::new(Registry::default()),
            agents: Mutex::new(BTreeMap::new()),
            bridge_sent: Mutex::new(BTreeMap::new()),
            inflight: Mutex::new(BTreeMap::new()),
            clients: Mutex::new(BTreeMap::new()),
            workspace_id: Mutex::new(None),
            owner_id: Uuid::new_v4().to_string(),
            untagged_tabs: Mutex::new(BTreeSet::new()),
        })
    }

    pub fn models(&self) -> &[ModelSpec] {
        &self.config.models
    }

    async fn track_client_process(&self, id: ConversationId, req: &NormalizedRequest) {
        if !req.opencode_session {
            return;
        }
        let (Some(proc_root), Some(server), Some(peer)) = (
            self.env.proc_root.clone(),
            self.env.server_addr,
            req.client_peer.filter(|peer| is_loopback_peer(peer.ip())),
        ) else {
            return;
        };
        {
            let mut clients = self.locked_clients();
            if clients.get(&id).is_some_and(|tracked| tracked.peer == peer) {
                return;
            }
            clients.remove(&id);
        }
        let process = match tokio::task::spawn_blocking(move || {
            client_process::find_client_process(&proc_root, peer, server)
        })
        .await
        {
            Ok(Some(process)) => process,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(conversation = %id, "client process lookup failed: {err}");
                return;
            }
        };
        let registry = self.locked_registry();
        if registry.get(id).is_none() {
            return;
        }
        self.locked_clients()
            .insert(id, TrackedClient { peer, process });
    }

    /// Close backing agents whose tracked loopback OpenCode process exited.
    pub async fn close_exited_clients(&self) {
        let Some(proc_root) = self.env.proc_root.clone() else {
            return;
        };
        let tracked = self
            .locked_clients()
            .iter()
            .map(|(id, client)| (*id, client.clone()))
            .collect::<Vec<_>>();
        let exited = match tokio::task::spawn_blocking(move || {
            tracked
                .into_iter()
                .filter(|(_, client)| !client_process::is_alive(&proc_root, &client.process))
                .collect::<Vec<_>>()
        })
        .await
        {
            Ok(exited) => exited,
            Err(err) => {
                tracing::warn!("client process check failed: {err}");
                return;
            }
        };
        for (id, client) in exited {
            let removed = {
                let mut registry = self.locked_registry();
                let mut clients = self.locked_clients();
                if clients.get(&id) != Some(&client) {
                    false
                } else {
                    clients.remove(&id);
                    registry.remove(id);
                    true
                }
            };
            if !removed {
                continue;
            }
            self.locked_bridge().remove(&id);
            let handle = self.locked_agents().remove(&id);
            if let Some(handle) = handle {
                tracing::info!(
                    pid = client.process.pid,
                    name = %handle.name,
                    "closing backing agent after its OpenCode client exited"
                );
                lifecycle::close_agent(&*self.herdr, &handle, &self.owner_id).await;
            }
        }
    }

    /// Check a hook token without leaking a timing-dependent comparison.
    pub fn hook_token_matches(&self, token: &str) -> bool {
        self.env.question_hook.as_ref().is_some_and(|hook| {
            super::server::constant_time_eq(token.as_bytes(), hook.token.as_bytes())
        })
    }

    /// Relay a Claude question to the client and build its hook response.
    pub async fn handle_question_hook(&self, input: HookInput) -> Option<Value> {
        if input.tool_name != "AskUserQuestion" {
            return None;
        }
        let id = self
            .locked_agents()
            .iter()
            .find(|(_, agent)| {
                agent.kind == AgentKind::Claude
                    && agent.session_id.as_deref() == Some(input.session_id.as_str())
            })
            .map(|(id, _)| *id)?;
        let inflight = self.locked_inflight().get(&id)?.clone();
        if !inflight.question_offered {
            return None;
        }
        let question_values = input
            .tool_input
            .get("questions")
            .and_then(Value::as_array)?;
        if question_values.is_empty() {
            return None;
        }
        let questions = question_values
            .iter()
            .filter_map(|question| question.get("question").and_then(Value::as_str))
            .map(str::to_string)
            .collect::<Vec<_>>();
        if questions.len() != question_values.len() {
            return None;
        }
        let first = &question_values[0];
        let dialog = Dialog {
            question: questions[0].clone(),
            options: first
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.get("label").and_then(Value::as_str))
                .map(str::to_string)
                .collect(),
            cursor: 0,
        };
        let (answer_tx, answer_rx) = oneshot::channel();
        let hook = Arc::new(Mutex::new(Some(HookQuestion {
            tool_input: input.tool_input.clone(),
            answer_tx,
        })));
        let (reply, call_id) = hook_question_reply(&input.tool_input);
        let seq = {
            let mut registry = self.locked_registry();
            let pending_blocks_hook = registry
                .get(id)
                .and_then(|record| record.pending_dialog.as_ref())
                .is_some_and(|pending| {
                    !pending.expired
                        && pending.hook.as_ref().is_none_or(|hook| {
                            hook.lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .is_some()
                        })
                });
            if pending_blocks_hook {
                return None;
            }
            let generation = registry.get(id)?.generation;
            registry.publish_dialog(
                id,
                generation,
                dialog,
                reply.clone(),
                Some(call_id),
                Some(Arc::clone(&hook)),
            )?
        };
        inflight.hook_pause.start();
        let mut guard = HookRequestGuard {
            service: self,
            id,
            hook: Arc::clone(&hook),
            pause: Arc::clone(&inflight.hook_pause),
            finished: false,
        };
        if inflight.tx.send(TurnState::Dialog { reply, seq }).is_err() {
            return None;
        }
        let timeout = Duration::from_millis(self.config.herdr.blocked_timeout_ms);
        let result = tokio::time::timeout(timeout, answer_rx).await;
        guard.finish();
        match result {
            Ok(Ok(HookAnswer::Answered(answers))) => {
                Some(question_hook::allow_response(&input.tool_input, &answers))
            }
            Ok(Ok(HookAnswer::Dismissed)) => Some(question_hook::deny_response(
                "The user dismissed this question",
            )),
            Ok(Err(_)) | Err(_) => {
                self.expire_hook_question(id, &hook, false);
                Some(question_hook::deny_response(
                    "No answer from the user in time",
                ))
            }
        }
    }

    /// Server start time in unix seconds.
    pub fn created(&self) -> u32 {
        self.created
    }

    pub fn api_key(&self) -> Option<&str> {
        self.env.api_key.as_deref()
    }

    pub fn allow_remote(&self) -> bool {
        self.config.server.allow_remote
    }

    pub fn log_requests(&self) -> bool {
        self.env.log_requests
    }

    /// Health snapshot; `herdr.reachable` is false when the server cannot be pinged.
    pub async fn health(&self) -> serde_json::Value {
        let herdr = match self.herdr.ping().await {
            Ok(pong) => serde_json::json!({
                "reachable": true,
                "version": pong.version,
                "protocol": pong.protocol,
            }),
            Err(err) => {
                tracing::warn!("herdr ping failed: {err}");
                serde_json::json!({"reachable": false, "error": err.to_string()})
            }
        };
        let reachable = herdr["reachable"].as_bool().unwrap_or(false);
        serde_json::json!({
            "status": if reachable { "ok" } else { "degraded" },
            "herdr": herdr,
            "conversations": self.locked_registry().count_total(),
            "agents": self.locked_agents().len(),
        })
    }

    /// Run one chat request end to end; a client disconnect must not cancel the turn,
    /// so the work runs in a detached task and every waiter joins its watch channel.
    pub async fn chat(self: &Arc<Self>, req: NormalizedRequest) -> Result<ChatOutcome, ApiError> {
        let _model = self
            .config
            .models
            .iter()
            .find(|model| model.id == req.model)
            .ok_or_else(|| ApiError::model_not_found(format!("model not found: {}", req.model)))?;
        // Coalesce title-generation style requests: a tool-less keyed `New`
        // waits so the tool-carrying main request can reserve first. Requests
        // with tools, and keyless requests, never wait.
        let first = self.locked_registry().classify(&req, Instant::now());
        if matches!(first, Classification::New { .. })
            && req.conversation_key.is_some()
            && req.tools.is_empty()
        {
            tokio::time::sleep(SIDE_REQUEST_COALESCE).await;
        }
        let reserved = self.reserve(&req);
        let client_id = match &reserved {
            Reserved::Finished(Ok(outcome)) => Some(outcome.conversation),
            Reserved::Join { id, .. } | Reserved::Answer { id, .. } => Some(*id),
            Reserved::Start(plan) => Some(plan.id),
            Reserved::Finished(Err(_)) => None,
        };
        if let Some(id) = client_id {
            self.track_client_process(id, &req).await;
        }
        let mut plan = match reserved {
            Reserved::Finished(result) => return result,
            Reserved::Join { id, rx } => return self.await_turn(rx, 0, id).await,
            Reserved::Answer {
                id,
                pending,
                snapshot,
                answer,
                rx,
                agent,
            } => {
                return self
                    .apply_dialog_answer(id, &pending, answer, rx, &agent, &snapshot)
                    .await;
            }
            Reserved::Start(plan) => plan,
        };
        // Forked or evicted agents close only after every lock is released.
        for handle in std::mem::take(&mut plan.close) {
            lifecycle::close_agent(&*self.herdr, &handle, &self.owner_id).await;
        }
        let id = plan.id;
        let rx = self.spawn_turn(plan, req);
        self.await_turn(rx, 0, id).await
    }

    /// Classify and reserve in one critical section so concurrent requests for
    /// the same conversation can never both take the `New` path. Lock order is
    /// registry -> agents -> inflight, and nothing here awaits.
    fn reserve(&self, req: &NormalizedRequest) -> Reserved {
        let now = Instant::now();
        let digest = conversation::request_digest(req);
        let mut registry = self.locked_registry();
        // opencode's Esc: the client hung up, so nobody waits for the in-flight
        // turn any more. Apply that request's abort semantics and classify again
        // instead of answering 409; the carried turn lock keeps the old task
        // away from the agent until it has finished.
        let mut superseded: Option<Arc<TurnLock>> = None;
        let classification = loop {
            let found = registry.classify(req, now);
            if superseded.is_none() {
                if let Classification::Busy(id) = found {
                    if let Some(lock) = self.orphaned_turn_lock(&registry, id) {
                        registry.supersede(id, now);
                        superseded = Some(lock);
                        continue;
                    }
                }
            }
            break found;
        };
        match classification {
            Classification::Retry { id, reply } => {
                let (agent_name, tab_label) = self.agent_labels(id);
                Reserved::Finished(Ok(ChatOutcome {
                    reply,
                    conversation: id,
                    agent_name,
                    tab_label,
                }))
            }
            Classification::JoinInflight(id) => {
                // The watch sender is inserted together with `begin`, so a joiner
                // that sees the in-flight marker under the registry lock always
                // finds the sender here.
                match self
                    .locked_inflight()
                    .get(&id)
                    .map(|turn| turn.tx.subscribe())
                {
                    Some(rx) => Reserved::Join { id, rx },
                    None => Reserved::Finished(Err(Self::busy_error(&registry, id))),
                }
            }
            Classification::Busy(id) => Reserved::Finished(Err(Self::busy_error(&registry, id))),
            Classification::DialogAnswer(id) => {
                self.reserve_dialog_answer(&mut registry, req, &digest, id)
            }
            Classification::SideRequest(_) => {
                Reserved::Finished(Err(ApiError::conversation_side_request(
                    "this request repeats a conversation key without the conversation's \
                     history and carries no assistant messages; if it is a side request \
                     such as title generation, point opencode's small_model at another \
                     provider",
                )))
            }
            Classification::Fork(old) => {
                registry.remove(old);
                self.locked_clients().remove(&old);
                let mut close = Vec::new();
                if let Some(handle) = self.locked_agents().remove(&old) {
                    close.push(handle);
                }
                match self.create_and_begin(&mut registry, req, &digest, now) {
                    Ok(mut plan) => {
                        plan.close.append(&mut close);
                        Reserved::Start(plan)
                    }
                    Err(err) => Reserved::Finished(Err(err)),
                }
            }
            Classification::New { .. } => {
                match self.create_and_begin(&mut registry, req, &digest, now) {
                    Ok(plan) => Reserved::Start(plan),
                    Err(err) => Reserved::Finished(Err(err)),
                }
            }
            Classification::Continue { id, delta } => {
                self.reserve_continue(&mut registry, req, &digest, id, delta, superseded)
            }
            Classification::ModelSwitch(id) => {
                self.reserve_model_switch(&mut registry, req, &digest, id, superseded)
            }
        }
    }

    /// Reserve a `ModelSwitch` turn: the record, its conversation id, and its
    /// history stay put; only the backing CLI is replaced. The fresh agent has no
    /// context of its own, so the first prompt replays the whole history. herdr
    /// cannot stop a running agent, so the old tab is closed and a new one opens;
    /// the threadId mapping itself does not change.
    fn reserve_model_switch(
        &self,
        registry: &mut Registry,
        req: &NormalizedRequest,
        digest: &str,
        id: ConversationId,
        superseded: Option<Arc<TurnLock>>,
    ) -> Reserved {
        // The record moves to another model, so the target model's own limit is
        // checked before anything is touched: a refused switch leaves the
        // conversation running the model it is already on.
        let mut close = Vec::new();
        if registry.count_for_model(&req.model) >= self.config.conversations.max_agents_per_model {
            // Only a conversation of the target model frees a slot in it.
            let victim = registry
                .evict_candidate(Instant::now())
                .filter(|victim| *victim != id)
                .filter(|victim| {
                    registry
                        .get(*victim)
                        .is_some_and(|record| record.model_id == req.model)
                });
            match victim {
                Some(victim) => {
                    registry.remove(victim);
                    self.locked_clients().remove(&victim);
                    if let Some(handle) = self.locked_agents().remove(&victim) {
                        tracing::info!(
                            name = %handle.name,
                            "evicting backing agent for the model switch"
                        );
                        close.push(handle);
                    }
                }
                None => {
                    return Reserved::Finished(Err(ApiError::too_many_backing_agents(format!(
                        "model `{}` already runs {} backing agents; none of them are idle long \
                         enough to reclaim for the switch",
                        req.model, self.config.conversations.max_agents_per_model
                    ))));
                }
            }
        }
        if let Some(handle) = self.locked_agents().remove(&id) {
            tracing::info!(
                name = %handle.name,
                model = %req.model,
                "switching the backing agent to another model"
            );
            close.push(handle);
        }
        registry.set_model(id, &req.model);
        self.reserve_spawn_turn(registry, req, digest, id, superseded, close)
    }

    /// Reserve a turn for a record whose backing agent is gone. A failed switch or
    /// respawn keeps the threadId mapping, so rebuild the agent and send it the
    /// whole conversation instead of failing every later request.
    fn reserve_rebuild(
        &self,
        registry: &mut Registry,
        req: &NormalizedRequest,
        digest: &str,
        id: ConversationId,
        superseded: Option<Arc<TurnLock>>,
    ) -> Reserved {
        self.reserve_spawn_turn(registry, req, digest, id, superseded, Vec::new())
    }

    /// Shared tail of both rebuild paths: a fresh agent, the whole history as its
    /// first prompt, and the record kept whatever else happens.
    fn reserve_spawn_turn(
        &self,
        registry: &mut Registry,
        req: &NormalizedRequest,
        digest: &str,
        id: ConversationId,
        superseded: Option<Arc<TurnLock>>,
        close: Vec<AgentHandle>,
    ) -> Reserved {
        self.locked_bridge().remove(&id);
        // This CLI has seen nothing, so a later request must replay as well in
        // case this turn never gets through.
        registry.reset_consumed(id);
        let bootstrap = conversation::bootstrap_flag(req);
        let prompt_text = if self.model_mode(&req.model) == ModelMode::Bridge {
            bridge::bridge_first_prompt(req, bootstrap)
        } else {
            prompt::self_exec_first_prompt(req, bootstrap)
        };
        let generation = registry.begin(id, digest.to_string(), req);
        let (tx, rx) = watch::channel(TurnState::Running);
        // A turn this request replaces keeps its lock so its task finishes before
        // this one prompts the agent.
        let turn_lock = superseded.unwrap_or_else(|| Arc::new(TurnLock::new(())));
        self.locked_inflight().insert(
            id,
            InflightTurn {
                tx: tx.clone(),
                turn_lock: Arc::clone(&turn_lock),
                question_offered: dialog::question_tool_offered(&req.tools),
                hook_pause: Arc::new(HookPause::default()),
            },
        );
        Reserved::Start(StartPlan {
            id,
            prompt_text,
            job: TurnJob::SpawnNew,
            new_record: false,
            tx,
            rx: Some(rx),
            turn_lock,
            generation,
            interrupt: false,
            close,
        })
    }

    /// Reserve a `Continue` turn. `superseded` carries the abandoned turn's lock
    /// when this request replaces it: the new turn then interrupts the agent and
    /// waits for the old task before prompting.
    fn reserve_continue(
        &self,
        registry: &mut Registry,
        req: &NormalizedRequest,
        digest: &str,
        id: ConversationId,
        delta: Vec<NormalizedMessage>,
        superseded: Option<Arc<TurnLock>>,
    ) -> Reserved {
        let Some(agent) = self.locked_agents().get(&id).cloned() else {
            // The record outlived its agent (a switch or respawn that failed to
            // launch): rebuild it and replay, instead of failing every retry.
            return self.reserve_rebuild(registry, req, digest, id, superseded);
        };
        // No turn has ever completed on this record (startup blocked or a
        // failed first turn): replay the full first prompt, not a delta.
        let ever_consumed = registry.get(id).is_some_and(|record| record.consumed > 0);
        let bridge = self.model_mode(&req.model) == ModelMode::Bridge;
        let prompt_text = match (bridge, ever_consumed) {
            (true, true) => {
                let changes = self.bridge_changes(id, req);
                bridge::bridge_next_prompt(&delta, &changes, req)
            }
            (true, false) => bridge::bridge_first_prompt(req, conversation::bootstrap_flag(req)),
            (false, true) => prompt::self_exec_next_prompt(&delta),
            (false, false) => {
                prompt::self_exec_first_prompt(req, conversation::bootstrap_flag(req))
            }
        };
        if prompt_text.trim().is_empty() {
            return Reserved::Finished(Err(ApiError::no_new_message(
                "no new user message after the conversation's consumed point",
            )));
        }
        let interrupt = superseded.is_some();
        let generation = registry.begin(id, digest.to_string(), req);
        let (tx, rx) = watch::channel(TurnState::Running);
        let turn_lock = superseded.unwrap_or_else(|| Arc::new(TurnLock::new(())));
        self.locked_inflight().insert(
            id,
            InflightTurn {
                tx: tx.clone(),
                turn_lock: Arc::clone(&turn_lock),
                question_offered: dialog::question_tool_offered(&req.tools),
                hook_pause: Arc::new(HookPause::default()),
            },
        );
        Reserved::Start(StartPlan {
            id,
            prompt_text,
            job: TurnJob::RunExisting {
                agent: Box::new(agent),
            },
            new_record: false,
            tx,
            rx: Some(rx),
            turn_lock,
            generation,
            interrupt,
            close: Vec::new(),
        })
    }

    /// Reserve a request that answers a relayed dialog. Expired relays and
    /// unmatched answers never press keys; a matched answer hands the key
    /// presses and the continuing turn to `chat`.
    fn reserve_dialog_answer(
        &self,
        registry: &mut Registry,
        req: &NormalizedRequest,
        digest: &str,
        id: ConversationId,
    ) -> Reserved {
        let Some(pending) = registry
            .get(id)
            .and_then(|record| record.pending_dialog.clone())
        else {
            return Reserved::Finished(Err(Self::busy_error(registry, id)));
        };
        let (agent_name, tab_label) = self.agent_labels(id);
        let hook_no_longer_waiting = pending.hook.as_ref().is_some_and(|hook| {
            pending.expired
                || hook
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_none()
        });
        if pending.expired || hook_no_longer_waiting {
            let reply = if hook_no_longer_waiting {
                hook_answer_not_delivered(&tab_label)
            } else {
                explanatory(
                    "[herdr-openai-gateway] That question expired before it was answered. Send your \
                     message again."
                        .to_string(),
                )
            };
            if hook_no_longer_waiting && self.locked_inflight().contains_key(&id) {
                return Reserved::Finished(Ok(ChatOutcome {
                    reply,
                    conversation: id,
                    agent_name,
                    tab_label,
                }));
            }
            registry.answer_dialog(id, digest.to_string(), req);
            registry.commit(id, reply.clone(), Instant::now());
            return Reserved::Finished(Ok(ChatOutcome {
                reply,
                conversation: id,
                agent_name,
                tab_label,
            }));
        }
        let hook_answer = pending.hook.as_ref().and_then(|hook| {
            let hook = hook.lock().unwrap_or_else(|error| error.into_inner());
            let tool_input = &hook.as_ref()?.tool_input;
            let questions = tool_input.get("questions")?.as_array()?;
            match req.messages.last()? {
                NormalizedMessage::Tool { content, .. } => {
                    let questions = questions
                        .iter()
                        .filter_map(|question| question.get("question").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    Some(question_hook::parse_answers(content, &questions))
                }
                NormalizedMessage::User(text) if questions.len() == 1 => {
                    let question = questions[0].get("question")?.as_str()?.to_string();
                    Some(HookAnswer::Answered(BTreeMap::from([(
                        question,
                        text.clone(),
                    )])))
                }
                _ => None,
            }
        });
        let answer = if pending.hook.is_some() {
            hook_answer.as_ref().map(|_| 0)
        } else {
            match req.messages.last() {
                Some(NormalizedMessage::Tool { content, .. }) => {
                    dialog::answer_from_tool_result(content, &pending.dialog)
                }
                Some(NormalizedMessage::User(text)) => {
                    dialog::answer_from_text(text, &pending.dialog)
                }
                _ => None,
            }
        };
        let Some(target) = answer else {
            // Unmatched answer: re-relay without pressing anything. The turn
            // stays in flight and the pending entry tracks the new history.
            let hook_tool_input = pending.hook.as_ref().and_then(|hook| {
                hook.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .map(|question| question.tool_input.clone())
            });
            let (reply, call_id) = if let Some(tool_input) = hook_tool_input {
                let (reply, call_id) = hook_question_reply(&tool_input);
                (reply, Some(call_id))
            } else {
                let agent_label = self
                    .locked_agents()
                    .get(&id)
                    .map(|handle| handle.kind.display_name().to_string())
                    .unwrap_or_default();
                let note = format!(
                    "Please answer with the number of one option, 1-{}.",
                    pending.dialog.options.len()
                );
                self.relay_reply(req, &pending.dialog, &agent_label, &tab_label, Some(&note))
            };
            registry.set_dialog(
                id,
                PendingDialog {
                    dialog: pending.dialog,
                    reply: reply.clone(),
                    call_id,
                    seq: pending.seq,
                    relayed_users: conversation::user_digests(req),
                    relayed_count: conversation::non_system_count(req) + 1,
                    expired: false,
                    hook: pending.hook,
                },
            );
            return Reserved::Finished(Ok(ChatOutcome {
                reply,
                conversation: id,
                agent_name,
                tab_label,
            }));
        };
        let Some((pending, snapshot)) = registry.answer_dialog(id, digest.to_string(), req) else {
            return Reserved::Finished(Err(Self::busy_error(registry, id)));
        };
        let rx = match self.locked_inflight().get(&id) {
            Some(turn) => turn.tx.subscribe(),
            None => {
                // The turn ended between classification and the reservation.
                registry.fail(id);
                registry.set_dialog(id, pending);
                return Reserved::Finished(Err(Self::busy_error(registry, id)));
            }
        };
        let Some(agent) = self.locked_agents().get(&id).cloned() else {
            registry.fail(id);
            registry.set_dialog(id, pending);
            return Reserved::Finished(Err(ApiError::backing_agent_exited(
                "the conversation lost its backing agent; send the message again",
            )));
        };
        Reserved::Answer {
            id,
            pending: Box::new(pending),
            snapshot: Box::new(snapshot),
            answer: match hook_answer {
                Some(answer) => DialogAnswer::Hook(answer),
                None => DialogAnswer::Tui(target),
            },
            rx,
            agent: Box::new(agent),
        }
    }

    /// Build the client-facing relay for `dialog`: a `question` tool call when
    /// the request offers that tool, numbered text otherwise. Returns the reply
    /// and the tool call id when one was generated.
    fn relay_reply(
        &self,
        req: &NormalizedRequest,
        dialog: &Dialog,
        agent_label: &str,
        tab_label: &str,
        note: Option<&str>,
    ) -> (AssistantReply, Option<String>) {
        build_relay(
            dialog::question_tool_offered(&req.tools),
            agent_label,
            tab_label,
            dialog,
            note,
        )
    }

    /// Capacity check, record creation, and in-flight reservation. The caller
    /// holds the registry lock; evicted agents close after it is released.
    fn create_and_begin(
        &self,
        registry: &mut Registry,
        req: &NormalizedRequest,
        digest: &str,
        now: Instant,
    ) -> Result<StartPlan, ApiError> {
        // Resolve the cwd before evicting: a rejected cwd must not drop a
        // victim whose tab would then never be closed.
        let model = self
            .config
            .models
            .iter()
            .find(|model| model.id == req.model);
        let cwd = lifecycle::resolve_cwd(
            req.cwd_header.as_deref(),
            req.client_cwd.as_deref(),
            model.and_then(|model| model.cwd.as_deref()),
            &self.config.defaults.cwd,
            &self.config.defaults.allowed_cwd_roots,
            &self.env.home,
        )?;
        let mut close = Vec::new();
        let over_capacity = registry.count_for_model(&req.model)
            >= self.config.conversations.max_agents_per_model
            || registry.count_total() >= self.config.conversations.max_agents_total;
        if over_capacity {
            match registry.evict_candidate(now) {
                Some(victim) => {
                    registry.remove(victim);
                    self.locked_clients().remove(&victim);
                    if let Some(handle) = self.locked_agents().remove(&victim) {
                        tracing::info!(name = %handle.name, "evicting backing agent for capacity");
                        close.push(handle);
                    }
                }
                None => {
                    return Err(ApiError::too_many_backing_agents(format!(
                        "at most {} backing agents per model and {} in total; none are idle \
                         long enough to reclaim",
                        self.config.conversations.max_agents_per_model,
                        self.config.conversations.max_agents_total,
                    )));
                }
            }
        }
        let id = registry.create(&req.model, req.conversation_key.clone(), cwd, now);
        let generation = registry.begin(id, digest.to_string(), req);
        let (tx, rx) = watch::channel(TurnState::Running);
        let turn_lock = Arc::new(TurnLock::new(()));
        self.locked_inflight().insert(
            id,
            InflightTurn {
                tx: tx.clone(),
                turn_lock: Arc::clone(&turn_lock),
                question_offered: dialog::question_tool_offered(&req.tools),
                hook_pause: Arc::new(HookPause::default()),
            },
        );
        let bootstrap = conversation::bootstrap_flag(req);
        let prompt_text = if model.is_some_and(|model| model.mode() == ModelMode::Bridge) {
            bridge::bridge_first_prompt(req, bootstrap)
        } else {
            prompt::self_exec_first_prompt(req, bootstrap)
        };
        Ok(StartPlan {
            id,
            prompt_text,
            job: TurnJob::SpawnNew,
            new_record: true,
            tx,
            rx: Some(rx),
            turn_lock,
            generation,
            interrupt: false,
            close,
        })
    }

    /// Close agents of conversations idle past `idle_ttl_secs`; run every 30 s.
    pub async fn reap_expired(&self) {
        let idle_ttl = Duration::from_secs(self.config.conversations.idle_ttl_secs);
        let expired = self.locked_registry().expired(Instant::now(), idle_ttl);
        for id in expired {
            let handle = {
                self.locked_registry().remove(id);
                self.locked_clients().remove(&id);
                self.locked_agents().remove(&id)
            };
            if let Some(handle) = handle {
                tracing::info!(name = %handle.name, "reaping idle backing agent");
                lifecycle::close_agent(&*self.herdr, &handle, &self.owner_id).await;
            }
        }
    }

    /// Close `oa:` tabs left over from a previous run; never creates the workspace.
    pub async fn startup_sweep(&self) {
        let workspaces = match self.herdr.workspace_list().await {
            Ok(workspaces) => workspaces,
            Err(err) => {
                tracing::warn!("cannot sweep orphan tabs: {err}");
                return;
            }
        };
        let Some(workspace) = workspaces
            .iter()
            .find(|ws| ws.label == self.config.herdr.workspace_label)
        else {
            return;
        };
        let closed =
            lifecycle::sweep_orphans(&*self.herdr, &workspace.workspace_id, &self.owner_id).await;
        if closed > 0 {
            tracing::info!(closed, "closed orphan backing tabs from a previous run");
        }
    }

    /// Reaper tick: refresh our owner tokens, then close `oa:` tabs that had no
    /// owner token on this tick and the previous one. Waiting for a second
    /// sighting avoids racing a launch between `tab.create` and its token report.
    /// Tabs of tracked agents are never closed here.
    pub async fn sweep_untagged_tabs(&self) {
        // Re-tag tracked panes first: refresh only finds panes whose token is
        // still ours, so a token that expired during a stall would stay lost.
        let tracked_panes: Vec<String> = self
            .locked_agents()
            .values()
            .map(|handle| handle.pane_id.clone())
            .collect();
        for pane_id in tracked_panes {
            if let Err(err) =
                lifecycle::report_owner_token(&*self.herdr, &pane_id, &self.owner_id).await
            {
                tracing::warn!(pane_id = %pane_id, "owner token re-report failed: {err}");
            }
        }
        let workspace_id = match self.herdr.workspace_list().await {
            Ok(workspaces) => workspaces
                .into_iter()
                .find(|ws| ws.label == self.config.herdr.workspace_label)
                .map(|ws| ws.workspace_id),
            Err(err) => {
                tracing::warn!("cannot sweep untagged tabs: {err}");
                None
            }
        };
        let Some(workspace_id) = workspace_id else {
            lock(&self.untagged_tabs).clear();
            return;
        };
        lifecycle::refresh_owner_tokens(&*self.herdr, &workspace_id, &self.owner_id).await;
        let untagged =
            match lifecycle::scan_owner_tokens(&*self.herdr, &workspace_id, &self.owner_id).await {
                Ok(scan) => scan.untagged,
                Err(err) => {
                    tracing::warn!("cannot sweep untagged tabs: {err}");
                    Vec::new()
                }
            };
        let seen_before = std::mem::replace(
            &mut *lock(&self.untagged_tabs),
            untagged.iter().map(|tab| tab.tab_id.clone()).collect(),
        );
        let tracked_tabs: BTreeSet<String> = self
            .locked_agents()
            .values()
            .map(|handle| handle.tab_id.clone())
            .collect();
        for tab in untagged {
            if !seen_before.contains(&tab.tab_id) || tracked_tabs.contains(&tab.tab_id) {
                continue;
            }
            tracing::info!(tab_id = %tab.tab_id, label = %tab.label, "closing untagged backing tab");
            if let Err(err) = self.herdr.tab_close(&tab.tab_id).await {
                tracing::warn!(tab_id = %tab.tab_id, "tab.close failed: {err}");
            }
        }
    }

    /// Close every backing agent tab this service owns.
    pub async fn shutdown(&self) {
        self.locked_clients().clear();
        let owned = std::mem::take(&mut *self.locked_agents());
        for handle in owned.into_values() {
            lifecycle::close_agent(&*self.herdr, &handle, &self.owner_id).await;
        }
    }

    /// Release all pending Claude hook requests during graceful shutdown.
    pub fn cancel_all_hook_questions(&self) {
        self.locked_registry().cancel_pending_hook_questions();
    }

    fn busy_error(registry: &Registry, id: ConversationId) -> ApiError {
        if let Some(pending) = registry
            .get(id)
            .and_then(|record| record.pending_dialog.as_ref())
        {
            let question = pending
                .dialog
                .question
                .lines()
                .next()
                .unwrap_or_default()
                .trim();
            if !question.is_empty() {
                return ApiError::conversation_busy(format!(
                    "the backing agent is waiting for an answer to: {question}"
                ));
            }
        }
        ApiError::conversation_busy(format!(
            "conversation {id} is processing another request; retry after it completes"
        ))
    }

    /// Per-launch additions on top of the model spec: Claude's agent-view env and
    /// the question-hook settings. The hook token lives in the tab env, so a CLI
    /// restarted inside the same tab keeps it without re-injecting env.
    fn launch_extras(&self, model: &ModelSpec) -> LaunchExtras {
        let mut extras = LaunchExtras::default();
        if model.kind != AgentKind::Claude {
            return extras;
        }
        if !model.env.contains_key(CLAUDE_DISABLE_AGENT_VIEW_ENV) {
            extras
                .env
                .insert(CLAUDE_DISABLE_AGENT_VIEW_ENV.to_string(), "1".to_string());
        }
        let Some(hook) = self.env.question_hook.as_ref() else {
            return extras;
        };
        extras.args = vec![
            "--settings".to_string(),
            question_hook::claude_settings_arg(
                &hook.url,
                self.config.herdr.blocked_timeout_ms / 1000 + 30,
            ),
        ];
        extras.env.insert(
            "HERDR_OPENAI_GATEWAY_HOOK_TOKEN".to_string(),
            hook.token.clone(),
        );
        extras
    }

    /// Restart the record's model inside the agent's own pane, resuming the CLI's
    /// session when the gateway still knows its id.
    async fn restart_agent_in_place_for(
        &self,
        id: ConversationId,
        agent: &mut AgentHandle,
        resume_session: Option<&str>,
    ) -> Result<(), LaunchError> {
        let Some(model_id) = self
            .locked_registry()
            .get(id)
            .map(|record| record.model_id.clone())
        else {
            return Err(LaunchError::Failed(ApiError::backing_agent_exited(
                "the backing agent exited and cannot be respawned",
            )));
        };
        let Some(model) = self.config.models.iter().find(|model| model.id == model_id) else {
            return Err(LaunchError::Failed(ApiError::backing_agent_exited(
                "the backing agent exited and cannot be respawned",
            )));
        };
        match lifecycle::restart_agent_in_place(
            &*self.herdr,
            agent,
            model,
            self.config.herdr.startup_timeout_ms,
            &self.launch_extras(model),
            resume_session,
            &self.owner_id,
        )
        .await
        {
            Ok(()) => Ok(()),
            // The restarted CLI waits on a dialog in the same tab: register the
            // half-ready handle so the next request awaits this same pane
            // instead of prompting a name herdr no longer knows.
            Err(lifecycle::SpawnError::StartupBlocked { .. }) => {
                if self.register_agent_or_close(id, agent).await {
                    Err(LaunchError::Blocked {
                        handle: agent.clone(),
                    })
                } else {
                    Err(LaunchError::Failed(ApiError::backing_agent_exited(
                        "the backing agent exited and cannot be respawned",
                    )))
                }
            }
            Err(err) => Err(LaunchError::Failed(err.into_api_error())),
        }
    }

    /// Launch and await the backing agent for a reserved record. On `Blocked` the
    /// handle is stored with `ready = false` so a later request awaits the same
    /// tab instead of spawning another agent.
    async fn spawn_agent_for(&self, id: ConversationId) -> Result<AgentHandle, LaunchError> {
        let timeout = self.config.herdr.startup_timeout_ms;
        let Some((cwd, model_id)) = self
            .locked_registry()
            .get(id)
            .map(|record| (record.cwd.clone(), record.model_id.clone()))
        else {
            return Err(LaunchError::Failed(ApiError::backing_agent_exited(
                "the backing agent exited and cannot be respawned",
            )));
        };
        let Some(model) = self.config.models.iter().find(|model| model.id == model_id) else {
            return Err(LaunchError::Failed(ApiError::backing_agent_exited(
                "the backing agent exited and cannot be respawned",
            )));
        };
        let mut workspace_id = match self.ensure_workspace().await {
            Ok(workspace_id) => workspace_id,
            Err(err) => return Err(LaunchError::Failed(err)),
        };
        let extras = self.launch_extras(model);
        let mut handle = match lifecycle::launch_agent(
            &*self.herdr,
            &workspace_id,
            model,
            &cwd,
            timeout,
            &extras,
            &self.owner_id,
        )
        .await
        {
            // The user may have closed the workspace or lobby tab by hand.
            Err(err) if stale_workspace(&err) => {
                *self.locked_workspace() = None;
                workspace_id = match self.ensure_workspace().await {
                    Ok(workspace_id) => workspace_id,
                    Err(err) => return Err(LaunchError::Failed(err)),
                };
                match lifecycle::launch_agent(
                    &*self.herdr,
                    &workspace_id,
                    model,
                    &cwd,
                    timeout,
                    &extras,
                    &self.owner_id,
                )
                .await
                {
                    Ok(handle) => handle,
                    Err(err) => return Err(LaunchError::Failed(err.into_api_error())),
                }
            }
            Ok(handle) => handle,
            Err(err) => return Err(LaunchError::Failed(err.into_api_error())),
        };
        match lifecycle::await_ready(&*self.herdr, &mut handle, timeout).await {
            Ok(()) => {
                // Register the handle right away so shutdown and reaping can see
                // the tab even while the first turn is still running.
                if self.register_agent_or_close(id, &handle).await {
                    Ok(handle)
                } else {
                    Err(LaunchError::Failed(ApiError::backing_agent_exited(
                        "the backing agent exited and cannot be respawned",
                    )))
                }
            }
            Err(SpawnError::StartupBlocked { .. }) => {
                if self.register_agent_or_close(id, &handle).await {
                    Err(LaunchError::Blocked { handle })
                } else {
                    Err(LaunchError::Failed(ApiError::backing_agent_exited(
                        "the backing agent exited and cannot be respawned",
                    )))
                }
            }
            Err(err) => Err(LaunchError::Failed(err.into_api_error())),
        }
    }

    async fn ensure_workspace(&self) -> Result<String, ApiError> {
        if let Some(id) = self.locked_workspace().clone() {
            return Ok(id);
        }
        let id = lifecycle::ensure_workspace(&*self.herdr, &self.config.herdr.workspace_label)
            .await
            .map_err(|err| ApiError::herdr_unavailable(err.to_string()))?;
        *self.locked_workspace() = Some(id.clone());
        Ok(id)
    }

    /// Run the reserved turn detached (the conversation was begun by `reserve`)
    /// and return a receiver every waiter, including the requester, joins.
    fn spawn_turn(
        self: &Arc<Self>,
        mut plan: StartPlan,
        req: NormalizedRequest,
    ) -> watch::Receiver<TurnState> {
        let id = plan.id;
        let generation = plan.generation;
        // The requester joins on the receiver that came with the reservation, so
        // the turn has had a receiver since the in-flight marker existed; a plan
        // without one would have to join late and could be superseded at once.
        let rx = plan.rx.take().unwrap_or_else(|| plan.tx.subscribe());
        let tx = plan.tx.clone();
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let result = service.execute_turn(plan, req).await;
            service.cancel_pending_hook_question(id, generation);
            if result.is_err() && !service.is_superseded(id, generation) {
                // Any error must clear the in-flight marker or the conversation
                // stays Busy forever; `fail` is idempotent. A turn that was
                // superseded must not clear the marker the newer request owns.
                service.locked_registry().fail(id);
            }
            let _ = tx.send(TurnState::Done(result));
            service.clear_inflight(id, &tx);
        });
        rx
    }

    fn cancel_pending_hook_question(&self, id: ConversationId, generation: u64) {
        let hook = {
            let registry = self.locked_registry();
            let Some(record) = registry
                .get(id)
                .filter(|record| record.generation == generation)
            else {
                return;
            };
            record
                .pending_dialog
                .as_ref()
                .and_then(|pending| pending.hook.clone())
        };
        if let Some(hook) = hook {
            hook.lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
    }

    fn expire_hook_question(
        &self,
        id: ConversationId,
        hook: &Arc<Mutex<Option<HookQuestion>>>,
        drop_sender: bool,
    ) {
        self.locked_registry()
            .expire_hook_question(id, hook, drop_sender);
    }

    /// Drop the in-flight entry for `id`, but only when the stored sender is the
    /// same channel: once this turn's registry marker was cleared, a newer
    /// request may already have reserved the id with its own sender, and this
    /// stale task must not remove that.
    fn clear_inflight(&self, id: ConversationId, tx: &watch::Sender<TurnState>) {
        let mut inflight = self.locked_inflight();
        if inflight
            .get(&id)
            .is_some_and(|current| current.tx.same_channel(tx))
        {
            inflight.remove(&id);
        }
    }

    /// The turn lock of an in-flight turn nobody waits for, i.e. one the client
    /// abandoned (opencode's Esc) that a new request may supersede. A turn with
    /// an answerable relayed dialog pending is never superseded.
    fn orphaned_turn_lock(&self, registry: &Registry, id: ConversationId) -> Option<Arc<TurnLock>> {
        if registry
            .get(id)
            .and_then(|record| record.pending_dialog.as_ref())
            .is_some_and(|pending| {
                !pending.expired
                    && pending.hook.as_ref().is_none_or(|hook| {
                        hook.lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .is_some()
                    })
            })
        {
            return None;
        }
        self.locked_inflight()
            .get(&id)
            .filter(|turn| turn.tx.receiver_count() == 0)
            .map(|turn| Arc::clone(&turn.turn_lock))
    }

    /// True when a newer request superseded this turn; its results must not land.
    fn is_superseded(&self, id: ConversationId, generation: u64) -> bool {
        self.locked_registry()
            .get(id)
            .is_some_and(|record| record.generation != generation)
    }

    /// Ask a working backing agent to stop the superseded turn, the same thing
    /// pressing Esc in its TUI does, wait for it to come back idle, then clear
    /// any prompt the CLI put back into its input box.
    async fn interrupt_agent(&self, agent: &AgentHandle) {
        let info = match self.herdr.agent_get(&agent.name).await {
            Ok(info) => info,
            Err(err) => {
                tracing::warn!(name = %agent.name, "agent.get failed before abort: {err}");
                return;
            }
        };
        if matches!(info.agent_status, AgentStatus::Idle | AgentStatus::Done) {
            return;
        }
        if let Err(err) = self.herdr.agent_send_keys(&agent.name, &["esc"]).await {
            tracing::warn!(name = %agent.name, "sending esc failed: {err}");
            return;
        }
        let wait = WaitSpec {
            until: vec![AgentStatus::Idle, AgentStatus::Done],
            timeout_ms: Some(ABORT_WAIT_MS),
        };
        match self.herdr.agent_wait(&agent.name, wait).await {
            Ok(_) => tracing::info!(name = %agent.name, "aborted the superseded turn"),
            Err(err) => {
                tracing::warn!(name = %agent.name, "agent stayed busy after esc: {err}")
            }
        }
        // Claude puts the interrupted prompt back into its input box, so the new
        // prompt would be appended to it; Codex needs no clearing at all.
        let clear = agent.kind.clear_input_keys();
        if clear.is_empty() {
            return;
        }
        if let Err(err) = self.herdr.agent_send_keys(&agent.name, clear).await {
            tracing::warn!(name = %agent.name, "clearing the input box failed: {err}");
        }
    }

    /// The dialog relay for one turn: reads the blocked agent's screen and
    /// publishes relays on the turn's watch channel.
    fn dialog_hook(
        self: &Arc<Self>,
        id: ConversationId,
        generation: u64,
        tx: &watch::Sender<TurnState>,
        req: &NormalizedRequest,
        agent: &AgentHandle,
    ) -> DialogHook<H> {
        DialogHook {
            service: Arc::clone(self),
            id,
            generation,
            tx: tx.clone(),
            question_offered: dialog::question_tool_offered(&req.tools),
            agent_label: agent.kind.display_name().to_string(),
            tab_label: agent.tab_label.clone(),
            pause: self
                .locked_inflight()
                .get(&id)
                .map(|turn| Arc::clone(&turn.hook_pause))
                .unwrap_or_else(|| Arc::new(HookPause::default())),
        }
    }

    /// Wait out a startup dialog instead of failing with 503: relay it, then run
    /// ONE post-dialog wait loop polling every `STARTUP_DIALOG_POLL` via
    /// `lookup_agent`. A new `blocked` status relays the new dialog; `idle`/
    /// `done` continuously for `POST_DIALOG_SETTLE` (herdr's settle delay, so a
    /// re-initializing TUI is not prompted too early) makes the agent ready; an
    /// agent missing for `AGENT_EXIT_CONFIRM` in a row exited on the answer.
    /// The whole wait is bounded by `blocked_timeout_ms` from the first relay.
    /// The handle stays in the agents map the whole time so shutdown and
    /// reaping see it. `known_blocked` is true when the caller just observed
    /// the agent blocked: its dialog is relayed before the first poll.
    async fn resolve_startup_blocked(
        self: &Arc<Self>,
        id: ConversationId,
        generation: u64,
        mut handle: AgentHandle,
        tx: &watch::Sender<TurnState>,
        req: &NormalizedRequest,
        known_blocked: bool,
    ) -> Result<AgentHandle, ApiError> {
        let hook = self.dialog_hook(id, generation, tx, req, &handle);
        let started = Instant::now();
        let blocked_window = Duration::from_millis(self.config.herdr.blocked_timeout_ms);
        let mut last_relayed: Option<Dialog> = None;
        if known_blocked {
            if let Some(current) = self.read_dialog(&handle).await {
                hook.relay(current.clone());
                last_relayed = Some(current);
            }
        }
        let mut settled_since: Option<Instant> = None;
        let mut missing_since: Option<Instant> = None;
        loop {
            tokio::time::sleep(STARTUP_DIALOG_POLL).await;
            match self.lookup_agent(&handle).await {
                Ok(Some(info)) if info.agent_status == AgentStatus::Blocked => {
                    missing_since = None;
                    settled_since = None;
                    // Relay the open dialog unless it is the one already
                    // relayed: a second startup dialog must reach the client
                    // even when the working gap between dialogs falls between
                    // two polls, but the same dialog is never re-published.
                    // The cursor is ignored: the answer's own key presses move
                    // it while the dialog stays the same one.
                    if let Some(current) = self.read_dialog(&handle).await {
                        let already_relayed = last_relayed.as_ref().is_some_and(|previous| {
                            previous.question == current.question
                                && previous.options == current.options
                        });
                        if !already_relayed {
                            hook.relay(current.clone());
                            last_relayed = Some(current);
                        }
                    }
                }
                Ok(Some(info)) => {
                    missing_since = None;
                    let settled = info.agent.as_deref() == Some(handle.kind.herdr_label())
                        && matches!(info.agent_status, AgentStatus::Idle | AgentStatus::Done);
                    if settled {
                        let since = *settled_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= POST_DIALOG_SETTLE {
                            if !handle.managed {
                                // The settled agent must be named before the
                                // turn can address it (see `await_ready_unmanaged`).
                                if let Err(err) =
                                    lifecycle::rename_unmanaged(&*self.herdr, &mut handle).await
                                {
                                    // The pane hosts an agent this turn cannot
                                    // name. Drop the handle and keep the record:
                                    // the next request rebuilds on this threadId.
                                    self.locked_agents().remove(&id);
                                    return Err(err.into_api_error());
                                }
                            }
                            handle.ready = true;
                            if !self.register_agent_or_close(id, &handle).await {
                                return Err(ApiError::backing_agent_exited(
                                    "the backing agent exited and cannot be respawned",
                                ));
                            }
                            return Ok(handle);
                        }
                    } else {
                        // `working`, `unknown`, or a kind mismatch: keep waiting.
                        settled_since = None;
                        if info.agent.as_deref().is_none_or(|agent| agent.is_empty()) {
                            // The pane reports no agent: count it as missing.
                            let since = *missing_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= AGENT_EXIT_CONFIRM {
                                return Err(self.agent_exited_after_dialog(id, &handle).await);
                            }
                        }
                    }
                }
                Ok(None) => {
                    settled_since = None;
                    let since = *missing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= AGENT_EXIT_CONFIRM {
                        // The answer closed the agent (e.g. "No, exit").
                        return Err(self.agent_exited_after_dialog(id, &handle).await);
                    }
                }
                Err(err) => {
                    // Transient lookup failure: keep waiting, the user can
                    // still answer in the herdr tab.
                    tracing::warn!(name = %handle.name, "agent lookup while blocked: {err}");
                }
            }
            if started.elapsed() >= blocked_window {
                return Err(ApiError::agent_startup_blocked(lifecycle::blocked_message(
                    &handle.tab_label,
                    &handle.cwd,
                )));
            }
        }
    }

    /// The answer exited the backing agent: close the tab and drop the handle. The
    /// record stays, so the next request rebuilds the agent on the same threadId.
    async fn agent_exited_after_dialog(
        &self,
        id: ConversationId,
        handle: &AgentHandle,
    ) -> ApiError {
        lifecycle::close_agent(&*self.herdr, handle, &self.owner_id).await;
        self.locked_agents().remove(&id);
        ApiError::backing_agent_exited("the backing agent exited after the dialog was answered")
    }

    /// `agent.get` by name, then by pane id: `Ok(None)` only when both say the
    /// agent is gone; transport errors stay `Err` so the caller keeps waiting.
    async fn lookup_agent(&self, handle: &AgentHandle) -> Result<Option<AgentInfo>, HerdrError> {
        match self.herdr.agent_get(&handle.name).await {
            Ok(info) => return Ok(Some(info)),
            Err(err) if !agent_gone(&err) => return Err(err),
            Err(_) => {}
        }
        match self.herdr.agent_get(&handle.pane_id).await {
            Ok(info) => Ok(Some(info)),
            Err(err) if agent_gone(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// `agent.read` (Detection) by name, retried once by pane id: during
    /// startup the name can lag the pane (see `lookup_agent`).
    async fn read_agent_screen(&self, agent: &AgentHandle) -> Result<String, HerdrError> {
        match self
            .herdr
            .agent_read(&agent.name, ReadSource::Detection, None)
            .await
        {
            Ok(screen) => Ok(screen),
            Err(err) => {
                tracing::warn!(name = %agent.name, "agent.read by name failed: {err}");
                self.herdr
                    .agent_read(&agent.pane_id, ReadSource::Detection, None)
                    .await
            }
        }
    }

    /// `agent.send_keys` by name, retried once by pane id.
    async fn send_agent_keys(&self, agent: &AgentHandle, keys: &[&str]) -> Result<(), HerdrError> {
        match self.herdr.agent_send_keys(&agent.name, keys).await {
            Ok(()) => Ok(()),
            Err(err) => {
                tracing::warn!(name = %agent.name, "agent.send_keys by name failed: {err}");
                self.herdr.agent_send_keys(&agent.pane_id, keys).await
            }
        }
    }

    /// Read and parse the agent's current screen as a dialog; `None` when the
    /// screen cannot be read.
    async fn read_dialog(&self, agent: &AgentHandle) -> Option<Dialog> {
        match self.read_agent_screen(agent).await {
            Ok(screen) => Some(dialog::parse_dialog(&screen)),
            Err(err) => {
                tracing::warn!(
                    name = %agent.name,
                    "cannot read the agent screen before the answer: {err}"
                );
                None
            }
        }
    }

    fn restore_expired_hook_answer(
        &self,
        id: ConversationId,
        pending: &PendingDialog,
        snapshot: &InflightSnapshot,
        agent: &AgentHandle,
    ) -> ChatOutcome {
        let mut expired = pending.clone();
        expired.expired = true;
        self.locked_registry()
            .restore_dialog_answer(id, expired, snapshot.clone());
        ChatOutcome {
            reply: hook_answer_not_delivered(&agent.tab_label),
            conversation: id,
            agent_name: agent.name.clone(),
            tab_label: agent.tab_label.clone(),
        }
    }

    /// Press the answered option into the agent's TUI, then join the continuing
    /// turn. Verification failures put the pending entry AND the in-flight
    /// snapshot back so the client can answer again; nothing is ever typed into
    /// the agent as a prompt.
    async fn apply_dialog_answer(
        &self,
        id: ConversationId,
        pending: &PendingDialog,
        answer: DialogAnswer,
        rx: watch::Receiver<TurnState>,
        agent: &AgentHandle,
        snapshot: &InflightSnapshot,
    ) -> Result<ChatOutcome, ApiError> {
        if let DialogAnswer::Hook(answer) = answer {
            let Some(hook) = &pending.hook else {
                return Ok(self.restore_expired_hook_answer(id, pending, snapshot, agent));
            };
            let question = hook
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let Some(question) = question else {
                return Ok(self.restore_expired_hook_answer(id, pending, snapshot, agent));
            };
            if question.answer_tx.send(answer).is_err() {
                return Ok(self.restore_expired_hook_answer(id, pending, snapshot, agent));
            }
            return self.await_turn(rx, pending.seq, id).await;
        }
        let DialogAnswer::Tui(target) = answer else {
            unreachable!("hook answer returned above");
        };
        let dialog_changed = || {
            ApiError::conversation_busy(
                "the agent's dialog changed before your answer was applied; send it again",
            )
        };
        let put_back = |service: &Self| {
            service
                .locked_registry()
                .restore_dialog_answer(id, pending.clone(), snapshot.clone());
        };
        // Settle first: two consecutive reads at least `DIALOG_SETTLE_GAP`
        // apart showing the same options and cursor. Never type into a screen
        // that is still repainting.
        let mut current = match self.settle_dialog(agent).await {
            Settle::Settled(dialog) => dialog,
            Settle::Unreadable => {
                put_back(self);
                return Err(dialog_changed());
            }
            Settle::Unsettled => {
                put_back(self);
                return Err(ApiError::new(
                    502,
                    "api_error",
                    "dialog_answer_failed",
                    "the agent's dialog screen never settled before the answer was applied",
                ));
            }
        };
        if current.options != pending.dialog.options {
            put_back(self);
            return Err(dialog_changed());
        }
        // One key at a time: after each press poll until the screen shows the
        // same options with a cursor that actually moved. Menus can wrap, so
        // the direction is always recomputed from the confirmed cursor.
        let max_presses = current.options.len().saturating_mul(2);
        let mut presses = 0usize;
        while current.cursor != target {
            if presses >= max_presses {
                put_back(self);
                return Err(ApiError::new(
                    502,
                    "api_error",
                    "dialog_answer_failed",
                    "the agent's dialog cursor never reached the chosen option",
                ));
            }
            let Some(key) = dialog::next_move_key(current.cursor, target) else {
                break;
            };
            if let Err(err) = self.send_agent_keys(agent, &[key]).await {
                tracing::warn!(name = %agent.name, "sending dialog keys failed: {err}");
            }
            presses += 1;
            match self
                .await_cursor_move(agent, current.cursor, &current.options)
                .await
            {
                CursorMove::Moved(next) => current = next,
                CursorMove::Unreadable | CursorMove::Changed => {
                    put_back(self);
                    return Err(dialog_changed());
                }
                CursorMove::TimedOut => {
                    put_back(self);
                    return Err(ApiError::new(
                        502,
                        "api_error",
                        "dialog_answer_failed",
                        "the agent's dialog screen did not react to a cursor key",
                    ));
                }
            }
        }
        // Enter only after two consecutive reads, at least `DIALOG_SETTLE_GAP`
        // apart, both show the cursor resting on the chosen option.
        match self.confirm_cursor(agent, target, &current.options).await {
            CursorConfirm::Confirmed => {}
            CursorConfirm::Unreadable | CursorConfirm::Changed => {
                put_back(self);
                return Err(dialog_changed());
            }
            CursorConfirm::Unconfirmed => {
                put_back(self);
                return Err(ApiError::new(
                    502,
                    "api_error",
                    "dialog_answer_failed",
                    "could not confirm the agent's dialog cursor on the chosen option",
                ));
            }
        }
        if let Err(err) = self.send_agent_keys(agent, &["enter"]).await {
            tracing::warn!(name = %agent.name, "sending enter to the dialog failed: {err}");
            put_back(self);
            return Err(ApiError::new(
                502,
                "api_error",
                "dialog_answer_failed",
                "sending enter to the agent's dialog failed",
            ));
        }
        self.await_turn(rx, pending.seq, id).await
    }

    /// Wait until two consecutive reads at least `DIALOG_SETTLE_GAP` apart
    /// show the same options and cursor, up to `DIALOG_SETTLE_TIMEOUT`.
    async fn settle_dialog(&self, agent: &AgentHandle) -> Settle {
        let deadline = Instant::now() + DIALOG_SETTLE_TIMEOUT;
        let Some(mut previous) = self.read_dialog(agent).await else {
            return Settle::Unreadable;
        };
        loop {
            if Instant::now() >= deadline {
                return Settle::Unsettled;
            }
            tokio::time::sleep(DIALOG_SETTLE_GAP).await;
            let Some(next) = self.read_dialog(agent).await else {
                return Settle::Unreadable;
            };
            if next.cursor == previous.cursor && next.options == previous.options {
                return Settle::Settled(next);
            }
            previous = next;
        }
    }

    /// After one cursor key, poll every `DIALOG_MOVE_POLL` until the screen
    /// shows the same options with a cursor different from `from`; that new
    /// cursor is the confirmed position.
    async fn await_cursor_move(
        &self,
        agent: &AgentHandle,
        from: usize,
        options: &[String],
    ) -> CursorMove {
        let deadline = Instant::now() + DIALOG_MOVE_TIMEOUT;
        loop {
            tokio::time::sleep(DIALOG_MOVE_POLL).await;
            let Some(next) = self.read_dialog(agent).await else {
                return CursorMove::Unreadable;
            };
            if next.options != options {
                return CursorMove::Changed;
            }
            if next.cursor != from {
                return CursorMove::Moved(next);
            }
            if Instant::now() >= deadline {
                return CursorMove::TimedOut;
            }
        }
    }

    /// Two consecutive reads at least `DIALOG_SETTLE_GAP` apart, both showing
    /// the cursor at `target` with unchanged options.
    async fn confirm_cursor(
        &self,
        agent: &AgentHandle,
        target: usize,
        options: &[String],
    ) -> CursorConfirm {
        let deadline = Instant::now() + DIALOG_SETTLE_TIMEOUT;
        let Some(mut previous) = self.read_dialog(agent).await else {
            return CursorConfirm::Unreadable;
        };
        loop {
            if previous.options != options {
                return CursorConfirm::Changed;
            }
            if previous.cursor != target {
                return CursorConfirm::Unconfirmed;
            }
            if Instant::now() >= deadline {
                return CursorConfirm::Unconfirmed;
            }
            tokio::time::sleep(DIALOG_SETTLE_GAP).await;
            let Some(next) = self.read_dialog(agent).await else {
                return CursorConfirm::Unreadable;
            };
            if next.cursor == previous.cursor && next.options == previous.options {
                return CursorConfirm::Confirmed;
            }
            previous = next;
        }
    }

    /// Join a turn's watch channel: the first terminal result, or a dialog relay
    /// with a `seq` newer than `after_seq`. Joiners and first requests use 0;
    /// answering a dialog uses that dialog's seq.
    async fn await_turn(
        &self,
        mut rx: watch::Receiver<TurnState>,
        after_seq: u64,
        id: ConversationId,
    ) -> Result<ChatOutcome, ApiError> {
        let no_result = || {
            ApiError::new(
                502,
                "api_error",
                "turn_failed",
                "the backing turn ended without a result",
            )
        };
        let settled = rx
            .wait_for(|state| match state {
                TurnState::Running => false,
                TurnState::Dialog { seq, .. } => *seq > after_seq,
                TurnState::Done(_) => true,
            })
            .await
            .is_ok();
        if !settled {
            return Err(no_result());
        }
        match &*rx.borrow() {
            TurnState::Dialog { reply, .. } => {
                let (agent_name, tab_label) = self.agent_labels(id);
                Ok(ChatOutcome {
                    reply: reply.clone(),
                    conversation: id,
                    agent_name,
                    tab_label,
                })
            }
            TurnState::Done(result) => result.clone(),
            TurnState::Running => Err(no_result()),
        }
    }

    async fn execute_turn(self: &Arc<Self>, plan: StartPlan, req: NormalizedRequest) -> TurnResult {
        let id = plan.id;
        let generation = plan.generation;
        let mut prompt_text = plan.prompt_text;
        let mut agent = match plan.job {
            TurnJob::SpawnNew => match self.spawn_agent_for(id).await {
                Ok(handle) => handle,
                Err(LaunchError::Blocked { handle }) => {
                    self.resolve_startup_blocked(id, generation, handle, &plan.tx, &req, true)
                        .await?
                }
                Err(LaunchError::Failed(err)) => {
                    // Only a record this request created is dropped; a model
                    // switch keeps its id and history so the next request can
                    // retry on the same conversation.
                    if plan.new_record && !self.is_superseded(id, generation) {
                        self.locked_registry().remove(id);
                        self.locked_clients().remove(&id);
                    }
                    return Err(err);
                }
            },
            TurnJob::RunExisting { agent } => {
                let mut agent = *agent;
                if !agent.ready {
                    // Startup was blocked earlier: await the SAME agent, never
                    // a new tab, with the same post-dialog settle rule the
                    // startup turn uses (idle/done for `POST_DIALOG_SETTLE`).
                    agent = self
                        .resolve_startup_blocked(id, generation, agent, &plan.tx, &req, false)
                        .await?;
                }
                agent
            }
        };
        if plan.interrupt {
            // This turn replaces an abandoned one: stop the agent's current work
            // before queueing the new prompt.
            self.interrupt_agent(&agent).await;
        }
        // One turn at a time may touch this conversation's agent: a superseding
        // turn waits here for the turn it replaced to finish.
        let _turn_guard = plan.turn_lock.lock().await;
        // An agent that exited since the last turn is replaced inside its own
        // pane, resuming its session when the gateway knows the id.
        let gone = match self.herdr.agent_get(&agent.name).await {
            Ok(_) => false,
            Err(HerdrError::Api { code, .. }) if code == "agent_not_found" => {
                // Mirror spawn polling: the name may lag the pane id.
                self.herdr.agent_get(&agent.pane_id).await.is_err()
            }
            Err(_) => false,
        };
        if gone {
            // The pane is still there, so put a new CLI in it instead of opening
            // another tab; resume restores the CLI's own context when its session
            // id is known.
            let resume = agent.session_id.clone();
            let replay = |req: &NormalizedRequest| {
                self.locked_bridge().remove(&id);
                // A CLI with nothing resumed has seen none of this conversation,
                // so a later request must replay too if this turn fails.
                self.locked_registry().reset_consumed(id);
                if self.model_mode(&req.model) == ModelMode::Bridge {
                    bridge::bridge_first_prompt(req, true)
                } else {
                    prompt::self_exec_first_prompt(req, true)
                }
            };
            match self
                .restart_agent_in_place_for(id, &mut agent, resume.as_deref())
                .await
            {
                Ok(()) => {
                    if resume.is_none() {
                        prompt_text = replay(&req);
                    }
                }
                Err(LaunchError::Blocked { handle }) => {
                    agent = self
                        .resolve_startup_blocked(id, generation, handle, &plan.tx, &req, true)
                        .await?;
                    // Same rule as the Ok arm: a restarted CLI that was resumed
                    // already holds the conversation, one that was not needs the
                    // whole history in its first prompt.
                    if resume.is_none() {
                        prompt_text = replay(&req);
                    }
                }
                Err(LaunchError::Failed(err)) => {
                    tracing::warn!(
                        name = %agent.name,
                        error = %err.message,
                        "cannot restart the agent in its pane; opening a new one"
                    );
                    lifecycle::close_agent(&*self.herdr, &agent, &self.owner_id).await;
                    agent = match self.spawn_agent_for(id).await {
                        Ok(handle) => handle,
                        Err(LaunchError::Blocked { handle }) => {
                            self.resolve_startup_blocked(
                                id, generation, handle, &plan.tx, &req, true,
                            )
                            .await?
                        }
                        // Respawn failure keeps the record; the task wrapper clears the
                        // in-flight marker so a following request is not stuck Busy.
                        Err(LaunchError::Failed(err)) => return Err(err),
                    };
                    prompt_text = replay(&req);
                }
            }
        }
        let roots = self
            .transcript_roots
            .get(&agent.model_id)
            .cloned()
            .unwrap_or_else(|| TranscriptRoots::Claude(Vec::new()));
        let hook = self.dialog_hook(id, generation, &plan.tx, &req, &agent);
        let outcome = turn::run_turn(
            &*self.herdr,
            &mut agent,
            &prompt_text,
            &self.turn_limits,
            &roots,
            &hook,
        )
        .await;
        // Keep transcript/session follow-ups (continued-in) for the next turn.
        // An abandoned turn must not put its handle back: the turn that replaced
        // it owns this conversation's agent now.
        if !self.is_superseded(id, generation) {
            self.register_agent_or_close(id, &agent).await;
        }
        let bridge = self.model_mode(&req.model) == ModelMode::Bridge;
        let reply = match outcome {
            Ok(TurnOutcome::Completed { text, stop }) if bridge => {
                self.bridge_reply(&req, &mut agent, text, stop, &roots, &hook)
                    .await
            }
            Ok(TurnOutcome::Completed { text, stop }) => AssistantReply {
                text: Some(text),
                tool_calls: Vec::new(),
                finish_reason: stop_finish(stop),
            },
            // Non-error outcomes are committed like normal replies so a client
            // retry finds them in the Retry cache instead of re-prompting the agent.
            Ok(TurnOutcome::Aborted { reason }) => explanatory(format!(
                "[herdr-openai-gateway] The backing agent's turn was interrupted ({reason}). \
                 Send your message again to continue."
            )),
            Ok(TurnOutcome::BlockedTimeout) => {
                // A relayed dialog the turn stopped waiting for stays pending,
                // marked expired, so a late answer gets the explanatory reply.
                self.locked_registry().expire_dialog(id);
                explanatory(format!(
                    "[herdr-openai-gateway] The backing agent {} (herdr workspace {}, tab {}) is \
                     waiting for approval. Approve or deny it in herdr, then send your \
                     message again.",
                    agent.name, self.config.herdr.workspace_label, agent.tab_label,
                ))
            }
            Ok(TurnOutcome::UpstreamError { message }) => explanatory(format!(
                "[herdr-openai-gateway] The backing agent reported an upstream error: {message}"
            )),
            Err(turn_error) => return Err(turn_error.into_api_error()),
        };
        if self.is_superseded(id, generation) {
            tracing::info!(
                name = %agent.name,
                "this turn was superseded; its reply is dropped"
            );
        } else {
            self.cancel_pending_hook_question(id, generation);
            self.locked_registry()
                .commit(id, reply.clone(), Instant::now());
            if bridge {
                // Remember what the agent has seen so the next Continue only sends changes.
                self.locked_bridge().insert(
                    id,
                    BridgeSent {
                        system_digest: bridge::system_digest(&req),
                        tools_digest: bridge::tools_digest(&req),
                    },
                );
            }
        }
        Ok(ChatOutcome {
            conversation: id,
            reply,
            agent_name: agent.name.clone(),
            tab_label: agent.tab_label.clone(),
        })
    }

    /// Bridge mode: parse the agent's text into tool calls, with exactly one
    /// repair turn when the block is invalid or a required call is missing.
    async fn bridge_reply(
        &self,
        req: &NormalizedRequest,
        agent: &mut AgentHandle,
        text: String,
        stop: StopKind,
        roots: &TranscriptRoots,
        hook: &DialogHook<H>,
    ) -> AssistantReply {
        let parsed = toolcall::parse_reply(&text, &req.tools, req.parallel_tool_calls);
        if !parsed.calls.is_empty() {
            let tool_calls = toolcall::to_tool_calls(&parsed);
            return AssistantReply {
                text: parsed.content,
                tool_calls,
                finish_reason: FinishReason::ToolCalls,
            };
        }
        let needs_repair = parsed.error.is_some() || tool_choice_requires(req);
        if !needs_repair {
            return AssistantReply {
                text: Some(text),
                tool_calls: Vec::new(),
                finish_reason: stop_finish(stop),
            };
        }
        let reason = parsed
            .error
            .unwrap_or_else(|| "a tool call is required".to_string());
        let repair_prompt = format!(
            "Your previous reply contained an invalid <tool_calls> block: {reason}. \
             Re-send the full reply following the protocol exactly."
        );
        match turn::run_turn(
            &*self.herdr,
            agent,
            &repair_prompt,
            &self.turn_limits,
            roots,
            hook,
        )
        .await
        {
            Ok(TurnOutcome::Completed {
                text: repair_text,
                stop: repair_stop,
            }) => {
                let repaired =
                    toolcall::parse_reply(&repair_text, &req.tools, req.parallel_tool_calls);
                if !repaired.calls.is_empty() {
                    let tool_calls = toolcall::to_tool_calls(&repaired);
                    AssistantReply {
                        text: repaired.content,
                        tool_calls,
                        finish_reason: FinishReason::ToolCalls,
                    }
                } else {
                    // Still no usable block: the raw repair text becomes the answer.
                    AssistantReply {
                        text: Some(repair_text),
                        tool_calls: Vec::new(),
                        finish_reason: stop_finish(repair_stop),
                    }
                }
            }
            // The repair turn itself failed: fall back to the original raw text.
            _ => AssistantReply {
                text: Some(text),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
            },
        }
    }

    fn model_mode(&self, model_id: &str) -> ModelMode {
        self.config
            .models
            .iter()
            .find(|model| model.id == model_id)
            .map(|model| model.mode())
            .unwrap_or(ModelMode::SelfExec)
    }

    /// Compare the request's System text and tools JSON against what the bridge
    /// agent was last sent.
    fn bridge_changes(&self, id: ConversationId, req: &NormalizedRequest) -> BridgeChanges {
        let stored = self.locked_bridge().get(&id).cloned();
        BridgeChanges {
            system_changed: stored
                .as_ref()
                .map(|sent| sent.system_digest != bridge::system_digest(req))
                .unwrap_or(true),
            tools_changed: stored
                .as_ref()
                .map(|sent| sent.tools_digest != bridge::tools_digest(req))
                .unwrap_or(true),
        }
    }

    fn agent_labels(&self, id: ConversationId) -> (String, String) {
        self.locked_agents()
            .get(&id)
            .map(|handle| (handle.name.clone(), handle.tab_label.clone()))
            .unwrap_or_default()
    }

    fn locked_registry(&self) -> MutexGuard<'_, Registry> {
        lock(&self.registry)
    }

    fn locked_agents(&self) -> MutexGuard<'_, BTreeMap<ConversationId, AgentHandle>> {
        lock(&self.agents)
    }

    fn locked_bridge(&self) -> MutexGuard<'_, BTreeMap<ConversationId, BridgeSent>> {
        lock(&self.bridge_sent)
    }

    fn locked_clients(&self) -> MutexGuard<'_, BTreeMap<ConversationId, TrackedClient>> {
        lock(&self.clients)
    }

    fn insert_agent_if_registered(&self, id: ConversationId, handle: AgentHandle) -> bool {
        let registry = self.locked_registry();
        if registry.get(id).is_none() {
            return false;
        }
        self.locked_agents().insert(id, handle);
        true
    }

    async fn close_unregistered_agent(&self, handle: &AgentHandle) {
        tracing::info!(
            name = %handle.name,
            "closing backing agent after its conversation was removed"
        );
        lifecycle::close_agent(&*self.herdr, handle, &self.owner_id).await;
    }

    async fn register_agent_or_close(&self, id: ConversationId, handle: &AgentHandle) -> bool {
        if self.insert_agent_if_registered(id, handle.clone()) {
            true
        } else {
            self.close_unregistered_agent(handle).await;
            false
        }
    }

    fn locked_inflight(&self) -> MutexGuard<'_, BTreeMap<ConversationId, InflightTurn>> {
        lock(&self.inflight)
    }

    fn locked_workspace(&self) -> MutexGuard<'_, Option<String>> {
        lock(&self.workspace_id)
    }
}

/// Relays a blocked agent's dialog to the client on the turn's watch channel.
/// The server never picks an option: it only publishes what the agent shows.
struct DialogHook<H: HerdrApi> {
    service: Arc<Service<H>>,
    id: ConversationId,
    generation: u64,
    tx: watch::Sender<TurnState>,
    question_offered: bool,
    agent_label: String,
    tab_label: String,
    pause: Arc<HookPause>,
}

impl<H: HerdrApi> DialogHook<H> {
    /// Publish an already-parsed dialog as this turn's next relay.
    fn relay(&self, parsed: Dialog) {
        let (reply, call_id) = build_relay(
            self.question_offered,
            &self.agent_label,
            &self.tab_label,
            &parsed,
            None,
        );
        let seq = {
            let mut registry = self.service.locked_registry();
            // The seq counter only ever increases: `answer_dialog` takes the
            // pending entry out, and the answering request waits for a relay
            // with a seq greater than the one it already received.
            match registry.publish_dialog(
                self.id,
                self.generation,
                parsed,
                reply.clone(),
                call_id,
                None,
            ) {
                Some(seq) => seq,
                None => return,
            }
        };
        let _ = self.tx.send(TurnState::Dialog { reply, seq });
    }
}

impl<H: HerdrApi> BlockedHook for DialogHook<H> {
    async fn on_blocked(&self, agent: &AgentHandle) {
        if self.pause.is_active() {
            return;
        }
        let screen = match self.service.read_agent_screen(agent).await {
            Ok(screen) => screen,
            Err(err) => {
                // The user can still answer in the herdr tab; the turn just
                // keeps waiting as before.
                tracing::warn!(name = %agent.name, "cannot read the blocked agent screen: {err}");
                return;
            }
        };
        self.relay(dialog::parse_dialog(&screen));
    }

    fn paused(&self) -> Duration {
        self.pause.paused()
    }

    fn session_updated(&self, agent: &AgentHandle) {
        if !self
            .service
            .insert_agent_if_registered(self.id, agent.clone())
        {
            let service = Arc::clone(&self.service);
            let agent = agent.clone();
            tokio::spawn(async move {
                service.close_unregistered_agent(&agent).await;
            });
        }
    }
}

/// Build the client-facing relay: a `question` tool call when the client offers
/// that tool, numbered text otherwise. Returns the reply and the tool call id
/// when one was generated.
fn build_relay(
    question_offered: bool,
    agent_label: &str,
    tab_label: &str,
    dialog: &Dialog,
    note: Option<&str>,
) -> (AssistantReply, Option<String>) {
    if question_offered {
        let call_id = toolcall::new_call_id();
        let reply = AssistantReply {
            text: None,
            tool_calls: vec![ToolCall {
                id: call_id.clone(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "question".to_string(),
                    arguments: dialog::relay_tool_arguments(agent_label, dialog).to_string(),
                },
            }],
            finish_reason: FinishReason::ToolCalls,
        };
        (reply, Some(call_id))
    } else {
        let reply = AssistantReply {
            text: Some(dialog::relay_text(agent_label, tab_label, dialog, note)),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
        };
        (reply, None)
    }
}

fn hook_question_reply(tool_input: &Value) -> (AssistantReply, String) {
    let call_id = toolcall::new_call_id();
    let reply = AssistantReply {
        text: None,
        tool_calls: vec![ToolCall {
            id: call_id.clone(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "question".to_string(),
                arguments: question_hook::question_tool_arguments(tool_input).to_string(),
            },
        }],
        finish_reason: FinishReason::ToolCalls,
    };
    (reply, call_id)
}

/// True when a herdr error says the agent no longer exists.
fn agent_gone(err: &HerdrError) -> bool {
    matches!(
        err,
        HerdrError::Api { code, .. }
            if code == "agent_not_found" || code == "agent_not_running"
    )
}

/// Poisoned locks only ever guard plain maps; recover the inner value.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn stop_finish(stop: StopKind) -> FinishReason {
    if stop == StopKind::MaxTokens {
        FinishReason::Length
    } else {
        FinishReason::Stop
    }
}

/// True when `tool_choice` demands a call: `"required"` or a named function.
fn tool_choice_requires(req: &NormalizedRequest) -> bool {
    match &req.tool_choice {
        Some(choice) => {
            choice.as_str() == Some("required")
                || choice.get("type").and_then(|value| value.as_str()) == Some("function")
        }
        None => false,
    }
}

fn explanatory(text: String) -> AssistantReply {
    AssistantReply {
        text: Some(text),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
    }
}

fn is_loopback_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
    }
}

fn hook_answer_not_delivered(tab_label: &str) -> AssistantReply {
    explanatory(format!(
        "[herdr-openai-gateway] Claude stopped waiting for this question before your answer arrived, so the answer was not delivered (for example, its session in herdr tab {tab_label} was interrupted or moved to the background). Check that tab, or send a new message to continue."
    ))
}

fn stale_workspace(err: &SpawnError) -> bool {
    matches!(
        err,
        SpawnError::Herdr(HerdrError::Api { code, .. })
            if code == "workspace_not_found" || code == "tab_not_found"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentKind;
    use crate::herdr::fake::{FakeDialog, FakeHerdr, ScriptedTurn, StartupBehavior};
    use crate::openai::{FunctionDef, NormalizedMessage, ToolDef};
    use std::path::Path;

    struct Fixture {
        dir: PathBuf,
        fake: Arc<FakeHerdr>,
        service: Arc<Service<FakeHerdr>>,
    }

    #[cfg(target_os = "linux")]
    struct FakeClientProc {
        root: PathBuf,
        pid: u32,
        peer: SocketAddr,
        server: SocketAddr,
    }

    #[cfg(target_os = "linux")]
    impl FakeClientProc {
        fn new() -> Self {
            use std::os::unix::fs::symlink;

            let root = std::env::temp_dir().join(format!(
                "herdr-openai-gateway-client-proc-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(root.join("net")).unwrap();
            std::fs::create_dir_all(root.join(std::process::id().to_string())).unwrap();
            symlink(std::process::id().to_string(), root.join("self")).unwrap();
            let mut proc = Self {
                root,
                pid: std::process::id() + 1_000_000,
                peer: "127.0.0.1:51000".parse().unwrap(),
                server: "127.0.0.1:8787".parse().unwrap(),
            };
            proc.set_alive();
            proc
        }

        fn set_alive(&mut self) {
            use std::os::unix::fs::symlink;

            let pid_dir = self.root.join(self.pid.to_string());
            let fd_dir = pid_dir.join("fd");
            std::fs::create_dir_all(&fd_dir).unwrap();
            let fields = std::iter::once("S".to_string())
                .chain((0..18).map(|_| "1".to_string()))
                .chain(std::iter::once("77".to_string()))
                .collect::<Vec<_>>();
            std::fs::write(
                pid_dir.join("stat"),
                format!(
                    "{} (opencode serve --stdio) {}\n",
                    self.pid,
                    fields.join(" ")
                ),
            )
            .unwrap();
            symlink("socket:[4321]", fd_dir.join("3")).unwrap();
            std::fs::write(
                self.root.join("net/tcp"),
                format!(
                    "sl local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n0: {} {} 01 00000000:00000000 00:00000000 00000000 1000 0 4321\n",
                    proc_endpoint(self.peer),
                    proc_endpoint(self.server)
                ),
            )
            .unwrap();
        }

        fn exit(&self) {
            std::fs::remove_dir_all(self.root.join(self.pid.to_string())).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for FakeClientProc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(target_os = "linux")]
    fn proc_endpoint(address: SocketAddr) -> String {
        let octets = match address.ip() {
            std::net::IpAddr::V4(ip) => ip.octets().into_iter().rev().collect::<Vec<_>>(),
            std::net::IpAddr::V6(_) => unreachable!(),
        };
        let ip = octets
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        format!("{ip}:{:04X}", address.port())
    }

    #[cfg(target_os = "linux")]
    fn enable_client_tracking(fixture: &mut Fixture, proc: &FakeClientProc) {
        let service = Arc::get_mut(&mut fixture.service).expect("unique service");
        service.env.proc_root = Some(proc.root.clone());
        service.env.server_addr = Some(proc.server);
    }

    #[cfg(target_os = "linux")]
    fn opencode_req(
        key: &str,
        messages: Vec<NormalizedMessage>,
        peer: SocketAddr,
    ) -> NormalizedRequest {
        let mut request = req("claude+agent-tools", Some(key), messages);
        request.client_peer = Some(peer);
        request.opencode_session = true;
        request
    }

    impl Fixture {
        fn new(config: Config, fake: FakeHerdr) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-openai-gateway-svc-{}-{}",
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
                api_key: None,
                log_requests: false,
                question_hook: None,
                proc_root: None,
                server_addr: None,
            };
            let service = Service::new(config, Arc::clone(&fake), env);
            Self { dir, fake, service }
        }

        fn cleanup(&self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn hook_fixture(delay_ms: u64) -> Fixture {
        hook_fixture_with_turns([ScriptedTurn::SlowReply {
            delay_ms,
            text: "late answer".to_string(),
        }])
    }

    fn hook_fixture_with_turns(turns: impl IntoIterator<Item = ScriptedTurn>) -> Fixture {
        let mut config = Config::builtin();
        config.herdr.blocked_timeout_ms = 60_000;
        let mut fixture = Fixture::new(config, FakeHerdr::new().with_turns(turns));
        Arc::get_mut(&mut fixture.service)
            .expect("unique service")
            .env
            .question_hook = Some(QuestionHookEnv {
            url: "http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use".to_string(),
            token: "test-hook-token".to_string(),
        });
        fixture
    }

    fn question_tool_def() -> ToolDef {
        ToolDef {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "question".to_string(),
                description: None,
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {"questions": {"type": "array"}}
                })),
            },
        }
    }

    fn hook_tool_input() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "Question 1",
                    "header": "First",
                    "options": [{"label": "A", "description": "First option"}],
                    "multiSelect": false
                },
                {
                    "question": "Question 2",
                    "header": "Second",
                    "options": [{"label": "B", "description": "Second option"}],
                    "multiSelect": false
                }
            ]
        })
    }

    async fn start_pending_hook(
        fixture: &Fixture,
        tool_input: Value,
        key: &str,
    ) -> (
        ConversationId,
        String,
        ChatOutcome,
        tokio::task::JoinHandle<Option<Value>>,
    ) {
        let mut request = req("claude+agent-tools", Some(key), vec![user("hello")]);
        request.tools.push(question_tool_def());
        let chat_service = Arc::clone(&fixture.service);
        let chat_task = tokio::spawn(async move { chat_service.chat(request).await });
        wait_for_prompt(&fixture.fake).await;
        let (id, session_id) = fixture
            .service
            .locked_agents()
            .iter()
            .next()
            .map(|(id, agent)| (*id, agent.session_id.clone().expect("session id")))
            .expect("agent");
        let hook_service = Arc::clone(&fixture.service);
        let hook_session_id = session_id.clone();
        let hook_task = tokio::spawn(async move {
            hook_service
                .handle_question_hook(HookInput {
                    session_id: hook_session_id,
                    tool_name: "AskUserQuestion".to_string(),
                    tool_use_id: "toolu_test".to_string(),
                    tool_input,
                })
                .await
        });
        let outcome = tokio::time::timeout(Duration::from_secs(2), chat_task)
            .await
            .expect("chat should receive the question relay")
            .expect("chat task")
            .expect("chat outcome");
        let tab = fixture
            .fake
            .calls()
            .into_iter()
            .find(|call| call.method == "tab.create")
            .expect("tab.create");
        assert_eq!(tab.param("env.CLAUDE_CODE_DISABLE_AGENT_VIEW"), Some("1"));
        (id, session_id, outcome, hook_task)
    }

    fn hook_answer_request(key: &str, outcome: &ChatOutcome, content: &str) -> NormalizedRequest {
        let call = &outcome.reply.tool_calls[0];
        req(
            "claude+agent-tools",
            Some(key),
            vec![
                user("hello"),
                NormalizedMessage::Assistant {
                    text: None,
                    tool_calls: outcome.reply.tool_calls.clone(),
                },
                NormalizedMessage::Tool {
                    call_id: call.id.clone(),
                    name: Some("question".to_string()),
                    content: content.to_string(),
                },
            ],
        )
    }

    fn expected_cancelled_hook_reply(tab_label: &str) -> String {
        format!(
            "[herdr-openai-gateway] Claude stopped waiting for this question before your answer arrived, so the answer was not delivered (for example, its session in herdr tab {tab_label} was interrupted or moved to the background). Check that tab, or send a new message to continue."
        )
    }

    fn user(text: &str) -> NormalizedMessage {
        NormalizedMessage::User(text.to_string())
    }

    fn assistant(text: &str) -> NormalizedMessage {
        NormalizedMessage::Assistant {
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
        }
    }

    fn req(model: &str, key: Option<&str>, messages: Vec<NormalizedMessage>) -> NormalizedRequest {
        NormalizedRequest {
            model: model.to_string(),
            messages,
            stream: false,
            include_usage: false,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: true,
            conversation_key: key.map(String::from),
            cwd_header: None,
            client_cwd: None,
            client_peer: None,
            opencode_session: false,
        }
    }

    fn prompt_calls(fake: &FakeHerdr) -> Vec<String> {
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

    /// Commands typed into a pane's shell, as `(text, pane_id)`.
    fn send_inputs(fake: &FakeHerdr) -> Vec<(String, String)> {
        fake.calls()
            .iter()
            .filter(|c| c.method == "pane.send_input")
            .map(|c| {
                (
                    c.param("text").unwrap_or_default().to_string(),
                    c.param("pane_id").unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    async fn wait_for_prompt(fake: &FakeHerdr) {
        // A post-dialog wait adds a `POST_DIALOG_SETTLE` pause before the
        // first prompt, so the budget must stay above it.
        for _ in 0..1200 {
            if fake.calls().iter().any(|c| c.method == "agent.prompt") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("agent.prompt was never recorded");
    }

    async fn wait_for_call(fake: &FakeHerdr, method: &str) {
        for _ in 0..400 {
            if fake.calls().iter().any(|c| c.method == method) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{method} was never recorded");
    }

    fn tab_create_count(fake: &FakeHerdr) -> usize {
        fake.calls()
            .iter()
            .filter(|c| c.method == "tab.create")
            .count()
    }

    fn tool_def() -> ToolDef {
        ToolDef {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "read".to_string(),
                description: None,
                parameters: None,
            },
        }
    }

    #[tokio::test]
    async fn new_conversation_spawns_and_replies() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("answer"));
        assert_eq!(outcome.reply.finish_reason, FinishReason::Stop);
        assert_eq!(outcome.conversation.to_string(), "c0");
        assert!(outcome.agent_name.starts_with("oa-claude-"));
        assert!(outcome.tab_label.starts_with("oa:claude+agent-tools:"));
        let tab = f
            .fake
            .calls()
            .into_iter()
            .find(|call| call.method == "tab.create")
            .expect("tab.create");
        assert_eq!(tab.param("env.CLAUDE_CODE_DISABLE_AGENT_VIEW"), Some("1"));
        assert_eq!(start_count(&f.fake), 1);
        assert_eq!(prompt_calls(&f.fake), vec!["hello".to_string()]);
        f.cleanup();
    }

    #[tokio::test]
    async fn retry_serves_the_cached_reply_without_re_prompting() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        let request = req("claude+agent-tools", Some("k1"), vec![user("hello")]);
        let first = f.service.chat(request.clone()).await.expect("first chat");
        let second = f.service.chat(request).await.expect("retry chat");
        assert_eq!(first.reply, second.reply);
        assert_eq!(second.conversation, first.conversation);
        assert_eq!(prompt_calls(&f.fake), vec!["hello".to_string()]);
        f.cleanup();
    }

    #[tokio::test]
    async fn continue_sends_only_the_new_user_text() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([
                ScriptedTurn::Reply("first".to_string()),
                ScriptedTurn::Reply("second".to_string()),
            ]),
        );
        f.service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("first chat");
        f.service
            .chat(req(
                "claude+agent-tools",
                Some("k1"),
                vec![user("hello"), assistant("first"), user("next question")],
            ))
            .await
            .expect("second chat");
        assert_eq!(start_count(&f.fake), 1, "no new spawn for a continuation");
        assert_eq!(
            prompt_calls(&f.fake),
            vec!["hello".to_string(), "next question".to_string()]
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn bridge_model_replies_with_tool_calls_and_unknown_model_is_404() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply(
                "thinking\n<tool_calls>[{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}]\
                 </tool_calls>"
                    .to_string(),
            )]),
        );
        let mut request = req("claude+client-tools", Some("k1"), vec![user("read a")]);
        request.tools = vec![tool_def()];
        let outcome = f.service.chat(request).await.expect("bridge chat");
        assert_eq!(outcome.reply.finish_reason, FinishReason::ToolCalls);
        assert_eq!(outcome.reply.text.as_deref(), Some("thinking"));
        assert_eq!(outcome.reply.tool_calls.len(), 1);
        assert_eq!(outcome.reply.tool_calls[0].function.name, "read");
        assert!(outcome.reply.tool_calls[0].id.starts_with("call_"));
        let prompt = prompt_calls(&f.fake)[0].clone();
        assert!(prompt.contains("[herdr-openai-gateway bridge protocol v1]"));
        assert!(prompt.contains("<tools>"));

        let err = f
            .service
            .chat(req("nope", None, vec![user("x")]))
            .await
            .expect_err("unknown");
        assert_eq!((err.status, err.code), (404, "model_not_found"));
        f.cleanup();
    }

    #[tokio::test]
    async fn codex_self_exec_completes_end_to_end() {
        let mut f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("codex answer".to_string())]),
        );
        Arc::get_mut(&mut f.service)
            .expect("unique service")
            .env
            .question_hook = Some(QuestionHookEnv {
            url: "http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use".to_string(),
            token: "test-hook-token".to_string(),
        });
        let outcome = f
            .service
            .chat(req("codex+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("codex chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("codex answer"));
        assert!(outcome.agent_name.starts_with("oa-codex-"));
        assert!(outcome.tab_label.starts_with("oa:codex+agent-tools:"));
        let calls = f.fake.calls();
        let starts: Vec<_> = calls.iter().filter(|c| c.method == "agent.start").collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].param("kind"), Some("codex"));
        assert!(
            !starts[0]
                .param("args")
                .unwrap_or_default()
                .contains("--session-id"),
            "codex gets no --session-id"
        );
        assert!(
            !starts[0]
                .param("args")
                .unwrap_or_default()
                .contains("--settings"),
            "codex gets no Claude hook settings"
        );
        let tab = calls
            .iter()
            .find(|call| call.method == "tab.create")
            .expect("tab.create");
        assert_eq!(tab.param("env.HERDR_OPENAI_GATEWAY_HOOK_TOKEN"), None);
        assert_eq!(tab.param("env.CLAUDE_CODE_DISABLE_AGENT_VIEW"), None);
        f.cleanup();
    }

    #[tokio::test]
    async fn claude_model_env_overrides_agent_view_disable_default() {
        let mut config = Config::builtin();
        let model = config
            .models
            .iter_mut()
            .find(|model| model.id == "claude+agent-tools")
            .expect("builtin Claude model");
        model.env.insert(
            "CLAUDE_CODE_DISABLE_AGENT_VIEW".to_string(),
            "0".to_string(),
        );
        let f = Fixture::new(
            config,
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        f.service
            .chat(req(
                "claude+agent-tools",
                Some("k-env-override"),
                vec![user("hi")],
            ))
            .await
            .expect("chat");
        let tab = f
            .fake
            .calls()
            .into_iter()
            .find(|call| call.method == "tab.create")
            .expect("tab.create");
        assert_eq!(tab.param("env.CLAUDE_CODE_DISABLE_AGENT_VIEW"), Some("0"));
        f.cleanup();
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_hook_questions() {
        let mut config = Config::builtin();
        config.herdr.blocked_timeout_ms = 60_000;
        let mut f = Fixture::new(
            config,
            FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
                delay_ms: 15_000,
                text: "late reply".to_string(),
            }]),
        );
        Arc::get_mut(&mut f.service)
            .expect("unique service")
            .env
            .question_hook = Some(QuestionHookEnv {
            url: "http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use".to_string(),
            token: "test-hook-token".to_string(),
        });
        let mut request = req(
            "claude+agent-tools",
            Some("k-hook-shutdown"),
            vec![user("hello")],
        );
        let mut question_tool = tool_def();
        question_tool.function.name = "question".to_string();
        question_tool.function.parameters = Some(serde_json::json!({
            "type": "object",
            "properties": {"questions": {"type": "array"}}
        }));
        request.tools.push(question_tool);
        let chat_service = Arc::clone(&f.service);
        let chat_task = tokio::spawn(async move { chat_service.chat(request).await });
        wait_for_prompt(&f.fake).await;
        let (id, session_id) = f
            .service
            .locked_agents()
            .iter()
            .next()
            .map(|(id, agent)| (*id, agent.session_id.clone().expect("session id")))
            .expect("agent");
        let hook_service = Arc::clone(&f.service);
        let hook_task = tokio::spawn(async move {
            hook_service
                .handle_question_hook(HookInput {
                    session_id,
                    tool_name: "AskUserQuestion".to_string(),
                    tool_use_id: "toolu_shutdown".to_string(),
                    tool_input: serde_json::json!({
                        "questions": [{"question": "Continue?", "header": "Continue", "options": []}]
                    }),
                })
                .await
        });
        for _ in 0..200 {
            if f.service
                .locked_registry()
                .get(id)
                .is_some_and(|record| record.pending_dialog.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            f.service
                .locked_registry()
                .get(id)
                .is_some_and(|record| record.pending_dialog.is_some()),
            "hook question should be pending before shutdown"
        );
        let _chat = tokio::time::timeout(Duration::from_secs(1), chat_task)
            .await
            .expect("client should receive the question relay")
            .expect("chat task");
        f.service.cancel_all_hook_questions();
        let response = tokio::time::timeout(Duration::from_millis(500), hook_task)
            .await
            .expect("hook should wake without waiting for blocked_timeout_ms")
            .expect("hook task")
            .expect("hook response");
        assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            response["hookSpecificOutput"]["permissionDecisionReason"],
            "No answer from the user in time"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn dropping_hook_request_expires_dialog_and_finishes_pause() {
        let f = hook_fixture(15_000);
        let (id, _, _, hook_task) = start_pending_hook(&f, hook_tool_input(), "k-hook-drop").await;
        let pause = Arc::clone(&f.service.locked_inflight()[&id].hook_pause);
        assert!(pause.is_active());
        hook_task.abort();
        assert!(hook_task.await.expect_err("aborted hook").is_cancelled());

        let pending = f
            .service
            .locked_registry()
            .get(id)
            .unwrap()
            .pending_dialog
            .clone()
            .unwrap();
        assert!(pending.expired);
        assert!(pending.hook.unwrap().lock().unwrap().is_none());
        assert!(!pause.is_active());
        let paused = pause.paused();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pause.paused(), paused);
        f.cleanup();
    }

    #[tokio::test]
    async fn cancelled_hook_question_can_be_replaced() {
        let f = hook_fixture(15_000);
        let (id, session_id, _, hook_task) =
            start_pending_hook(&f, hook_tool_input(), "k-hook-replace").await;
        hook_task.abort();
        let _ = hook_task.await;
        let mut state_rx = f.service.locked_inflight()[&id].tx.subscribe();
        let service = Arc::clone(&f.service);
        let mut input = hook_tool_input();
        input["questions"][0]["question"] = Value::String("Replacement question".to_string());
        let hook_task = tokio::spawn(async move {
            service
                .handle_question_hook(HookInput {
                    session_id,
                    tool_name: "AskUserQuestion".to_string(),
                    tool_use_id: "toolu_replacement".to_string(),
                    tool_input: input,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), state_rx.changed())
            .await
            .expect("replacement relay should be published")
            .expect("turn state");
        let TurnState::Dialog { reply, .. } = state_rx.borrow().clone() else {
            panic!("replacement should publish a dialog relay");
        };
        let arguments: Value = serde_json::from_str(&reply.tool_calls[0].function.arguments)
            .expect("question arguments");
        assert_eq!(
            arguments["questions"][0]["question"],
            "Replacement question"
        );
        hook_task.abort();
        let _ = hook_task.await;
        f.cleanup();
    }

    #[tokio::test]
    async fn cancelled_hook_answer_explains_and_does_not_commit_inflight_turn() {
        let f = hook_fixture(15_000);
        let (id, _, outcome, hook_task) =
            start_pending_hook(&f, hook_tool_input(), "k-hook-late-answer").await;
        hook_task.abort();
        let _ = hook_task.await;
        let answer = f
            .service
            .chat(hook_answer_request(
                "k-hook-late-answer",
                &outcome,
                "User has answered your questions: \"Question 1\"=\"A\", \"Question 2\"=\"B\".",
            ))
            .await
            .expect("late answer should be explanatory success");
        assert_eq!(
            answer.reply.text.as_deref(),
            Some(expected_cancelled_hook_reply(&outcome.tab_label).as_str())
        );
        assert!(f.service.locked_inflight().contains_key(&id));
        assert!(f
            .service
            .locked_registry()
            .get(id)
            .unwrap()
            .last_reply
            .is_none());
        f.cleanup();
    }

    #[tokio::test]
    async fn hook_answer_race_with_missing_question_returns_explanation() {
        let f = hook_fixture(15_000);
        let (id, _, outcome, hook_task) =
            start_pending_hook(&f, hook_tool_input(), "k-hook-race").await;
        let call = &outcome.reply.tool_calls[0];
        let request = hook_answer_request(
            "k-hook-race",
            &outcome,
            "User has answered your questions: \"Question 1\"=\"A\", \"Question 2\"=\"B\".",
        );
        let Reserved::Answer {
            id: reserved_id,
            pending,
            snapshot,
            answer,
            rx,
            agent,
        } = f.service.reserve(&request)
        else {
            panic!("hook reply should reserve an answer");
        };
        assert_eq!(reserved_id, id);
        assert_eq!(pending.call_id.as_deref(), Some(call.id.as_str()));
        pending.hook.as_ref().unwrap().lock().unwrap().take();
        let applied = f
            .service
            .apply_dialog_answer(id, &pending, answer, rx, &agent, &snapshot)
            .await
            .expect("expired hook answer should succeed");
        assert_eq!(
            applied.reply.text.as_deref(),
            Some(expected_cancelled_hook_reply(&outcome.tab_label).as_str())
        );
        let hook_result = tokio::time::timeout(Duration::from_secs(1), hook_task)
            .await
            .expect("hook wait should be released")
            .expect("hook task")
            .expect("hook response");
        assert_eq!(
            hook_result["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn cancelled_hook_dialog_allows_new_message_to_supersede_orphaned_turn() {
        let f = hook_fixture_with_turns([
            ScriptedTurn::SlowReply {
                delay_ms: 15_000,
                text: "abandoned answer".to_string(),
            },
            ScriptedTurn::Reply("new reply".to_string()),
        ]);
        let (id, _, outcome, hook_task) =
            start_pending_hook(&f, hook_tool_input(), "k-hook-supersede").await;
        hook_task.abort();
        let _ = hook_task.await;

        let mut answer_messages = hook_answer_request(
            "k-hook-supersede",
            &outcome,
            "User has answered your questions: \"Question 1\"=\"A\", \"Question 2\"=\"B\".",
        )
        .messages;
        let explanation = expected_cancelled_hook_reply(&outcome.tab_label);
        let answer = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k-hook-supersede"),
                answer_messages.clone(),
            ))
            .await
            .expect("cancelled answer should get an explanation");
        assert_eq!(answer.reply.text.as_deref(), Some(explanation.as_str()));
        answer_messages.push(NormalizedMessage::Assistant {
            text: Some(explanation),
            tool_calls: Vec::new(),
        });
        answer_messages.push(user("continue after interruption"));

        let continued = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k-hook-supersede"),
                answer_messages,
            ))
            .await
            .expect("new message should supersede the orphaned turn");
        assert_eq!(continued.reply.text.as_deref(), Some("new reply"));
        let keys = keys_sent(&f.fake);
        assert!(keys.iter().any(|keys| keys == "esc"));
        assert!(keys.iter().any(|keys| keys == "ctrl+c"));
        assert_eq!(
            prompt_calls(&f.fake).last().map(String::as_str),
            Some("continue after interruption")
        );
        assert!(f
            .service
            .locked_registry()
            .get(id)
            .unwrap()
            .pending_dialog
            .is_none());
        f.cleanup();
    }

    #[tokio::test]
    async fn live_hook_dialog_still_blocks_orphaned_turn_supersede() {
        let f = hook_fixture(15_000);
        let (id, _, _, hook_task) =
            start_pending_hook(&f, hook_tool_input(), "k-hook-live-block").await;
        let busy = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k-hook-live-block"),
                vec![user("hello"), user("new message")],
            ))
            .await
            .expect_err("live hook dialog must block supersede");
        assert_eq!((busy.status, busy.code), (409, "conversation_busy"));
        assert!(
            keys_sent(&f.fake).is_empty(),
            "live hook is not interrupted"
        );
        assert!(f
            .service
            .orphaned_turn_lock(&f.service.locked_registry(), id)
            .is_none());
        hook_task.abort();
        let _ = hook_task.await;
        f.cleanup();
    }

    #[tokio::test]
    async fn second_request_while_inflight_is_busy() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
                delay_ms: 300,
                text: "slow".to_string(),
            }]),
        );
        let service = Arc::clone(&f.service);
        let first = tokio::spawn(async move {
            service
                .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
                .await
        });
        wait_for_prompt(&f.fake).await;
        let busy = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k1"),
                vec![user("hello"), user("while busy")],
            ))
            .await
            .expect_err("busy");
        assert_eq!((busy.status, busy.code), (409, "conversation_busy"));
        let outcome = first.await.expect("task").expect("first chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("slow"));
        f.cleanup();
    }

    #[tokio::test]
    async fn concurrent_keyed_requests_during_spawn_reserve_once() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup_delay(400)
                .with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        let service = Arc::clone(&f.service);
        let first = tokio::spawn(async move {
            let mut request = req("claude+agent-tools", Some("k1"), vec![user("hello")]);
            request.tools = vec![tool_def()];
            service.chat(request).await
        });
        wait_for_call(&f.fake, "agent.start").await;
        let mut second = req(
            "claude+agent-tools",
            Some("k1"),
            vec![user("hello"), user("extra while busy")],
        );
        second.tools = vec![tool_def()];
        let busy = f.service.chat(second).await.expect_err("busy");
        assert_eq!((busy.status, busy.code), (409, "conversation_busy"));
        let outcome = first.await.expect("task").expect("first chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("answer"));
        assert_eq!(start_count(&f.fake), 1, "one agent for one conversation");
        f.cleanup();
    }

    #[tokio::test]
    async fn tool_less_keyed_request_coalesces_behind_the_main_request() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        let service = Arc::clone(&f.service);
        // opencode fires the tool-less title request first, ~10 ms before the main one.
        let title = tokio::spawn(async move {
            service
                .chat(req(
                    "claude+agent-tools",
                    Some("k1"),
                    vec![user("same text")],
                ))
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut main = req("claude+agent-tools", Some("k1"), vec![user("same text")]);
        main.tools = vec![tool_def()];
        let outcome = f.service.chat(main).await.expect("main chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("answer"));
        let side = title.await.expect("task").expect_err("title must lose");
        assert_eq!(side.status, 409);
        assert!(
            matches!(side.code, "conversation_busy" | "conversation_side_request"),
            "got {}",
            side.code
        );
        assert_eq!(start_count(&f.fake), 1, "the title request must not spawn");
        f.cleanup();
    }

    #[tokio::test]
    async fn identical_concurrent_duplicates_join_the_same_turn() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup_delay(400)
                .with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        let service = Arc::clone(&f.service);
        let first = tokio::spawn(async move {
            service
                .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
                .await
        });
        wait_for_call(&f.fake, "agent.start").await;
        let second = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("join");
        let first = first.await.expect("task").expect("first chat");
        assert_eq!(first.reply.text.as_deref(), Some("answer"));
        assert_eq!(second.reply, first.reply);
        assert_eq!(second.conversation, first.conversation);
        assert_eq!(start_count(&f.fake), 1);
        f.cleanup();
    }

    #[tokio::test]
    async fn respawn_failure_clears_inflight_for_the_next_request() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([
                ScriptedTurn::Reply("one".to_string()),
                ScriptedTurn::Exit,
                ScriptedTurn::Reply("two".to_string()),
            ]),
        );
        let first = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("first chat");
        let err = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k1"),
                vec![user("hello"), assistant("one"), user("during exit")],
            ))
            .await
            .expect_err("exited mid-turn");
        assert_eq!((err.status, err.code), (502, "backing_agent_exited"));
        // The respawn cannot start a new agent; the error must not leave the
        // conversation stuck as Busy. The tab is closed, so there is no pane to
        // restart into and the respawn goes straight to a new launch.
        let tab_id = f
            .service
            .locked_agents()
            .get(&first.conversation)
            .expect("agent")
            .tab_id
            .clone();
        f.fake.tab_close(&tab_id).await.expect("close tab");
        f.fake.set_startup(StartupBehavior::Fails);
        let follow = req(
            "claude+agent-tools",
            Some("k1"),
            vec![user("hello"), assistant("one"), user("after exit")],
        );
        let err = f
            .service
            .chat(follow.clone())
            .await
            .expect_err("respawn fails");
        assert_eq!((err.status, err.code), (502, "agent_start_failed"));
        f.fake.set_startup(StartupBehavior::Ready);
        let outcome = f.service.chat(follow).await.expect("not stuck busy");
        assert_eq!(outcome.reply.text.as_deref(), Some("two"));
        f.cleanup();
    }

    #[tokio::test]
    async fn startup_blocked_relay_reuses_the_same_tab_and_first_prompt() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::BlockedUntilPolled(1))
                .with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        let request = req(
            "claude+agent-tools",
            Some("k1"),
            vec![user("one"), user("two")],
        );
        // The blocked startup is relayed instead of failing: the screen has no
        // options, so the relay points the user at the herdr tab.
        let relay = f.service.chat(request.clone()).await.expect("relay");
        assert_eq!(relay.conversation.to_string(), "c0");
        let text = relay.reply.text.expect("relay text");
        assert!(
            text.contains("This dialog needs input in the herdr TUI"),
            "got {text}"
        );
        let blocked_tabs: Vec<String> = f
            .fake
            .tab_labels("w1")
            .into_iter()
            .filter(|label| label.starts_with("oa:"))
            .collect();
        assert_eq!(blocked_tabs.len(), 1, "the blocked tab is kept");
        assert!(text.contains(&blocked_tabs[0]), "the relay names the tab");

        // The block clears while the turn waits; the same turn then completes.
        wait_for_prompt(&f.fake).await;
        let outcome = f.service.chat(request).await.expect("join the turn");
        assert_eq!(outcome.reply.text.as_deref(), Some("answer"));
        assert_eq!(start_count(&f.fake), 1, "no second agent_start");
        assert_eq!(tab_create_count(&f.fake), 1, "no second tab_create");
        let prompts = prompt_calls(&f.fake);
        let last = prompts.last().expect("prompt");
        assert!(
            last.contains("Conversation so far:"),
            "first-turn prompt, got {last}"
        );
        assert!(last.contains("Current message:"));
        f.cleanup();
    }

    #[tokio::test]
    async fn capacity_limit_without_evictable_agent_is_429() {
        let mut config = Config::builtin();
        config.conversations.max_agents_per_model = 1;
        let f = Fixture::new(
            config,
            FakeHerdr::new().with_turns([
                ScriptedTurn::Reply("one".to_string()),
                ScriptedTurn::Reply("two".to_string()),
            ]),
        );
        f.service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("first chat");
        let err = f
            .service
            .chat(req("claude+agent-tools", Some("k2"), vec![user("other")]))
            .await
            .expect_err("over capacity");
        assert_eq!((err.status, err.code), (429, "too_many_backing_agents"));
        assert_eq!(start_count(&f.fake), 1);
        f.cleanup();
    }

    #[tokio::test]
    async fn eviction_rolls_back_when_the_cwd_header_is_invalid() {
        let mut config = Config::builtin();
        config.conversations.max_agents_per_model = 1;
        let f = Fixture::new(
            config,
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("one".to_string())]),
        );
        let first = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("first chat");
        // Age the first conversation past the eviction floor.
        let old = Instant::now()
            .checked_sub(Duration::from_secs(61))
            .expect("machine uptime");
        {
            let mut registry = f.service.locked_registry();
            registry.abort(
                first.conversation,
                &req("claude+agent-tools", Some("k1"), vec![user("hello")]),
                old,
            );
        }
        // Over capacity would evict the aged conversation, but the cwd header is
        // rejected first: the eviction must not have happened.
        let mut second = req("claude+agent-tools", Some("k2"), vec![user("other")]);
        second.cwd_header = Some("/outside-the-allowed-roots".to_string());
        let err = f.service.chat(second).await.expect_err("cwd rejected");
        assert_eq!((err.status, err.code), (403, "cwd_not_allowed"));
        assert_eq!(
            f.service.locked_registry().count_total(),
            1,
            "the victim record survives"
        );
        assert!(
            f.service.locked_agents().contains_key(&first.conversation),
            "the victim keeps its handle"
        );
        assert_eq!(f.fake.tab_labels("w1").len(), 2, "lobby + victim tab open");
        f.cleanup();
    }

    #[test]
    fn clear_inflight_only_removes_its_own_channel() {
        let f = Fixture::new(Config::builtin(), FakeHerdr::new());
        let id = f.service.locked_registry().create(
            "claude",
            None,
            PathBuf::from("/tmp"),
            Instant::now(),
        );
        let (stale_tx, _stale_rx) = watch::channel::<TurnState>(TurnState::Running);
        let (current_tx, _current_rx) = watch::channel::<TurnState>(TurnState::Running);
        let entry = |tx: &watch::Sender<TurnState>| InflightTurn {
            tx: tx.clone(),
            turn_lock: Arc::new(TurnLock::new(())),
            question_offered: false,
            hook_pause: Arc::new(HookPause::default()),
        };
        f.service.locked_inflight().insert(id, entry(&current_tx));
        // A finishing stale turn must not drop the entry of a newer reservation.
        f.service.clear_inflight(id, &stale_tx);
        assert!(f.service.locked_inflight().contains_key(&id));
        f.service.clear_inflight(id, &current_tx);
        assert!(!f.service.locked_inflight().contains_key(&id));
        f.cleanup();
    }

    #[test]
    fn reserved_turn_has_a_receiver_before_its_requester_joins() {
        let f = Fixture::new(Config::builtin(), FakeHerdr::new());
        let Reserved::Start(plan) =
            f.service
                .reserve(&req("claude+agent-tools", Some("k1"), vec![user("hello")]))
        else {
            panic!("a new keyed request must reserve a turn");
        };
        let id = plan.id;
        // The requester has not joined yet, but the reservation came with a
        // receiver, so a concurrent request must not read this turn as abandoned.
        assert_eq!(
            f.service
                .locked_inflight()
                .get(&id)
                .map(|turn| turn.tx.receiver_count()),
            Some(1)
        );
        let registry = f.service.locked_registry();
        assert!(
            f.service.orphaned_turn_lock(&registry, id).is_none(),
            "a freshly reserved turn is not supersedeable"
        );
        drop(plan);
        // Only once that receiver is gone does the turn become orphaned.
        assert!(f.service.orphaned_turn_lock(&registry, id).is_some());
        drop(registry);
        f.cleanup();
    }

    #[tokio::test]
    async fn orphaned_turn_is_superseded_by_the_next_request() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([
                ScriptedTurn::SlowReply {
                    delay_ms: 300,
                    text: "abandoned reply".to_string(),
                },
                ScriptedTurn::Reply("new reply".to_string()),
            ]),
        );
        let service = Arc::clone(&f.service);
        let first = tokio::spawn(async move {
            service
                .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
                .await
        });
        wait_for_prompt(&f.fake).await;
        // The client hangs up (opencode's Esc): nobody waits for that turn now.
        first.abort();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let outcome = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k1"),
                vec![user("hello"), user("new question")],
            ))
            .await
            .expect("superseded");
        assert_eq!(outcome.reply.text.as_deref(), Some("new reply"));
        assert_eq!(start_count(&f.fake), 1, "the same agent keeps the turn");

        let keys: Vec<_> = f
            .fake
            .calls()
            .into_iter()
            .filter(|c| c.method == "agent.send_keys")
            .collect();
        // Claude puts the interrupted prompt back in its input box, so it gets
        // one ctrl+c after the esc; the new prompt must not be appended to it.
        assert_eq!(keys.len(), 2, "the abandoned turn is interrupted once");
        assert_eq!(keys[0].param("keys"), Some("esc"));
        assert_eq!(keys[1].param("keys"), Some("ctrl+c"));

        // The aborted message counts as consumed: only the new text is prompted.
        assert_eq!(
            prompt_calls(&f.fake),
            vec!["hello".to_string(), "new question".to_string()]
        );
        // The abandoned turn's reply was never committed.
        let record = f
            .service
            .locked_registry()
            .get(outcome.conversation)
            .cloned()
            .expect("record");
        assert_eq!(
            record.last_reply.as_ref().and_then(|r| r.text.as_deref()),
            Some("new reply")
        );
        assert_eq!(record.inflight, None);
        f.cleanup();
    }

    #[tokio::test]
    async fn superseded_codex_turn_gets_esc_but_never_ctrl_c() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([
                ScriptedTurn::SlowReply {
                    delay_ms: 300,
                    text: "abandoned reply".to_string(),
                },
                ScriptedTurn::Reply("new reply".to_string()),
            ]),
        );
        let service = Arc::clone(&f.service);
        let first = tokio::spawn(async move {
            service
                .chat(req("codex+agent-tools", Some("k1"), vec![user("hello")]))
                .await
        });
        wait_for_prompt(&f.fake).await;
        first.abort();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let outcome = f
            .service
            .chat(req(
                "codex+agent-tools",
                Some("k1"),
                vec![user("hello"), user("new question")],
            ))
            .await
            .expect("superseded");
        assert_eq!(outcome.reply.text.as_deref(), Some("new reply"));
        // Codex leaves its composer empty after Esc, and ctrl+c on an empty
        // composer quits the CLI, so only the esc may be sent.
        let keys: Vec<_> = f
            .fake
            .calls()
            .into_iter()
            .filter(|c| c.method == "agent.send_keys")
            .map(|c| c.param("keys").unwrap_or_default().to_string())
            .collect();
        assert_eq!(keys, ["esc"]);
        f.cleanup();
    }

    #[tokio::test]
    async fn in_flight_turn_with_a_live_waiter_is_never_superseded() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
                delay_ms: 300,
                text: "slow".to_string(),
            }]),
        );
        let service = Arc::clone(&f.service);
        let first = tokio::spawn(async move {
            service
                .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
                .await
        });
        wait_for_prompt(&f.fake).await;
        let busy = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k1"),
                vec![user("hello"), user("while busy")],
            ))
            .await
            .expect_err("busy");
        assert_eq!((busy.status, busy.code), (409, "conversation_busy"));
        assert!(
            !f.fake.calls().iter().any(|c| c.method == "agent.send_keys"),
            "a live turn must not be interrupted"
        );
        let outcome = first.await.expect("task").expect("first chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("slow"));
        f.cleanup();
    }

    #[tokio::test]
    async fn reap_expired_closes_idle_agent_tabs() {
        let mut config = Config::builtin();
        config.conversations.idle_ttl_secs = 0;
        let f = Fixture::new(
            config,
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        f.service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("chat");
        assert_eq!(f.fake.tab_labels("w1").len(), 2); // lobby + agent tab
        f.service.reap_expired().await;
        assert_eq!(f.fake.tab_labels("w1"), vec!["lobby".to_string()]);
        f.cleanup();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn exited_opencode_client_closes_its_agent_and_record() {
        let proc = FakeClientProc::new();
        let mut f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        enable_client_tracking(&mut f, &proc);
        let outcome = f
            .service
            .chat(opencode_req(
                "k-client-exit",
                vec![user("hello")],
                proc.peer,
            ))
            .await
            .expect("chat");
        assert_eq!(
            f.service
                .locked_clients()
                .get(&outcome.conversation)
                .unwrap()
                .process
                .pid,
            proc.pid
        );

        proc.exit();
        f.service.close_exited_clients().await;
        assert!(f
            .service
            .locked_registry()
            .get(outcome.conversation)
            .is_none());
        assert!(!f
            .service
            .locked_clients()
            .contains_key(&outcome.conversation));
        assert!(f
            .service
            .locked_agents()
            .get(&outcome.conversation)
            .is_none());
        assert_eq!(
            f.fake
                .calls()
                .iter()
                .filter(|call| call.method == "tab.close")
                .count(),
            1
        );
        f.cleanup();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn exited_client_during_agent_start_closes_unregistered_agent() {
        let proc = FakeClientProc::new();
        let mut f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup_delay(1_000)
                .with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        enable_client_tracking(&mut f, &proc);
        let service = Arc::clone(&f.service);
        let peer = proc.peer;
        let chat = tokio::spawn(async move {
            service
                .chat(opencode_req(
                    "k-client-start-exit",
                    vec![user("hello")],
                    peer,
                ))
                .await
        });
        wait_for_call(&f.fake, "agent.start").await;
        for _ in 0..400 {
            if !f.service.locked_clients().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(f.service.locked_clients().len(), 1);

        proc.exit();
        f.service.close_exited_clients().await;
        let error = tokio::time::timeout(Duration::from_secs(3), chat)
            .await
            .expect("request should finish after the client exits")
            .expect("chat task")
            .expect_err("removed conversation must fail to launch its agent");
        assert_eq!(error.code, "backing_agent_exited");
        assert!(f.service.locked_agents().is_empty());
        assert_eq!(
            f.fake
                .calls()
                .iter()
                .filter(|call| call.method == "tab.close")
                .count(),
            1
        );
        f.cleanup();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn request_without_opencode_session_is_not_tracked() {
        let proc = FakeClientProc::new();
        let mut f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        enable_client_tracking(&mut f, &proc);
        let mut request = req(
            "claude+agent-tools",
            Some("k-no-client-track"),
            vec![user("hi")],
        );
        request.client_peer = Some(proc.peer);
        f.service.chat(request).await.expect("chat");
        assert!(f.service.locked_clients().is_empty());
        f.cleanup();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn exited_client_closes_agent_and_finishes_inflight_turn() {
        let proc = FakeClientProc::new();
        let mut f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::SlowReply {
                delay_ms: 500,
                text: "late answer".to_string(),
            }]),
        );
        enable_client_tracking(&mut f, &proc);
        let service = Arc::clone(&f.service);
        let peer = proc.peer;
        let chat = tokio::spawn(async move {
            service
                .chat(opencode_req("k-client-inflight", vec![user("hello")], peer))
                .await
        });
        wait_for_prompt(&f.fake).await;
        let id = *f
            .service
            .locked_clients()
            .keys()
            .next()
            .expect("tracked client");
        proc.exit();
        f.service.close_exited_clients().await;

        let error = tokio::time::timeout(Duration::from_secs(2), chat)
            .await
            .expect("turn should end after its agent closes")
            .expect("chat task")
            .expect_err("closed backing agent must fail the turn");
        assert_eq!(error.code, "backing_agent_exited");
        assert!(f.service.locked_registry().get(id).is_none());
        assert!(!f.service.locked_inflight().contains_key(&id));
        assert!(!f.service.locked_clients().contains_key(&id));
        assert!(!f.service.locked_agents().contains_key(&id));
        f.cleanup();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn live_opencode_client_keeps_its_agent() {
        let proc = FakeClientProc::new();
        let mut f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        enable_client_tracking(&mut f, &proc);
        let outcome = f
            .service
            .chat(opencode_req(
                "k-client-alive",
                vec![user("hello")],
                proc.peer,
            ))
            .await
            .expect("chat");
        f.service.close_exited_clients().await;
        assert!(f
            .service
            .locked_registry()
            .get(outcome.conversation)
            .is_some());
        assert!(f
            .service
            .locked_agents()
            .get(&outcome.conversation)
            .is_some());
        assert!(f
            .service
            .locked_clients()
            .contains_key(&outcome.conversation));
        assert!(!f.fake.calls().iter().any(|call| call.method == "tab.close"));
        f.cleanup();
    }

    #[tokio::test]
    async fn startup_sweep_closes_orphans_without_creating_the_workspace() {
        let fake = FakeHerdr::new();
        let created = fake
            .workspace_create("openai-api", Path::new("/tmp"), false)
            .await
            .expect("seed workspace");
        fake.tab_create(crate::herdr::TabCreate {
            workspace_id: &created.workspace.workspace_id,
            cwd: None,
            label: Some("oa:claude:aaaaaa"),
            env: &std::collections::BTreeMap::new(),
            focus: false,
        })
        .await
        .expect("orphan tab");
        let f = Fixture::new(Config::builtin(), fake);
        f.service.startup_sweep().await;
        // Only the `oa:` orphan is closed; the pre-existing root tab is left alone.
        assert_eq!(f.fake.tab_labels("w1"), vec!["openai-api".to_string()]);
        assert_eq!(
            f.fake
                .calls()
                .iter()
                .filter(|c| c.method == "workspace.create")
                .count(),
            1,
            "startup_sweep must not create the workspace"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn periodic_sweep_closes_untagged_tabs_on_the_second_tick() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("chat");
        let mut seeded = BTreeMap::new();
        for (label, owner) in [
            ("oa:claude:aaaaaa", None),
            ("oa:claude:bbbbbb", Some("other-instance")),
            ("oa:claude:cccccc", None),
        ] {
            let created = f
                .fake
                .tab_create(crate::herdr::TabCreate {
                    workspace_id: "w1",
                    cwd: None,
                    label: Some(label),
                    env: &BTreeMap::new(),
                    focus: false,
                })
                .await
                .expect("seed tab");
            if let Some(owner) = owner {
                let tokens =
                    BTreeMap::from([(lifecycle::OWNER_TOKEN_KEY.to_string(), owner.to_string())]);
                f.fake
                    .pane_report_tokens(&created.root_pane.pane_id, "test", &tokens, None)
                    .await
                    .expect("tag");
            }
            seeded.insert(label, created.root_pane.pane_id);
        }
        let refreshes = |fake: &FakeHerdr| {
            fake.calls()
                .into_iter()
                .filter(|c| {
                    c.method == "pane.report_metadata"
                        && c.param("source") == Some(lifecycle::OWNER_TOKEN_SOURCE)
                })
                .filter_map(|c| c.param("pane_id").map(String::from))
                .collect::<Vec<_>>()
        };
        let before = refreshes(&f.fake).len();

        f.service.sweep_untagged_tabs().await;
        let labels = f.fake.tab_labels("w1");
        assert!(
            labels.contains(&"oa:claude:aaaaaa".to_string()),
            "first sighting"
        );
        assert!(
            labels.contains(&"oa:claude:cccccc".to_string()),
            "first sighting"
        );
        // Only our agent's pane gets its token refreshed (tracked re-tag + refresh).
        let refreshed = refreshes(&f.fake);
        let agent_tab = f
            .fake
            .tab_list("w1")
            .await
            .expect("tabs")
            .into_iter()
            .find(|tab| tab.label == outcome.tab_label)
            .expect("agent tab");
        let agent_pane = f
            .fake
            .pane_list("w1")
            .await
            .expect("panes")
            .into_iter()
            .find(|pane| pane.tab_id == agent_tab.tab_id)
            .expect("agent pane");
        let new_reports = &refreshed[before..];
        assert!(!new_reports.is_empty(), "{refreshed:?}");
        assert!(
            new_reports
                .iter()
                .all(|pane_id| *pane_id == agent_pane.pane_id),
            "{refreshed:?}"
        );

        // `cccccc` finishes its launch between the ticks: tagged now, so it stays.
        let tokens = BTreeMap::from([(
            lifecycle::OWNER_TOKEN_KEY.to_string(),
            f.service.owner_id.clone(),
        )]);
        f.fake
            .pane_report_tokens(&seeded["oa:claude:cccccc"], "test", &tokens, None)
            .await
            .expect("tag in-flight tab");

        f.service.sweep_untagged_tabs().await;
        let labels = f.fake.tab_labels("w1");
        assert!(!labels.contains(&"oa:claude:aaaaaa".to_string()));
        assert!(labels.contains(&"oa:claude:bbbbbb".to_string()));
        assert!(labels.contains(&"oa:claude:cccccc".to_string()));
        assert!(labels.contains(&outcome.tab_label));
        f.cleanup();
    }

    #[tokio::test]
    async fn periodic_sweep_retags_tracked_agents_whose_token_expired() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        f.service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("chat");
        let handle = f
            .service
            .locked_agents()
            .values()
            .next()
            .cloned()
            .expect("tracked agent");
        f.fake.clear_pane_tokens(&handle.pane_id);
        let before = f.fake.calls().len();

        f.service.sweep_untagged_tabs().await;
        f.service.sweep_untagged_tabs().await;

        assert!(f.fake.tab_labels("w1").contains(&handle.tab_label));
        let calls = f.fake.calls();
        let report = calls[before..]
            .iter()
            .find(|c| {
                c.method == "pane.report_metadata"
                    && c.param("pane_id") == Some(handle.pane_id.as_str())
            })
            .expect("token re-reported");
        assert_eq!(report.param("source"), Some(lifecycle::OWNER_TOKEN_SOURCE));
        assert_eq!(
            report.param("token.oa_owner"),
            Some(f.service.owner_id.as_str())
        );
        assert_eq!(report.param("ttl_ms"), Some("90000"));
        f.cleanup();
    }

    #[tokio::test]
    async fn periodic_sweep_never_closes_a_tracked_tab() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        f.service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("chat");
        // Point the tracked handle at a missing pane so the re-report fails,
        // and drop the real pane's token: the tab now looks untagged.
        let handle = {
            let mut agents = f.service.locked_agents();
            let tracked = agents.values_mut().next().expect("tracked agent");
            let real = tracked.clone();
            tracked.pane_id = "w1:p-gone".to_string();
            real
        };
        f.fake.clear_pane_tokens(&handle.pane_id);

        f.service.sweep_untagged_tabs().await;
        f.service.sweep_untagged_tabs().await;

        assert!(f.fake.tab_labels("w1").contains(&handle.tab_label));
        assert!(!f
            .fake
            .calls()
            .iter()
            .any(|c| c.method == "tab.close" && c.param("tab_id") == Some(handle.tab_id.as_str())));
        f.cleanup();
    }

    #[tokio::test]
    async fn shutdown_closes_every_owned_tab() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        f.service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("chat");
        f.service.shutdown().await;
        assert_eq!(f.fake.tab_labels("w1"), vec!["lobby".to_string()]);
        f.cleanup();
    }

    #[tokio::test]
    async fn blocked_timeout_commits_an_explanatory_reply() {
        let mut config = Config::builtin();
        config.herdr.blocked_timeout_ms = 50;
        let f = Fixture::new(
            config,
            FakeHerdr::new()
                .with_read_fails()
                .with_turns([ScriptedTurn::BlockedForever]),
        );
        let request = req("claude+agent-tools", Some("k1"), vec![user("hello")]);
        let outcome = f.service.chat(request.clone()).await.expect("blocked chat");
        let text = outcome.reply.text.clone().unwrap_or_default();
        assert!(text.contains("waiting for approval"), "got {text}");
        assert!(text.contains(&outcome.agent_name));
        assert!(text.contains(&outcome.tab_label));
        assert_eq!(outcome.reply.finish_reason, FinishReason::Stop);
        // The retry gets the same explanation from the cache, not a second prompt.
        let retry = f.service.chat(request).await.expect("retry");
        assert_eq!(retry.reply, outcome.reply);
        assert_eq!(prompt_calls(&f.fake).len(), 1);
        f.cleanup();
    }

    #[tokio::test]
    async fn exited_agent_resumes_in_its_own_pane_and_sends_only_the_delta() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([
                ScriptedTurn::Reply("one".to_string()),
                ScriptedTurn::Exit,
                ScriptedTurn::Reply("two".to_string()),
            ]),
        );
        let first = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("first chat");
        let (pane_id, session_id) = {
            let agents = f.service.locked_agents();
            let agent = agents.get(&first.conversation).expect("agent");
            (agent.pane_id.clone(), agent.session_id.clone())
        };
        let session_id = session_id.expect("claude session id");
        // The agent exits during the second turn: the client sees 502.
        let err = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k1"),
                vec![user("hello"), assistant("one"), user("during exit")],
            ))
            .await
            .expect_err("exited mid-turn");
        assert_eq!((err.status, err.code), (502, "backing_agent_exited"));
        // The next request finds the agent gone and restarts it in the same pane.
        let outcome = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k1"),
                vec![user("hello"), assistant("one"), user("after exit")],
            ))
            .await
            .expect("third chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("two"));
        assert_eq!(start_count(&f.fake), 1, "no new tab for a restart");
        assert_eq!(
            f.fake
                .calls()
                .iter()
                .filter(|c| c.method == "tab.close")
                .count(),
            0,
            "the original tab is never closed"
        );
        let sends = send_inputs(&f.fake);
        assert_eq!(
            sends.len(),
            1,
            "the CLI is restarted by typing into the pane"
        );
        let (line, pane) = &sends[0];
        assert_eq!(pane, &pane_id, "the same pane hosts the new CLI");
        assert!(
            line.contains("--resume") && line.contains(&session_id),
            "the restart resumes the session: {line}"
        );
        assert_eq!(
            prompt_calls(&f.fake).last().expect("prompt"),
            "after exit",
            "a resumed CLI keeps its own context, so only the delta is sent"
        );
        assert_eq!(
            f.service
                .locked_agents()
                .get(&first.conversation)
                .expect("agent")
                .pane_id,
            pane_id,
            "the conversation still points at the same pane"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn exited_codex_agent_resumes_with_its_rollout_session_id() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([
                ScriptedTurn::Reply("one".to_string()),
                ScriptedTurn::Exit,
                ScriptedTurn::Reply("two".to_string()),
            ]),
        );
        let first = f
            .service
            .chat(req("codex+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("first chat");
        let session_id = f
            .service
            .locked_agents()
            .get(&first.conversation)
            .expect("agent")
            .session_id
            .clone()
            .expect("codex session id from agent_session");
        let err = f
            .service
            .chat(req(
                "codex+agent-tools",
                Some("k1"),
                vec![user("hello"), assistant("one"), user("during exit")],
            ))
            .await
            .expect_err("exited mid-turn");
        assert_eq!((err.status, err.code), (502, "backing_agent_exited"));
        let outcome = f
            .service
            .chat(req(
                "codex+agent-tools",
                Some("k1"),
                vec![user("hello"), assistant("one"), user("after exit")],
            ))
            .await
            .expect("third chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("two"));
        assert_eq!(start_count(&f.fake), 1, "no new tab for a restart");
        let line = send_inputs(&f.fake).pop().expect("restart line").0;
        assert!(
            line.starts_with("codex resume ") && line.contains(&session_id),
            "codex resumes through its subcommand: {line}"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn closed_pane_respawns_in_a_new_tab_with_bootstrap() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([
                ScriptedTurn::Reply("one".to_string()),
                ScriptedTurn::Reply("two".to_string()),
            ]),
        );
        let first = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("first chat");
        let tab_id = f
            .service
            .locked_agents()
            .get(&first.conversation)
            .expect("agent")
            .tab_id
            .clone();
        // The tab is closed by hand: no pane left to restart into.
        f.fake.tab_close(&tab_id).await.expect("close tab");
        let outcome = f
            .service
            .chat(req(
                "claude+agent-tools",
                Some("k1"),
                vec![user("hello"), assistant("one"), user("after close")],
            ))
            .await
            .expect("second chat");
        assert_eq!(outcome.reply.text.as_deref(), Some("two"));
        assert_eq!(start_count(&f.fake), 2, "a new tab hosts the respawned CLI");
        let prompts = prompt_calls(&f.fake);
        assert!(
            prompts
                .last()
                .expect("prompt")
                .contains("Conversation so far:"),
            "the new CLI gets the full history"
        );
        assert!(prompts.last().expect("prompt").contains("after close"));
        f.cleanup();
    }

    #[tokio::test]
    async fn turn_error_fails_the_turn_so_the_same_request_re_sends() {
        let mut config = Config::builtin();
        config.herdr.completion_grace_ms = 300;
        let f = Fixture::new(config, FakeHerdr::new().with_turns([ScriptedTurn::Stall]));
        let request = req("claude+agent-tools", Some("k1"), vec![user("hello")]);
        let err = f.service.chat(request.clone()).await.expect_err("stalled");
        assert_eq!((err.status, err.code), (502, "agent_prompt_stalled"));
        // No reply was committed: the same request continues and re-sends the prompt.
        let outcome = f
            .service
            .chat(request)
            .await
            .expect_err("still scripted empty");
        // The scripted queue is empty now, so the fake reports no completion.
        assert_eq!(
            (outcome.status, outcome.code),
            (502, "completion_not_observed")
        );
        assert_eq!(prompt_calls(&f.fake).len(), 2, "prompt re-sent after fail");
        f.cleanup();
    }

    #[tokio::test]
    async fn health_reports_fake_reachability_and_counts() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::Reply("answer".to_string())]),
        );
        let health = f.service.health().await;
        assert_eq!(health["status"], "ok");
        assert_eq!(health["herdr"]["reachable"], true);
        assert_eq!(health["herdr"]["protocol"], 22);
        assert_eq!(health["conversations"], 0);
        f.service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect("chat");
        let health = f.service.health().await;
        assert_eq!(health["conversations"], 1);
        assert_eq!(health["agents"], 1);
        f.cleanup();
    }

    #[tokio::test]
    async fn unavailable_herdr_maps_to_503() {
        let f = Fixture::new(Config::builtin(), FakeHerdr::new().with_unavailable());
        let err = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect_err("unavailable");
        assert_eq!((err.status, err.code), (503, "herdr_unavailable"));
        let health = f.service.health().await;
        assert_eq!(health["status"], "degraded");
        assert_eq!(health["herdr"]["reachable"], false);
        f.cleanup();
    }

    #[test]
    fn models_lists_every_configured_id() {
        let f = Fixture::new(Config::builtin(), FakeHerdr::new());
        assert_eq!(
            f.service
                .models()
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "claude+agent-tools",
                "claude+client-tools",
                "codex+agent-tools",
                "codex+client-tools",
            ]
        );
        f.cleanup();
    }

    #[test]
    fn transcript_roots_prepend_model_env_dir_and_track_kind() {
        let mut model = crate::config::ModelSpec {
            id: "claude".to_string(),
            kind: AgentKind::Claude,
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
        };
        model
            .env
            .insert("CLAUDE_CONFIG_DIR".to_string(), "/model/dir".to_string());
        let codex_model = crate::config::ModelSpec {
            id: "codex".to_string(),
            kind: AgentKind::Codex,
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
        };
        let config = Config {
            models: vec![model, codex_model],
            ..Config::builtin()
        };
        let f = Fixture::new(config, FakeHerdr::new());
        let roots = &f.service.transcript_roots["claude"];
        assert!(
            matches!(roots, TranscriptRoots::Claude(list) if *list[0] == *"/model/dir"),
            "got {roots:?}"
        );
        let codex_roots = &f.service.transcript_roots["codex"];
        assert!(
            matches!(codex_roots, TranscriptRoots::Codex { sessions_root, .. }
                if *sessions_root == f.dir.join("codex-sessions")),
            "codex root must be the injected sessions root, got {codex_roots:?}"
        );
        f.cleanup();
    }

    #[test]
    fn variant_config_homes_reach_the_transcript_roots() {
        let mut model = crate::config::ModelSpec {
            id: "claude-personal".to_string(),
            kind: AgentKind::Claude,
            command: Some("claude-personal".to_string()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
        };
        model
            .env
            .insert("CLAUDE_CONFIG_DIR".to_string(), "/model/dir".to_string());
        let config = Config {
            models: vec![model],
            ..Config::builtin()
        };
        let f = Fixture::new(config, FakeHerdr::new());
        let mut env = f.service.env.clone();
        env.variant_config_homes.insert(
            "claude-personal".to_string(),
            std::path::PathBuf::from("/variant/claude"),
        );
        // Roots are computed in `Service::new`, so build a service with the map set.
        let service = Service::new(f.service.config.clone(), f.fake.clone(), env);
        let roots = &service.transcript_roots["claude-personal"];
        assert!(
            matches!(roots, TranscriptRoots::Claude(list)
                if list[0] == *"/variant/claude"
                    && list[1] == *"/model/dir"),
            "variant home must come first, got {roots:?}"
        );
        f.cleanup();
    }

    fn trust_dialog() -> FakeDialog {
        FakeDialog {
            screen_title: "Accessing workspace:".to_string(),
            options: vec![
                "No, exit".to_string(),
                "Yes, I trust this folder".to_string(),
            ],
            cursor: 0,
            accept: 1,
        }
    }

    fn permission_dialog() -> FakeDialog {
        FakeDialog {
            screen_title: "Do you want to create notes.txt?".to_string(),
            options: vec!["Yes".to_string(), "No".to_string()],
            cursor: 0,
            accept: 0,
        }
    }

    /// Claude's 3-option permission prompt from the live report; `accept` is
    /// the option that keeps the agent alive when `enter` lands on it.
    fn permission3_dialog(cursor: usize, accept: usize) -> FakeDialog {
        FakeDialog {
            screen_title: "Do you want to create relay2.txt?".to_string(),
            options: vec![
                "Yes".to_string(),
                "Yes, and switch to accept edits for this session".to_string(),
                "Always allow this command".to_string(),
            ],
            cursor,
            accept,
        }
    }

    fn question_tool() -> ToolDef {
        ToolDef {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "question".to_string(),
                description: None,
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "questions": { "type": "array", "items": { "type": "object" } },
                    },
                })),
            },
        }
    }

    /// The client history after it received `relay` as the assistant reply.
    fn relayed_history(
        request: &NormalizedRequest,
        relay: &AssistantReply,
    ) -> Vec<NormalizedMessage> {
        let mut messages = request.messages.clone();
        messages.push(NormalizedMessage::Assistant {
            text: relay.text.clone(),
            tool_calls: relay.tool_calls.clone(),
        });
        messages
    }

    fn keys_sent(fake: &FakeHerdr) -> Vec<String> {
        fake.calls()
            .iter()
            .filter(|c| c.method == "agent.send_keys")
            .filter_map(|c| c.param("keys").map(String::from))
            .collect()
    }

    fn keys_sent_to(fake: &FakeHerdr, target: &str) -> Vec<String> {
        fake.calls()
            .iter()
            .filter(|c| c.method == "agent.send_keys" && c.param("target") == Some(target))
            .filter_map(|c| c.param("keys").map(String::from))
            .collect()
    }

    fn oa_tabs(fake: &FakeHerdr) -> Vec<String> {
        fake.tab_labels("w1")
            .into_iter()
            .filter(|label| label.starts_with("oa:"))
            .collect()
    }

    #[tokio::test]
    async fn startup_dialog_is_relayed_and_answered_with_a_number() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::Reply("answer to test".to_string())]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");
        let text = relay.reply.text.clone().expect("numbered text relay");
        assert!(text.contains("1. No, exit   (current)"), "got {text}");
        assert!(text.contains("2. Yes, I trust this folder"), "got {text}");
        assert_eq!(relay.reply.finish_reason, FinishReason::Stop);

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("2"));
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("answer");
        assert_eq!(outcome.reply.text.as_deref(), Some("answer to test"));

        assert_eq!(
            keys_sent(&f.fake),
            vec!["down".to_string(), "enter".to_string()]
        );
        assert_eq!(start_count(&f.fake), 1, "no second agent");
        assert_eq!(tab_create_count(&f.fake), 1, "no second tab");
        let prompts = prompt_calls(&f.fake);
        assert_eq!(prompts.len(), 1, "the agent is prompted exactly once");
        assert!(prompts[0].contains("test"));
        assert!(
            !prompts[0].contains('2'),
            "the answer is never typed as a prompt"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn startup_dialog_is_relayed_as_question_tool_when_offered() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::Reply("answer to test".to_string())]),
        );
        let mut first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        first.tools = vec![question_tool()];
        let relay = f.service.chat(first.clone()).await.expect("relay");
        assert_eq!(relay.reply.finish_reason, FinishReason::ToolCalls);
        assert_eq!(relay.reply.tool_calls.len(), 1);
        let call = &relay.reply.tool_calls[0];
        assert_eq!(call.function.name, "question");
        assert!(call.function.arguments.contains("No, exit"));
        assert!(call.function.arguments.contains("Yes, I trust this folder"));

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(NormalizedMessage::Tool {
            call_id: call.id.clone(),
            name: Some("question".to_string()),
            content: "User has answered your questions: \"Accessing workspace:\"=\"Yes, I trust \
                      this folder\". You can now continue with the user's answers in mind."
                .to_string(),
        });
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("answer");
        assert_eq!(outcome.reply.text.as_deref(), Some("answer to test"));
        assert_eq!(
            keys_sent(&f.fake),
            vec!["down".to_string(), "enter".to_string()]
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn mid_turn_dialog_is_answered_and_the_same_turn_continues() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([
                ScriptedTurn::DialogThenReply(trust_dialog(), "final reply".to_string()),
                ScriptedTurn::Reply("after next".to_string()),
            ]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("run it")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");
        let text = relay.reply.text.clone().expect("text");
        assert!(text.contains("2. Yes, I trust this folder"), "got {text}");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("2"));
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("answer");
        assert_eq!(outcome.reply.text.as_deref(), Some("final reply"));

        let mut follow = relayed_history(&first, &relay.reply);
        follow.push(user("2"));
        follow.push(NormalizedMessage::Assistant {
            text: Some("final reply".to_string()),
            tool_calls: Vec::new(),
        });
        follow.push(user("next"));
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), follow))
            .await
            .expect("follow-up");
        assert_eq!(outcome.reply.text.as_deref(), Some("after next"));
        let prompts = prompt_calls(&f.fake);
        let last = prompts.last().expect("prompt");
        assert!(last.contains("next"), "got {last}");
        assert!(
            !last.contains("Yes, I trust this folder"),
            "the dialog answer is not re-sent: {last}"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn two_dialogs_in_one_turn_are_answered_in_sequence() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new().with_turns([ScriptedTurn::DialogsThenReply(
                vec![trust_dialog(), permission_dialog()],
                "final reply".to_string(),
            )]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("run it")]);
        let relay_a = f.service.chat(first.clone()).await.expect("relay A");
        let text_a = relay_a.reply.text.clone().expect("text relay A");
        assert!(
            text_a.contains("2. Yes, I trust this folder"),
            "got {text_a}"
        );

        // Answering A must return relay B, not hang waiting for a seq that
        // restarts at 1.
        let mut answer_a = relayed_history(&first, &relay_a.reply);
        answer_a.push(user("2"));
        let relay_b = tokio::time::timeout(
            Duration::from_secs(5),
            f.service
                .chat(req("claude+agent-tools", Some("k1"), answer_a)),
        )
        .await
        .expect("the second relay must not hang")
        .expect("relay B");
        let text_b = relay_b.reply.text.clone().expect("text relay B");
        assert!(
            text_b.contains("Do you want to create notes.txt?"),
            "got {text_b}"
        );
        assert!(text_b.contains("1. Yes   (current)"), "got {text_b}");

        let answer_b = vec![
            user("run it"),
            NormalizedMessage::Assistant {
                text: Some(text_a),
                tool_calls: Vec::new(),
            },
            user("2"),
            NormalizedMessage::Assistant {
                text: Some(text_b),
                tool_calls: Vec::new(),
            },
            user("1"),
        ];
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            f.service
                .chat(req("claude+agent-tools", Some("k1"), answer_b)),
        )
        .await
        .expect("the final reply must not hang")
        .expect("answer B");
        assert_eq!(outcome.reply.text.as_deref(), Some("final reply"));

        assert_eq!(
            keys_sent(&f.fake),
            vec!["down".to_string(), "enter".to_string(), "enter".to_string()]
        );
        assert_eq!(start_count(&f.fake), 1, "one agent for the turn");
        let prompts = prompt_calls(&f.fake);
        assert_eq!(prompts.len(), 1, "the agent is prompted exactly once");
        assert!(prompts[0].contains("run it"));
        f.cleanup();
    }

    #[tokio::test]
    async fn startup_dialog_then_a_turn_dialog_are_answered_in_sequence() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::DialogThenReply(
                    permission_dialog(),
                    "final reply".to_string(),
                )]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        let relay_a = f.service.chat(first.clone()).await.expect("relay A");
        let text_a = relay_a.reply.text.clone().expect("text relay A");
        assert!(
            text_a.contains("2. Yes, I trust this folder"),
            "got {text_a}"
        );

        let mut answer_a = relayed_history(&first, &relay_a.reply);
        answer_a.push(user("2"));
        let relay_b = tokio::time::timeout(
            Duration::from_secs(5),
            f.service
                .chat(req("claude+agent-tools", Some("k1"), answer_a)),
        )
        .await
        .expect("the turn dialog must not hang")
        .expect("relay B");
        let text_b = relay_b.reply.text.clone().expect("text relay B");
        assert!(
            text_b.contains("Do you want to create notes.txt?"),
            "got {text_b}"
        );

        let answer_b = vec![
            user("test"),
            NormalizedMessage::Assistant {
                text: Some(text_a),
                tool_calls: Vec::new(),
            },
            user("2"),
            NormalizedMessage::Assistant {
                text: Some(text_b),
                tool_calls: Vec::new(),
            },
            user("1"),
        ];
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer_b))
            .await
            .expect("answer B");
        assert_eq!(outcome.reply.text.as_deref(), Some("final reply"));

        assert_eq!(start_count(&f.fake), 1, "no second agent");
        assert_eq!(tab_create_count(&f.fake), 1, "no second tab");
        let prompts = prompt_calls(&f.fake);
        assert_eq!(prompts.len(), 1, "the agent is prompted exactly once");
        assert!(prompts[0].contains("test"));
        assert!(
            !prompts[0].contains('2'),
            "the answers are never typed as a prompt"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn name_lookup_failure_falls_back_to_the_pane_id_for_reads_and_keys() {
        let f =
            Fixture::new(
                Config::builtin(),
                FakeHerdr::new().with_name_target_fails().with_turns([
                    ScriptedTurn::DialogThenReply(trust_dialog(), "final reply".to_string()),
                ]),
            );
        let first = req("claude+agent-tools", Some("k1"), vec![user("run it")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");
        let text = relay.reply.text.clone().expect("text");
        assert!(
            text.contains("2. Yes, I trust this folder"),
            "the relay still happens: {text}"
        );

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("2"));
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("answer");
        assert_eq!(outcome.reply.text.as_deref(), Some("final reply"));

        let pane_id = f
            .fake
            .calls()
            .iter()
            .find(|c| c.method == "agent.start")
            .and_then(|c| c.param("pane_id"))
            .expect("pane id")
            .to_string();
        assert_eq!(
            keys_sent_to(&f.fake, &pane_id),
            vec!["down".to_string(), "enter".to_string()],
            "the keys land via the pane id"
        );
        assert_eq!(
            keys_sent_to(&f.fake, &relay.agent_name),
            vec!["down".to_string(), "enter".to_string()],
            "each name attempt is retried once"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn failed_answer_put_back_lets_an_identical_retry_apply() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::Reply("answer to test".to_string())]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("2"));
        let answer_req = req("claude+agent-tools", Some("k1"), answer);

        // The first apply cannot read the screen: the pending entry and the
        // in-flight snapshot are put back.
        f.fake.set_read_fails(true);
        let err = f
            .service
            .chat(answer_req.clone())
            .await
            .expect_err("dialog changed");
        assert_eq!((err.status, err.code), (409, "conversation_busy"));
        assert!(
            err.message.contains("dialog changed before your answer"),
            "got {}",
            err.message
        );
        assert!(keys_sent(&f.fake).is_empty(), "no keys were sent");

        // The identical retry must answer again, not join the old request and
        // only re-show the old relay.
        f.fake.set_read_fails(false);
        let outcome = f
            .service
            .chat(answer_req)
            .await
            .expect("the retry is applied");
        assert_eq!(outcome.reply.text.as_deref(), Some("answer to test"));
        assert_eq!(
            keys_sent(&f.fake),
            vec!["down".to_string(), "enter".to_string()]
        );
        f.cleanup();
    }

    /// The live race: the TUI repaints late, so a key batch read back too
    /// early overshoots a wrapping menu and the wrong option gets pressed.
    /// Step-by-step confirmation must still land on the answered option.
    #[tokio::test]
    async fn render_lag_below_the_cursor_still_presses_the_answered_option() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_turns([ScriptedTurn::DialogThenReply(
                    permission3_dialog(0, 2),
                    "final reply".to_string(),
                )])
                .with_render_lag(2),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("run it")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");
        let text = relay.reply.text.clone().expect("text");
        assert!(text.contains("3. Always allow this command"), "got {text}");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("3"));
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("answer");
        assert_eq!(outcome.reply.text.as_deref(), Some("final reply"));
        assert_eq!(
            f.fake.entered_options(),
            vec![2],
            "enter must land on the answered option only"
        );
        assert_eq!(
            keys_sent(&f.fake),
            vec!["down".to_string(), "down".to_string(), "enter".to_string()]
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn render_lag_above_the_cursor_still_presses_the_answered_option() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_turns([ScriptedTurn::DialogThenReply(
                    permission3_dialog(2, 0),
                    "final reply".to_string(),
                )])
                .with_render_lag(2),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("run it")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("1"));
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("answer");
        assert_eq!(outcome.reply.text.as_deref(), Some("final reply"));
        assert_eq!(
            f.fake.entered_options(),
            vec![0],
            "enter must land on the answered option only"
        );
        assert_eq!(
            keys_sent(&f.fake),
            vec!["up".to_string(), "up".to_string(), "enter".to_string()]
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn frozen_screen_never_presses_enter_and_puts_the_answer_back() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_turns([ScriptedTurn::DialogThenReply(
                    permission3_dialog(0, 2),
                    "final reply".to_string(),
                )])
                .with_render_lag(usize::MAX),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("run it")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("3"));
        let answer_req = req("claude+agent-tools", Some("k1"), answer);
        let err = f
            .service
            .chat(answer_req.clone())
            .await
            .expect_err("a frozen screen must not be answered blindly");
        assert_eq!((err.status, err.code), (502, "dialog_answer_failed"));
        assert!(
            f.fake.entered_options().is_empty(),
            "enter is never sent on a frozen screen"
        );
        assert!(
            !keys_sent(&f.fake).contains(&"enter".to_string()),
            "no enter on a frozen screen"
        );

        // The pending entry and snapshot were put back: the identical retry
        // classifies as DialogAnswer again and gets applied.
        f.fake.set_render_lag(0);
        let outcome = f
            .service
            .chat(answer_req)
            .await
            .expect("the retry is applied");
        assert_eq!(outcome.reply.text.as_deref(), Some("final reply"));
        assert_eq!(
            f.fake.entered_options(),
            vec![2],
            "the retry presses the answered option"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn options_changing_mid_move_return_busy_without_enter() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_turns([ScriptedTurn::DialogThenReply(
                    permission3_dialog(0, 2),
                    "final reply".to_string(),
                )])
                .with_options_change_on_key(),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("run it")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("3"));
        let err = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect_err("the options changed under the answer");
        assert_eq!((err.status, err.code), (409, "conversation_busy"));
        assert!(
            err.message.contains("dialog changed before your answer"),
            "got {}",
            err.message
        );
        assert!(
            f.fake.entered_options().is_empty(),
            "enter is never sent when the options change"
        );
        assert!(
            !keys_sent(&f.fake).contains(&"enter".to_string()),
            "no enter when the options change"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn unmatched_answer_re_relays_with_a_note_and_no_keys() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::Reply("answer to test".to_string())]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");

        let mut maybe = relayed_history(&first, &relay.reply);
        maybe.push(user("maybe"));
        let reread = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), maybe.clone()))
            .await
            .expect("re-relay");
        let text = reread.reply.text.expect("text");
        assert!(
            text.contains("Please answer with the number of one option, 1-2."),
            "got {text}"
        );
        assert!(
            keys_sent(&f.fake).is_empty(),
            "no keys for an unmatched answer"
        );
        assert!(
            f.service
                .locked_inflight()
                .contains_key(&relay.conversation),
            "the turn stays in flight"
        );

        let mut answer = maybe;
        answer.push(NormalizedMessage::Assistant {
            text: Some(text),
            tool_calls: Vec::new(),
        });
        answer.push(user("2"));
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("answer");
        assert_eq!(outcome.reply.text.as_deref(), Some("answer to test"));
        assert_eq!(
            keys_sent(&f.fake),
            vec!["down".to_string(), "enter".to_string()]
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn continuation_while_dialog_pending_is_busy_with_the_question() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::Reply("answer to test".to_string())]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");

        // Not answer-shaped: two new user messages, so the existing Busy rule
        // applies; the pending question makes the 409 message name it.
        let mut follow = relayed_history(&first, &relay.reply);
        follow.push(user("one more"));
        follow.push(user("and another"));
        let err = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), follow))
            .await
            .expect_err("busy while the dialog is pending");
        assert_eq!((err.status, err.code), (409, "conversation_busy"));
        assert!(
            err.message
                .contains("waiting for an answer to: Accessing workspace:"),
            "got {}",
            err.message
        );
        assert!(
            !keys_sent(&f.fake).iter().any(|keys| keys.contains("esc")),
            "a pending dialog is never superseded"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn identical_retry_while_dialog_pending_gets_the_same_relay() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::Reply("answer to test".to_string())]),
        );
        let mut first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        first.tools = vec![question_tool()];
        let relay = f.service.chat(first.clone()).await.expect("relay");
        let retry = f.service.chat(first).await.expect("identical retry");
        assert_eq!(retry.reply.tool_calls.len(), 1);
        assert_eq!(
            retry.reply.tool_calls[0].id, relay.reply.tool_calls[0].id,
            "the retry gets the same relay"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn answering_the_exit_option_on_the_startup_dialog_exits_the_agent() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::Reply("never prompted".to_string())]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("1"));
        let started = Instant::now();
        let err = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect_err("the agent exits on No, exit");
        assert_eq!((err.status, err.code), (502, "backing_agent_exited"));
        assert!(
            err.message.contains("exited after the dialog was answered"),
            "got {}",
            err.message
        );
        // The transient non-blocked poll must not fall through to the startup
        // timeout: the exit is confirmed within `AGENT_EXIT_CONFIRM`.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the exit must be confirmed quickly, got {:?}",
            started.elapsed()
        );
        assert!(oa_tabs(&f.fake).is_empty(), "the tab is closed");
        assert!(
            f.service.locked_agents().get(&relay.conversation).is_none(),
            "the exited agent is dropped"
        );
        assert!(
            f.service
                .locked_registry()
                .get(relay.conversation)
                .is_some(),
            "the record survives: the threadId mapping does not change"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn startup_dialog_answer_waits_for_the_agent_to_settle_before_prompting() {
        // The live finding: after the trust answer the agent already reports
        // `idle` + `interactive_ready` but drops prompts for a while. The
        // prompt must wait out `POST_DIALOG_SETTLE`, not land in a screen that
        // discards it.
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_prompt_ignored_after_dialog(1_000)
                .with_turns([ScriptedTurn::Reply("answer to test".to_string())]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("2"));
        let started = Instant::now();
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("answer");
        assert!(
            started.elapsed() >= POST_DIALOG_SETTLE,
            "the prompt must wait out the settle window, got {:?}",
            started.elapsed()
        );
        assert_eq!(outcome.reply.text.as_deref(), Some("answer to test"));
        let prompts = prompt_calls(&f.fake);
        assert_eq!(prompts.len(), 1, "exactly one prompt, after the window");
        assert!(prompts[0].contains("test"));
        f.cleanup();
    }

    #[tokio::test]
    async fn a_second_startup_dialog_is_relayed_and_not_treated_as_ready() {
        let f = Fixture::new(
            Config::builtin(),
            FakeHerdr::new()
                .with_startup(StartupBehavior::Dialogs(vec![
                    trust_dialog(),
                    permission_dialog(),
                ]))
                .with_turns([ScriptedTurn::Reply("never yet".to_string())]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("test")]);
        let relay_a = f.service.chat(first.clone()).await.expect("relay A");
        let text_a = relay_a.reply.text.clone().expect("text relay A");
        assert!(
            text_a.contains("2. Yes, I trust this folder"),
            "got {text_a}"
        );

        // Answering the first dialog must return the SECOND dialog's relay,
        // not treat the brief non-blocked window as ready.
        let mut answer_a = relayed_history(&first, &relay_a.reply);
        answer_a.push(user("2"));
        let relay_b = tokio::time::timeout(
            Duration::from_secs(5),
            f.service
                .chat(req("claude+agent-tools", Some("k1"), answer_a)),
        )
        .await
        .expect("the second startup dialog must not hang")
        .expect("relay B");
        let text_b = relay_b.reply.text.expect("text relay B");
        assert!(
            text_b.contains("Do you want to create notes.txt?"),
            "got {text_b}"
        );
        assert!(
            prompt_calls(&f.fake).is_empty(),
            "the agent is not prompted while a startup dialog is open"
        );
        f.cleanup();
    }

    #[tokio::test]
    async fn unmanaged_startup_dialog_is_answered_settled_then_renamed() {
        // Phase 6c: an unmanaged (detected) agent has no name until it settles;
        // the post-dialog wait must rename it before the turn prompts by name.
        let mut config = Config::builtin();
        config.models.push(crate::config::ModelSpec {
            id: "claude-personal".to_string(),
            kind: AgentKind::Claude,
            command: Some("claude-personal".to_string()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
        });
        let f = Fixture::new(
            config,
            FakeHerdr::new()
                .with_variant_command("claude-personal", "claude")
                .with_startup(StartupBehavior::Dialog(trust_dialog()))
                .with_turns([ScriptedTurn::Reply("variant answer".to_string())]),
        );
        let first = req("claude-personal", Some("k1"), vec![user("test")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");
        let text = relay.reply.text.clone().expect("numbered text relay");
        assert!(text.contains("2. Yes, I trust this folder"), "got {text}");

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("2"));
        let outcome = f
            .service
            .chat(req("claude-personal", Some("k1"), answer))
            .await
            .expect("answer");
        assert_eq!(outcome.reply.text.as_deref(), Some("variant answer"));
        assert_eq!(
            start_count(&f.fake),
            0,
            "the variant is never agent.start-ed"
        );
        assert_eq!(tab_create_count(&f.fake), 1, "no second tab");
        let rename = f
            .fake
            .calls()
            .into_iter()
            .find(|c| c.method == "agent.rename")
            .expect("the settled agent is renamed");
        assert_eq!(rename.param("name"), Some(outcome.agent_name.as_str()));
        let prompts = prompt_calls(&f.fake);
        assert_eq!(prompts.len(), 1, "the agent is prompted exactly once");
        assert!(prompts[0].contains("test"));
        f.cleanup();
    }

    #[tokio::test]
    async fn expired_dialog_answer_gets_an_explanatory_reply_without_keys() {
        let mut config = Config::builtin();
        config.herdr.blocked_timeout_ms = 300;
        let f = Fixture::new(
            config,
            FakeHerdr::new().with_turns([ScriptedTurn::DialogThenReply(
                trust_dialog(),
                "too late".to_string(),
            )]),
        );
        let first = req("claude+agent-tools", Some("k1"), vec![user("run it")]);
        let relay = f.service.chat(first.clone()).await.expect("relay");
        // The turn stops waiting for an answer; the relay expires.
        tokio::time::sleep(Duration::from_millis(600)).await;

        let mut answer = relayed_history(&first, &relay.reply);
        answer.push(user("2"));
        let outcome = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), answer))
            .await
            .expect("late answer");
        let text = outcome.reply.text.expect("text");
        assert!(
            text.contains("That question expired before it was answered"),
            "got {text}"
        );
        assert!(keys_sent(&f.fake).is_empty(), "no keys after expiry");
        f.cleanup();
    }

    #[tokio::test]
    async fn startup_blocked_with_unreadable_screen_falls_back_to_503() {
        let mut config = Config::builtin();
        config.herdr.blocked_timeout_ms = 300;
        let f = Fixture::new(
            config,
            FakeHerdr::new()
                .with_read_fails()
                .with_startup(StartupBehavior::Blocked),
        );
        let err = f
            .service
            .chat(req("claude+agent-tools", Some("k1"), vec![user("hello")]))
            .await
            .expect_err("6a fallback");
        assert_eq!((err.status, err.code), (503, "agent_startup_blocked"));
        assert_eq!(
            oa_tabs(&f.fake).len(),
            1,
            "the tab is kept for the fallback"
        );
        f.cleanup();
    }
}
