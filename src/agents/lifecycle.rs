//! Backing agent lifecycle: workspace find-or-create, naming, spawn, close, orphan sweep.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::config::{AgentKind, ModelSpec};
use crate::herdr::{
    AgentStart, AgentStatus, HerdrApi, HerdrError, PaneInfo, ReadSource, TabCreate, TabInfo,
};
use crate::openai::ApiError;

/// Tab labels owned by herdr-openai-server start with this prefix; nothing else is ever closed.
pub const TAB_LABEL_PREFIX: &str = "oa:";
/// `pane.report_metadata` source for the owner token.
pub const OWNER_TOKEN_SOURCE: &str = "herdr-openai-server";
/// Pane token naming the server instance that owns a backing tab; tabs another
/// live instance owns are never closed.
pub const OWNER_TOKEN_KEY: &str = "oa_owner";
/// Owner token lifetime: three reaper ticks, so one missed refresh keeps it.
pub const OWNER_TOKEN_TTL_MS: u64 = 90_000;

const PANE_BUSY_RETRY: Duration = Duration::from_millis(100);
const PANE_BUSY_WINDOW: Duration = Duration::from_secs(5);
const STARTUP_POLL: Duration = Duration::from_millis(100);
const NAME_RETRY_LIMIT: usize = 3;

/// Slug for agent names: lowercase, non-alphanumerics collapse to one `-`, max 20 chars.
fn model_slug(model_id: &str) -> String {
    let mut slug = String::new();
    for c in model_id.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(20).collect();
    slug.trim_matches('-').to_string()
}

/// herdr agent name: `oa-<slug>-<6hex>`, matching `[a-z][a-z0-9_-]{0,31}`.
pub fn agent_name(model_id: &str, suffix_hex: &str) -> String {
    format!("oa-{}-{suffix_hex}", model_slug(model_id))
}

/// herdr tab label for a backing agent: `oa:<model_id>:<6hex>`.
pub fn tab_label(model_id: &str, suffix_hex: &str) -> String {
    format!("{TAB_LABEL_PREFIX}{model_id}:{suffix_hex}")
}

/// Six lowercase hex characters derived from a v4 uuid.
pub fn random_suffix_hex() -> String {
    Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(6)
        .collect()
}

/// Pick the backing agent cwd: request header, then the working directory the client
/// names in its system prompt, then model cwd, then default. The result is
/// canonicalized and must sit inside one of the allowed roots; the error names the
/// source of the rejected path.
pub fn resolve_cwd(
    header: Option<&str>,
    client_cwd: Option<&str>,
    model_cwd: Option<&Path>,
    default_cwd: &Path,
    allowed_roots: &[PathBuf],
    home: &str,
) -> Result<PathBuf, ApiError> {
    let (requested, source) = match header.filter(|value| !value.is_empty()) {
        Some(value) => (PathBuf::from(value), "X-Herdr-Cwd"),
        None => match client_cwd.filter(|value| !value.is_empty()) {
            Some(value) => (PathBuf::from(value), "the client's Working directory"),
            None => match model_cwd {
                Some(path) => (path.to_path_buf(), "the model cwd"),
                None => (default_cwd.to_path_buf(), "defaults.cwd"),
            },
        },
    };
    let expanded = crate::config::expand_tilde(&requested, home);
    let canonical = std::fs::canonicalize(&expanded).map_err(|_| {
        ApiError::cwd_not_allowed(format!(
            "cwd does not exist: {} (from {source})",
            expanded.display()
        ))
    })?;
    let roots: Vec<PathBuf> = allowed_roots
        .iter()
        .map(|root| {
            std::fs::canonicalize(crate::config::expand_tilde(root, home))
                .unwrap_or_else(|_| root.clone())
        })
        .collect();
    if roots.iter().any(|root| canonical.starts_with(root)) {
        return Ok(canonical);
    }
    Err(ApiError::cwd_not_allowed(format!(
        "cwd {} is outside the allowed roots (from {source})",
        canonical.display()
    )))
}

/// A live backing agent and where it lives inside herdr.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    pub name: String,
    pub kind: AgentKind,
    pub model_id: String,
    pub pane_id: String,
    pub tab_id: String,
    pub tab_label: String,
    pub workspace_id: String,
    /// The cwd this agent was launched in; named when startup is blocked.
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    pub transcript: Option<PathBuf>,
    /// False after `launch_agent` until `await_ready` confirms the agent is interactive.
    pub ready: bool,
    /// True when herdr started the process (`agent.start`); false when the
    /// command was typed into the tab's shell, so herdr only detects it.
    pub managed: bool,
}

/// Why spawning a backing agent failed; maps onto the HTTP error codes.
#[derive(Debug)]
pub enum SpawnError {
    /// The agent is waiting for input (trust/onboarding); the tab is kept for the user.
    StartupBlocked {
        tab_label: String,
        cwd: PathBuf,
    },
    StartFailed,
    Timeout,
    Herdr(HerdrError),
}

/// What a blocked startup means for the user: the tab and its cwd say where to answer.
pub fn blocked_message(tab_label: &str, cwd: &Path) -> String {
    format!(
        "backing agent is waiting for input in herdr tab {tab_label} (cwd {}), usually a \
         first-run dialog such as folder trust or login; answer it in that herdr tab and \
         resend, or use a folder this CLI already trusts",
        cwd.display()
    )
}

impl SpawnError {
    pub fn into_api_error(self) -> ApiError {
        match self {
            Self::StartupBlocked { tab_label, cwd } => {
                ApiError::agent_startup_blocked(blocked_message(&tab_label, &cwd))
            }
            Self::StartFailed => ApiError::agent_start_failed("backing agent failed to start"),
            Self::Timeout => {
                ApiError::agent_start_failed("backing agent did not become ready in time")
            }
            Self::Herdr(err) => ApiError::herdr_unavailable(err.to_string()),
        }
    }
}

