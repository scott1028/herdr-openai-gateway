//! Turn execution state machine: prompt the backing agent, follow herdr status
//! transitions, and confirm completion from the agent's own transcript JSONL.

use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::AgentKind;
use crate::herdr::{
    AgentInfo, AgentSessionInfo, AgentStatus, HerdrApi, HerdrError, ReadSource, WaitSpec,
};
use crate::openai::ApiError;
use crate::prompt;
use crate::transcript::{self, claude, codex, StopKind, TurnScan};

use super::lifecycle::AgentHandle;

/// Timeouts and polling cadence for one turn.
#[derive(Debug, Clone)]
pub struct TurnLimits {
    pub turn_timeout_ms: u64,
    pub blocked_timeout_ms: u64,
    pub completion_grace_ms: u64,
    /// 200 ms in production; small in tests.
    pub poll_interval: Duration,
}

/// Terminal result of a turn, including non-error outcomes the client should see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed { text: String, stop: StopKind },
    Aborted { reason: String },
    BlockedTimeout,
    UpstreamError { message: String },
}

/// Failure of the turn machinery; maps onto the HTTP error codes.
#[derive(Debug)]
pub enum TurnError {
    AgentExited,
    Stalled,
    Timeout,
    CompletionNotObserved,
    /// A Codex agent never reported its session id, so the rollout cannot be
    /// found; `home_display` names the CODEX_HOME that was searched.
    TranscriptUnavailable {
        home_display: String,
    },
    Herdr(HerdrError),
    PromptTooLarge,
}

impl TurnError {
    pub fn into_api_error(self) -> ApiError {
        match self {
            Self::AgentExited => {
                ApiError::backing_agent_exited("the backing agent exited during the turn")
            }
            Self::Stalled => {
                ApiError::agent_prompt_stalled("the backing agent did not accept the prompt")
            }
            Self::Timeout => ApiError::turn_timeout("the turn exceeded its timeout"),
            Self::CompletionNotObserved => ApiError::completion_not_observed(
                "turn completion was not observed in the agent transcript",
            ),
            Self::TranscriptUnavailable { home_display } => {
                ApiError::completion_not_observed(format!(
                    "codex session id was not reported by herdr and no rollout was found under \
                     {home_display}; run: CODEX_HOME={home_display} herdr integration install \
                     codex (requires integration v8)"
                ))
            }
            Self::Herdr(err) => ApiError::herdr_unavailable(err.to_string()),
            Self::PromptTooLarge => ApiError::prompt_too_large("prompt exceeds the size limit"),
        }
    }
}

/// One-shot wait for an agent that was still busy before the prompt.
const PRECHECK_WAIT_MS: u64 = 30_000;
/// Window to look for evidence the prompt landed after `agent_prompt_stalled`.
const STALL_POLL_MS: u64 = 10_000;

/// Where transcript files live for one turn, per backing agent kind.
#[derive(Debug, Clone)]
pub enum TranscriptRoots {
    /// Claude roots holding `projects/*/<session_id>.jsonl`.
    Claude(Vec<PathBuf>),
    /// Codex `sessions` root holding `YYYY/MM/DD/rollout-*-<session_id>.jsonl`;
    /// `home_display` is the CODEX_HOME it came from, shown to the client.
    Codex {
        sessions_root: PathBuf,
        home_display: String,
    },
}

/// Called right before every wait for a blocked agent to clear, so the service
/// can relay the agent's dialog to the client. Must not change turn behavior.
pub trait BlockedHook: Send + Sync {
    fn on_blocked(&self, agent: &AgentHandle) -> impl Future<Output = ()> + Send;

    /// Return the duration added to this turn's timeout deadline.
    fn paused(&self) -> Duration {
        Duration::ZERO
    }

    /// Notify the hook after Claude switches to a continued-in session.
    fn session_updated(&self, _agent: &AgentHandle) {}
}

/// No-op hook for turn tests and callers without a client to relay to.
impl BlockedHook for () {
    async fn on_blocked(&self, _agent: &AgentHandle) {}
}

/// Turn parser selected by the backing agent's kind.
enum TurnParser {
    Claude(claude::ClaudeTurnParser),
    Codex(codex::CodexTurnParser),
}

impl TurnParser {
    fn for_kind(kind: AgentKind) -> Self {
        match kind {
            AgentKind::Claude => Self::Claude(claude::ClaudeTurnParser::new()),
            AgentKind::Codex => Self::Codex(codex::CodexTurnParser::new()),
        }
    }

    fn feed(&mut self, entry: &Value) {
        match self {
            Self::Claude(parser) => parser.feed(entry),
            Self::Codex(parser) => parser.feed(entry),
        }
    }

    fn scan(&self, agent_idle: bool) -> TurnScan {
        match self {
            Self::Claude(parser) => parser.scan(agent_idle),
            Self::Codex(parser) => parser.scan(agent_idle),
        }
    }
}

