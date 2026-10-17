//! In-memory `HerdrApi` double for tests: records every call, scripts agent turn
//! outcomes, and writes synthetic Claude transcript JSONL when a reply lands.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use serde_json::json;
use uuid::Uuid;

use super::{
    AgentInfo, AgentSessionInfo, AgentStart, AgentStarted, AgentStatus, HerdrApi, HerdrError,
    PaneInfo, Pong, ReadSource, TabCreate, TabCreated, TabInfo, WaitSpec, WorkspaceCreated,
    WorkspaceInfo,
};

/// How the fake agent behaves when started.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StartupBehavior {
    /// Starts and becomes `idle` with `interactive_ready = true`.
    #[default]
    Ready,
    /// Starts into a blocked state (trust/onboarding dialog).
    Blocked,
    /// Starts into a scripted dialog: `agent_get` reports `blocked` and
    /// `agent_read` (Detection) shows the dialog until keys answer it.
    Dialog(FakeDialog),
    /// Starts into scripted dialogs: the first is open, and each next one opens
    /// after the previous one is accepted (the agent works briefly between them).
    Dialogs(Vec<FakeDialog>),
    /// Process exits right away: `idle`, not ready, nothing pending.
    Fails,
    /// First `agent_start` fails with `agent_pane_busy`, the next one succeeds.
    PaneBusyOnce,
    /// First `agent_start` fails with `agent_name_taken`, the next one succeeds.
    NameTakenOnce,
    /// The first three `agent_get` calls by name fail with `agent_not_found` while
    /// the launch settles; lookups by pane id always work.
    NameResolvesLate,
    /// Reports `blocked` for the first n `agent_get` calls, then becomes ready.
    BlockedUntilPolled(u32),
}

/// A scripted TUI dialog. While active, `agent_get` reports `blocked` and
/// `agent_read` (Detection) renders `screen_title` plus the options, with `❯ `
/// before the current one. `send_keys` `down`/`up` move the cursor (menus wrap
/// like Claude's); `enter` on `accept` closes the dialog, `enter` on any other
/// option exits the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeDialog {
    pub screen_title: String,
    pub options: Vec<String>,
    pub cursor: usize,
    pub accept: usize,
}

/// What one logical key does to an active fake dialog.
enum DialogKey {
    Cursor(usize),
    Accept,
    Exit,
    Ignore,
}

/// Scripted outcome of the next `agent_prompt`, consumed in order.
#[derive(Debug, Clone)]
pub enum ScriptedTurn {
    /// Prompt succeeds and the reply is written to the transcript immediately.
    Reply(String),
    /// Prompt waits `delay_ms` before the reply lands; exercises in-flight joins.
    SlowReply { delay_ms: u64, text: String },
    /// Prompt returns status `blocked`; the reply is written on a later `agent_wait`.
    BlockedThenReply(String),
    /// Prompt returns status `blocked` and never resolves.
    BlockedForever,
    /// Prompt runs into a scripted dialog; closing it writes the reply.
    DialogThenReply(FakeDialog, String),
    /// Prompt runs into several scripted dialogs in sequence: closing one makes
    /// the agent work briefly and show the next; closing the last writes the
    /// reply.
    DialogsThenReply(Vec<FakeDialog>, String),
    /// The agent exits: the prompt fails with `agent_not_running` and `agent_get`
    /// then fails with `agent_not_found`.
    Exit,
    /// The prompt fails with `agent_prompt_stalled` and nothing is written.
    Stall,
    /// The prompt fails with `agent_prompt_stalled`, but the reply IS written to the
    /// transcript, so a later scan can still observe the turn.
    StallThenReply(String),
    /// Prompt succeeds, but the transcript records an upstream error turn end.
    UpstreamError(String),
}

/// One recorded API call: method name plus key params for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeCall {
    pub method: &'static str,
    pub params: Vec<(String, String)>,
}

impl FakeCall {
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

fn call<K: Into<String>>(method: &'static str, params: Vec<(K, String)>) -> FakeCall {
    FakeCall {
        method,
        params: params
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    }
}

#[derive(Debug)]
struct FakeAgent {
    name: String,
    kind: String,
    /// Label reported in `AgentInfo.agent`; overridden by `agent_label_override` for
    /// kind-mismatch tests.
    reported_agent: String,
    pane_id: String,
    tab_id: String,
    workspace_id: String,
    session_id: Option<String>,
    status: AgentStatus,
    interactive_ready: bool,
    launch_pending: bool,
    /// Codex only: `agent_session` is reported only after the first prompt.
    prompted: bool,
    /// `(prompt, reply)` written to the transcript when a later wait unblocks the agent.
    pending_reply: Option<(String, String)>,
    /// Active scripted dialog; while set, the agent reports `blocked`.
    dialog: Option<FakeDialog>,
    /// Set once `agent_get` has reported this agent; with
    /// `exit_after_seen` the next report removes it.
    seen: bool,
    /// Remaining scripted dialogs of a `DialogsThenReply` turn; the next one
    /// opens `NEXT_DIALOG_DELAY` after the current one closes.
    pending_dialogs: std::collections::VecDeque<FakeDialog>,
    next_dialog_at: Option<Instant>,
    /// Real cursor values sampled by recent `agent_read` calls: the rendered
    /// cursor trails the real one by `State::render_lag` reads, like a slow TUI.
    render_queue: std::collections::VecDeque<usize>,
    /// Option indexes `enter` actually selected on this agent's dialogs.
    entered_options: Vec<usize>,
    /// Set once the exit option was chosen: the next `agent_get` reports a
    /// non-blocked status, the one after removes the agent, like a CLI that
    /// leaves `blocked` briefly before it exits.
    exiting: bool,
    exit_poll_seen: bool,
    /// Until this instant the agent reports `idle` + `interactive_ready` but
    /// drops prompts (`agent_prompt_stalled`, no transcript), like Claude
    /// re-initializing after a startup dialog was accepted.
    ignore_prompts_until: Option<Instant>,
    started_at: Instant,
}

#[derive(Debug, Default)]
struct State {
    workspaces: Vec<WorkspaceInfo>,
    tabs: Vec<TabInfo>,
    /// `(pane_id, tab_id)` pairs created by `tab_create`.
    panes: Vec<(String, String)>,
    /// Metadata tokens per pane id, set by `pane_report_tokens`.
    pane_tokens: BTreeMap<String, BTreeMap<String, String>>,
    /// Every `pane_report_tokens` call fails.
    report_tokens_fails: bool,
    agents: Vec<FakeAgent>,
    turns: VecDeque<ScriptedTurn>,
    startup: StartupBehavior,
    pane_busy_served: bool,
    name_taken_served: bool,
    name_late_remaining: u32,
    blocked_polls_remaining: u32,
    startup_delay_ms: u64,
    agent_label_override: Option<String>,
    unavailable: bool,
    next_workspace: u64,
    next_tab: u64,
    next_pane: u64,
    transcript_root: Option<PathBuf>,
    codex_root: Option<PathBuf>,
    hide_agent_session: bool,
    read_fails: bool,
    name_target_fails: bool,
    /// `agent_read` (Detection) shows a dialog cursor key's effect only after
    /// this many further reads; `usize::MAX` freezes the rendered cursor.
    render_lag: usize,
    /// The next cursor key replaces the open dialog's options.
    options_change_on_key: bool,
    /// Command names that launch unmanaged agents: a `pane.send_input` whose
    /// first word matches creates the agent in that pane, like a shell
    /// function starting the real CLI. `(command, kind)` pairs.
    variant_commands: Vec<(String, String)>,
    /// When set, the first `agent.rename` fails with `agent_name_taken`.
    rename_taken_once: bool,
    /// `agent_get` removes an agent after it has been reported once,
    /// simulating an unmanaged agent that exits before becoming ready.
    exit_after_seen: bool,
    /// After a startup dialog is accepted, prompts are dropped with
    /// `agent_prompt_stalled` for this many ms while the agent already reports
    /// `idle` + `interactive_ready`; 0 disables it.
    prompt_ignore_window_ms: u64,
    calls: Vec<FakeCall>,
}

/// Test double implementing [`HerdrApi`] fully in memory.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct FakeHerdr {
    state: Mutex<State>,
}

