use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
pub mod socket;

#[cfg(unix)]
pub use socket::SocketHerdr;

/// In-memory double of the API used by tests; not part of the public docs.
#[doc(hidden)]
pub mod fake;

/// Oldest server protocol this client accepts.
pub const MIN_PROTOCOL: u32 = 20;
/// Newest server protocol tested against; newer servers only warn.
pub const TESTED_MAX_PROTOCOL: u32 = 22;

const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";
const SESSION_ENV_VAR: &str = "HERDR_SESSION";
const DEFAULT_SESSION_NAME: &str = "default";

/// Client view of the herdr socket API. Live code uses `SocketHerdr`; tests use a fake.
pub trait HerdrApi: Send + Sync + 'static {
    fn ping(&self) -> impl Future<Output = Result<Pong, HerdrError>> + Send;

    fn workspace_list(&self)
        -> impl Future<Output = Result<Vec<WorkspaceInfo>, HerdrError>> + Send;
    fn workspace_create(
        &self,
        label: &str,
        cwd: &Path,
        focus: bool,
    ) -> impl Future<Output = Result<WorkspaceCreated, HerdrError>> + Send;

    fn tab_list(
        &self,
        workspace_id: &str,
    ) -> impl Future<Output = Result<Vec<TabInfo>, HerdrError>> + Send;
    fn tab_create(
        &self,
        params: TabCreate<'_>,
    ) -> impl Future<Output = Result<TabCreated, HerdrError>> + Send;
    fn tab_rename(
        &self,
        tab_id: &str,
        label: &str,
    ) -> impl Future<Output = Result<TabInfo, HerdrError>> + Send;
    fn tab_close(&self, tab_id: &str) -> impl Future<Output = Result<(), HerdrError>> + Send;

    /// Type `text` into a pane's shell, optionally followed by key names such
    /// as `Enter` (herdr `pane.send_input`).
    fn pane_send_input(
        &self,
        pane_id: &str,
        text: &str,
        keys: &[&str],
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;
    fn pane_list(
        &self,
        workspace_id: &str,
    ) -> impl Future<Output = Result<Vec<PaneInfo>, HerdrError>> + Send;
    /// Set metadata tokens on a pane (herdr `pane.report_metadata`); they
    /// expire after `ttl_ms` unless reported again.
    fn pane_report_tokens(
        &self,
        pane_id: &str,
        source: &str,
        tokens: &BTreeMap<String, String>,
        ttl_ms: Option<u64>,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    fn agent_get(&self, target: &str)
        -> impl Future<Output = Result<AgentInfo, HerdrError>> + Send;
    fn agent_start(
        &self,
        params: AgentStart<'_>,
    ) -> impl Future<Output = Result<AgentStarted, HerdrError>> + Send;
    fn agent_rename(
        &self,
        target: &str,
        name: &str,
    ) -> impl Future<Output = Result<AgentInfo, HerdrError>> + Send;
    fn agent_prompt(
        &self,
        target: &str,
        text: &str,
        wait: Option<WaitSpec>,
    ) -> impl Future<Output = Result<AgentInfo, HerdrError>> + Send;
    fn agent_wait(
        &self,
        target: &str,
        wait: WaitSpec,
    ) -> impl Future<Output = Result<AgentInfo, HerdrError>> + Send;
    fn agent_send_keys(
        &self,
        target: &str,
        keys: &[&str],
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;
    fn agent_read(
        &self,
        target: &str,
        source: ReadSource,
        lines: Option<u32>,
    ) -> impl Future<Output = Result<String, HerdrError>> + Send;
}

/// Parameters for `tab.create`.
#[derive(Debug)]
pub struct TabCreate<'a> {
    pub workspace_id: &'a str,
    pub cwd: Option<&'a Path>,
    pub label: Option<&'a str>,
    pub env: &'a BTreeMap<String, String>,
    pub focus: bool,
}

/// Parameters for `agent.start`.
#[derive(Debug)]
pub struct AgentStart<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub pane_id: &'a str,
    pub args: &'a [String],
    pub timeout_ms: Option<u64>,
}

/// Wait condition for `agent.prompt` and `agent.wait`.
#[derive(Debug, Clone)]
pub struct WaitSpec {
    pub until: Vec<AgentStatus>,
    pub timeout_ms: Option<u64>,
}

impl WaitSpec {
    fn until_param(&self) -> serde_json::Value {
        serde_json::json!({ "until": self.until, "timeout_ms": self.timeout_ms })
    }
}