pub async fn run_turn<H: HerdrApi, Bk: BlockedHook>(
    h: &H,
    agent: &mut AgentHandle,
    prompt_text: &str,
    limits: &TurnLimits,
    roots: &TranscriptRoots,
    hook: &Bk,
) -> Result<TurnOutcome, TurnError> {
    let text = prompt::sanitize(prompt_text);
    prompt::check_prompt_size(&text).map_err(|_| TurnError::PromptTooLarge)?;

    // Pre-check: an exited agent is fatal; a busy or blocked agent gets resolved first.
    match h.agent_get(&agent.name).await {
        Ok(info) => {
            apply_agent_session(agent, info.agent_session.as_ref());
            if info.agent_status == AgentStatus::Blocked {
                hook.on_blocked(agent).await;
                if wait_unblock(h, &agent.name, limits.blocked_timeout_ms)
                    .await?
                    .is_none()
                {
                    return Ok(TurnOutcome::BlockedTimeout);
                }
            } else if matches!(
                info.agent_status,
                AgentStatus::Working | AgentStatus::Unknown
            ) {
                let wait = WaitSpec {
                    until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked],
                    timeout_ms: Some(PRECHECK_WAIT_MS),
                };
                match h.agent_wait(&agent.name, wait).await {
                    Ok(next) if next.agent_status == AgentStatus::Blocked => {
                        hook.on_blocked(agent).await;
                        if wait_unblock(h, &agent.name, limits.blocked_timeout_ms)
                            .await?
                            .is_none()
                        {
                            return Ok(TurnOutcome::BlockedTimeout);
                        }
                    }
                    Ok(_) => {}
                    Err(HerdrError::Api { code, .. }) if code == "timeout" => {}
                    Err(err) => return Err(exit_or_herdr(err)),
                }
            }
        }
        Err(err) => return Err(exit_or_herdr(err)),
    }

    // Transcript cursor: skip existing history; unknown files are located on later polls.
    if agent.transcript.is_none() {
        refresh_codex_session(h, agent).await;
        agent.transcript = locate_transcript(agent, roots);
    }
    let mut cursor = transcript::JsonlCursor {
        path: agent.transcript.clone(),
        ..transcript::JsonlCursor::default()
    };
    if cursor.path.is_some() {
        let _ = cursor.read_new_lines();
    }

    let turn_deadline = Instant::now() + Duration::from_millis(limits.turn_timeout_ms);
    let prompt_wait = WaitSpec {
        until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked],
        timeout_ms: Some(limits.turn_timeout_ms),
    };
    // Created before the prompt so lines read during stall polling still reach the parser.
    let mut parser = TurnParser::for_kind(agent.kind);
    let mut info = match h
        .agent_prompt(&agent.name, &text, Some(prompt_wait.clone()))
        .await
    {
        Ok(info) => info,
        Err(HerdrError::Api { code, .. }) if code == "agent_blocked" => {
            hook.on_blocked(agent).await;
            match wait_unblock(h, &agent.name, limits.blocked_timeout_ms).await? {
                None => return Ok(TurnOutcome::BlockedTimeout),
                Some(next)
                    if matches!(next.agent_status, AgentStatus::Idle | AgentStatus::Done) =>
                {
                    // Retry the prompt exactly once after the block cleared.
                    match h.agent_prompt(&agent.name, &text, Some(prompt_wait)).await {
                        Ok(info) => info,
                        Err(HerdrError::Api { code, .. }) if code == "timeout" => {
                            recover_prompt_timeout(h, agent, turn_deadline, hook).await?
                        }
                        Err(err) => return Err(exit_or_herdr(err)),
                    }
                }
                Some(next) => next,
            }
        }
        Err(HerdrError::Api { code, .. }) if code == "agent_prompt_stalled" => {
            match stall_poll(h, agent, &mut cursor, &mut parser, limits, roots).await {
                Some(next) => next,
                None => return Err(TurnError::Stalled),
            }
        }
        Err(HerdrError::Api { code, .. }) if code == "timeout" => {
            recover_prompt_timeout(h, agent, turn_deadline, hook).await?
        }
        Err(err) => return Err(exit_or_herdr(err)),
    };
    apply_agent_session(agent, info.agent_session.as_ref());

    let mut grace_deadline: Option<Instant> = None;
    loop {
        match info.agent_status {
            AgentStatus::Blocked => {
                grace_deadline = None;
                hook.on_blocked(agent).await;
                match wait_unblock(h, &agent.name, limits.blocked_timeout_ms).await? {
                    Some(next) => info = next,
                    None => return Ok(TurnOutcome::BlockedTimeout),
                }
            }
            AgentStatus::Working | AgentStatus::Unknown => {
                grace_deadline = None;
                let deadline = turn_deadline + hook.paused();
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(TurnError::Timeout);
                }
                let wait = WaitSpec {
                    until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked],
                    timeout_ms: Some(remaining.as_millis() as u64),
                };
                info = match h.agent_wait(&agent.name, wait).await {
                    Ok(next) => next,
                    Err(HerdrError::Api { code, .. }) if code == "timeout" => {
                        if Instant::now() >= turn_deadline + hook.paused() {
                            return Err(TurnError::Timeout);
                        }
                        continue;
                    }
                    Err(err) => return Err(exit_or_herdr(err)),
                };
            }
            AgentStatus::Idle | AgentStatus::Done => {
                let deadline = *grace_deadline.get_or_insert_with(|| {
                    Instant::now() + Duration::from_millis(limits.completion_grace_ms)
                });
                for line in read_new_lines(agent, &mut cursor, roots) {
                    let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    parser.feed(&entry);
                }
                match parser.scan(true) {
                    TurnScan::Completed { text, stop } => {
                        return Ok(TurnOutcome::Completed { text, stop })
                    }
                    TurnScan::Aborted { reason } => return Ok(TurnOutcome::Aborted { reason }),
                    TurnScan::UpstreamError { message } => {
                        return Ok(TurnOutcome::UpstreamError { message })
                    }
                    TurnScan::SwitchFile { session_id } => {
                        // continued-in (Claude only): follow the new session file from offset 0.
                        if let TranscriptRoots::Claude(list) = roots {
                            if let Some(path) = claude::locate_claude_transcript(list, &session_id)
                            {
                                agent.session_id = Some(session_id);
                                hook.session_updated(agent);
                                agent.transcript = Some(path.clone());
                                cursor = transcript::JsonlCursor {
                                    path: Some(path),
                                    ..transcript::JsonlCursor::default()
                                };
                                parser = TurnParser::for_kind(agent.kind);
                            }
                        }
                    }
                    TurnScan::NotStarted | TurnScan::InProgress => {
                        match h.agent_get(&agent.name).await {
                            Ok(next) => {
                                apply_agent_session(agent, next.agent_session.as_ref());
                                if matches!(
                                    next.agent_status,
                                    AgentStatus::Working
                                        | AgentStatus::Unknown
                                        | AgentStatus::Blocked
                                ) {
                                    info = next;
                                    continue;
                                }
                            }
                            Err(err) => return Err(exit_or_herdr(err)),
                        }
                        if Instant::now() >= deadline {
                            if agent.kind == AgentKind::Codex && agent.transcript.is_none() {
                                let home_display = match roots {
                                    TranscriptRoots::Codex { home_display, .. } => {
                                        home_display.clone()
                                    }
                                    TranscriptRoots::Claude(_) => String::new(),
                                };
                                return Err(TurnError::TranscriptUnavailable { home_display });
                            }
                            // Pane content is log-only; never returned to the client.
                            let tail = h
                                .agent_read(&agent.name, ReadSource::RecentUnwrapped, Some(40))
                                .await
                                .unwrap_or_default();
                            tracing::warn!(
                                name = %agent.name,
                                tail = %tail,
                                "turn completion not observed in the transcript"
                            );
                            return Err(TurnError::CompletionNotObserved);
                        }
                        tokio::time::sleep(limits.poll_interval).await;
                    }
                }
            }
        }
    }
}