impl FakeHerdr {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_startup(self, behavior: StartupBehavior) -> Self {
        self.set_startup(behavior);
        self
    }

    /// Change the startup behavior at runtime, e.g. to fail only a later respawn.
    pub fn set_startup(&self, behavior: StartupBehavior) {
        let name_late_remaining = if behavior == StartupBehavior::NameResolvesLate {
            3
        } else {
            0
        };
        let blocked_polls_remaining = match &behavior {
            StartupBehavior::BlockedUntilPolled(n) => *n,
            _ => 0,
        };
        let mut state = self.lock();
        state.startup = behavior;
        state.name_late_remaining = name_late_remaining;
        state.blocked_polls_remaining = blocked_polls_remaining;
    }

    /// `agent_get` reports launched agents as still launching for this long,
    /// so spawns take real wall-clock time.
    pub fn with_startup_delay(self, delay_ms: u64) -> Self {
        self.lock().startup_delay_ms = delay_ms;
        self
    }

    pub fn with_agent_label(self, label: &str) -> Self {
        self.lock().agent_label_override = Some(label.to_string());
        self
    }

    pub fn with_turns(self, turns: impl IntoIterator<Item = ScriptedTurn>) -> Self {
        self.lock().turns = turns.into_iter().collect();
        self
    }

    pub fn with_transcript_root(self, root: impl AsRef<Path>) -> Self {
        self.lock().transcript_root = Some(root.as_ref().to_path_buf());
        self
    }

    /// Root that receives synthetic Codex rollout files (`YYYY/MM/DD/rollout-*.jsonl`).
    pub fn with_codex_root(self, root: impl AsRef<Path>) -> Self {
        self.lock().codex_root = Some(root.as_ref().to_path_buf());
        self
    }

    /// `agent_get` never reports `agent_session`, simulating a missing agent integration.
    pub fn with_agent_session_hidden(self) -> Self {
        self.lock().hide_agent_session = true;
        self
    }

    /// Every API call fails with `HerdrError::Unavailable`, simulating a dead server.
    pub fn with_unavailable(self) -> Self {
        self.lock().unavailable = true;
        self
    }

    /// Every `agent_read` fails, simulating a pane that cannot be read.
    pub fn with_read_fails(self) -> Self {
        self.set_read_fails(true);
        self
    }

    /// Toggle `agent_read` failures at runtime, e.g. around one answer attempt.
    pub fn set_read_fails(&self, fails: bool) {
        self.lock().read_fails = fails;
    }

    /// `agent_read` and `agent_send_keys` fail when addressed by agent NAME
    /// (pane-id targets still work), simulating a name that lags the pane.
    pub fn with_name_target_fails(self) -> Self {
        self.lock().name_target_fails = true;
        self
    }

    /// The rendered dialog cursor trails the real one by `lag` reads: a key's
    /// effect shows only after `lag` further `agent_read` calls. `usize::MAX`
    /// freezes the rendered screen while keys still move the real cursor.
    pub fn with_render_lag(self, lag: usize) -> Self {
        self.set_render_lag(lag);
        self
    }

    /// Toggle the dialog render lag at runtime, e.g. around one answer attempt.
    pub fn set_render_lag(&self, lag: usize) {
        self.lock().render_lag = lag;
    }

    /// The next dialog cursor key replaces the open dialog's options, like the
    /// agent redrawing the prompt with different choices.
    pub fn with_options_change_on_key(self) -> Self {
        self.lock().options_change_on_key = true;
        self
    }

    /// A `pane.send_input` whose first word is `command` launches an agent of
    /// `kind` in that pane, like a shell function starting the real CLI.
    pub fn with_variant_command(self, command: &str, kind: &str) -> Self {
        self.lock()
            .variant_commands
            .push((command.to_string(), kind.to_string()));
        self
    }

    /// Every `pane.report_metadata` call fails, simulating a rejected token report.
    pub fn with_report_tokens_fails(self) -> Self {
        self.lock().report_tokens_fails = true;
        self
    }

    /// Drop a pane's metadata tokens, simulating their TTL expiring.
    pub fn clear_pane_tokens(&self, pane_id: &str) {
        self.lock().pane_tokens.remove(pane_id);
    }

    /// The first `agent.rename` call fails with `agent_name_taken`.
    pub fn with_rename_taken_once(self) -> Self {
        self.lock().rename_taken_once = true;
        self
    }

    /// `agent_get` reports each agent once, then it disappears, simulating an
    /// unmanaged agent that exits before becoming ready.
    pub fn with_agent_exits_after_seen(self) -> Self {
        self.lock().exit_after_seen = true;
        self
    }

    /// After a startup dialog is accepted the fake keeps reporting `idle` +
    /// `interactive_ready` but drops prompts with `agent_prompt_stalled` (no
    /// transcript entry) for this long, like Claude re-initializing after the
    /// trust answer lands.
    pub fn with_prompt_ignored_after_dialog(self, ms: u64) -> Self {
        self.lock().prompt_ignore_window_ms = ms;
        self
    }

    /// The option index each `enter` actually selected, across agents, in order.
    pub fn entered_options(&self) -> Vec<usize> {
        self.lock()
            .agents
            .iter()
            .flat_map(|agent| agent.entered_options.clone())
            .collect()
    }

    pub fn calls(&self) -> Vec<FakeCall> {
        self.lock().calls.clone()
    }