/// Pane screen source for `agent.read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadSource {
    Visible,
    Recent,
    RecentUnwrapped,
    Detection,
}

impl ReadSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Recent => "recent",
            Self::RecentUnwrapped => "recent_unwrapped",
            Self::Detection => "detection",
        }
    }
}

/// Agent lifecycle status as reported by herdr; unknown wire values map to `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Lenient mirror of `WorkspaceInfo`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkspaceInfo {
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub active_tab_id: String,
}

/// Lenient mirror of `TabInfo`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TabInfo {
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub pane_count: u32,
}

/// Lenient mirror of `PaneInfo`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PaneInfo {
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    /// Metadata tokens; herdr omits the field when empty.
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

/// Lenient mirror of `AgentSessionInfo`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentSessionInfo {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub value: String,
}

/// Lenient mirror of `AgentInfo`; `interactive_ready` and `launch_pending` are
/// omitted by the server when false.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentInfo {
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub interactive_ready: bool,
    #[serde(default)]
    pub launch_pending: bool,
    #[serde(default)]
    pub state_change_seq: u64,
}

/// `workspace_created` result.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkspaceCreated {
    #[serde(default)]
    pub workspace: WorkspaceInfo,
    #[serde(default)]
    pub tab: TabInfo,
    #[serde(default)]
    pub root_pane: PaneInfo,
}

/// `tab_created` result.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TabCreated {
    #[serde(default)]
    pub tab: TabInfo,
    #[serde(default)]
    pub root_pane: PaneInfo,
}

/// `agent_started` result.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentStarted {
    #[serde(default)]
    pub agent: AgentInfo,
    #[serde(default)]
    pub argv: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WorkspaceListResult {
    #[serde(default)]
    workspaces: Vec<WorkspaceInfo>,
}

#[derive(Debug, Default, Deserialize)]
struct TabListResult {
    #[serde(default)]
    tabs: Vec<TabInfo>,
}

#[derive(Debug, Default, Deserialize)]
struct PaneListResult {
    #[serde(default)]
    panes: Vec<PaneInfo>,
}

#[derive(Debug, Default, Deserialize)]
struct TabInfoResult {
    #[serde(default)]
    tab: TabInfo,
}

#[derive(Debug, Default, Deserialize)]
struct AgentInfoResult {
    #[serde(default)]
    agent: AgentInfo,
}

#[derive(Debug, Default, Deserialize)]
struct PaneReadResult {
    #[serde(default)]
    read: PaneReadBody,
}

#[derive(Debug, Default, Deserialize)]
struct PaneReadBody {
    #[serde(default)]
    text: String,
}

/// Check the `result.type` tag and decode the value into `T`; a wrong tag is a protocol error.
fn decode_result<T>(result: serde_json::Value, expected: &str) -> Result<T, HerdrError>
where
    T: serde::de::DeserializeOwned,
{
    let got = result
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if got != expected {
        return Err(HerdrError::Protocol(format!(
            "expected result type {expected:?}, got {got:?}"
        )));
    }
    serde_json::from_value(result)
        .map_err(|err| HerdrError::Protocol(format!("malformed {expected} result: {err}")))
}

#[derive(Debug)]
pub enum HerdrError {
    /// Server socket is missing or refuses connections; herdr is probably not running.
    /// `socket` is the path this client tried, so the message says where to look.
    Unavailable { socket: PathBuf, source: io::Error },
    /// Response line could not be decoded or violates the envelope contract.
    Protocol(String),
    /// Server answered with an error envelope.
    Api { code: String, message: String },
    /// Client-side timeout for a short call.
    Timeout,
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { socket, source } => write!(
                f,
                "herdr is not running or not reachable at {} ({source}); start herdr, or pass \
                 --herdr-session / --herdr-socket for the session you use, then retry",
                socket.display()
            ),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::Api { code, message } => write!(f, "herdr api error [{code}]: {message}"),
            Self::Timeout => write!(f, "herdr request timed out"),
        }
    }
}

impl std::error::Error for HerdrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unavailable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Outgoing request envelope. `params` is always sent, even when empty.
#[derive(Debug, Serialize)]
pub struct RequestEnvelope<'a> {
    pub id: &'a str,
    pub method: &'a str,
    pub params: &'a serde_json::Value,
}