/// `agent_not_found`/`agent_not_running` mean the backing agent is gone; everything else
/// stays a herdr error.
fn exit_or_herdr(err: HerdrError) -> TurnError {
    if let HerdrError::Api { code, .. } = &err {
        if code == "agent_not_found" || code == "agent_not_running" {
            return TurnError::AgentExited;
        }
    }
    TurnError::Herdr(err)
}

/// Wait for a blocked agent to move; `None` means the blocked window elapsed.
async fn wait_unblock<H: HerdrApi>(
    h: &H,
    name: &str,
    timeout_ms: u64,
) -> Result<Option<AgentInfo>, TurnError> {
    let wait = WaitSpec {
        until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Working],
        timeout_ms: Some(timeout_ms),
    };
    match h.agent_wait(name, wait).await {
        Ok(info) => Ok(Some(info)),
        Err(HerdrError::Api { code, .. }) if code == "timeout" => Ok(None),
        Err(err) => Err(exit_or_herdr(err)),
    }
}

/// After `agent_prompt_stalled`, look for a new transcript user entry or a status
/// change to `working`; either means the prompt landed. Lines read here are fed to
/// the shared parser so the turn start is not lost.
async fn stall_poll<H: HerdrApi>(
    h: &H,
    agent: &mut AgentHandle,
    cursor: &mut transcript::JsonlCursor,
    parser: &mut TurnParser,
    limits: &TurnLimits,
    roots: &TranscriptRoots,
) -> Option<AgentInfo> {
    let deadline = Instant::now() + Duration::from_millis(STALL_POLL_MS);
    while Instant::now() < deadline {
        let mut landed = false;
        for line in read_new_lines(agent, cursor, roots) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            parser.feed(&entry);
            if is_prompt_landing(&entry, agent.kind) {
                landed = true;
            }
        }
        if landed {
            let info = h.agent_get(&agent.name).await.ok()?;
            apply_agent_session(agent, info.agent_session.as_ref());
            return Some(info);
        }
        if let Ok(info) = h.agent_get(&agent.name).await {
            apply_agent_session(agent, info.agent_session.as_ref());
            if info.agent_status == AgentStatus::Working {
                return Some(info);
            }
        }
        tokio::time::sleep(limits.poll_interval).await;
    }
    None
}