    pub fn tab_labels(&self, workspace_id: &str) -> Vec<String> {
        self.lock()
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace_id)
            .map(|tab| tab.label.clone())
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

impl FakeAgent {
    fn info(&self, report_session: bool) -> AgentInfo {
        let report = report_session && (self.kind != "codex" || self.prompted);
        // An active dialog always dominates the reported lifecycle state.
        let dialog_open = self.dialog.is_some();
        AgentInfo {
            terminal_id: format!("term-{}", self.name),
            name: Some(self.name.clone()),
            agent: Some(self.reported_agent.clone()),
            agent_status: if dialog_open {
                AgentStatus::Blocked
            } else {
                self.status
            },
            agent_session: report
                .then(|| self.session_id.clone())
                .flatten()
                .map(|value| AgentSessionInfo {
                    source: if self.kind == "codex" {
                        "integration".to_string()
                    } else {
                        "args".to_string()
                    },
                    agent: self.kind.clone(),
                    kind: "id".to_string(),
                    value,
                }),
            pane_id: self.pane_id.clone(),
            tab_id: self.tab_id.clone(),
            workspace_id: self.workspace_id.clone(),
            cwd: None,
            interactive_ready: if dialog_open {
                false
            } else {
                self.interactive_ready
            },
            launch_pending: self.launch_pending,
            state_change_seq: 0,
        }
    }
}

fn api_error(code: &str, message: &str) -> HerdrError {
    HerdrError::Api {
        code: code.to_string(),
        message: message.to_string(),
    }
}

/// The Detection-source screen for an active dialog: title, then the options
/// with `❯ ` before the current one and two spaces before the others. The
/// caller passes the RENDERED cursor, which may trail the dialog's real one.
fn dialog_screen(script: &FakeDialog, cursor: usize) -> String {
    let mut screen = format!("{}\n\n", script.screen_title);
    for (index, option) in script.options.iter().enumerate() {
        if index == cursor {
            screen.push_str(&format!(" \u{276f} {option}\n"));
        } else {
            screen.push_str(&format!("   {option}\n"));
        }
    }
    screen
}

/// The cursor the rendered screen shows: the real cursor delayed by `lag`
/// reads, so a key's effect appears only after `lag` further reads.
fn lagged_cursor(agent: &mut FakeAgent, real: usize, lag: usize) -> usize {
    agent.render_queue.push_back(real);
    while agent.render_queue.len() > lag.saturating_add(1) {
        agent.render_queue.pop_front();
    }
    agent.render_queue.front().copied().unwrap_or(real)
}

/// Open a scripted dialog and restart its render lag.
fn show_dialog(agent: &mut FakeAgent, dialog: FakeDialog) {
    agent.dialog = Some(dialog);
    agent.render_queue.clear();
}

/// What one logical key does to a fake dialog. Menus wrap like Claude's:
/// `down` on the last option goes to the first, `up` on the first to the last.
fn dialog_key(script: &FakeDialog, keys: &[&str]) -> DialogKey {
    let count = script.options.len();
    match keys {
        [key] if *key == "down" && count > 0 => DialogKey::Cursor((script.cursor + 1) % count),
        [key] if *key == "up" && count > 0 => {
            DialogKey::Cursor((script.cursor + count - 1) % count)
        }
        [key] if *key == "enter" && script.cursor == script.accept => DialogKey::Accept,
        [key] if *key == "enter" => DialogKey::Exit,
        _ => DialogKey::Ignore,
    }
}

/// How long the fake agent stays `working` between two scripted dialogs of a
/// `DialogsThenReply` turn, so waiters observe the blocked → working → blocked
/// transition like with a real agent.
const NEXT_DIALOG_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Open the next scripted dialog once its delay has elapsed.
fn open_due_dialog(agent: &mut FakeAgent) {
    if agent.next_dialog_at.is_none_or(|at| Instant::now() < at) {
        return;
    }
    agent.next_dialog_at = None;
    if let Some(next) = agent.pending_dialogs.pop_front() {
        show_dialog(agent, next);
        agent.status = AgentStatus::Blocked;
    }
}

fn unavailable_error() -> HerdrError {
    HerdrError::Unavailable {
        socket: PathBuf::from("/fake/herdr.sock"),
        source: std::io::Error::other("fake herdr is unavailable"),
    }
}

fn session_id_from_args(args: &[String]) -> Option<String> {
    args.iter()
        .position(|arg| arg == "--session-id")
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// Append the synthetic transcript entries for one completed turn.
fn write_transcript(state: &State, agent: &FakeAgent, prompt: &str, reply: &str) {
    if agent.kind == "codex" {
        write_codex_rollout(state, agent, prompt, reply, None);
        return;
    }
    let (Some(root), Some(session_id)) =
        (state.transcript_root.as_ref(), agent.session_id.as_ref())
    else {
        return;
    };
    let dir = root.join("projects").join("fake");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %err, "fake herdr: cannot create transcript dir");
        return;
    }
    let entries = [
        json!({"type": "user", "message": {"content": prompt}}),
        json!({
            "type": "assistant",
            "message": {
                "id": format!("fake-{}", session_id),
                "content": [{"type": "text", "text": reply}],
                "stop_reason": "end_turn",
            },
        }),
        json!({"type": "system", "subtype": "turn_duration", "isMeta": false}),
    ];
    append_entries(&dir.join(format!("{session_id}.jsonl")), &entries);
}

/// Append the synthetic transcript entries for a turn that ended in an upstream error.
fn write_transcript_error(state: &State, agent: &FakeAgent, prompt: &str, error: &str) {
    if agent.kind == "codex" {
        write_codex_rollout(state, agent, prompt, "", Some(error));
        return;
    }
    let (Some(root), Some(session_id)) =
        (state.transcript_root.as_ref(), agent.session_id.as_ref())
    else {
        return;
    };
    let dir = root.join("projects").join("fake");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %err, "fake herdr: cannot create transcript dir");
        return;
    }
    let entries = [
        json!({"type": "user", "message": {"content": prompt}}),
        json!({
            "type": "assistant",
            "isApiErrorMessage": true,
            "message": {
                "id": format!("fake-err-{session_id}"),
                "content": [{"type": "text", "text": error}],
                "stop_reason": "end_turn",
            },
        }),
    ];
    append_entries(&dir.join(format!("{session_id}.jsonl")), &entries);
}

/// Append synthetic Codex rollout lines for one turn: `session_meta` once per file,
/// then `task_started`, the final answer item, and `task_complete`.
fn write_codex_rollout(
    state: &State,
    agent: &FakeAgent,
    prompt: &str,
    reply: &str,
    error: Option<&str>,
) {
    let (Some(root), Some(session_id)) = (state.codex_root.as_ref(), agent.session_id.as_ref())
    else {
        return;
    };
    let dir = root.join("2026").join("10").join("14");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %err, "fake herdr: cannot create codex rollout dir");
        return;
    }
    let path = dir.join(format!("rollout-2026-10-14T00-00-00-{session_id}.jsonl"));
    let turn_id = format!(
        "turn-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut entries: Vec<serde_json::Value> = Vec::new();
    if !path.exists() {
        entries
            .push(json!({"type": "session_meta", "payload": {"id": session_id, "cwd": "/fake"}}));
    }
    entries.push(
        json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": turn_id}}),
    );
    entries
        .push(json!({"type": "event_msg", "payload": {"type": "user_message", "message": prompt}}));
    if let Some(error) = error {
        entries.push(json!({
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": turn_id,
                "last_agent_message": "",
                "error": {"message": error, "codex_error_info": {"kind": "fake"}},
            },
        }));
    } else {
        entries.push(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": reply}],
            },
        }));
        entries.push(json!({
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": turn_id, "last_agent_message": reply},
        }));
    }
    append_entries(&path, &entries);
}