/// Lenient mirror of the pong result; unknown fields are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pong {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub protocol: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

/// Response envelope: success carries a tagged `result`, failure carries `error`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ResponseEnvelope {
    Success {
        #[serde(default)]
        id: String,
        result: serde_json::Value,
    },
    Error {
        #[serde(default)]
        id: String,
        error: ErrorBody,
    },
}

/// Decode one NDJSON response line into a result value or an api error.
pub fn decode_response(line: &str) -> Result<serde_json::Value, HerdrError> {
    let envelope: ResponseEnvelope = serde_json::from_str(line)
        .map_err(|err| HerdrError::Protocol(format!("malformed response line: {err}")))?;
    match envelope {
        ResponseEnvelope::Success { result, .. } => Ok(result),
        ResponseEnvelope::Error { error, .. } => Err(HerdrError::Api {
            code: error.code,
            message: error.message,
        }),
    }
}

/// Decode a pong result and enforce the supported protocol window.
pub fn parse_pong(result: serde_json::Value) -> Result<Pong, HerdrError> {
    let pong: Pong = serde_json::from_value(result)
        .map_err(|err| HerdrError::Protocol(format!("malformed pong result: {err}")))?;
    if pong.protocol < MIN_PROTOCOL {
        return Err(HerdrError::Protocol(format!(
            "server protocol {} is below the supported minimum {MIN_PROTOCOL}",
            pong.protocol
        )));
    }
    if pong.protocol > TESTED_MAX_PROTOCOL {
        tracing::warn!(
            protocol = pong.protocol,
            tested_max = TESTED_MAX_PROTOCOL,
            "server protocol is newer than the tested maximum"
        );
    }
    Ok(pong)
}

/// CLI flags that override socket path resolution.
#[derive(Debug, Default, Clone)]
pub struct SocketFlags {
    pub socket: Option<PathBuf>,
    pub session: Option<String>,
}

/// Resolve the herdr API socket path. Precedence:
/// `--herdr-socket`, `--herdr-session N`, `HERDR_SOCKET_PATH`, `HERDR_SESSION`
/// (ignoring `default`), then `<config>/herdr/herdr.sock`.
/// Pure: `env` supplies environment variables so every level is unit-testable.
pub fn resolve_socket_path<F>(flags: &SocketFlags, env: F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    let config_dir = || match env("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("herdr"),
        None => PathBuf::from(env("HOME").unwrap_or_default())
            .join(".config")
            .join("herdr"),
    };
    if let Some(path) = &flags.socket {
        return path.clone();
    }
    if let Some(name) = flags.session.as_deref().filter(|name| !name.is_empty()) {
        return session_socket_path(&config_dir(), name);
    }
    if let Some(path) = env(SOCKET_PATH_ENV_VAR).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(name) =
        env(SESSION_ENV_VAR).filter(|name| !name.is_empty() && name != DEFAULT_SESSION_NAME)
    {
        return session_socket_path(&config_dir(), &name);
    }
    config_dir().join("herdr.sock")
}