/// Find the dedicated workspace by exact label; otherwise create it and rename the
/// root tab to `lobby` (closing the last tab would close the workspace).
pub async fn ensure_workspace<H: HerdrApi>(h: &H, label: &str) -> Result<String, HerdrError> {
    let workspaces = h.workspace_list().await?;
    if let Some(workspace) = workspaces.iter().find(|ws| ws.label == label) {
        return Ok(workspace.workspace_id.clone());
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let created = h.workspace_create(label, Path::new(&home), false).await?;
    h.tab_rename(&created.tab.tab_id, "lobby").await?;
    Ok(created.workspace.workspace_id)
}

/// Per-launch additions on top of the model spec.
#[derive(Debug, Default)]
pub struct LaunchExtras {
    /// Appended after the model's args.
    pub args: Vec<String>,
    /// Added to the tab env over the model's env.
    pub env: BTreeMap<String, String>,
}

/// Launch a backing agent: create the tab, tag its pane with `owner_id`, and
/// start the process. The returned handle has `ready = false`; call
/// [`await_ready`] before prompting it.
pub async fn launch_agent<H: HerdrApi>(
    h: &H,
    workspace_id: &str,
    model: &ModelSpec,
    cwd: &Path,
    startup_timeout_ms: u64,
    extras: &LaunchExtras,
    owner_id: &str,
) -> Result<AgentHandle, SpawnError> {
    let suffix = random_suffix_hex();
    let label = tab_label(&model.id, &suffix);
    let mut env = model.env.clone();
    env.extend(extras.env.clone());
    let created = h
        .tab_create(TabCreate {
            workspace_id,
            cwd: Some(cwd),
            label: Some(&label),
            env: &env,
            focus: false,
        })
        .await
        .map_err(SpawnError::Herdr)?;
    let tab_id = created.tab.tab_id.clone();
    let pane_id = created.root_pane.pane_id.clone();
    // An untagged tab looks like an orphan to sweeps, so tag it first.
    if let Err(err) = report_owner_token(h, &pane_id, owner_id).await {
        close_backing_tab(h, &tab_id).await;
        return Err(SpawnError::Herdr(err));
    }

    let (session_id, kind_args) = match model.kind {
        AgentKind::Claude => {
            let id = Uuid::new_v4().to_string();
            (Some(id.clone()), vec!["--session-id".to_string(), id])
        }
        AgentKind::Codex => (None, Vec::new()),
    };
    let mut args = model.args.clone();
    args.extend_from_slice(&extras.args);
    args.extend(kind_args);

    let base = model.kind.herdr_label();
    let command = model.command.as_deref().unwrap_or(base);
    let managed = command == base;

    let mut name = agent_name(&model.id, &suffix);
    if managed {
        let busy_deadline = Instant::now() + PANE_BUSY_WINDOW;
        let mut name_retries = 0usize;
        loop {
            match h
                .agent_start(AgentStart {
                    name: &name,
                    kind: model.kind.herdr_label(),
                    pane_id: &pane_id,
                    args: &args,
                    timeout_ms: Some(startup_timeout_ms),
                })
                .await
            {
                Ok(_) => break,
                Err(HerdrError::Api { code, .. }) if code == "agent_pane_busy" => {
                    if Instant::now() >= busy_deadline {
                        close_backing_tab(h, &tab_id).await;
                        return Err(SpawnError::StartFailed);
                    }
                    tokio::time::sleep(PANE_BUSY_RETRY).await;
                }
                Err(HerdrError::Api { code, .. }) if code == "agent_name_taken" => {
                    name_retries += 1;
                    if name_retries > NAME_RETRY_LIMIT {
                        close_backing_tab(h, &tab_id).await;
                        return Err(SpawnError::StartFailed);
                    }
                    name = agent_name(&model.id, &random_suffix_hex());
                }
                Err(err) => {
                    close_backing_tab(h, &tab_id).await;
                    return Err(SpawnError::Herdr(err));
                }
            }
        }
    } else {
        // herdr cannot start a shell function, so type the command into the
        // tab's shell like `herdr pane run` does; herdr detects the agent.
        let mut line = command.to_string();
        for arg in &args {
            line.push(' ');
            line.push_str(&quote_arg(arg));
        }
        if let Err(err) = h.pane_send_input(&pane_id, &line, &["Enter"]).await {
            close_backing_tab(h, &tab_id).await;
            return Err(SpawnError::Herdr(err));
        }
    }

    Ok(AgentHandle {
        name,
        kind: model.kind,
        model_id: model.id.clone(),
        pane_id,
        tab_id,
        tab_label: label,
        workspace_id: workspace_id.to_string(),
        cwd: cwd.to_path_buf(),
        session_id,
        transcript: None,
        ready: false,
        managed,
    })
}

/// POSIX single-quote one command-line argument; plain words stay bare.
fn quote_arg(arg: &str) -> String {
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '.' | '/' | ':' | '=' | '@' | '%' | '+' | ',' | '-')
    };
    if !arg.is_empty() && arg.chars().all(safe) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Poll a launched agent until it is interactive; sets `ready = true` on success.
/// On `StartupBlocked` the tab is kept so the user can answer the dialog in the
/// herdr TUI; every other failure closes the backing tab.
pub async fn await_ready<H: HerdrApi>(
    h: &H,
    agent: &mut AgentHandle,
    startup_timeout_ms: u64,
) -> Result<(), SpawnError> {
    if agent.managed {
        return await_ready_managed(h, agent, startup_timeout_ms).await;
    }
    await_ready_unmanaged(h, agent, startup_timeout_ms).await
}