fn append_entries(path: &Path, entries: &[serde_json::Value]) {
    let text = entries
        .iter()
        .map(|entry| format!("{entry}\n"))
        .collect::<String>();
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write as _;
            file.write_all(text.as_bytes())
        });
    if let Err(err) = appended {
        tracing::warn!(error = %err, path = %path.display(), "fake herdr: cannot write transcript");
    }
}

impl HerdrApi for FakeHerdr {
    async fn ping(&self) -> Result<Pong, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        self.lock()
            .calls
            .push(call("ping", Vec::<(String, String)>::new()));
        Ok(Pong {
            version: "fake-0.0.0".to_string(),
            protocol: 22,
        })
    }

    async fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state
            .calls
            .push(call("workspace.list", Vec::<(String, String)>::new()));
        Ok(state.workspaces.clone())
    }

    async fn workspace_create(
        &self,
        label: &str,
        cwd: &Path,
        focus: bool,
    ) -> Result<WorkspaceCreated, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "workspace.create",
            vec![
                ("label", label.to_string()),
                ("cwd", cwd.display().to_string()),
                ("focus", focus.to_string()),
            ],
        ));
        state.next_workspace += 1;
        let workspace_id = format!("w{}", state.next_workspace);
        state.next_tab += 1;
        state.next_pane += 1;
        let tab_id = format!("{workspace_id}:t{}", state.next_tab);
        let pane_id = format!("{workspace_id}:p{}", state.next_pane);
        let workspace = WorkspaceInfo {
            workspace_id: workspace_id.clone(),
            label: label.to_string(),
            active_tab_id: tab_id.clone(),
        };
        let tab = TabInfo {
            tab_id: tab_id.clone(),
            workspace_id: workspace_id.clone(),
            label: label.to_string(),
            pane_count: 1,
        };
        state.workspaces.push(workspace.clone());
        state.tabs.push(tab.clone());
        state.panes.push((pane_id.clone(), tab_id.clone()));
        Ok(WorkspaceCreated {
            workspace,
            tab,
            root_pane: PaneInfo {
                pane_id,
                tab_id,
                workspace_id,
                tokens: BTreeMap::new(),
            },
        })
    }

    async fn tab_list(&self, workspace_id: &str) -> Result<Vec<TabInfo>, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "tab.list",
            vec![("workspace_id", workspace_id.to_string())],
        ));
        Ok(state
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace_id)
            .cloned()
            .collect())
    }

    async fn tab_create(&self, params: TabCreate<'_>) -> Result<TabCreated, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut recorded: Vec<(String, String)> = vec![
            ("workspace_id".to_string(), params.workspace_id.to_string()),
            (
                "cwd".to_string(),
                params
                    .cwd
                    .map(|cwd| cwd.display().to_string())
                    .unwrap_or_default(),
            ),
            (
                "label".to_string(),
                params.label.unwrap_or_default().to_string(),
            ),
            ("focus".to_string(), params.focus.to_string()),
        ];
        for (key, value) in params.env {
            recorded.push((format!("env.{key}"), value.clone()));
        }
        let mut state = self.lock();
        state.calls.push(call("tab.create", recorded));
        if !state
            .workspaces
            .iter()
            .any(|ws| ws.workspace_id == params.workspace_id)
        {
            return Err(api_error("workspace_not_found", "unknown workspace"));
        }
        state.next_tab += 1;
        state.next_pane += 1;
        let tab_id = format!("{}:t{}", params.workspace_id, state.next_tab);
        let pane_id = format!("{}:p{}", params.workspace_id, state.next_pane);
        let tab = TabInfo {
            tab_id: tab_id.clone(),
            workspace_id: params.workspace_id.to_string(),
            label: params.label.unwrap_or_default().to_string(),
            pane_count: 1,
        };
        state.tabs.push(tab.clone());
        state.panes.push((pane_id.clone(), tab_id.clone()));
        Ok(TabCreated {
            tab,
            root_pane: PaneInfo {
                pane_id,
                tab_id,
                workspace_id: params.workspace_id.to_string(),
                tokens: BTreeMap::new(),
            },
        })
    }

    async fn tab_rename(&self, tab_id: &str, label: &str) -> Result<TabInfo, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "tab.rename",
            vec![("tab_id", tab_id.to_string()), ("label", label.to_string())],
        ));
        let tab = state
            .tabs
            .iter_mut()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or_else(|| api_error("tab_not_found", "unknown tab"))?;
        tab.label = label.to_string();
        Ok(tab.clone())
    }

    async fn tab_close(&self, tab_id: &str) -> Result<(), HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state
            .calls
            .push(call("tab.close", vec![("tab_id", tab_id.to_string())]));
        let before = state.tabs.len();
        state.tabs.retain(|tab| tab.tab_id != tab_id);
        if state.tabs.len() == before {
            return Err(api_error("tab_not_found", "unknown tab"));
        }
        state.agents.retain(|agent| agent.tab_id != tab_id);
        // Closing the last tab closes the workspace, like real herdr.
        let alive_workspaces: Vec<String> = state
            .tabs
            .iter()
            .map(|tab| tab.workspace_id.clone())
            .collect();
        state
            .workspaces
            .retain(|workspace| alive_workspaces.contains(&workspace.workspace_id));
        Ok(())
    }

    async fn pane_list(&self, workspace_id: &str) -> Result<Vec<PaneInfo>, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "pane.list",
            vec![("workspace_id", workspace_id.to_string())],
        ));
        // Panes of closed tabs are gone, like in real herdr.
        Ok(state
            .panes
            .iter()
            .filter(|(_, tab_id)| {
                state
                    .tabs
                    .iter()
                    .any(|tab| tab.tab_id == *tab_id && tab.workspace_id == workspace_id)
            })
            .map(|(pane_id, tab_id)| PaneInfo {
                pane_id: pane_id.clone(),
                tab_id: tab_id.clone(),
                workspace_id: workspace_id.to_string(),
                tokens: state.pane_tokens.get(pane_id).cloned().unwrap_or_default(),
            })
            .collect())
    }

    async fn pane_report_tokens(
        &self,
        pane_id: &str,
        source: &str,
        tokens: &BTreeMap<String, String>,
        ttl_ms: Option<u64>,
    ) -> Result<(), HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut recorded = vec![
            ("pane_id".to_string(), pane_id.to_string()),
            ("source".to_string(), source.to_string()),
            (
                "ttl_ms".to_string(),
                ttl_ms.map(|ms| ms.to_string()).unwrap_or_default(),
            ),
        ];
        for (key, value) in tokens {
            recorded.push((format!("token.{key}"), value.clone()));
        }
        let mut state = self.lock();
        state.calls.push(call("pane.report_metadata", recorded));
        if state.report_tokens_fails {
            return Err(api_error(
                "invalid_metadata_token",
                "fake token report fails",
            ));
        }
        let known = state.panes.iter().any(|(id, tab_id)| {
            id == pane_id && state.tabs.iter().any(|tab| tab.tab_id == *tab_id)
        });
        if !known {
            return Err(api_error("pane_not_found", "no such pane"));
        }
        // Tokens merge per key, like herdr's per-pane token map.
        state
            .pane_tokens
            .entry(pane_id.to_string())
            .or_default()
            .extend(tokens.clone());
        Ok(())
    }

    async fn pane_send_input(
        &self,
        pane_id: &str,
        text: &str,
        keys: &[&str],
    ) -> Result<(), HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "pane.send_input",
            vec![
                ("pane_id", pane_id.to_string()),
                ("text", text.to_string()),
                ("keys", keys.join(",")),
            ],
        ));
        let tab_id = state
            .panes
            .iter()
            .find(|(id, _)| id == pane_id)
            .map(|(_, tab_id)| tab_id.clone())
            .ok_or_else(|| api_error("pane_not_found", "no such pane"))?;
        // A typed command that matches a scripted variant launches the agent
        // in the pane, like a shell function exec'ing the real CLI.
        let first_word = text.split_whitespace().next().unwrap_or("");
        let kind = state
            .variant_commands
            .iter()
            .find(|(command, _)| command == first_word)
            .map(|(_, kind)| kind.clone());
        let Some(kind) = kind else {
            return Ok(());
        };
        let workspace_id = state
            .tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.workspace_id.clone())
            .unwrap_or_default();
        let args: Vec<String> = text.split_whitespace().skip(1).map(String::from).collect();
        // Same scripted startup behaviors as `agent_start`, but an agent herdr
        // only detects never reports `interactive_ready`.
        let (status, dialog, pending_dialogs) = match &state.startup {
            StartupBehavior::Blocked | StartupBehavior::BlockedUntilPolled(_) => (
                AgentStatus::Blocked,
                None,
                std::collections::VecDeque::new(),
            ),
            StartupBehavior::Dialog(script) => (
                AgentStatus::Blocked,
                Some(script.clone()),
                std::collections::VecDeque::new(),
            ),
            StartupBehavior::Dialogs(scripts) => {
                let mut scripts = scripts.clone().into_iter();
                let first = scripts.next();
                (AgentStatus::Blocked, first, scripts.collect())
            }
            _ => (AgentStatus::Idle, None, std::collections::VecDeque::new()),
        };
        let agent = FakeAgent {
            name: format!("unmanaged-{pane_id}"),
            reported_agent: state
                .agent_label_override
                .clone()
                .unwrap_or_else(|| kind.clone()),
            kind: kind.clone(),
            pane_id: pane_id.to_string(),
            tab_id,
            workspace_id,
            session_id: match kind.as_str() {
                "codex" => Some(Uuid::new_v4().to_string()),
                _ => session_id_from_args(&args),
            },
            status,
            interactive_ready: false,
            launch_pending: false,
            prompted: false,
            pending_reply: None,
            dialog,
            seen: false,
            pending_dialogs,
            next_dialog_at: None,
            render_queue: std::collections::VecDeque::new(),
            entered_options: Vec::new(),
            exiting: false,
            exit_poll_seen: false,
            ignore_prompts_until: None,
            started_at: Instant::now(),
        };
        state.agents.push(agent);
        Ok(())
    }

    async fn agent_get(&self, target: &str) -> Result<AgentInfo, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state
            .calls
            .push(call("agent.get", vec![("target", target.to_string())]));
        if state.startup == StartupBehavior::NameResolvesLate
            && state.name_late_remaining > 0
            && state.agents.iter().any(|agent| agent.name == target)
        {
            state.name_late_remaining -= 1;
            return Err(api_error("agent_not_found", "name not resolvable yet"));
        }
        let index = state
            .agents
            .iter()
            .position(|agent| agent.name == target || agent.pane_id == target)
            .ok_or_else(|| api_error("agent_not_found", "no such agent"))?;
        if state.exit_after_seen && state.agents[index].seen {
            state.agents.remove(index);
            return Err(api_error("agent_not_found", "agent exited"));
        }
        if state.agents[index].exiting {
            // The exit answer was applied: report one non-blocked poll, then
            // the agent is gone for both name and pane id.
            if state.agents[index].exit_poll_seen {
                state.agents.remove(index);
                return Err(api_error(
                    "agent_not_found",
                    "agent exited after the dialog",
                ));
            }
            state.agents[index].exit_poll_seen = true;
        }
        state.agents[index].seen = true;
        open_due_dialog(&mut state.agents[index]);
        if matches!(state.startup, StartupBehavior::BlockedUntilPolled(_))
            && state.agents[index].status == AgentStatus::Blocked
        {
            if state.blocked_polls_remaining > 0 {
                state.blocked_polls_remaining -= 1;
            } else {
                state.agents[index].status = AgentStatus::Idle;
                state.agents[index].interactive_ready = true;
            }
        }
        let mut info = state.agents[index].info(!state.hide_agent_session);
        if state.startup_delay_ms > 0
            && info.interactive_ready
            && state.agents[index].started_at.elapsed()
                < std::time::Duration::from_millis(state.startup_delay_ms)
        {
            info.interactive_ready = false;
            info.launch_pending = true;
        }
        Ok(info)
    }

    async fn agent_start(&self, params: AgentStart<'_>) -> Result<AgentStarted, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "agent.start",
            vec![
                ("name", params.name.to_string()),
                ("kind", params.kind.to_string()),
                ("pane_id", params.pane_id.to_string()),
                ("args", params.args.join(" ")),
            ],
        ));
        if state.startup == StartupBehavior::PaneBusyOnce && !state.pane_busy_served {
            state.pane_busy_served = true;
            return Err(api_error("agent_pane_busy", "pane is busy"));
        }
        if state.startup == StartupBehavior::NameTakenOnce && !state.name_taken_served {
            state.name_taken_served = true;
            return Err(api_error("agent_name_taken", "name is taken"));
        }
        if state.agents.iter().any(|agent| agent.name == params.name) {
            return Err(api_error("agent_name_taken", "name is taken"));
        }
        let tab_id = state
            .panes
            .iter()
            .find(|(pane_id, _)| pane_id == params.pane_id)
            .map(|(_, tab_id)| tab_id.clone())
            .ok_or_else(|| api_error("pane_not_found", "no such pane"))?;
        let workspace_id = state
            .tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.workspace_id.clone())
            .unwrap_or_default();
        let (status, interactive_ready, launch_pending, dialog, pending_dialogs) =
            match &state.startup {
                StartupBehavior::Blocked => (
                    AgentStatus::Blocked,
                    false,
                    false,
                    None,
                    std::collections::VecDeque::new(),
                ),
                StartupBehavior::BlockedUntilPolled(_) => (
                    AgentStatus::Blocked,
                    false,
                    false,
                    None,
                    std::collections::VecDeque::new(),
                ),
                StartupBehavior::Dialog(script) => (
                    AgentStatus::Blocked,
                    false,
                    false,
                    Some(script.clone()),
                    std::collections::VecDeque::new(),
                ),
                StartupBehavior::Dialogs(scripts) => {
                    let mut scripts = scripts.clone().into_iter();
                    let first = scripts.next();
                    (AgentStatus::Blocked, false, false, first, scripts.collect())
                }
                StartupBehavior::Fails => (
                    AgentStatus::Idle,
                    false,
                    false,
                    None,
                    std::collections::VecDeque::new(),
                ),
                _ => (
                    AgentStatus::Idle,
                    true,
                    false,
                    None,
                    std::collections::VecDeque::new(),
                ),
            };
        let agent = FakeAgent {
            name: params.name.to_string(),
            kind: params.kind.to_string(),
            reported_agent: state
                .agent_label_override
                .clone()
                .unwrap_or_else(|| params.kind.to_string()),
            pane_id: params.pane_id.to_string(),
            tab_id,
            workspace_id,
            // Codex generates its own session id; it is only reported after the first prompt.
            session_id: match params.kind {
                "codex" => Some(Uuid::new_v4().to_string()),
                _ => session_id_from_args(params.args),
            },
            status,
            interactive_ready,
            launch_pending,
            prompted: false,
            pending_reply: None,
            dialog,
            seen: false,
            pending_dialogs,
            next_dialog_at: None,
            render_queue: std::collections::VecDeque::new(),
            entered_options: Vec::new(),
            exiting: false,
            exit_poll_seen: false,
            ignore_prompts_until: None,
            started_at: Instant::now(),
        };
        let started = AgentStarted {
            agent: agent.info(!state.hide_agent_session),
            argv: params.args.to_vec(),
        };
        state.agents.push(agent);
        Ok(started)
    }

    async fn agent_rename(&self, target: &str, name: &str) -> Result<AgentInfo, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "agent.rename",
            vec![("target", target.to_string()), ("name", name.to_string())],
        ));
        if state.rename_taken_once {
            state.rename_taken_once = false;
            return Err(api_error("agent_name_taken", "name is taken"));
        }
        if state.agents.iter().any(|agent| agent.name == name) {
            return Err(api_error("agent_name_taken", "name is taken"));
        }
        let index = state
            .agents
            .iter()
            .position(|agent| agent.name == target || agent.pane_id == target)
            .ok_or_else(|| api_error("agent_not_found", "no such agent"))?;
        state.agents[index].name = name.to_string();
        Ok(state.agents[index].info(!state.hide_agent_session))
    }

    async fn agent_prompt(
        &self,
        target: &str,
        text: &str,
        wait: Option<WaitSpec>,
    ) -> Result<AgentInfo, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        // The slow plan is decided while locked; every other scripted outcome
        // returns before the block ends so the guard is dead at the sleep below.
        let (delay_ms, reply) = {
            let mut state = self.lock();
            let mut recorded = vec![("target", target.to_string()), ("text", text.to_string())];
            if let Some(spec) = &wait {
                let until = spec
                    .until
                    .iter()
                    .map(|status| {
                        serde_json::to_value(status)
                            .ok()
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                recorded.push(("until", until));
                recorded.push((
                    "timeout_ms",
                    spec.timeout_ms.map(|ms| ms.to_string()).unwrap_or_default(),
                ));
            }
            state.calls.push(call("agent.prompt", recorded));
            let index = state
                .agents
                .iter()
                .position(|agent| agent.name == target || agent.pane_id == target)
                .ok_or_else(|| api_error("agent_not_found", "no such agent"))?;
            if state.agents[index]
                .ignore_prompts_until
                .is_some_and(|until| Instant::now() < until)
            {
                // Re-initializing after the startup dialog: the text is
                // dropped, no transcript entry, and the scripted turn stays
                // queued for the prompt that lands after the window.
                return Err(api_error(
                    "agent_prompt_stalled",
                    "the agent is still re-initializing",
                ));
            }
            state.agents[index].prompted = true;
            match state.turns.pop_front() {
                Some(ScriptedTurn::SlowReply {
                    delay_ms,
                    text: reply,
                }) => {
                    // The agent is busy while the reply is late, like real herdr.
                    state.agents[index].status = AgentStatus::Working;
                    (delay_ms, reply)
                }
                Some(ScriptedTurn::Reply(reply)) => {
                    write_transcript(&state, &state.agents[index], text, &reply);
                    state.agents[index].status = AgentStatus::Idle;
                    state.agents[index].interactive_ready = true;
                    return Ok(state.agents[index].info(!state.hide_agent_session));
                }
                Some(ScriptedTurn::UpstreamError(message)) => {
                    write_transcript_error(&state, &state.agents[index], text, &message);
                    state.agents[index].status = AgentStatus::Idle;
                    state.agents[index].interactive_ready = true;
                    return Ok(state.agents[index].info(!state.hide_agent_session));
                }
                Some(ScriptedTurn::BlockedThenReply(reply)) => {
                    state.agents[index].status = AgentStatus::Blocked;
                    state.agents[index].pending_reply = Some((text.to_string(), reply));
                    return Ok(state.agents[index].info(!state.hide_agent_session));
                }
                Some(ScriptedTurn::BlockedForever) => {
                    state.agents[index].status = AgentStatus::Blocked;
                    return Ok(state.agents[index].info(!state.hide_agent_session));
                }
                Some(ScriptedTurn::DialogThenReply(script, reply)) => {
                    show_dialog(&mut state.agents[index], script);
                    state.agents[index].status = AgentStatus::Blocked;
                    state.agents[index].pending_reply = Some((text.to_string(), reply));
                    return Ok(state.agents[index].info(!state.hide_agent_session));
                }
                Some(ScriptedTurn::DialogsThenReply(scripts, reply)) => {
                    let mut scripts = scripts.into_iter();
                    if let Some(first) = scripts.next() {
                        show_dialog(&mut state.agents[index], first);
                    }
                    state.agents[index].pending_dialogs = scripts.collect();
                    state.agents[index].status = AgentStatus::Blocked;
                    state.agents[index].pending_reply = Some((text.to_string(), reply));
                    return Ok(state.agents[index].info(!state.hide_agent_session));
                }
                Some(ScriptedTurn::Exit) => {
                    state.agents.remove(index);
                    return Err(api_error("agent_not_running", "agent exited"));
                }
                Some(ScriptedTurn::Stall) => {
                    return Err(api_error("agent_prompt_stalled", "prompt did not land"));
                }
                Some(ScriptedTurn::StallThenReply(reply)) => {
                    write_transcript(&state, &state.agents[index], text, &reply);
                    return Err(api_error("agent_prompt_stalled", "prompt reported stalled"));
                }
                None => return Ok(state.agents[index].info(!state.hide_agent_session)),
            }
        };
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        let mut state = self.lock();
        let index = state
            .agents
            .iter()
            .position(|agent| agent.name == target || agent.pane_id == target)
            .ok_or_else(|| api_error("agent_not_found", "no such agent"))?;
        write_transcript(&state, &state.agents[index], text, &reply);
        state.agents[index].status = AgentStatus::Idle;
        state.agents[index].interactive_ready = true;
        Ok(state.agents[index].info(!state.hide_agent_session))
    }

    async fn agent_wait(&self, target: &str, wait: WaitSpec) -> Result<AgentInfo, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let wanted: Vec<AgentStatus> = wait.until;
        {
            let mut state = self.lock();
            let until = wanted
                .iter()
                .map(|status| {
                    serde_json::to_value(status)
                        .ok()
                        .and_then(|value| value.as_str().map(String::from))
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(",");
            state.calls.push(call(
                "agent.wait",
                vec![
                    ("target", target.to_string()),
                    ("until", until.clone()),
                    (
                        "timeout_ms",
                        wait.timeout_ms.map(|ms| ms.to_string()).unwrap_or_default(),
                    ),
                ],
            ));
        }
        // An active dialog makes the wait block like the real server: poll until
        // the dialog closes, the agent exits, or the timeout elapses. The guard
        // is never held across the poll sleep.
        let deadline =
            Instant::now() + std::time::Duration::from_millis(wait.timeout_ms.unwrap_or(3_600_000));
        loop {
            {
                let mut state = self.lock();
                let index = state
                    .agents
                    .iter()
                    .position(|agent| agent.name == target || agent.pane_id == target)
                    .ok_or_else(|| api_error("agent_not_found", "no such agent"))?;
                open_due_dialog(&mut state.agents[index]);
                let agent = &state.agents[index];
                let dialog_open = agent.dialog.is_some();
                // Between two scripted dialogs the agent is briefly `working`
                // with the next one queued: keep polling so waiters see the
                // blocked -> working -> blocked transition.
                let between_dialogs = !dialog_open && !agent.pending_dialogs.is_empty();
                if (!dialog_open && !between_dialogs) || wanted.contains(&agent.status) {
                    // Resolve a dialog-less blocked reply only when no dialog
                    // is open; an open dialog's answer comes from `send_keys`.
                    if !dialog_open && state.agents[index].status == AgentStatus::Blocked {
                        if let Some((prompt, reply)) = state.agents[index].pending_reply.take() {
                            if wanted.contains(&AgentStatus::Idle) {
                                write_transcript(&state, &state.agents[index], &prompt, &reply);
                                state.agents[index].status = AgentStatus::Idle;
                                state.agents[index].interactive_ready = true;
                            } else {
                                state.agents[index].pending_reply = Some((prompt, reply));
                            }
                        }
                    }
                    if wanted.contains(&state.agents[index].status) {
                        return Ok(state.agents[index].info(!state.hide_agent_session));
                    }
                    return Err(api_error("timeout", "wait condition not met in time"));
                }
            }
            if Instant::now() >= deadline {
                return Err(api_error("timeout", "wait condition not met in time"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    async fn agent_send_keys(&self, target: &str, keys: &[&str]) -> Result<(), HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "agent.send_keys",
            vec![("target", target.to_string()), ("keys", keys.join(","))],
        ));
        let index = state
            .agents
            .iter()
            .position(|agent| agent.name == target || agent.pane_id == target)
            .ok_or_else(|| api_error("agent_not_found", "no such agent"))?;
        if state.name_target_fails && state.agents[index].name == target {
            return Err(api_error(
                "agent_not_found",
                "the agent name does not resolve in this fake",
            ));
        }
        open_due_dialog(&mut state.agents[index]);
        // A single logical key on an active dialog moves its cursor (menus
        // wrap), closes it, or exits the agent (answering a reject option,
        // like "No, exit"). `enter` records the option index it selected.
        let decision = state.agents[index]
            .dialog
            .as_ref()
            .map(|script| dialog_key(script, keys));
        if let Some(cursor) = state.agents[index]
            .dialog
            .as_ref()
            .filter(|_| matches!(decision, Some(DialogKey::Accept | DialogKey::Exit)))
            .map(|script| script.cursor)
        {
            state.agents[index].entered_options.push(cursor);
        }
        match decision {
            Some(DialogKey::Cursor(cursor)) => {
                if let Some(script) = state.agents[index].dialog.as_mut() {
                    script.cursor = cursor;
                }
                if state.options_change_on_key {
                    state.options_change_on_key = false;
                    if let Some(script) = state.agents[index].dialog.as_mut() {
                        script.options = script
                            .options
                            .iter()
                            .map(|option| format!("{option} (changed)"))
                            .collect();
                    }
                }
            }
            Some(DialogKey::Accept) => {
                let finished = {
                    let agent = &mut state.agents[index];
                    agent.dialog = None;
                    if agent.pending_dialogs.is_empty() {
                        agent.status = AgentStatus::Idle;
                        agent.interactive_ready = true;
                        true
                    } else {
                        // The agent keeps working; the next scripted dialog
                        // opens after a short delay.
                        agent.status = AgentStatus::Working;
                        agent.next_dialog_at = Some(Instant::now() + NEXT_DIALOG_DELAY);
                        false
                    }
                };
                if finished {
                    let pending = state.agents[index].pending_reply.take();
                    match pending {
                        Some((prompt, reply)) => {
                            write_transcript(&state, &state.agents[index], &prompt, &reply);
                        }
                        // A STARTUP dialog (no turn was prompted yet): the CLI
                        // re-initializes and drops prompts for the configured
                        // window while already reporting idle.
                        None if state.prompt_ignore_window_ms > 0 => {
                            state.agents[index].ignore_prompts_until = Some(
                                Instant::now()
                                    + std::time::Duration::from_millis(
                                        state.prompt_ignore_window_ms,
                                    ),
                            );
                        }
                        None => {}
                    }
                }
            }
            Some(DialogKey::Exit) => {
                // The exit option was chosen: the agent leaves `blocked`
                // briefly before it exits, so the next poll sees a non-blocked
                // status and the one after finds it gone.
                let agent = &mut state.agents[index];
                agent.dialog = None;
                agent.status = AgentStatus::Unknown;
                agent.exiting = true;
            }
            Some(DialogKey::Ignore) | None => {}
        }
        Ok(())
    }

    async fn agent_read(
        &self,
        target: &str,
        source: ReadSource,
        lines: Option<u32>,
    ) -> Result<String, HerdrError> {
        if self.lock().unavailable {
            return Err(unavailable_error());
        }
        let mut state = self.lock();
        state.calls.push(call(
            "agent.read",
            vec![
                ("target", target.to_string()),
                ("source", source.as_str().to_string()),
                ("lines", lines.map(|n| n.to_string()).unwrap_or_default()),
            ],
        ));
        if state.read_fails {
            return Err(api_error("pane_read_failed", "fake pane read fails"));
        }
        let index = state
            .agents
            .iter()
            .position(|agent| agent.name == target || agent.pane_id == target)
            .ok_or_else(|| api_error("agent_not_found", "no such agent"))?;
        if state.name_target_fails && state.agents[index].name == target {
            return Err(api_error(
                "agent_not_found",
                "the agent name does not resolve in this fake",
            ));
        }
        open_due_dialog(&mut state.agents[index]);
        if source == ReadSource::Detection {
            let lag = state.render_lag;
            if let Some(script) = state.agents[index].dialog.clone() {
                let cursor = lagged_cursor(&mut state.agents[index], script.cursor, lag);
                return Ok(dialog_screen(&script, cursor));
            }
        }
        Ok("fake pane output".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-fake-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    async fn spawn_ready_agent(fake: &FakeHerdr) -> (String, String) {
        let created = fake
            .workspace_create("openai-api", Path::new("/tmp"), false)
            .await
            .expect("workspace");
        let tab = fake
            .tab_create(TabCreate {
                workspace_id: &created.workspace.workspace_id,
                cwd: Some(Path::new("/tmp")),
                label: Some("oa:claude:abc123"),
                env: &BTreeMap::new(),
                focus: false,
            })
            .await
            .expect("tab");
        let args = vec!["--session-id".to_string(), "sess-1".to_string()];
        fake.agent_start(AgentStart {
            name: "oa-claude-abc123",
            kind: "claude",
            pane_id: &tab.root_pane.pane_id,
            args: &args,
            timeout_ms: Some(1000),
        })
        .await
        .expect("start");
        (created.workspace.workspace_id, tab.tab.tab_id)
    }

    #[tokio::test]
    async fn records_calls_and_scripts_reply_transcript() {
        let root = temp_dir("reply");
        let fake = FakeHerdr::new()
            .with_transcript_root(&root)
            .with_turns([ScriptedTurn::Reply("answer".to_string())]);
        let (workspace_id, _) = spawn_ready_agent(&fake).await;
        let info = fake
            .agent_prompt(
                "oa-claude-abc123",
                "hello",
                Some(WaitSpec {
                    until: vec![AgentStatus::Idle],
                    timeout_ms: Some(10),
                }),
            )
            .await
            .expect("prompt");
        assert_eq!(info.agent_status, AgentStatus::Idle);
        let transcript = root.join("projects").join("fake").join("sess-1.jsonl");
        let text = std::fs::read_to_string(&transcript).expect("transcript written");
        assert!(text.contains("\"type\":\"user\""));
        assert!(text.contains("hello"));
        assert!(text.contains("answer"));
        assert!(text.contains("stop_reason"));
        assert!(text.contains("turn_duration"));

        let calls = fake.calls();
        assert_eq!(calls[0].method, "workspace.create");
        let prompt = calls
            .iter()
            .find(|c| c.method == "agent.prompt")
            .expect("call");
        assert_eq!(prompt.param("text"), Some("hello"));
        assert_eq!(prompt.param("until"), Some("idle"));
        assert_eq!(prompt.param("timeout_ms"), Some("10"));
        assert_eq!(
            fake.tab_labels(&workspace_id),
            vec!["openai-api".to_string(), "oa:claude:abc123".to_string()]
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    async fn spawn_codex_agent(fake: &FakeHerdr) {
        let created = fake
            .workspace_create("openai-api", Path::new("/tmp"), false)
            .await
            .expect("workspace");
        let tab = fake
            .tab_create(TabCreate {
                workspace_id: &created.workspace.workspace_id,
                cwd: Some(Path::new("/tmp")),
                label: Some("oa:codex:abc123"),
                env: &BTreeMap::new(),
                focus: false,
            })
            .await
            .expect("tab");
        fake.agent_start(AgentStart {
            name: "oa-codex-abc123",
            kind: "codex",
            pane_id: &tab.root_pane.pane_id,
            args: &[],
            timeout_ms: Some(1000),
        })
        .await
        .expect("start");
    }

    #[tokio::test]
    async fn codex_reports_session_after_prompt_and_writes_rollout() {
        let root = temp_dir("codex");
        let fake = FakeHerdr::new()
            .with_codex_root(&root)
            .with_turns([ScriptedTurn::Reply("rollout reply".to_string())]);
        spawn_codex_agent(&fake).await;
        let info = fake.agent_get("oa-codex-abc123").await.expect("agent");
        assert!(
            info.agent_session.is_none(),
            "no session id before the first prompt"
        );

        fake.agent_prompt("oa-codex-abc123", "hello", None)
            .await
            .expect("prompt");
        let info = fake.agent_get("oa-codex-abc123").await.expect("agent");
        let session = info.agent_session.expect("session after prompt");
        assert_eq!(session.kind, "id");
        assert_eq!(session.agent, "codex");

        let located = crate::transcript::codex::locate_codex_rollout(&root, &session.value)
            .expect("rollout file");
        assert!(located.starts_with(root.join("2026").join("10").join("14")));
        let text = std::fs::read_to_string(&located).expect("rollout readable");
        assert!(text.contains("\"session_meta\""));
        assert!(text.contains("\"task_started\""));
        assert!(text.contains("final_answer"));
        assert!(text.contains("rollout reply"));
        assert!(text.contains("\"task_complete\""));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn hidden_agent_session_is_never_reported() {
        let root = temp_dir("codex-hidden");
        let fake = FakeHerdr::new()
            .with_codex_root(&root)
            .with_agent_session_hidden()
            .with_turns([ScriptedTurn::Reply("x".to_string())]);
        spawn_codex_agent(&fake).await;
        fake.agent_prompt("oa-codex-abc123", "hello", None)
            .await
            .expect("prompt");
        let info = fake.agent_get("oa-codex-abc123").await.expect("agent");
        assert!(info.agent_session.is_none());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn startup_behaviors_and_exit_script() {
        let fake = FakeHerdr::new().with_startup(StartupBehavior::Blocked);
        let _ = spawn_ready_agent(&fake).await;
        let info = fake.agent_get("oa-claude-abc123").await.expect("agent");
        assert_eq!(info.agent_status, AgentStatus::Blocked);
        assert!(!info.interactive_ready);

        let fake = FakeHerdr::new().with_turns([ScriptedTurn::Exit]);
        let _ = spawn_ready_agent(&fake).await;
        let err = fake
            .agent_prompt("oa-claude-abc123", "go", None)
            .await
            .expect_err("exit");
        match err {
            HerdrError::Api { code, .. } => assert_eq!(code, "agent_not_running"),
            other => panic!("expected api error, got {other:?}"),
        }
        let err = fake.agent_get("oa-claude-abc123").await.expect_err("gone");
        match err {
            HerdrError::Api { code, .. } => assert_eq!(code, "agent_not_found"),
            other => panic!("expected api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closing_last_tab_closes_workspace() {
        let fake = FakeHerdr::new();
        let created = fake
            .workspace_create("openai-api", Path::new("/tmp"), false)
            .await
            .expect("workspace");
        fake.tab_close(&created.tab.tab_id).await.expect("close");
        assert!(fake.workspace_list().await.expect("list").is_empty());
    }
}