/// Codex: pull the session id out of `agent.get`'s `agent_session` until it lands.
async fn refresh_codex_session<H: HerdrApi>(h: &H, agent: &mut AgentHandle) {
    if agent.kind != AgentKind::Codex || agent.session_id.is_some() || agent.transcript.is_some() {
        return;
    }
    if let Ok(info) = h.agent_get(&agent.name).await {
        apply_agent_session(agent, info.agent_session.as_ref());
    }
}

async fn recover_prompt_timeout<H: HerdrApi>(
    h: &H,
    agent: &AgentHandle,
    turn_deadline: Instant,
    hook: &impl BlockedHook,
) -> Result<AgentInfo, TurnError> {
    if Instant::now() >= turn_deadline + hook.paused() {
        return Err(TurnError::Timeout);
    }
    h.agent_get(&agent.name).await.map_err(exit_or_herdr)
}

/// Record a reported session for a Codex agent: kind `id` gives the rollout session
/// id, kind `path` gives the rollout file directly.
fn apply_agent_session(agent: &mut AgentHandle, session: Option<&AgentSessionInfo>) {
    if agent.kind != AgentKind::Codex {
        return;
    }
    let Some(session) = session.filter(|session| !session.value.is_empty()) else {
        return;
    };
    match session.kind.as_str() {
        "id" => agent.session_id = Some(session.value.clone()),
        "path" => agent.transcript = Some(PathBuf::from(&session.value)),
        _ => {}
    }
}

/// Locate the transcript file from what the handle already knows.
fn locate_transcript(agent: &AgentHandle, roots: &TranscriptRoots) -> Option<PathBuf> {
    if let Some(path) = agent.transcript.as_ref() {
        return Some(path.clone());
    }
    let session_id = agent.session_id.as_deref()?;
    match roots {
        TranscriptRoots::Claude(list) => claude::locate_claude_transcript(list, session_id),
        TranscriptRoots::Codex { sessions_root, .. } => {
            codex::locate_codex_rollout(sessions_root, session_id)
        }
    }
}

/// Read newly appended transcript lines, locating the file when it was not there yet.
fn read_new_lines(
    agent: &mut AgentHandle,
    cursor: &mut transcript::JsonlCursor,
    roots: &TranscriptRoots,
) -> Vec<String> {
    if cursor.path.is_none() {
        if let Some(path) = locate_transcript(agent, roots) {
            cursor.path = Some(path.clone());
            agent.transcript = Some(path);
        }
    }
    match cursor.read_new_lines() {
        Ok(lines) => lines,
        Err(err) => {
            tracing::warn!(error = %err, "transcript read failed");
            Vec::new()
        }
    }
}

/// True for a transcript entry showing this kind's prompt landed.
fn is_prompt_landing(entry: &Value, kind: AgentKind) -> bool {
    match kind {
        AgentKind::Claude => is_claude_user_text_entry(entry),
        AgentKind::Codex => {
            entry.get("type").and_then(Value::as_str) == Some("event_msg")
                && entry
                    .get("payload")
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("user_message")
        }
    }
}