fn session_socket_path(config_dir: &Path, session: &str) -> PathBuf {
    config_dir.join("sessions").join(session).join("herdr.sock")
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn flags(socket: Option<&str>, session: Option<&str>) -> SocketFlags {
        SocketFlags {
            socket: socket.map(PathBuf::from),
            session: session.map(String::from),
        }
    }

    #[test]
    fn flag_socket_wins_over_everything() {
        let path = resolve_socket_path(
            &flags(Some("/tmp/explicit.sock"), Some("work")),
            env_from(&[
                ("HERDR_SOCKET_PATH", "/tmp/env.sock"),
                ("HERDR_SESSION", "env-session"),
                ("XDG_CONFIG_HOME", "/xdg"),
            ]),
        );
        assert_eq!(path, PathBuf::from("/tmp/explicit.sock"));
    }

    #[test]
    fn flag_session_beats_env_vars() {
        let path = resolve_socket_path(
            &flags(None, Some("work")),
            env_from(&[
                ("HERDR_SOCKET_PATH", "/tmp/ignored.sock"),
                ("HERDR_SESSION", "ignored"),
                ("XDG_CONFIG_HOME", "/xdg"),
            ]),
        );
        assert_eq!(path, PathBuf::from("/xdg/herdr/sessions/work/herdr.sock"));
    }

    #[test]
    fn env_socket_path_beats_env_session() {
        let path = resolve_socket_path(
            &flags(None, None),
            env_from(&[
                ("HERDR_SOCKET_PATH", "/tmp/live.sock"),
                ("HERDR_SESSION", "work"),
                ("XDG_CONFIG_HOME", "/xdg"),
            ]),
        );
        assert_eq!(path, PathBuf::from("/tmp/live.sock"));
    }

    #[test]
    fn env_session_selects_session_socket() {
        let path = resolve_socket_path(
            &flags(None, None),
            env_from(&[("HERDR_SESSION", "work"), ("HOME", "/home/u")]),
        );
        assert_eq!(
            path,
            PathBuf::from("/home/u/.config/herdr/sessions/work/herdr.sock")
        );
    }

    #[test]
    fn env_session_default_is_ignored() {
        let path = resolve_socket_path(
            &flags(None, None),
            env_from(&[("HERDR_SESSION", "default"), ("HOME", "/home/u")]),
        );
        assert_eq!(path, PathBuf::from("/home/u/.config/herdr/herdr.sock"));
    }

    #[test]
    fn xdg_config_home_default_socket() {
        let path =
            resolve_socket_path(&flags(None, None), env_from(&[("XDG_CONFIG_HOME", "/xdg")]));
        assert_eq!(path, PathBuf::from("/xdg/herdr/herdr.sock"));
    }

    #[test]
    fn home_fallback_default_socket() {
        let path = resolve_socket_path(&flags(None, None), env_from(&[("HOME", "/home/u")]));
        assert_eq!(path, PathBuf::from("/home/u/.config/herdr/herdr.sock"));
    }

    #[test]
    fn unavailable_display_names_the_socket_and_the_fix() {
        let err = HerdrError::Unavailable {
            socket: PathBuf::from("/tmp/herdr-display.sock"),
            source: io::Error::new(
                io::ErrorKind::NotFound,
                "No such file or directory (os error 2)",
            ),
        };
        let message = err.to_string();
        assert!(message.contains("/tmp/herdr-display.sock"), "got {message}");
        assert!(message.contains("start herdr"), "got {message}");
        let source: &dyn std::error::Error = err.source().expect("io error source");
        assert!(source.is::<io::Error>(), "source must be the io error");
    }

    #[test]
    fn decodes_success_envelope() {
        let line = r#"{"id":"r1","result":{"type":"pong","version":"0.9.3","protocol":22}}"#;
        let result = decode_response(line).expect("success");
        assert_eq!(result["type"], "pong");
    }

    #[test]
    fn decodes_error_envelope() {
        let line = r#"{"id":"r1","error":{"code":"unauthorized","message":"missing token"}}"#;
        match decode_response(line) {
            Err(HerdrError::Api { code, message }) => {
                assert_eq!(code, "unauthorized");
                assert_eq!(message, "missing token");
            }
            other => panic!("expected api error, got {other:?}"),
        }
    }

    #[test]
    fn ignores_unknown_fields() {
        let line = r#"{"id":"r1","extra":true,"result":{"type":"pong","version":"1.2.3","protocol":21,"capabilities":{"future":true},"nested":{"a":[1,2]}}}"#;
        let result = decode_response(line).expect("success");
        let pong = parse_pong(result).expect("pong");
        assert_eq!(pong.version, "1.2.3");
        assert_eq!(pong.protocol, 21);
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let line = r#"{"result":{"type":"pong","protocol":20}}"#;
        let result = decode_response(line).expect("success");
        let pong = parse_pong(result).expect("pong");
        assert_eq!(pong.version, "");
        let line = r#"{"id":"r1","error":{"code":"not_found"}}"#;
        match decode_response(line) {
            Err(HerdrError::Api { code, message }) => {
                assert_eq!(code, "not_found");
                assert_eq!(message, "");
            }
            other => panic!("expected api error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_line_is_protocol_error() {
        assert!(matches!(
            decode_response("not json"),
            Err(HerdrError::Protocol(_))
        ));
        assert!(matches!(
            decode_response(r#"{"id":"r1","status":"weird"}"#),
            Err(HerdrError::Protocol(_))
        ));
    }

    #[test]
    fn pong_protocol_22_accepted() {
        let result = serde_json::json!({"type": "pong", "version": "0.9.3", "protocol": 22});
        let pong = parse_pong(result).expect("protocol 22 must be accepted");
        assert_eq!(pong.protocol, 22);
    }

    #[test]
    fn pong_protocol_19_rejected() {
        let result = serde_json::json!({"type": "pong", "version": "0.9.0", "protocol": 19});
        assert!(matches!(parse_pong(result), Err(HerdrError::Protocol(_))));
    }

    #[test]
    fn decodes_workspace_list_result() {
        let result = serde_json::json!({
            "type": "workspace_list",
            "workspaces": [{
                "workspace_id": "w1", "number": 1, "label": "openai-api", "focused": false,
                "pane_count": 1, "tab_count": 1, "active_tab_id": "w1:t1",
                "agent_status": "unknown", "future_field": 1
            }]
        });
        let list = decode_result::<WorkspaceListResult>(result, "workspace_list").expect("decode");
        assert_eq!(list.workspaces[0].workspace_id, "w1");
        assert_eq!(list.workspaces[0].label, "openai-api");
        assert_eq!(list.workspaces[0].active_tab_id, "w1:t1");
    }

    #[test]
    fn decodes_workspace_created_result() {
        let result = serde_json::json!({
            "type": "workspace_created",
            "workspace": {"workspace_id": "w2", "number": 2, "label": "openai-api",
                          "focused": false, "pane_count": 1, "tab_count": 1,
                          "active_tab_id": "w2:t1", "agent_status": "unknown"},
            "tab": {"tab_id": "w2:t1", "workspace_id": "w2", "number": 1, "label": "openai-api",
                    "focused": false, "pane_count": 1, "agent_status": "unknown"},
            "root_pane": {"pane_id": "w2:p1", "terminal_id": "t9", "workspace_id": "w2",
                          "tab_id": "w2:t1", "focused": false}
        });
        let created =
            decode_result::<WorkspaceCreated>(result, "workspace_created").expect("decode");
        assert_eq!(created.workspace.workspace_id, "w2");
        assert_eq!(created.tab.tab_id, "w2:t1");
        assert_eq!(created.root_pane.pane_id, "w2:p1");
    }

    #[test]
    fn decodes_tab_list_result() {
        let result = serde_json::json!({
            "type": "tab_list",
            "tabs": [{
                "tab_id": "w1:t2", "workspace_id": "w1", "number": 2, "label": "oa:claude:abc123",
                "focused": true, "pane_count": 1, "agent_status": "idle"
            }]
        });
        let tabs = decode_result::<TabListResult>(result, "tab_list").expect("decode");
        assert_eq!(tabs.tabs[0].tab_id, "w1:t2");
        assert_eq!(tabs.tabs[0].label, "oa:claude:abc123");
    }

    #[test]
    fn decodes_tab_created_result() {
        let result = serde_json::json!({
            "type": "tab_created",
            "tab": {"tab_id": "w1:t3", "workspace_id": "w1", "number": 3, "label": "oa:claude:abc123",
                    "focused": false, "pane_count": 1, "agent_status": "unknown"},
            "root_pane": {"pane_id": "w1:p3", "terminal_id": "t10", "workspace_id": "w1",
                          "tab_id": "w1:t3", "focused": false}
        });
        let created = decode_result::<TabCreated>(result, "tab_created").expect("decode");
        assert_eq!(created.tab.tab_id, "w1:t3");
        assert_eq!(created.root_pane.pane_id, "w1:p3");
    }

    #[test]
    fn decodes_pane_list_result() {
        let result = serde_json::json!({
            "type": "pane_list",
            "panes": [
                {"pane_id": "w1:p3", "terminal_id": "t10", "workspace_id": "w1",
                 "tab_id": "w1:t3", "focused": false, "agent_status": "idle",
                 "tokens": {"oa_owner": "owner-1"}},
                {"pane_id": "w1:p4", "terminal_id": "t11", "workspace_id": "w1",
                 "tab_id": "w1:t4", "focused": false, "agent_status": "unknown"}
            ]
        });
        let list = decode_result::<PaneListResult>(result, "pane_list").expect("decode");
        assert_eq!(list.panes[0].tab_id, "w1:t3");
        assert_eq!(
            list.panes[0].tokens.get("oa_owner").map(String::as_str),
            Some("owner-1")
        );
        assert!(list.panes[1].tokens.is_empty());
    }

    #[test]
    fn decodes_tab_info_result() {
        let result = serde_json::json!({
            "type": "tab_info",
            "tab": {"tab_id": "w1:t1", "workspace_id": "w1", "number": 1, "label": "lobby",
                    "focused": false, "pane_count": 1, "agent_status": "unknown"}
        });
        let tab = decode_result::<TabInfoResult>(result, "tab_info").expect("decode");
        assert_eq!(tab.tab.label, "lobby");
    }

    #[test]
    fn decodes_ok_result() {
        let result = serde_json::json!({"type": "ok"});
        decode_result::<serde_json::Value>(result, "ok").expect("decode");
    }

    #[test]
    fn decodes_agent_info_result() {
        let result = serde_json::json!({
            "type": "agent_info",
            "agent": {
                "terminal_id": "t11", "name": "oa-claude-abc123", "agent": "claude",
                "agent_status": "idle", "screen_detection_skipped": false,
                "agent_session": {"source": "integration", "agent": "claude",
                                  "kind": "id", "value": "6f9c1a44-1111-4111-8111-000000000002"},
                "workspace_id": "w1", "tab_id": "w1:t3", "pane_id": "w1:p3", "focused": false,
                "cwd": "/home/u/project", "state_change_seq": 7, "revision": 3,
                "interactive_ready": true
            }
        });
        let info = decode_result::<AgentInfoResult>(result, "agent_info").expect("decode");
        let agent = info.agent;
        assert_eq!(agent.name.as_deref(), Some("oa-claude-abc123"));
        assert_eq!(agent.agent_status, AgentStatus::Idle);
        assert!(agent.interactive_ready);
        assert!(!agent.launch_pending);
        assert_eq!(
            agent.agent_session.as_ref().expect("session").value,
            "6f9c1a44-1111-4111-8111-000000000002"
        );
    }

    #[test]
    fn decodes_agent_started_result() {
        let result = serde_json::json!({
            "type": "agent_started",
            "agent": {"terminal_id": "t12", "name": "oa-claude-abc123", "agent": "claude",
                      "agent_status": "unknown", "workspace_id": "w1", "tab_id": "w1:t3",
                      "pane_id": "w1:p3", "focused": false, "launch_pending": true,
                      "state_change_seq": 1},
            "argv": ["claude", "--session-id", "6f9c1a44-1111-4111-8111-000000000002"]
        });
        let started = decode_result::<AgentStarted>(result, "agent_started").expect("decode");
        assert!(started.agent.launch_pending);
        assert!(!started.agent.interactive_ready);
        assert_eq!(started.argv.len(), 3);
    }

    #[test]
    fn decodes_agent_prompted_result() {
        let result = serde_json::json!({
            "type": "agent_prompted",
            "agent": {"terminal_id": "t13", "name": "oa-claude-abc123", "agent": "claude",
                      "agent_status": "working", "workspace_id": "w1", "tab_id": "w1:t3",
                      "pane_id": "w1:p3", "focused": false, "state_change_seq": 4}
        });
        let info = decode_result::<AgentInfoResult>(result, "agent_prompted").expect("decode");
        assert_eq!(info.agent.agent_status, AgentStatus::Working);
    }

    #[test]
    fn decodes_pane_read_result() {
        let result = serde_json::json!({
            "type": "pane_read",
            "read": {"text": "line one\nline two", "source": "recent_unwrapped",
                     "lines": 40, "format": "text", "future": true}
        });
        let read = decode_result::<PaneReadResult>(result, "pane_read").expect("decode");
        assert_eq!(read.read.text, "line one\nline two");
    }

    #[test]
    fn wrong_result_type_is_protocol_error() {
        let result = serde_json::json!({"type": "agent_info", "agent": {}});
        let err = decode_result::<TabListResult>(result, "tab_list").expect_err("must fail");
        assert!(matches!(err, HerdrError::Protocol(_)));
        let result = serde_json::json!({"no_type": true});
        assert!(matches!(
            decode_result::<TabListResult>(result, "tab_list"),
            Err(HerdrError::Protocol(_))
        ));
    }

    #[test]
    fn unknown_agent_status_maps_to_unknown() {
        let status: AgentStatus = serde_json::from_str(r#""quantum_flux""#).expect("lenient");
        assert_eq!(status, AgentStatus::Unknown);
        let status: AgentStatus = serde_json::from_str(r#""blocked""#).expect("snake case");
        assert_eq!(status, AgentStatus::Blocked);
        let spec = WaitSpec {
            until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked],
            timeout_ms: Some(1000),
        };
        assert_eq!(spec.until_param()["until"][0], "idle");
    }
}
