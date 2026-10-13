use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Model id suffix for self-exec: the agent runs its own tools.
pub const AGENT_TOOLS_SUFFIX: &str = "+agent-tools";
/// Model id suffix for the tool-call bridge: the client runs the tools.
pub const CLIENT_TOOLS_SUFFIX: &str = "+client-tools";

#[derive(Debug)]
pub enum ConfigError {
    Io { path: PathBuf, message: String },
    Toml(String),
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, message } => {
                write!(f, "failed to read config {}: {message}", path.display())
            }
            Self::Toml(msg) => write!(f, "invalid config: {msg}"),
            Self::Validation(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub herdr: HerdrConfig,
    pub conversations: ConversationsConfig,
    pub defaults: DefaultsConfig,
    pub discovery: DiscoveryConfig,
    pub models: Vec<ModelSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub api_key_env: String,
    pub allow_remote: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8787".to_string(),
            api_key_env: "HERDR_OPENAI_SERVER_API_KEY".to_string(),
            allow_remote: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HerdrConfig {
    pub socket: Option<PathBuf>,
    pub session: Option<String>,
    pub workspace_label: String,
    pub startup_timeout_ms: u64,
    pub turn_timeout_ms: u64,
    pub blocked_timeout_ms: u64,
    pub completion_grace_ms: u64,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self {
            socket: None,
            session: None,
            workspace_label: "openai-api".to_string(),
            startup_timeout_ms: 60_000,
            turn_timeout_ms: 1_800_000,
            blocked_timeout_ms: 600_000,
            completion_grace_ms: 15_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConversationsConfig {
    pub idle_ttl_secs: u64,
    pub max_agents_per_model: usize,
    pub max_agents_total: usize,
}

impl Default for ConversationsConfig {
    fn default() -> Self {
        Self {
            idle_ttl_secs: 1800,
            max_agents_per_model: 3,
            max_agents_total: 8,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    pub cwd: PathBuf,
    pub allowed_cwd_roots: Vec<PathBuf>,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("~"),
            allowed_cwd_roots: vec![PathBuf::from("~")],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    pub shell: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub timeout_ms: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            shell: None,
            include: Vec::new(),
            exclude: Vec::new(),
            timeout_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    /// Human-readable product name used in client-facing relay text.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// Kind label accepted by herdr's `agent.start`.
    pub fn herdr_label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Keys that clear what the CLI leaves in its input box after an Esc.
    /// Claude restores the interrupted prompt, so it needs one ctrl+c; Codex
    /// leaves its composer empty and ctrl+c on an empty composer quits the CLI.
    pub fn clear_input_keys(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["ctrl+c"],
            Self::Codex => &[],
        }
    }
}

impl<'de> Deserialize<'de> for AgentKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => Err(serde::de::Error::custom(format!(
                "unknown model kind {other:?}: only claude and codex are supported"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelMode {
    SelfExec,
    Bridge,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub id: String,
    pub kind: AgentKind,
    /// Shell command that starts this CLI; `None` means the kind's base name.
    /// A variant command (e.g. `claude-personal`) is typed into the tab's
    /// shell instead of started through herdr, which only knows `claude`/`codex`.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl ModelSpec {
    /// `+client-tools` suffix selects the tool-call bridge mode.
    pub fn mode(&self) -> ModelMode {
        if self.id.ends_with(CLIENT_TOOLS_SUFFIX) {
            ModelMode::Bridge
        } else {
            ModelMode::SelfExec
        }
    }

    /// Human-readable mode summary for `/v1/models`.
    pub fn description(&self) -> String {
        let command = self.command.as_deref().unwrap_or(self.kind.herdr_label());
        match self.mode() {
            ModelMode::SelfExec => format!(
                "{command}: the agent runs its own tools in a herdr tab; the client gets only the final text"
            ),
            ModelMode::Bridge => format!(
                "{command}: the client runs its own tools; the agent only replies with tool_calls"
            ),
        }
    }
}

const CLAUDE_FORBIDDEN_ARGS: &[&str] = &[
    "--session-id",
    "--resume",
    "-r",
    "--continue",
    "-c",
    "-p",
    "--print",
];
const CODEX_FORBIDDEN_ARGS: &[&str] = &["-C", "--cd"];

/// Extra args that turn a Claude CLI into the tool-call bridge.
pub const CLAUDE_BRIDGE_ARGS: &[&str] = &["--tools", "", "--strict-mcp-config"];
/// Extra args that keep a Codex CLI read-only for the tool-call bridge.
pub const CODEX_BRIDGE_ARGS: &[&str] = &["-s", "read-only", "-a", "never"];

impl Config {
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Config =
            toml::from_str(text).map_err(|err| ConfigError::Toml(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => {
                let path = default_config_path(|key| std::env::var(key).ok());
                if !path.exists() {
                    return Ok(Self::builtin());
                }
                path
            }
        };
        let text = std::fs::read_to_string(&path).map_err(|err| ConfigError::Io {
            path: path.clone(),
            message: err.to_string(),
        })?;
        Self::from_toml_str(&text)
    }

    /// Built-in defaults: four models, no config file needed.
    pub fn builtin() -> Self {
        Self {
            models: builtin_models(),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut seen_ids = BTreeSet::new();
        for model in &self.models {
            validate_model_id(&model.id)?;
            if !seen_ids.insert(model.id.clone()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate model id: {}",
                    model.id
                )));
            }
            if let Some(command) = &model.command {
                validate_command(&model.id, command, model.kind)?;
            }
            for arg in &model.args {
                if arg.chars().any(char::is_control) {
                    return Err(ConfigError::Validation(format!(
                        "model {}: args must not contain control characters",
                        model.id
                    )));
                }
            }
            match model.kind {
                AgentKind::Claude => {
                    if let Some(arg) = find_forbidden(&model.args, CLAUDE_FORBIDDEN_ARGS) {
                        return Err(ConfigError::Validation(format!(
                            "model {}: claude args must not contain {arg}",
                            model.id
                        )));
                    }
                    if model.mode() == ModelMode::Bridge
                        && !model
                            .args
                            .iter()
                            .any(|a| a == "--tools" || a.starts_with("--tools="))
                    {
                        tracing::warn!(
                            model = %model.id,
                            "bridge claude model without --tools still runs built-in tools"
                        );
                    }
                }
                AgentKind::Codex => {
                    if let Some(arg) = find_forbidden(&model.args, CODEX_FORBIDDEN_ARGS) {
                        return Err(ConfigError::Validation(format!(
                            "model {}: codex args must not contain {arg}",
                            model.id
                        )));
                    }
                    if let Some(first) = model.args.first() {
                        if !first.starts_with('-') {
                            return Err(ConfigError::Validation(format!(
                                "model {}: codex args must not start with a subcommand",
                                model.id
                            )));
                        }
                    }
                }
            }
        }
        let startup = self.herdr.startup_timeout_ms;
        if startup <= 3000 || startup > 300_000 {
            return Err(ConfigError::Validation(
                "herdr.startup_timeout_ms must be in (3000, 300000]".to_string(),
            ));
        }
        Ok(())
    }

    /// Models the server offers: discovered variants first when discovery is
    /// on and found any, with `[[models]]` entries replacing by id or adding;
    /// otherwise the configured `[[models]]`, or the built-ins when none.
    pub fn effective_models(&self, variants: &[crate::discovery::Variant]) -> Vec<ModelSpec> {
        if self.discovery.enabled && !variants.is_empty() {
            let mut models = crate::discovery::variant_models(variants);
            for spec in &self.models {
                match models.iter_mut().find(|model| model.id == spec.id) {
                    Some(model) => *model = spec.clone(),
                    None => models.push(spec.clone()),
                }
            }
            return models;
        }
        if self.models.is_empty() {
            builtin_models()
        } else {
            self.models.clone()
        }
    }
}

fn validate_command(model_id: &str, command: &str, kind: AgentKind) -> Result<(), ConfigError> {
    let valid = !command.is_empty()
        && command.len() <= 64
        && command
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    if !valid {
        return Err(ConfigError::Validation(format!(
            "model {model_id}: command {command:?} must match [A-Za-z0-9._+-]{{1,64}}"
        )));
    }
    if !command.starts_with(kind.herdr_label()) {
        return Err(ConfigError::Validation(format!(
            "model {model_id}: command {command:?} must start with {}",
            kind.herdr_label()
        )));
    }
    Ok(())
}

fn find_forbidden<'a>(args: &'a [String], forbidden: &[&'a str]) -> Option<&'a str> {
    args.iter().find_map(|arg| {
        forbidden
            .iter()
            .find(|flag| arg == *flag || arg.starts_with(&format!("{flag}=")))
            .copied()
    })
}

fn validate_model_id(id: &str) -> Result<(), ConfigError> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    if !valid {
        return Err(ConfigError::Validation(format!(
            "model id {id:?} must match [A-Za-z0-9._+-]{{1,64}}"
        )));
    }
    Ok(())
}

fn builtin_models() -> Vec<ModelSpec> {
    [
        ("claude+agent-tools", AgentKind::Claude, &[][..]),
        ("claude+client-tools", AgentKind::Claude, CLAUDE_BRIDGE_ARGS),
        ("codex+agent-tools", AgentKind::Codex, &[][..]),
        ("codex+client-tools", AgentKind::Codex, CODEX_BRIDGE_ARGS),
    ]
    .into_iter()
    .map(|(id, kind, args)| ModelSpec {
        id: id.to_string(),
        kind,
        command: None,
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        env: BTreeMap::new(),
        cwd: None,
    })
    .collect()
}

/// Resolve the default config path: `$XDG_CONFIG_HOME|~/.config` + `herdr-openai-server/config.toml`.
pub fn default_config_path<F>(env: F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    let base = match env("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env("HOME").unwrap_or_default()).join(".config"),
    };
    base.join("herdr-openai-server").join("config.toml")
}

/// Expand a leading `~` or `~/` against the given home directory.
pub fn expand_tilde(path: &Path, home: &str) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        PathBuf::from(home)
    } else if let Some(rest) = text.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else {
        path.to_path_buf()
    }
}

/// Render a path under `home` with a leading `~` for display.
pub fn tilde_display(path: &Path, home: &str) -> String {
    let text = path.to_string_lossy();
    if text == home {
        "~".to_string()
    } else if let Some(rest) = text.strip_prefix(&format!("{home}/")) {
        format!("~/{rest}")
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_uses_all_defaults_and_builtin_models() {
        let config = Config::from_toml_str("").expect("empty toml is valid");
        assert_eq!(config.server.listen, "127.0.0.1:8787");
        assert_eq!(config.server.api_key_env, "HERDR_OPENAI_SERVER_API_KEY");
        assert!(!config.server.allow_remote);
        assert_eq!(config.herdr.workspace_label, "openai-api");
        assert_eq!(config.herdr.startup_timeout_ms, 60_000);
        assert_eq!(config.herdr.turn_timeout_ms, 1_800_000);
        assert_eq!(config.herdr.blocked_timeout_ms, 600_000);
        assert_eq!(config.herdr.completion_grace_ms, 15_000);
        assert_eq!(config.conversations.idle_ttl_secs, 1800);
        assert_eq!(config.conversations.max_agents_per_model, 3);
        assert_eq!(config.conversations.max_agents_total, 8);
        assert_eq!(config.defaults.cwd, PathBuf::from("~"));
        assert_eq!(config.defaults.allowed_cwd_roots, vec![PathBuf::from("~")]);
        assert!(config.discovery.enabled);
        assert_eq!(config.discovery.shell, None);
        assert!(config.discovery.include.is_empty());
        assert!(config.discovery.exclude.is_empty());
        assert_eq!(config.discovery.timeout_ms, 10_000);

        assert!(config.models.is_empty());
        let models = config.effective_models(&[]);
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].id, "claude+agent-tools");
        assert_eq!(models[0].kind, AgentKind::Claude);
        assert!(models[0].args.is_empty());
        assert_eq!(models[1].id, "claude+client-tools");
        assert_eq!(
            models[1].args,
            ["--tools", "", "--strict-mcp-config"].map(String::from)
        );
        assert_eq!(models[2].id, "codex+agent-tools");
        assert_eq!(models[2].kind, AgentKind::Codex);
        assert!(models[2].args.is_empty());
        assert_eq!(models[3].id, "codex+client-tools");
        assert_eq!(
            models[3].args,
            ["-s", "read-only", "-a", "never"].map(String::from)
        );
        assert_eq!(
            models[0].description(),
            "claude: the agent runs its own tools in a herdr tab; the client gets only the final text"
        );
        assert_eq!(
            models[1].description(),
            "claude: the client runs its own tools; the agent only replies with tool_calls"
        );
    }

    #[test]
    fn partial_toml_keeps_other_sections_at_defaults() {
        let config = Config::from_toml_str(
            r#"
[server]
listen = "127.0.0.1:9000"

[[models]]
id = "my-claude"
kind = "claude"
args = ["--permission-mode", "acceptEdits"]
"#,
        )
        .expect("partial toml");
        assert_eq!(config.server.listen, "127.0.0.1:9000");
        assert_eq!(config.server.api_key_env, "HERDR_OPENAI_SERVER_API_KEY");
        assert_eq!(config.herdr.workspace_label, "openai-api");
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].mode(), ModelMode::SelfExec);
    }

    #[test]
    fn model_mode_from_id_suffix() {
        let bridge = ModelSpec {
            id: "claude+client-tools".to_string(),
            kind: AgentKind::Claude,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        };
        assert_eq!(bridge.mode(), ModelMode::Bridge);
        let agent = ModelSpec {
            id: "claude+agent-tools".to_string(),
            kind: AgentKind::Claude,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        };
        assert_eq!(agent.mode(), ModelMode::SelfExec);
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let err = Config::from_toml_str("bogus = 1").expect_err("must fail");
        assert!(matches!(err, ConfigError::Toml(_)), "got {err}");
    }

    #[test]
    fn unknown_kind_mentions_supported_kinds() {
        let err = Config::from_toml_str(
            r#"
[[models]]
id = "gpt"
kind = "gemini"
"#,
        )
        .expect_err("must fail");
        let message = err.to_string();
        assert!(
            message.contains("only claude and codex are supported"),
            "got {message}"
        );
    }

    #[test]
    fn model_id_charset_and_length_are_enforced() {
        let bad = Config {
            models: vec![model_spec("bad id!")],
            ..Config::builtin()
        };
        assert!(matches!(bad.validate(), Err(ConfigError::Validation(_))));
        let long = Config {
            models: vec![model_spec(&"x".repeat(65))],
            ..Config::builtin()
        };
        assert!(matches!(long.validate(), Err(ConfigError::Validation(_))));
        let ok = Config {
            models: vec![model_spec("A.b_c+d-1")],
            ..Config::builtin()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn duplicate_model_ids_are_rejected() {
        let config = Config {
            models: vec![model_spec("dup"), model_spec("dup")],
            ..Config::builtin()
        };
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));
    }

    #[test]
    fn claude_forbidden_args_are_rejected() {
        for arg in [
            "--session-id",
            "--session-id=abc",
            "--resume",
            "-r",
            "--continue",
            "-c",
            "-p",
            "--print",
        ] {
            let mut spec = model_spec("c");
            spec.kind = AgentKind::Claude;
            spec.args = vec![arg.to_string()];
            let config = Config {
                models: vec![spec],
                ..Config::builtin()
            };
            assert!(
                matches!(config.validate(), Err(ConfigError::Validation(_))),
                "claude arg {arg} must be rejected"
            );
        }
    }

    #[test]
    fn codex_forbidden_args_and_subcommands_are_rejected() {
        let mut spec = model_spec("cx");
        spec.kind = AgentKind::Codex;
        spec.args = vec!["-C".to_string(), "/tmp".to_string()];
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut spec = model_spec("cx");
        spec.kind = AgentKind::Codex;
        spec.args = vec!["exec".to_string()];
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut spec = model_spec("cx");
        spec.kind = AgentKind::Codex;
        spec.args = vec!["--cd".to_string()];
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));
    }

    #[test]
    fn control_characters_in_args_are_rejected() {
        let mut spec = model_spec("c");
        spec.args = vec!["a\u{7}b".to_string()];
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));
    }

    #[test]
    fn startup_timeout_window_is_enforced() {
        let mut config = Config::builtin();
        config.herdr.startup_timeout_ms = 3000;
        assert!(config.validate().is_err());
        config.herdr.startup_timeout_ms = 300_001;
        assert!(config.validate().is_err());
        config.herdr.startup_timeout_ms = 3001;
        assert!(config.validate().is_ok());
        config.herdr.startup_timeout_ms = 300_000;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn bridge_claude_without_tools_is_only_a_warning() {
        let mut spec = model_spec("claude+client-tools");
        spec.kind = AgentKind::Claude;
        spec.args = vec![];
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn builtin_config_is_valid() {
        assert!(Config::builtin().validate().is_ok());
    }

    #[test]
    fn config_example_parses_and_matches_the_builtin_defaults() {
        let example =
            Config::from_toml_str(include_str!("../config.example.toml")).expect("example parses");
        assert!(example.validate().is_ok());
        // The example lists every option at its default value, so it must stay
        // identical to the built-in config; commented-out lines are the only
        // difference allowed.
        assert_eq!(format!("{example:?}"), format!("{:?}", Config::builtin()));
    }

    #[test]
    fn expand_tilde_handles_home_paths() {
        assert_eq!(
            expand_tilde(Path::new("~"), "/home/u"),
            PathBuf::from("/home/u")
        );
        assert_eq!(
            expand_tilde(Path::new("~/projects"), "/home/u"),
            PathBuf::from("/home/u/projects")
        );
        assert_eq!(
            expand_tilde(Path::new("/abs/path"), "/home/u"),
            PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_tilde(Path::new("relative"), "/home/u"),
            PathBuf::from("relative")
        );
    }

    #[test]
    fn default_config_path_precedence() {
        let path = default_config_path(|key| match key {
            "XDG_CONFIG_HOME" => Some("/xdg".to_string()),
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        });
        assert_eq!(path, PathBuf::from("/xdg/herdr-openai-server/config.toml"));
        let path = default_config_path(|key| match key {
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        });
        assert_eq!(
            path,
            PathBuf::from("/home/u/.config/herdr-openai-server/config.toml")
        );
    }

    #[test]
    fn load_missing_explicit_path_is_an_error() {
        let err = Config::load(Some(Path::new(
            "/nonexistent/herdr-openai-server-config.toml",
        )))
        .expect_err("missing explicit path must fail");
        assert!(matches!(err, ConfigError::Io { .. }), "got {err}");
    }

    #[test]
    fn load_explicit_file_parses() {
        let dir =
            std::env::temp_dir().join(format!("herdr-openai-server-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[server]\nlisten = \"127.0.0.1:1234\"\n").expect("write config");
        let config = Config::load(Some(&path)).expect("load explicit");
        assert_eq!(config.server.listen, "127.0.0.1:1234");
        assert!(config.models.is_empty());
        assert_eq!(config.effective_models(&[]).len(), 4);
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn model_command_charset_and_base_name_are_enforced() {
        let mut spec = model_spec("c");
        spec.command = Some("claude-personal".to_string());
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        assert!(config.validate().is_ok());

        let mut spec = model_spec("c");
        spec.command = Some("claude personal".to_string());
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut spec = model_spec("c");
        spec.command = Some("my-claude".to_string());
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        let err = config.validate().expect_err("must fail");
        assert!(
            err.to_string().contains("must start with claude"),
            "got {err}"
        );

        let mut spec = model_spec("cx");
        spec.kind = AgentKind::Codex;
        spec.command = Some("claude-x".to_string());
        let config = Config {
            models: vec![spec],
            ..Config::builtin()
        };
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));
    }

    #[test]
    fn effective_models_use_variants_then_override_by_id() {
        let variants = vec![
            crate::discovery::Variant {
                name: "claude".to_string(),
                kind: AgentKind::Claude,
                config_home: None,
            },
            crate::discovery::Variant {
                name: "claude-personal".to_string(),
                kind: AgentKind::Claude,
                config_home: None,
            },
        ];
        let config = Config::default();
        let models = config.effective_models(&variants);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "claude+agent-tools",
                "claude+client-tools",
                "claude-personal+agent-tools",
                "claude-personal+client-tools"
            ]
        );

        // A [[models]] entry with the same id replaces; a new id appends.
        let mut override_spec = model_spec("claude-personal");
        override_spec.command = Some("claude-personal".to_string());
        override_spec.args = vec!["--permission-mode".to_string()];
        let extra = model_spec("my-extra");
        let config = Config {
            models: vec![override_spec, extra],
            ..Config::builtin()
        };
        let models = config.effective_models(&variants);
        let personal = models
            .iter()
            .find(|m| m.id == "claude-personal")
            .expect("present");
        assert_eq!(personal.args, ["--permission-mode"].map(String::from));
        assert!(models.iter().any(|m| m.id == "my-extra"));
        assert_eq!(models.last().map(|m| m.id.as_str()), Some("my-extra"));
    }

    #[test]
    fn effective_models_fall_back_without_variants() {
        let config = Config::builtin();
        assert_eq!(config.effective_models(&[]).len(), 4);

        let mut config = Config::builtin();
        config.discovery.enabled = false;
        let variants = vec![crate::discovery::Variant {
            name: "claude-personal".to_string(),
            kind: AgentKind::Claude,
            config_home: None,
        }];
        assert_eq!(config.effective_models(&variants).len(), 4);

        let custom = model_spec("only-one");
        let config = Config {
            models: vec![custom],
            ..Config::builtin()
        };
        let models = config.effective_models(&[]);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "only-one");
    }

    #[test]
    fn tilde_display_shortens_home_prefixes() {
        assert_eq!(tilde_display(Path::new("/home/u"), "/home/u"), "~");
        assert_eq!(
            tilde_display(Path::new("/home/u/.codex"), "/home/u"),
            "~/.codex"
        );
        assert_eq!(tilde_display(Path::new("/other"), "/home/u"), "/other");
    }

    fn model_spec(id: &str) -> ModelSpec {
        ModelSpec {
            id: id.to_string(),
            kind: AgentKind::Claude,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }
}