async fn await_ready_managed<H: HerdrApi>(
    h: &H,
    agent: &mut AgentHandle,
    startup_timeout_ms: u64,
) -> Result<(), SpawnError> {
    let deadline = Instant::now() + Duration::from_millis(startup_timeout_ms);
    loop {
        // Mirror herdr's own `wait_for_named_agent`: the name may not resolve yet
        // while the launch is pending, so fall back to the pane id and, when both
        // fail, keep polling until the startup deadline.
        let info = match h.agent_get(&agent.name).await {
            Ok(info) => Some(info),
            Err(_) => h.agent_get(&agent.pane_id).await.ok(),
        };
        if let Some(info) = info {
            if let Some(actual) = info
                .agent
                .as_deref()
                .filter(|actual| *actual != agent.kind.herdr_label())
            {
                tracing::warn!(
                    name = %agent.name,
                    expected = agent.kind.herdr_label(),
                    detected = actual,
                    "backing agent kind mismatch"
                );
                close_backing_tab(h, &agent.tab_id).await;
                return Err(SpawnError::StartFailed);
            }
            if info.interactive_ready
                && matches!(info.agent_status, AgentStatus::Idle | AgentStatus::Done)
            {
                agent.ready = true;
                return Ok(());
            }
            if info.agent_status == AgentStatus::Blocked {
                // Keep the tab so the user can answer the dialog in the herdr TUI.
                return Err(SpawnError::StartupBlocked {
                    tab_label: agent.tab_label.clone(),
                    cwd: agent.cwd.clone(),
                });
            }
            if matches!(info.agent_status, AgentStatus::Idle | AgentStatus::Done)
                && !info.launch_pending
                && !info.interactive_ready
            {
                let tail = h
                    .agent_read(&agent.name, ReadSource::RecentUnwrapped, Some(40))
                    .await
                    .unwrap_or_default();
                tracing::warn!(name = %agent.name, tail = %tail, "backing agent failed to start");
                close_backing_tab(h, &agent.tab_id).await;
                return Err(SpawnError::StartFailed);
            }
        }
        if Instant::now() >= deadline {
            close_backing_tab(h, &agent.tab_id).await;
            return Err(SpawnError::Timeout);
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

/// How long an unmanaged agent must stay idle before herdr considers it settled;
/// this mirrors herdr's own detection settle delay.
const UNMANAGED_SETTLE: Duration = Duration::from_secs(3);

/// Poll an agent herdr only detects (no `agent.start`): the CLI was typed into
/// the shell, so there is no `interactive_ready` and no name until we rename.
async fn await_ready_unmanaged<H: HerdrApi>(
    h: &H,
    agent: &mut AgentHandle,
    startup_timeout_ms: u64,
) -> Result<(), SpawnError> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(startup_timeout_ms);
    let mut idle_since: Option<tokio::time::Instant> = None;
    let mut seen = false;
    loop {
        let info = match h.agent_get(&agent.pane_id).await {
            Ok(info) if info.agent.as_deref().is_some_and(|a| !a.is_empty()) => Some(info),
            Ok(_) => None,
            Err(HerdrError::Api { code, .. })
                if code == "agent_not_found" || code == "agent_not_running" =>
            {
                None
            }
            Err(_) => None,
        };
        let Some(info) = info else {
            if seen {
                return agent_gone(h, agent).await;
            }
            if tokio::time::Instant::now() >= deadline {
                close_backing_tab(h, &agent.tab_id).await;
                return Err(SpawnError::Timeout);
            }
            tokio::time::sleep(STARTUP_POLL).await;
            continue;
        };
        seen = true;
        if let Some(actual) = info
            .agent
            .as_deref()
            .filter(|actual| *actual != agent.kind.herdr_label())
        {
            tracing::warn!(
                name = %agent.name,
                expected = agent.kind.herdr_label(),
                detected = actual,
                "backing agent kind mismatch"
            );
            close_backing_tab(h, &agent.tab_id).await;
            return Err(SpawnError::StartFailed);
        }
        if info.agent_status == AgentStatus::Blocked {
            // Keep the tab so the user can answer the dialog in the herdr TUI.
            return Err(SpawnError::StartupBlocked {
                tab_label: agent.tab_label.clone(),
                cwd: agent.cwd.clone(),
            });
        }
        if matches!(info.agent_status, AgentStatus::Idle | AgentStatus::Done) {
            let since = *idle_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= UNMANAGED_SETTLE {
                return rename_unmanaged(h, agent).await;
            }
        } else {
            idle_since = None;
        }
        if tokio::time::Instant::now() >= deadline {
            close_backing_tab(h, &agent.tab_id).await;
            return Err(SpawnError::Timeout);
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

/// Name the detected agent so later calls can address it; on a taken name pick
/// a new suffix and retry like the managed path does.
pub(crate) async fn rename_unmanaged<H: HerdrApi>(
    h: &H,
    agent: &mut AgentHandle,
) -> Result<(), SpawnError> {
    let mut name = agent.name.clone();
    let mut retries = 0usize;
    loop {
        match h.agent_rename(&agent.pane_id, &name).await {
            Ok(_) => {
                agent.name = name;
                agent.ready = true;
                return Ok(());
            }
            Err(HerdrError::Api { code, .. }) if code == "agent_name_taken" => {
                retries += 1;
                if retries > NAME_RETRY_LIMIT {
                    close_backing_tab(h, &agent.tab_id).await;
                    return Err(SpawnError::StartFailed);
                }
                name = agent_name(&agent.model_id, &random_suffix_hex());
            }
            Err(err) => {
                close_backing_tab(h, &agent.tab_id).await;
                return Err(SpawnError::Herdr(err));
            }
        }
    }
}

/// The detected agent exited before it settled: log the pane tail, close the tab.
async fn agent_gone<H: HerdrApi>(h: &H, agent: &AgentHandle) -> Result<(), SpawnError> {
    let tail = h
        .agent_read(&agent.pane_id, ReadSource::RecentUnwrapped, Some(40))
        .await
        .unwrap_or_default();
    tracing::warn!(
        name = %agent.name,
        pane_id = %agent.pane_id,
        tail = %tail,
        "backing agent exited before it was ready"
    );
    close_backing_tab(h, &agent.tab_id).await;
    Err(SpawnError::StartFailed)
}

/// Launch and await readiness in one step.
pub async fn spawn_agent<H: HerdrApi>(
    h: &H,
    workspace_id: &str,
    model: &ModelSpec,
    cwd: &Path,
    startup_timeout_ms: u64,
    owner_id: &str,
) -> Result<AgentHandle, SpawnError> {
    let mut handle = launch_agent(
        h,
        workspace_id,
        model,
        cwd,
        startup_timeout_ms,
        &LaunchExtras::default(),
        owner_id,
    )
    .await?;
    await_ready(h, &mut handle, startup_timeout_ms).await?;
    Ok(handle)
}

async fn close_backing_tab<H: HerdrApi>(h: &H, tab_id: &str) {
    let _ = h.tab_close(tab_id).await;
}

/// Report `oa_owner = owner_id` on a pane with the owner token TTL.
pub(crate) async fn report_owner_token<H: HerdrApi>(
    h: &H,
    pane_id: &str,
    owner_id: &str,
) -> Result<(), HerdrError> {
    let tokens = BTreeMap::from([(OWNER_TOKEN_KEY.to_string(), owner_id.to_string())]);
    h.pane_report_tokens(
        pane_id,
        OWNER_TOKEN_SOURCE,
        &tokens,
        Some(OWNER_TOKEN_TTL_MS),
    )
    .await
}

/// Owner token values on the panes of one tab.
fn tab_owners<'a>(panes: &'a [PaneInfo], tab_id: &str) -> Vec<&'a str> {
    panes
        .iter()
        .filter(|pane| pane.tab_id == tab_id)
        .filter_map(|pane| pane.tokens.get(OWNER_TOKEN_KEY))
        .map(String::as_str)
        .collect()
}

/// Best-effort close: refresh the tab via `agent.get`, then close it only when the
/// label is ours and no other instance owns it.
pub async fn close_agent<H: HerdrApi>(h: &H, agent: &AgentHandle, owner_id: &str) {
    let mut tab_id = agent.tab_id.clone();
    let mut workspace_id = agent.workspace_id.clone();
    match h.agent_get(&agent.name).await {
        Ok(info) => {
            tab_id = info.tab_id;
            workspace_id = info.workspace_id;
        }
        Err(HerdrError::Api { code, .. }) if code == "agent_not_found" => {}
        Err(err) => tracing::warn!(name = %agent.name, "agent.get failed during close: {err}"),
    }
    close_tab_if_ours(h, &workspace_id, &tab_id, owner_id).await;
}

async fn close_tab_if_ours<H: HerdrApi>(h: &H, workspace_id: &str, tab_id: &str, owner_id: &str) {
    let tabs = match h.tab_list(workspace_id).await {
        Ok(tabs) => tabs,
        Err(err) => {
            tracing::warn!(tab_id, "tab.list failed during close: {err}");
            return;
        }
    };
    let Some(tab) = tabs.iter().find(|tab| tab.tab_id == tab_id) else {
        return;
    };
    if !tab.label.starts_with(TAB_LABEL_PREFIX) {
        return;
    }
    // A missing token (never reported, or expired) still allows the close.
    let panes = match h.pane_list(workspace_id).await {
        Ok(panes) => panes,
        Err(err) => {
            tracing::warn!(tab_id, "pane.list failed during close: {err}");
            return;
        }
    };
    if let Some(owner) = tab_owners(&panes, tab_id)
        .into_iter()
        .find(|owner| *owner != owner_id)
    {
        tracing::info!(
            tab_id,
            owner,
            "not closing a backing tab another instance owns"
        );
        return;
    }
    if let Err(err) = h.tab_close(tab_id).await {
        tracing::warn!(tab_id, "tab.close failed: {err}");
    }
}

/// `oa:` tabs of one workspace, split by the owner tokens on their panes.
#[derive(Debug, Default)]
pub struct OwnerScan {
    /// No pane carries an owner token.
    pub untagged: Vec<TabInfo>,
    /// A pane carries another instance's token.
    pub foreign: Vec<TabInfo>,
}

/// List the workspace's `oa:` tabs that are untagged or owned by another instance;
/// tabs carrying only `owner_id` are in neither list.
pub async fn scan_owner_tokens<H: HerdrApi>(
    h: &H,
    workspace_id: &str,
    owner_id: &str,
) -> Result<OwnerScan, HerdrError> {
    let tabs = h.tab_list(workspace_id).await?;
    let panes = h.pane_list(workspace_id).await?;
    let mut scan = OwnerScan::default();
    for tab in tabs {
        if !tab.label.starts_with(TAB_LABEL_PREFIX) {
            continue;
        }
        let owners = tab_owners(&panes, &tab.tab_id);
        if owners.is_empty() {
            scan.untagged.push(tab);
        } else if owners.iter().any(|owner| *owner != owner_id) {
            scan.foreign.push(tab);
        }
    }
    Ok(scan)
}

/// Close leftover backing-agent tabs that no instance owns; returns how many were
/// closed. Tabs another instance owns are logged and kept.
pub async fn sweep_orphans<H: HerdrApi>(h: &H, workspace_id: &str, owner_id: &str) -> usize {
    let Ok(scan) = scan_owner_tokens(h, workspace_id, owner_id).await else {
        return 0;
    };
    for tab in &scan.foreign {
        tracing::info!(
            tab_id = %tab.tab_id,
            label = %tab.label,
            "keeping a backing tab another instance owns"
        );
    }
    let mut closed = 0;
    for tab in scan.untagged {
        if h.tab_close(&tab.tab_id).await.is_ok() {
            closed += 1;
        }
    }
    closed
}

/// Re-report the owner token, with a fresh TTL, on every pane `owner_id` owns.
pub async fn refresh_owner_tokens<H: HerdrApi>(h: &H, workspace_id: &str, owner_id: &str) {
    let panes = match h.pane_list(workspace_id).await {
        Ok(panes) => panes,
        Err(err) => {
            tracing::warn!("pane.list failed during owner token refresh: {err}");
            return;
        }
    };
    let ours = panes.iter().filter(|pane| {
        pane.tokens
            .get(OWNER_TOKEN_KEY)
            .is_some_and(|owner| owner == owner_id)
    });
    for pane in ours {
        if let Err(err) = report_owner_token(h, &pane.pane_id, owner_id).await {
            tracing::warn!(pane_id = %pane.pane_id, "owner token refresh failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FakeHerdr, StartupBehavior};
    use std::collections::BTreeMap;

    const OWNER: &str = "owner-test";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-server-lc-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn model(id: &str) -> ModelSpec {
        ModelSpec {
            id: id.to_string(),
            kind: AgentKind::Claude,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    fn assert_name_grammar(name: &str) {
        let bytes_ok = name.len() <= 32
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        assert!(bytes_ok, "bad agent name grammar: {name:?}");
    }

    #[test]
    fn agent_name_grammar_and_slug() {
        let name = agent_name("claude+client-tools", "abc123");
        assert_eq!(name, "oa-claude-client-tools-abc123");
        assert_name_grammar(&name);
        assert_eq!(
            agent_name("Claude_Pro 3", "ff00aa"),
            "oa-claude-pro-3-ff00aa"
        );
        assert_name_grammar(&agent_name("Claude_Pro 3", "ff00aa"));

        let long = agent_name(&"x".repeat(40), "abc123");
        assert_eq!(long, format!("oa-{}-abc123", "x".repeat(20)));
        assert_name_grammar(&long);

        let odd = agent_name("!!!", "abc123");
        assert_eq!(odd, "oa--abc123");
        assert_name_grammar(&odd);

        assert_eq!(
            tab_label("claude+client-tools", "abc123"),
            "oa:claude+client-tools:abc123"
        );
        assert!(tab_label("claude", "abc123").starts_with(TAB_LABEL_PREFIX));
        let suffix = random_suffix_hex();
        assert_eq!(suffix.len(), 6);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolve_cwd_precedence_and_guards() {
        let dir = temp_dir("cwd");
        let home = dir.join("home");
        let project = home.join("projects").join("repo");
        std::fs::create_dir_all(&project).expect("mkdir");
        let from_client = home.join("projects").join("client");
        std::fs::create_dir_all(&from_client).expect("mkdir");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");
        let roots = vec![home.clone()];
        let home_str = home.display().to_string();

        let resolved = resolve_cwd(
            Some("~/projects/repo"),
            Some(outside.display().to_string().as_str()),
            Some(Path::new("/should/be/ignored")),
            Path::new("~"),
            &roots,
            &home_str,
        )
        .expect("header wins");
        assert_eq!(resolved, std::fs::canonicalize(&project).expect("canon"));

        let resolved = resolve_cwd(
            None,
            Some("~/projects/client"),
            Some(Path::new("/should/be/ignored")),
            Path::new("~"),
            &roots,
            &home_str,
        )
        .expect("client cwd beats the model cwd");
        assert_eq!(
            resolved,
            std::fs::canonicalize(&from_client).expect("canon")
        );

        let resolved = resolve_cwd(
            None,
            None,
            Some(Path::new("~")),
            Path::new("/ignored"),
            &roots,
            &home_str,
        )
        .expect("model cwd");
        assert_eq!(resolved, std::fs::canonicalize(&home).expect("canon"));

        let resolved =
            resolve_cwd(None, None, None, Path::new("~"), &roots, &home_str).expect("default");
        assert_eq!(resolved, std::fs::canonicalize(&home).expect("canon"));

        let err = resolve_cwd(
            Some(outside.display().to_string().as_str()),
            None,
            None,
            Path::new("~"),
            &roots,
            &home_str,
        )
        .expect_err("outside roots");
        assert_eq!(err.code, "cwd_not_allowed");
        assert!(err.message.contains("(from X-Herdr-Cwd)"), "got {err}");

        let err = resolve_cwd(
            None,
            Some(outside.display().to_string().as_str()),
            None,
            Path::new("~"),
            &roots,
            &home_str,
        )
        .expect_err("client cwd outside roots");
        assert_eq!(err.code, "cwd_not_allowed");
        assert!(
            err.message.contains("the client's Working directory"),
            "got {err}"
        );

        let err = resolve_cwd(
            Some("/nonexistent/path"),
            None,
            None,
            Path::new("~"),
            &roots,
            &home_str,
        )
        .expect_err("missing dir");
        assert_eq!(err.code, "cwd_not_allowed");
        assert!(err.message.contains("(from X-Herdr-Cwd)"), "got {err}");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn ensure_workspace_finds_existing_by_label() {
        let fake = FakeHerdr::new();
        let created = fake
            .workspace_create("openai-api", Path::new("/tmp"), false)
            .await
            .expect("seed workspace");
        let id = ensure_workspace(&fake, "openai-api").await.expect("ensure");
        assert_eq!(id, created.workspace.workspace_id);
        // Only the seed call exists; ensure_workspace did not create another workspace.
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|c| c.method == "workspace.create")
                .count(),
            1
        );
        assert_eq!(fake.workspace_list().await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn ensure_workspace_creates_and_renames_lobby() {
        let fake = FakeHerdr::new();
        let id = ensure_workspace(&fake, "openai-api").await.expect("ensure");
        assert_eq!(fake.tab_labels(&id), vec!["lobby".to_string()]);
        let rename = fake
            .calls()
            .into_iter()
            .find(|c| c.method == "tab.rename")
            .expect("rename call");
        assert_eq!(rename.param("label"), Some("lobby"));
    }

    async fn spawn_in_fake(fake: &FakeHerdr) -> Result<AgentHandle, SpawnError> {
        let workspace_id = ensure_workspace(fake, "openai-api").await.expect("ws");
        spawn_agent(
            fake,
            &workspace_id,
            &model("claude"),
            Path::new("/tmp"),
            1000,
            OWNER,
        )
        .await
    }

    #[tokio::test]
    async fn spawn_creates_tab_and_starts_with_session_id() {
        let fake = FakeHerdr::new();
        let handle = spawn_in_fake(&fake).await.expect("spawn");
        assert!(handle.name.starts_with("oa-claude-"));
        assert_name_grammar(&handle.name);
        assert!(handle.session_id.is_some());
        assert!(handle.transcript.is_none());

        let calls = fake.calls();
        let tab_call = calls
            .iter()
            .find(|c| c.method == "tab.create")
            .expect("tab.create");
        assert_eq!(tab_call.param("focus"), Some("false"));
        assert!(tab_call
            .param("label")
            .is_some_and(|label| label.starts_with(TAB_LABEL_PREFIX)));
        let start_call = calls
            .iter()
            .find(|c| c.method == "agent.start")
            .expect("start");
        let args: Vec<&str> = start_call
            .param("args")
            .unwrap_or_default()
            .split(' ')
            .collect();
        assert_eq!(args.last().copied(), handle.session_id.as_deref());
        assert_eq!(args[args.len().saturating_sub(2)], "--session-id");
        let session: Result<uuid::Uuid, _> =
            handle.session_id.as_deref().unwrap_or_default().parse();
        assert!(session.is_ok(), "session id must be a uuid");
    }

    #[tokio::test]
    async fn launch_claude_adds_hook_settings_and_keeps_token_in_tab_env() {
        let fake = FakeHerdr::new();
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        let settings = crate::question_hook::claude_settings_arg(
            "http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use",
            60,
        );
        let extras = LaunchExtras {
            args: vec!["--settings".to_string(), settings.clone()],
            env: BTreeMap::from([(
                "HERDR_OPENAI_SERVER_HOOK_TOKEN".to_string(),
                "secret-value".to_string(),
            )]),
        };
        launch_agent(
            &fake,
            &workspace_id,
            &model("claude"),
            Path::new("/tmp"),
            1000,
            &extras,
            OWNER,
        )
        .await
        .expect("launch");

        let calls = fake.calls();
        let start = calls
            .iter()
            .find(|call| call.method == "agent.start")
            .expect("agent.start");
        let args = start.param("args").expect("args");
        assert!(
            args.starts_with("--settings {\"hooks\"") || args.starts_with("--settings '{\"hooks\"")
        );
        assert!(args.contains("--session-id"));
        assert!(!args.contains("secret-value"));
        let tab = calls
            .iter()
            .find(|call| call.method == "tab.create")
            .expect("tab.create");
        assert_eq!(
            tab.param("env.HERDR_OPENAI_SERVER_HOOK_TOKEN"),
            Some("secret-value")
        );
    }

    #[tokio::test]
    async fn launch_unmanaged_claude_quotes_settings_without_exposing_token() {
        let fake = FakeHerdr::new().with_variant_command("claude-personal", "claude");
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        let mut model = model("claude-personal");
        model.command = Some("claude-personal".to_string());
        let settings = crate::question_hook::claude_settings_arg(
            "http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use",
            60,
        );
        let extras = LaunchExtras {
            args: vec!["--settings".to_string(), settings],
            env: BTreeMap::from([(
                "HERDR_OPENAI_SERVER_HOOK_TOKEN".to_string(),
                "secret-value".to_string(),
            )]),
        };
        launch_agent(
            &fake,
            &workspace_id,
            &model,
            Path::new("/tmp"),
            1000,
            &extras,
            OWNER,
        )
        .await
        .expect("launch");

        let calls = fake.calls();
        let command = calls
            .iter()
            .find(|call| call.method == "pane.send_input")
            .and_then(|call| call.param("text"))
            .expect("typed command");
        assert!(command.contains("--settings '{\"hooks\""), "{command}");
        assert!(command.contains("--session-id"));
        assert!(!command.contains("secret-value"));
        let tab = calls
            .iter()
            .find(|call| call.method == "tab.create")
            .expect("tab.create");
        assert_eq!(
            tab.param("env.HERDR_OPENAI_SERVER_HOOK_TOKEN"),
            Some("secret-value")
        );
    }

    #[tokio::test]
    async fn spawn_retries_pane_busy() {
        let fake = FakeHerdr::new().with_startup(StartupBehavior::PaneBusyOnce);
        let handle = spawn_in_fake(&fake).await.expect("spawn after retry");
        let starts = fake
            .calls()
            .into_iter()
            .filter(|c| c.method == "agent.start")
            .count();
        assert_eq!(starts, 2);
        assert!(handle.session_id.is_some());
    }

    #[tokio::test]
    async fn spawn_regenerates_taken_name() {
        let fake = FakeHerdr::new().with_startup(StartupBehavior::NameTakenOnce);
        let handle = spawn_in_fake(&fake).await.expect("spawn after rename");
        let names: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.method == "agent.start")
            .filter_map(|c| c.param("name").map(String::from))
            .collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert_eq!(names[1], handle.name);
    }

    #[tokio::test]
    async fn spawn_name_resolves_late_falls_back_to_pane() {
        let fake = FakeHerdr::new().with_startup(StartupBehavior::NameResolvesLate);
        let handle = spawn_in_fake(&fake).await.expect("spawn after late name");
        let calls = fake.calls();
        let gets: Vec<&str> = calls
            .iter()
            .filter(|c| c.method == "agent.get")
            .filter_map(|c| c.param("target"))
            .collect();
        assert!(gets.contains(&handle.name.as_str()));
        assert!(gets.contains(&handle.pane_id.as_str()));
    }

    #[tokio::test]
    async fn spawn_kind_mismatch_closes_tab() {
        let fake = FakeHerdr::new().with_agent_label("codex");
        let err = spawn_in_fake(&fake).await.expect_err("mismatch");
        assert!(matches!(err, SpawnError::StartFailed));
        assert_eq!(err.into_api_error().code, "agent_start_failed");
        assert_eq!(fake.tab_labels("w1"), vec!["lobby".to_string()]);
    }

    #[tokio::test]
    async fn spawn_blocked_keeps_tab_and_reports_label() {
        let fake = FakeHerdr::new().with_startup(StartupBehavior::Blocked);
        let err = spawn_in_fake(&fake).await.expect_err("blocked");
        let (tab_label, cwd) = match &err {
            SpawnError::StartupBlocked { tab_label, cwd } => (tab_label.clone(), cwd.clone()),
            other => panic!("expected StartupBlocked, got {other:?}"),
        };
        assert!(tab_label.starts_with(TAB_LABEL_PREFIX));
        assert_eq!(cwd, PathBuf::from("/tmp"));
        let api = err.into_api_error();
        assert_eq!(api.code, "agent_startup_blocked");
        assert_eq!(
            api.message,
            format!(
                "backing agent is waiting for input in herdr tab {tab_label} (cwd /tmp), \
                 usually a first-run dialog such as folder trust or login; answer it in that \
                 herdr tab and resend, or use a folder this CLI already trusts"
            )
        );
        assert_eq!(fake.tab_labels("w1").len(), 2); // lobby + agent tab
    }

    #[tokio::test]
    async fn spawn_failure_closes_tab() {
        let fake = FakeHerdr::new().with_startup(StartupBehavior::Fails);
        let err = spawn_in_fake(&fake).await.expect_err("fails");
        assert!(matches!(err, SpawnError::StartFailed));
        assert_eq!(err.into_api_error().code, "agent_start_failed");
        assert_eq!(fake.tab_labels("w1"), vec!["lobby".to_string()]);
    }

    #[tokio::test]
    async fn close_agent_closes_only_our_tab() {
        let fake = FakeHerdr::new();
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        let extra = fake
            .tab_create(TabCreate {
                workspace_id: &workspace_id,
                cwd: None,
                label: Some("user-tab"),
                env: &BTreeMap::new(),
                focus: false,
            })
            .await
            .expect("user tab");
        let handle = spawn_agent(
            &fake,
            &workspace_id,
            &model("claude"),
            Path::new("/tmp"),
            1000,
            OWNER,
        )
        .await
        .expect("spawn");
        close_agent(&fake, &handle, OWNER).await;
        let labels = fake.tab_labels(&workspace_id);
        assert!(labels.contains(&"lobby".to_string()));
        assert!(labels.contains(&"user-tab".to_string()));
        assert!(!labels.iter().any(|l| l.starts_with(TAB_LABEL_PREFIX)));
        assert!(!fake.calls().iter().any(
            |c| c.method == "tab.close" && c.param("tab_id") == Some(extra.tab.tab_id.as_str())
        ));
    }

    #[tokio::test]
    async fn sweep_orphans_closes_only_our_tabs() {
        let fake = FakeHerdr::new();
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        for label in ["oa:claude:aaaaaa", "oa:codex:bbbbbb", "keep-me"] {
            fake.tab_create(TabCreate {
                workspace_id: &workspace_id,
                cwd: None,
                label: Some(label),
                env: &BTreeMap::new(),
                focus: false,
            })
            .await
            .expect("tab");
        }
        let closed = sweep_orphans(&fake, &workspace_id, OWNER).await;
        assert_eq!(closed, 2);
        let labels = fake.tab_labels(&workspace_id);
        assert!(labels.contains(&"lobby".to_string()));
        assert!(labels.contains(&"keep-me".to_string()));
        assert!(!labels.iter().any(|l| l.starts_with(TAB_LABEL_PREFIX)));
    }

    /// An `oa:`-style tab created outside `launch_agent`, optionally tagged with `owner`.
    async fn seed_tab(
        fake: &FakeHerdr,
        workspace_id: &str,
        label: &str,
        owner: Option<&str>,
    ) -> crate::herdr::TabCreated {
        let created = fake
            .tab_create(TabCreate {
                workspace_id,
                cwd: None,
                label: Some(label),
                env: &BTreeMap::new(),
                focus: false,
            })
            .await
            .expect("tab");
        if let Some(owner) = owner {
            let tokens = BTreeMap::from([(OWNER_TOKEN_KEY.to_string(), owner.to_string())]);
            fake.pane_report_tokens(&created.root_pane.pane_id, "test", &tokens, None)
                .await
                .expect("tag pane");
        }
        created
    }

    fn token_reports(fake: &FakeHerdr) -> Vec<crate::herdr::fake::FakeCall> {
        fake.calls()
            .into_iter()
            .filter(|c| c.method == "pane.report_metadata")
            .collect()
    }

    #[tokio::test]
    async fn launch_tags_the_root_pane_before_starting_the_agent() {
        let fake = FakeHerdr::new();
        let handle = spawn_in_fake(&fake).await.expect("spawn");
        let reports = token_reports(&fake);
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.param("pane_id"), Some(handle.pane_id.as_str()));
        assert_eq!(report.param("source"), Some(OWNER_TOKEN_SOURCE));
        assert_eq!(report.param("token.oa_owner"), Some(OWNER));
        assert_eq!(report.param("ttl_ms"), Some("90000"));

        let methods: Vec<&str> = fake.calls().iter().map(|c| c.method).collect();
        let report_at = methods.iter().position(|m| *m == "pane.report_metadata");
        let start_at = methods.iter().position(|m| *m == "agent.start");
        assert!(report_at < start_at, "tag before start: {methods:?}");

        let panes = fake.pane_list(&handle.workspace_id).await.expect("panes");
        let pane = panes
            .iter()
            .find(|pane| pane.pane_id == handle.pane_id)
            .expect("agent pane");
        assert_eq!(
            pane.tokens.get(OWNER_TOKEN_KEY).map(String::as_str),
            Some(OWNER)
        );
    }

    #[tokio::test]
    async fn launch_closes_the_tab_when_the_token_report_fails() {
        let fake = FakeHerdr::new().with_report_tokens_fails();
        let err = spawn_in_fake(&fake).await.expect_err("token report fails");
        assert!(matches!(err, SpawnError::Herdr(_)), "got {err:?}");
        assert_eq!(fake.tab_labels("w1"), vec!["lobby".to_string()]);
        assert!(!fake.calls().iter().any(|c| c.method == "agent.start"));
    }

    #[tokio::test]
    async fn close_agent_respects_owner_tokens() {
        let fake = FakeHerdr::new();
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        let claude = model("claude");
        let launch = |owner: &'static str| {
            spawn_agent(
                &fake,
                &workspace_id,
                &claude,
                Path::new("/tmp"),
                1000,
                owner,
            )
        };
        let foreign = launch("other-instance").await.expect("foreign spawn");
        let ours = launch(OWNER).await.expect("our spawn");
        let untagged = seed_tab(&fake, &workspace_id, "oa:claude:cccccc", None).await;
        let untagged_handle = AgentHandle {
            name: "oa-claude-cccccc".to_string(),
            tab_id: untagged.tab.tab_id.clone(),
            tab_label: "oa:claude:cccccc".to_string(),
            pane_id: untagged.root_pane.pane_id.clone(),
            ..ours.clone()
        };

        close_agent(&fake, &foreign, OWNER).await;
        close_agent(&fake, &ours, OWNER).await;
        close_agent(&fake, &untagged_handle, OWNER).await;

        let labels = fake.tab_labels(&workspace_id);
        assert_eq!(labels, vec!["lobby".to_string(), foreign.tab_label.clone()]);
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| c.method == "tab.close"
                    && c.param("tab_id") == Some(foreign.tab_id.as_str()))
        );
    }

    #[tokio::test]
    async fn sweep_orphans_respects_owner_tokens() {
        let fake = FakeHerdr::new();
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        seed_tab(&fake, &workspace_id, "oa:claude:aaaaaa", None).await;
        let foreign = seed_tab(&fake, &workspace_id, "oa:claude:bbbbbb", Some("other")).await;
        seed_tab(&fake, &workspace_id, "oa:claude:dddddd", Some(OWNER)).await;
        let user = seed_tab(&fake, &workspace_id, "keep-me", None).await;

        let closed = sweep_orphans(&fake, &workspace_id, OWNER).await;
        assert_eq!(closed, 1);
        assert_eq!(
            fake.tab_labels(&workspace_id),
            vec![
                "lobby".to_string(),
                "oa:claude:bbbbbb".to_string(),
                "oa:claude:dddddd".to_string(),
                "keep-me".to_string(),
            ]
        );
        let closes: Vec<String> = fake
            .calls()
            .iter()
            .filter(|c| c.method == "tab.close")
            .filter_map(|c| c.param("tab_id").map(String::from))
            .collect();
        assert!(!closes.contains(&foreign.tab.tab_id));
        assert!(!closes.contains(&user.tab.tab_id));
    }

    #[tokio::test]
    async fn refresh_owner_tokens_reports_only_our_panes() {
        let fake = FakeHerdr::new();
        let workspace_id = ensure_workspace(&fake, "openai-api").await.expect("ws");
        let ours = seed_tab(&fake, &workspace_id, "oa:claude:aaaaaa", Some(OWNER)).await;
        seed_tab(&fake, &workspace_id, "oa:claude:bbbbbb", Some("other")).await;
        seed_tab(&fake, &workspace_id, "oa:claude:cccccc", None).await;
        let before = token_reports(&fake).len();

        refresh_owner_tokens(&fake, &workspace_id, OWNER).await;

        let reports = token_reports(&fake);
        let refreshed = &reports[before..];
        assert_eq!(refreshed.len(), 1, "{refreshed:?}");
        assert_eq!(
            refreshed[0].param("pane_id"),
            Some(ours.root_pane.pane_id.as_str())
        );
        assert_eq!(refreshed[0].param("source"), Some(OWNER_TOKEN_SOURCE));
        assert_eq!(refreshed[0].param("token.oa_owner"), Some(OWNER));
        assert_eq!(refreshed[0].param("ttl_ms"), Some("90000"));
    }

    #[test]
    fn quote_arg_keeps_safe_words_and_escapes_the_rest() {
        assert_eq!(quote_arg("plain-arg.x/y=z"), "plain-arg.x/y=z");
        assert_eq!(quote_arg(""), "''");
        assert_eq!(quote_arg("two words"), "'two words'");
        assert_eq!(quote_arg("it's"), "'it'\\''s'");
    }

    fn variant_model(id: &str, command: &str, args: &[&str]) -> ModelSpec {
        ModelSpec {
            id: id.to_string(),
            kind: AgentKind::Claude,
            command: Some(command.to_string()),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    async fn spawn_variant_in_fake(
        fake: &FakeHerdr,
        model: &ModelSpec,
    ) -> Result<AgentHandle, SpawnError> {
        let workspace_id = ensure_workspace(fake, "openai-api").await.expect("ws");
        spawn_agent(fake, &workspace_id, model, Path::new("/tmp"), 10_000, OWNER).await
    }

    #[tokio::test(start_paused = true)]
    async fn unmanaged_spawn_types_command_then_renames() {
        let fake = FakeHerdr::new().with_variant_command("claude-personal", "claude");
        let model = variant_model(
            "claude-personal+client-tools",
            "claude-personal",
            &["--tools", ""],
        );
        let handle = spawn_variant_in_fake(&fake, &model)
            .await
            .expect("spawn variant");
        assert!(handle.ready);
        assert!(!handle.managed);
        assert!(handle.session_id.is_some());

        let calls = fake.calls();
        assert!(!calls.iter().any(|c| c.method == "agent.start"));
        let send = calls
            .iter()
            .find(|c| c.method == "pane.send_input")
            .expect("pane.send_input");
        assert_eq!(send.param("pane_id"), Some(handle.pane_id.as_str()));
        assert_eq!(send.param("keys"), Some("Enter"));
        let text = send.param("text").unwrap_or_default();
        // Empty bridge arg is quoted; the session id arg stays bare.
        assert!(
            text.starts_with("claude-personal --tools '' --session-id "),
            "got {text}"
        );
        assert!(text.ends_with(handle.session_id.as_deref().unwrap_or_default()));

        let rename = calls
            .iter()
            .find(|c| c.method == "agent.rename")
            .expect("agent.rename");
        assert_eq!(rename.param("target"), Some(handle.pane_id.as_str()));
        assert_eq!(rename.param("name"), Some(handle.name.as_str()));
    }

    #[tokio::test(start_paused = true)]
    async fn unmanaged_rename_conflict_retries_new_suffix() {
        let fake = FakeHerdr::new()
            .with_variant_command("claude-personal", "claude")
            .with_rename_taken_once();
        let handle = spawn_variant_in_fake(
            &fake,
            &variant_model("claude-personal", "claude-personal", &[]),
        )
        .await
        .expect("spawn after rename retry");
        let names: Vec<String> = fake
            .calls()
            .iter()
            .filter(|c| c.method == "agent.rename")
            .filter_map(|c| c.param("name").map(String::from))
            .collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert_eq!(names[1], handle.name);
    }

    #[tokio::test(start_paused = true)]
    async fn unmanaged_blocked_startup_keeps_tab() {
        let fake = FakeHerdr::new()
            .with_variant_command("claude-personal", "claude")
            .with_startup(StartupBehavior::Blocked);
        let err = spawn_variant_in_fake(
            &fake,
            &variant_model("claude-personal", "claude-personal", &[]),
        )
        .await
        .expect_err("blocked");
        assert!(matches!(err, SpawnError::StartupBlocked { .. }));
        assert_eq!(fake.tab_labels("w1").len(), 2); // lobby + agent tab
    }

    #[tokio::test(start_paused = true)]
    async fn unmanaged_kind_mismatch_closes_tab() {
        let fake = FakeHerdr::new()
            .with_variant_command("claude-personal", "claude")
            .with_agent_label("codex");
        let err = spawn_variant_in_fake(
            &fake,
            &variant_model("claude-personal", "claude-personal", &[]),
        )
        .await
        .expect_err("mismatch");
        assert!(matches!(err, SpawnError::StartFailed));
        assert_eq!(fake.tab_labels("w1"), vec!["lobby".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn unmanaged_agent_exiting_before_ready_fails() {
        let fake = FakeHerdr::new()
            .with_variant_command("claude-personal", "claude")
            .with_agent_exits_after_seen();
        let err = spawn_variant_in_fake(
            &fake,
            &variant_model("claude-personal", "claude-personal", &[]),
        )
        .await
        .expect_err("exited");
        assert!(matches!(err, SpawnError::StartFailed));
        assert_eq!(fake.tab_labels("w1"), vec!["lobby".to_string()]);
    }
}