/// True for a transcript entry that is a non-meta user entry with text content.
fn is_claude_user_text_entry(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match entry.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("text")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::lifecycle::{ensure_workspace, spawn_agent};
    use crate::config::{AgentKind, ModelSpec};
    use crate::herdr::fake::{FakeHerdr, ScriptedTurn};
    use crate::herdr::{
        AgentInfo, AgentStart, AgentStarted, HerdrApi, HerdrError, PaneInfo, Pong, TabCreate,
        TabCreated, TabInfo, WaitSpec, WorkspaceCreated, WorkspaceInfo,
    };
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-server-turn-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn model() -> ModelSpec {
        ModelSpec {
            id: "claude".to_string(),
            kind: AgentKind::Claude,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    fn limits(blocked_timeout_ms: u64) -> TurnLimits {
        TurnLimits {
            turn_timeout_ms: 5_000,
            blocked_timeout_ms,
            completion_grace_ms: 300,
            poll_interval: Duration::from_millis(5),
        }
    }

    fn claude_roots(root: &Path) -> TranscriptRoots {
        TranscriptRoots::Claude(vec![root.to_path_buf()])
    }

    fn codex_roots(root: &Path) -> TranscriptRoots {
        TranscriptRoots::Codex {
            sessions_root: root.to_path_buf(),
            home_display: "~/.codex".to_string(),
        }
    }

    fn codex_model() -> ModelSpec {
        ModelSpec {
            id: "codex".to_string(),
            kind: AgentKind::Codex,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    struct PausedWaitHerdr {
        inner: FakeHerdr,
        waits: AtomicUsize,
        prompt_status: AgentStatus,
        prompt_timeout: bool,
        switch_session: Option<(PathBuf, String, String)>,
    }

    impl HerdrApi for PausedWaitHerdr {
        fn ping(&self) -> impl Future<Output = Result<Pong, HerdrError>> + Send {
            self.inner.ping()
        }
        fn workspace_list(
            &self,
        ) -> impl Future<Output = Result<Vec<WorkspaceInfo>, HerdrError>> + Send {
            self.inner.workspace_list()
        }
        fn workspace_create(
            &self,
            label: &str,
            cwd: &Path,
            focus: bool,
        ) -> impl Future<Output = Result<WorkspaceCreated, HerdrError>> + Send {
            self.inner.workspace_create(label, cwd, focus)
        }
        fn tab_list(
            &self,
            workspace_id: &str,
        ) -> impl Future<Output = Result<Vec<TabInfo>, HerdrError>> + Send {
            self.inner.tab_list(workspace_id)
        }
        fn tab_create(
            &self,
            params: TabCreate<'_>,
        ) -> impl Future<Output = Result<TabCreated, HerdrError>> + Send {
            self.inner.tab_create(params)
        }
        fn tab_rename(
            &self,
            tab_id: &str,
            label: &str,
        ) -> impl Future<Output = Result<TabInfo, HerdrError>> + Send {
            self.inner.tab_rename(tab_id, label)
        }
        fn tab_close(&self, tab_id: &str) -> impl Future<Output = Result<(), HerdrError>> + Send {
            self.inner.tab_close(tab_id)
        }
        fn pane_send_input(
            &self,
            pane_id: &str,
            text: &str,
            keys: &[&str],
        ) -> impl Future<Output = Result<(), HerdrError>> + Send {
            self.inner.pane_send_input(pane_id, text, keys)
        }
        fn pane_list(
            &self,
            workspace_id: &str,
        ) -> impl Future<Output = Result<Vec<PaneInfo>, HerdrError>> + Send {
            self.inner.pane_list(workspace_id)
        }
        fn pane_report_tokens(
            &self,
            pane_id: &str,
            source: &str,
            tokens: &BTreeMap<String, String>,
            ttl_ms: Option<u64>,
        ) -> impl Future<Output = Result<(), HerdrError>> + Send {
            self.inner
                .pane_report_tokens(pane_id, source, tokens, ttl_ms)
        }
        fn agent_get(
            &self,
            target: &str,
        ) -> impl Future<Output = Result<AgentInfo, HerdrError>> + Send {
            self.inner.agent_get(target)
        }
        fn agent_start(
            &self,
            params: AgentStart<'_>,
        ) -> impl Future<Output = Result<AgentStarted, HerdrError>> + Send {
            self.inner.agent_start(params)
        }
        fn agent_rename(
            &self,
            target: &str,
            name: &str,
        ) -> impl Future<Output = Result<AgentInfo, HerdrError>> + Send {
            self.inner.agent_rename(target, name)
        }
        async fn agent_prompt(
            &self,
            target: &str,
            text: &str,
            wait: Option<WaitSpec>,
        ) -> Result<AgentInfo, HerdrError> {
            let prompt_wait_ms = wait.as_ref().and_then(|wait| wait.timeout_ms);
            let info = self.inner.agent_prompt(target, text, wait).await?;
            if self.prompt_timeout {
                tokio::time::sleep(Duration::from_millis(prompt_wait_ms.unwrap_or_default())).await;
                return Err(HerdrError::Api {
                    code: "timeout".to_string(),
                    message: "prompt wait timed out".to_string(),
                });
            }
            if let Some((root, old_session, new_session)) = &self.switch_session {
                let old_path = root
                    .join("projects")
                    .join("fake")
                    .join(format!("{old_session}.jsonl"));
                use std::io::Write as _;
                let mut old_file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(old_path)
                    .expect("old transcript");
                writeln!(
                    old_file,
                    "{}",
                    serde_json::json!({
                        "type": "continued-in",
                        "continuedInSessionId": new_session
                    })
                )
                .expect("append continued-in");
                let new_path = root
                    .join("projects")
                    .join("fake")
                    .join(format!("{new_session}.jsonl"));
                std::fs::write(
                    new_path,
                    format!(
                        "{}\n{}\n{}\n",
                        serde_json::json!({"type":"user","message":{"content":"hello"}}),
                        serde_json::json!({"type":"assistant","message":{"id":"continued","content":[{"type":"text","text":"continued answer"}],"stop_reason":"end_turn"}}),
                        serde_json::json!({"type":"system","subtype":"turn_duration","isMeta":false})
                    ),
                )
                .expect("write continued transcript");
            }
            Ok(AgentInfo {
                agent_status: self.prompt_status,
                ..info
            })
        }
        async fn agent_wait(&self, target: &str, wait: WaitSpec) -> Result<AgentInfo, HerdrError> {
            if self.prompt_status == AgentStatus::Working
                && self.waits.fetch_add(1, Ordering::SeqCst) == 0
            {
                tokio::time::sleep(Duration::from_millis(wait.timeout_ms.unwrap_or(1))).await;
                return Err(HerdrError::Api {
                    code: "timeout".to_string(),
                    message: "wait timed out".to_string(),
                });
            }
            self.inner.agent_wait(target, wait).await
        }
        fn agent_send_keys(
            &self,
            target: &str,
            keys: &[&str],
        ) -> impl Future<Output = Result<(), HerdrError>> + Send {
            self.inner.agent_send_keys(target, keys)
        }
        fn agent_read(
            &self,
            target: &str,
            source: ReadSource,
            lines: Option<u32>,
        ) -> impl Future<Output = Result<String, HerdrError>> + Send {
            self.inner.agent_read(target, source, lines)
        }
    }

    struct PauseAfterTimeout {
        duration: Duration,
        checks: AtomicUsize,
    }

    struct FixedPause(Duration);

    impl BlockedHook for FixedPause {
        async fn on_blocked(&self, _agent: &AgentHandle) {}

        fn paused(&self) -> Duration {
            self.0
        }
    }

    impl BlockedHook for PauseAfterTimeout {
        async fn on_blocked(&self, _agent: &AgentHandle) {}

        fn paused(&self) -> Duration {
            if self.checks.fetch_add(1, Ordering::SeqCst) == 0 {
                Duration::ZERO
            } else {
                self.duration
            }
        }
    }

    #[derive(Default)]
    struct SessionTracker(Mutex<Option<String>>);

    impl BlockedHook for SessionTracker {
        async fn on_blocked(&self, _agent: &AgentHandle) {}

        fn session_updated(&self, agent: &AgentHandle) {
            *self.0.lock().unwrap() = agent.session_id.clone();
        }
    }

    async fn setup(
        turns: impl IntoIterator<Item = ScriptedTurn>,
    ) -> (FakeHerdr, AgentHandle, PathBuf) {
        let root = temp_dir("setup");
        let fake = FakeHerdr::new()
            .with_transcript_root(&root)
            .with_turns(turns);
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        let handle = spawn_agent(
            &fake,
            &workspace_id,
            &model(),
            Path::new("/tmp"),
            1_000,
            "owner-test",
        )
        .await
        .expect("spawn");
        (fake, handle, root)
    }

    #[tokio::test]
    async fn reply_completes_from_transcript() {
        let (_fake, mut agent, root) = setup([ScriptedTurn::Reply("the answer".to_string())]).await;
        let outcome = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect("turn");
        assert_eq!(
            outcome,
            TurnOutcome::Completed {
                text: "the answer".to_string(),
                stop: StopKind::EndTurn
            }
        );
        assert!(
            agent.transcript.is_some(),
            "transcript path should be recorded"
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn hook_pause_extends_the_working_turn_deadline() {
        let (fake, mut agent, root) = setup([ScriptedTurn::Reply("answer".to_string())]).await;
        let herdr = PausedWaitHerdr {
            inner: fake,
            waits: AtomicUsize::new(0),
            prompt_status: AgentStatus::Working,
            prompt_timeout: false,
            switch_session: None,
        };
        let mut limits = limits(1_000);
        limits.turn_timeout_ms = 40;
        let outcome = run_turn(
            &herdr,
            &mut agent,
            "hello",
            &limits,
            &claude_roots(&root),
            &PauseAfterTimeout {
                duration: Duration::from_millis(150),
                checks: AtomicUsize::new(0),
            },
        )
        .await
        .expect("paused turn completes");
        assert_eq!(
            outcome,
            TurnOutcome::Completed {
                text: "answer".to_string(),
                stop: StopKind::EndTurn,
            }
        );
        assert_eq!(herdr.waits.load(Ordering::SeqCst), 2);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn prompt_timeout_recovers_when_hook_pause_extends_the_deadline() {
        let (fake, mut agent, root) = setup([ScriptedTurn::Reply("answer".to_string())]).await;
        let herdr = PausedWaitHerdr {
            inner: fake,
            waits: AtomicUsize::new(0),
            prompt_status: AgentStatus::Idle,
            prompt_timeout: true,
            switch_session: None,
        };
        let mut turn_limits = limits(1_000);
        turn_limits.turn_timeout_ms = 40;
        let outcome = run_turn(
            &herdr,
            &mut agent,
            "hello",
            &turn_limits,
            &claude_roots(&root),
            &FixedPause(Duration::from_millis(150)),
        )
        .await
        .expect("paused prompt completes");
        assert_eq!(
            outcome,
            TurnOutcome::Completed {
                text: "answer".to_string(),
                stop: StopKind::EndTurn,
            }
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn prompt_timeout_without_hook_pause_stays_a_timeout() {
        let (fake, mut agent, root) = setup([ScriptedTurn::Reply("answer".to_string())]).await;
        let herdr = PausedWaitHerdr {
            inner: fake,
            waits: AtomicUsize::new(0),
            prompt_status: AgentStatus::Idle,
            prompt_timeout: true,
            switch_session: None,
        };
        let err = run_turn(
            &herdr,
            &mut agent,
            "hello",
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect_err("unpaused prompt times out");
        assert!(matches!(err, TurnError::Timeout));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn continued_in_notifies_hook_of_the_new_session_id() {
        let (fake, mut agent, root) =
            setup([ScriptedTurn::Reply("first session".to_string())]).await;
        let old_session = agent.session_id.clone().expect("old session id");
        let new_session = "continued-session".to_string();
        let herdr = PausedWaitHerdr {
            inner: fake,
            waits: AtomicUsize::new(0),
            prompt_status: AgentStatus::Idle,
            prompt_timeout: false,
            switch_session: Some((root.clone(), old_session, new_session.clone())),
        };
        let tracker = SessionTracker::default();
        let outcome = run_turn(
            &herdr,
            &mut agent,
            "hello",
            &limits(1_000),
            &claude_roots(&root),
            &tracker,
        )
        .await
        .expect("continued turn");
        assert_eq!(
            outcome,
            TurnOutcome::Completed {
                text: "continued answer".to_string(),
                stop: StopKind::EndTurn,
            }
        );
        assert_eq!(agent.session_id.as_deref(), Some(new_session.as_str()));
        assert_eq!(
            tracker.0.lock().unwrap().as_deref(),
            Some(new_session.as_str())
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn blocked_then_reply_completes() {
        let (_fake, mut agent, root) =
            setup([ScriptedTurn::BlockedThenReply("late answer".to_string())]).await;
        let outcome = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect("turn");
        assert_eq!(
            outcome,
            TurnOutcome::Completed {
                text: "late answer".to_string(),
                stop: StopKind::EndTurn
            }
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn blocked_forever_times_out() {
        let (_fake, mut agent, root) = setup([ScriptedTurn::BlockedForever]).await;
        let outcome = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(20),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect("turn");
        assert_eq!(outcome, TurnOutcome::BlockedTimeout);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn stalled_prompt_reports_stalled() {
        let (_fake, mut agent, root) = setup([ScriptedTurn::Stall]).await;
        let err = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect_err("stalled");
        assert!(matches!(err, TurnError::Stalled));
        assert_eq!(err.into_api_error().code, "agent_prompt_stalled");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn stalled_then_reply_completes() {
        let (_fake, mut agent, root) =
            setup([ScriptedTurn::StallThenReply("recovered".to_string())]).await;
        let outcome = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect("turn");
        assert_eq!(
            outcome,
            TurnOutcome::Completed {
                text: "recovered".to_string(),
                stop: StopKind::EndTurn
            }
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn agent_exit_reports_exited() {
        let (_fake, mut agent, root) = setup([ScriptedTurn::Exit]).await;
        let err = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect_err("exited");
        assert!(matches!(err, TurnError::AgentExited));
        assert_eq!(err.into_api_error().code, "backing_agent_exited");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn exited_agent_fails_precheck() {
        let (fake, mut agent, root) = setup([ScriptedTurn::Reply("x".to_string())]).await;
        fake.tab_close(&agent.tab_id).await.expect("kill agent");
        let err = run_turn(
            &fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect_err("precheck");
        assert!(matches!(err, TurnError::AgentExited));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn no_transcript_activity_is_completion_not_observed() {
        let (_fake, mut agent, root) = setup([]).await;
        let mut limits = limits(1_000);
        limits.completion_grace_ms = 100;
        let err = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits,
            &claude_roots(&root),
            &(),
        )
        .await
        .expect_err("no completion");
        assert!(matches!(err, TurnError::CompletionNotObserved));
        assert_eq!(err.into_api_error().code, "completion_not_observed");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn oversized_prompt_is_rejected_before_any_call() {
        let (fake, mut agent, root) = setup([ScriptedTurn::Reply("x".to_string())]).await;
        let huge = "x".repeat(prompt::MAX_PROMPT_BYTES + 1);
        let err = run_turn(
            &fake,
            &mut agent,
            &huge,
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect_err("too large");
        assert!(matches!(err, TurnError::PromptTooLarge));
        assert_eq!(err.into_api_error().code, "prompt_too_large");
        assert!(!fake.calls().iter().any(|c| c.method == "agent.prompt"));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn escape_injection_is_stripped_before_prompt() {
        let (fake, mut agent, root) = setup([ScriptedTurn::Reply("ok".to_string())]).await;
        let outcome = run_turn(
            &fake,
            &mut agent,
            "hi \x1b[201~pasted content\x1b[200~",
            &limits(1_000),
            &claude_roots(&root),
            &(),
        )
        .await
        .expect("turn");
        assert!(matches!(outcome, TurnOutcome::Completed { .. }));
        let calls = fake.calls();
        let prompt_call = calls
            .iter()
            .rev()
            .find(|c| c.method == "agent.prompt")
            .expect("prompt call");
        let sent = prompt_call.param("text").expect("text param");
        assert!(!sent.contains('\x1b'), "escape must not reach herdr");
        assert!(sent.contains("pasted content"));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn codex_reply_completes_from_rollout() {
        let fake = FakeHerdr::new().with_turns([ScriptedTurn::Reply("codex answer".to_string())]);
        let (_fake, mut agent, root) = codex_setup(fake).await;
        let outcome = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &codex_roots(&root),
            &(),
        )
        .await
        .expect("turn");
        assert_eq!(
            outcome,
            TurnOutcome::Completed {
                text: "codex answer".to_string(),
                stop: StopKind::EndTurn
            }
        );
        assert!(
            agent.session_id.is_some(),
            "session id should come from agent_session"
        );
        assert!(
            agent.transcript.is_some(),
            "transcript path should be recorded"
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn codex_session_never_reported_is_transcript_unavailable() {
        let fake = FakeHerdr::new()
            .with_turns([ScriptedTurn::Reply("x".to_string())])
            .with_agent_session_hidden();
        let (_fake, mut agent, root) = codex_setup(fake).await;
        let err = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &codex_roots(&root),
            &(),
        )
        .await
        .expect_err("no session id");
        assert!(matches!(err, TurnError::TranscriptUnavailable { .. }));
        let api_error = err.into_api_error();
        assert_eq!(api_error.code, "completion_not_observed");
        assert!(
            api_error
                .message
                .contains("CODEX_HOME=~/.codex herdr integration install codex"),
            "message must name the home and the install command: {}",
            api_error.message
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn codex_error_reply_is_upstream_error() {
        let fake = FakeHerdr::new().with_turns([ScriptedTurn::UpstreamError(
            "Your workspace is out of credits".to_string(),
        )]);
        let (_fake, mut agent, root) = codex_setup(fake).await;
        let outcome = run_turn(
            &_fake,
            &mut agent,
            "hello",
            &limits(1_000),
            &codex_roots(&root),
            &(),
        )
        .await
        .expect("turn");
        assert_eq!(
            outcome,
            TurnOutcome::UpstreamError {
                message: "Your workspace is out of credits".to_string()
            }
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    async fn codex_setup(fake: FakeHerdr) -> (FakeHerdr, AgentHandle, PathBuf) {
        let root = temp_dir("codex-setup");
        let fake = fake.with_codex_root(&root);
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        let handle = spawn_agent(
            &fake,
            &workspace_id,
            &codex_model(),
            Path::new("/tmp"),
            1_000,
            "owner-test",
        )
        .await
        .expect("spawn");
        (fake, handle, root)
    }

    #[test]
    fn prompt_landing_detection() {
        let entry = |line: &str| -> bool {
            is_prompt_landing(
                &serde_json::from_str::<Value>(line).expect("json"),
                AgentKind::Claude,
            )
        };
        assert!(entry(r#"{"type":"user","message":{"content":"go"}}"#));
        assert!(entry(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"go"}]}}"#
        ));
        assert!(!entry(
            r#"{"type":"user","isMeta":true,"message":{"content":"go"}}"#
        ));
        assert!(!entry(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"x"}]}}"#
        ));
        assert!(!entry(r#"{"type":"assistant","message":{"content":"x"}}"#));

        let codex_entry = |line: &str| -> bool {
            is_prompt_landing(
                &serde_json::from_str::<Value>(line).expect("json"),
                AgentKind::Codex,
            )
        };
        assert!(codex_entry(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"go"}}"#
        ));
        assert!(!codex_entry(
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"go"}}"#
        ));
        assert!(!codex_entry(
            r#"{"type":"response_item","payload":{"type":"message","role":"user"}}"#
        ));
    }
}
