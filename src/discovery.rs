//! Discovery of agent CLI variants: shell functions, aliases and commands
//! named `claude`, `codex`, or `<base>-<suffix>` in the user's shell.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use std::collections::BTreeMap;

use crate::config::{
    tilde_display, AgentKind, DiscoveryConfig, ModelSpec, CLAUDE_BRIDGE_ARGS, CODEX_BRIDGE_ARGS,
};

/// A discovered CLI variant to offer as a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub name: String,
    pub kind: AgentKind,
    /// Config dir the variant exports (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`), when found.
    pub config_home: Option<PathBuf>,
}

/// Result of one discovery run.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub variants: Vec<Variant>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Function,
    Alias,
    File,
}

/// One name printed by the discovery script, with its definition body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub kind: CandidateKind,
    pub body: String,
}

const MARKER: &str = "@@herdr-openai-gateway@@";

/// Bash script: marker line per matching name, then its definition.
const BASH_SCRIPT: &str = r#"for name in $(compgen -A function -A alias -A command | grep -E '^(claude|codex)(-[A-Za-z0-9._-]+)?$' | sort -u); do
  case "$(type -t "$name" 2>/dev/null)" in
    function) printf '@@herdr-openai-gateway@@ %s function\n' "$name"; declare -f "$name" ;;
    alias) printf '@@herdr-openai-gateway@@ %s alias\n' "$name"; alias "$name" ;;
    file) printf '@@herdr-openai-gateway@@ %s file\n' "$name" ;;
  esac
done"#;

/// zsh script: same marker contract as the bash one.
const ZSH_SCRIPT: &str = r#"for name in ${(k)functions} ${(k)aliases} ${(k)commands}; do
  [[ $name =~ '^(claude|codex)(-[A-Za-z0-9._-]+)?$' ]] || continue
  case "$(whence -w "$name" 2>/dev/null)" in
    *:function) printf '@@herdr-openai-gateway@@ %s function\n' "$name"; print -r -- "${functions[$name]}" ;;
    *:alias) printf '@@herdr-openai-gateway@@ %s alias\n' "$name"; alias "$name" ;;
    *:command) printf '@@herdr-openai-gateway@@ %s file\n' "$name" ;;
  esac
done"#;

/// Run the interactive shell and collect the variants it defines.
pub async fn discover<F>(cfg: &DiscoveryConfig, env: F) -> Discovery
where
    F: Fn(&str) -> Option<String>,
{
    if !cfg.enabled {
        return Discovery::default();
    }
    let shell = cfg
        .shell
        .as_deref()
        .filter(|shell| !shell.is_empty())
        .map(str::to_string)
        .or_else(|| env("SHELL").filter(|shell| !shell.is_empty()));
    let Some(shell) = shell else {
        return Discovery {
            warnings: vec![
                "discovery: no shell configured and $SHELL is unset; no agent variants found"
                    .to_string(),
            ],
            ..Discovery::default()
        };
    };
    let script = match Path::new(&shell).file_name().and_then(|n| n.to_str()) {
        Some("bash") => BASH_SCRIPT,
        Some("zsh") => ZSH_SCRIPT,
        _ => {
            return Discovery {
                warnings: vec![format!(
                    "discovery: unsupported shell {shell:?}; only bash and zsh are supported"
                )],
                ..Discovery::default()
            };
        }
    };
    let mut command = tokio::process::Command::new(&shell);
    command
        .arg("-ic")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(home) = env("HOME").filter(|home| !home.is_empty()) {
        command.env("HOME", home);
    }
    let output =
        match tokio::time::timeout(Duration::from_millis(cfg.timeout_ms), command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => {
                return Discovery {
                    warnings: vec![format!("discovery: failed to run {shell}: {err}")],
                    ..Discovery::default()
                };
            }
            Err(_) => {
                return Discovery {
                    warnings: vec![format!(
                        "discovery: {shell} timed out after {} ms",
                        cfg.timeout_ms
                    )],
                    ..Discovery::default()
                };
            }
        };
    if !output.status.success() {
        return Discovery {
            warnings: vec![format!("discovery: {shell} exited with {}", output.status)],
            ..Discovery::default()
        };
    }
    let home = env("HOME").unwrap_or_default();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut variants: Vec<Variant> = parse_candidates(&text)
        .iter()
        .filter_map(|candidate| classify(candidate, &cfg.include, &cfg.exclude, &home))
        .collect();
    variants.sort_by(|a, b| a.name.cmp(&b.name));
    Discovery {
        variants,
        warnings: Vec::new(),
    }
}

/// Decode marker lines into candidates; shell noise before and between them is
/// ignored. A function shadows an alias, an alias shadows a file of the same name.
pub fn parse_candidates(output: &str) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix(MARKER) else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let (Some(name), Some(kind_word)) = (parts.next(), parts.next()) else {
            continue;
        };
        let kind = match kind_word {
            "function" => CandidateKind::Function,
            "alias" => CandidateKind::Alias,
            "file" => CandidateKind::File,
            _ => continue,
        };
        let mut body_lines: Vec<&str> = Vec::new();
        while lines
            .peek()
            .is_some_and(|next| !next.trim().starts_with(MARKER))
        {
            body_lines.push(lines.next().unwrap_or(""));
        }
        let candidate = Candidate {
            name: name.to_string(),
            kind,
            body: body_lines.join("\n"),
        };
        match candidates.iter_mut().find(|c| c.name == candidate.name) {
            Some(existing) => {
                if kind_rank(candidate.kind) > kind_rank(existing.kind) {
                    *existing = candidate;
                }
            }
            None => candidates.push(candidate),
        }
    }
    candidates
}

fn kind_rank(kind: CandidateKind) -> u8 {
    match kind {
        CandidateKind::Function => 2,
        CandidateKind::Alias => 1,
        CandidateKind::File => 0,
    }
}

/// Decide whether a candidate is an agent variant worth offering.
pub fn classify(
    candidate: &Candidate,
    include: &[String],
    exclude: &[String],
    home: &str,
) -> Option<Variant> {
    if exclude.contains(&candidate.name) {
        return None;
    }
    let kind = kind_from_name(&candidate.name)?;
    let base = candidate.name == kind.herdr_label();
    let forced = include.contains(&candidate.name);
    let accepted = base
        || forced
        || match candidate.kind {
            CandidateKind::Function | CandidateKind::Alias => {
                has_standalone_token(&candidate.body, kind.herdr_label())
            }
            CandidateKind::File => false,
        };
    if !accepted {
        return None;
    }
    Some(Variant {
        name: candidate.name.clone(),
        kind,
        config_home: config_home_from_body(&candidate.body, kind, home),
    })
}

fn kind_from_name(name: &str) -> Option<AgentKind> {
    if name == "claude" || name.starts_with("claude-") {
        Some(AgentKind::Claude)
    } else if name == "codex" || name.starts_with("codex-") {
        Some(AgentKind::Codex)
    } else {
        None
    }
}

/// True when `token` appears in `body` with non-word characters on both sides,
/// so `command codex` counts but `codex_flags_ref` does not.
fn has_standalone_token(body: &str, token: &str) -> bool {
    let bytes = body.as_bytes();
    let mut from = 0usize;
    while let Some(offset) = body[from..].find(token) {
        let start = from + offset;
        let end = start + token.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// The config dir a variant exports: `CLAUDE_CONFIG_DIR=` / `CODEX_HOME=`,
/// quoted or not, with `$HOME`, `${HOME}` and `~` expanded. `None` when the
/// value is absent or still contains `$` after expansion.
pub fn config_home_from_body(body: &str, kind: AgentKind, home: &str) -> Option<PathBuf> {
    let var = match kind {
        AgentKind::Claude => "CLAUDE_CONFIG_DIR",
        AgentKind::Codex => "CODEX_HOME",
    };
    let bytes = body.as_bytes();
    let mut from = 0usize;
    while let Some(offset) = body[from..].find(var) {
        let start = from + offset;
        let end = start + var.len();
        from = end;
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        if !before_ok || !body[end..].starts_with('=') {
            continue;
        }
        let value = body[end + 1..].trim_start();
        let raw = if let Some(rest) = value.strip_prefix('"') {
            rest.split('"').next().unwrap_or("")
        } else if let Some(rest) = value.strip_prefix('\'') {
            rest.split('\'').next().unwrap_or("")
        } else {
            value.split_whitespace().next().unwrap_or("")
        };
        if raw.is_empty() {
            return None;
        }
        let expanded = raw.replace("$HOME", home).replace("${HOME}", home);
        if expanded.contains('$') {
            return None;
        }
        return Some(crate::config::expand_tilde(Path::new(&expanded), home));
    }
    None
}

/// Models for the variants: each `v+agent-tools` and `v+client-tools`, `claude` and `codex`
/// first, then the rest alphabetically.
pub fn variant_models(variants: &[Variant]) -> Vec<ModelSpec> {
    let mut ordered: Vec<&Variant> = variants.iter().collect();
    ordered.sort_by(|a, b| {
        base_rank(&a.name)
            .cmp(&base_rank(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut models = Vec::with_capacity(ordered.len() * 2);
    for variant in ordered {
        models.push(model_for(variant, &[]));
        let bridge = match variant.kind {
            AgentKind::Claude => CLAUDE_BRIDGE_ARGS,
            AgentKind::Codex => CODEX_BRIDGE_ARGS,
        };
        models.push(model_for(variant, bridge));
    }
    models
}

fn base_rank(name: &str) -> u8 {
    match name {
        "claude" => 0,
        "codex" => 1,
        _ => 2,
    }
}

fn model_for(variant: &Variant, args: &[&str]) -> ModelSpec {
    let id = if args.is_empty() {
        format!("{}{}", variant.name, crate::config::AGENT_TOOLS_SUFFIX)
    } else {
        format!("{}{}", variant.name, crate::config::CLIENT_TOOLS_SUFFIX)
    };
    ModelSpec {
        id,
        kind: variant.kind,
        command: Some(variant.name.clone()),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        env: BTreeMap::new(),
        cwd: None,
    }
}

/// Warn about Codex variants whose Codex home lacks the herdr integration.
pub fn codex_integration_warnings(variants: &[Variant], home: &str) -> Vec<String> {
    variants
        .iter()
        .filter(|variant| variant.kind == AgentKind::Codex)
        .filter_map(|variant| {
            let codex_home = variant
                .config_home
                .clone()
                .unwrap_or_else(|| PathBuf::from(home).join(".codex"));
            if codex_integration_installed(&codex_home) {
                return None;
            }
            let display = tilde_display(&codex_home, home);
            Some(format!(
                "codex variant {}: herdr codex integration not found in {display}; run: \
                 CODEX_HOME={display} herdr integration install codex",
                variant.name
            ))
        })
        .collect()
}

fn codex_integration_installed(codex_home: &Path) -> bool {
    std::fs::read_to_string(codex_home.join("hooks.json"))
        .is_ok_and(|text| text.contains("herdr-agent-state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, kind: CandidateKind, body: &str) -> Candidate {
        Candidate {
            name: name.to_string(),
            kind,
            body: body.to_string(),
        }
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    // Fixtures are the user's real definitions, verbatim.
    fn fixture_candidates() -> Vec<Candidate> {
        vec![
            candidate(
                "claude",
                CandidateKind::File,
                "",
            ),
            candidate(
                "claude-lm-studio",
                CandidateKind::Alias,
                "alias claude-lm-studio='CLAUDE_CONFIG_DIR=\"$HOME/.claude-lm-studio\" command claude'",
            ),
            candidate(
                "claude-personal",
                CandidateKind::Function,
                r#"claude-personal () {
  local ANTHROPIC_BASE_URL=$ANTHROPIC_BASE_URL;
  export ANTHROPIC_BASE_URL;
  mkdir -p ~/.claude-personal;
  EDITOR="$(command -v ~/tools/helix.AppImage || command -v vim || echo vi)" CLAUDE_CONFIG_DIR="$HOME/.claude-personal" command claude "$@"
}"#,
            ),
            candidate("codex", CandidateKind::File, ""),
            candidate(
                "codex-baseline",
                CandidateKind::Function,
                r#"codex-baseline () {
  command codex "$@"
}"#,
            ),
            candidate(
                "codex-headroom",
                CandidateKind::Function,
                r#"codex-headroom () {
  local -a codex_flags;
  codex-global-flags codex_flags;
  command headroom wrap codex "${codex_flags[@]}" "$@"
}"#,
            ),
            candidate(
                "codex-lm-studio",
                CandidateKind::Function,
                r#"codex-lm-studio () {
  CODEX_HOME="$HOME/.codex-lm-studio" command codex "$@"
}"#,
            ),
            candidate(
                "codex-personal",
                CandidateKind::Function,
                r#"codex-personal () {
  local OPENAI_BASE_URL=$OPENAI_BASE_URL;
  local -a codex_flags;
  codex-global-flags codex_flags;
  export OPENAI_BASE_URL;
  mkdir -p ~/.codex-personal;
  EDITOR="$(command -v ~/tools/helix.AppImage || command -v vim || echo vi)" CODEX_HOME="$HOME/.codex-personal" command codex "${codex_flags[@]}" "$@"
}"#,
            ),
            candidate(
                "codex-pool",
                CandidateKind::Function,
                r#"codex-pool () {
  CODEX_HOME="$HOME/.codex-pool" command codex "$@"
}"#,
            ),
            candidate("claude-desktop", CandidateKind::File, ""),
            candidate(
                "codex-global-flags",
                CandidateKind::Function,
                r#"codex-global-flags () {
  local -n codex_flags_ref="$1";
  codex_flags_ref=(
    --disable shell_snapshot
    -c 'sandbox_mode="danger-full-access"'
  );
}"#,
            ),
        ]
    }

    #[test]
    fn parse_candidates_skips_noise_and_dedupes_by_kind() {
        let output = "\
Last login on the console
some banner text
@@herdr-openai-gateway@@ claude file
@@herdr-openai-gateway@@ claude function
claude () {
  command claude \"$@\"
}
@@herdr-openai-gateway@@ codex alias
alias codex='command codex --profile x'
";
        let candidates = parse_candidates(output);
        assert_eq!(candidates.len(), 2);
        let claude = candidates
            .iter()
            .find(|c| c.name == "claude")
            .expect("claude");
        assert_eq!(
            claude.kind,
            CandidateKind::Function,
            "function wins over file"
        );
        assert!(claude.body.contains("command claude"));
        let codex = candidates
            .iter()
            .find(|c| c.name == "codex")
            .expect("codex");
        assert_eq!(codex.kind, CandidateKind::Alias);
        assert_eq!(codex.body, "alias codex='command codex --profile x'");
    }

    #[test]
    fn classify_accepts_the_nine_variants_and_rejects_the_two() {
        let variants: Vec<Variant> = fixture_candidates()
            .iter()
            .filter_map(|c| classify(c, &[], &[], "/home/u"))
            .collect();
        let names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            strings(&[
                "claude",
                "claude-lm-studio",
                "claude-personal",
                "codex",
                "codex-baseline",
                "codex-headroom",
                "codex-lm-studio",
                "codex-personal",
                "codex-pool",
            ])
        );
        let personal = variants
            .iter()
            .find(|v| v.name == "claude-personal")
            .expect("variant");
        assert_eq!(
            personal.config_home,
            Some(PathBuf::from("/home/u/.claude-personal"))
        );
        let headroom = variants
            .iter()
            .find(|v| v.name == "codex-headroom")
            .expect("variant");
        assert_eq!(headroom.kind, AgentKind::Codex);
        assert_eq!(headroom.config_home, None);
    }

    #[test]
    fn classify_honours_include_and_exclude() {
        let desktop = candidate("claude-desktop", CandidateKind::File, "");
        assert!(classify(&desktop, &[], &[], "/home/u").is_none());
        let forced = classify(&desktop, &strings(&["claude-desktop"]), &[], "/home/u")
            .expect("include forces it in");
        assert_eq!(forced.kind, AgentKind::Claude);
        assert_eq!(forced.config_home, None);

        let candidates = fixture_candidates();
        let excluded = strings(&["codex-personal"]);
        let names: Vec<String> = candidates
            .iter()
            .filter_map(|c| classify(c, &[], &excluded, "/home/u"))
            .map(|v| v.name)
            .collect();
        assert!(!names.contains(&"codex-personal".to_string()));
        assert_eq!(names.len(), 8);
    }

    #[test]
    fn config_home_from_body_expands_the_usual_forms() {
        let body = |value: &str| format!("EDITOR=vi {value} command claude \"$@\"");
        assert_eq!(
            config_home_from_body(
                &body("CLAUDE_CONFIG_DIR=\"$HOME/.x\""),
                AgentKind::Claude,
                "/home/u"
            ),
            Some(PathBuf::from("/home/u/.x"))
        );
        assert_eq!(
            config_home_from_body(
                &body("CLAUDE_CONFIG_DIR=${HOME}/.x"),
                AgentKind::Claude,
                "/home/u"
            ),
            Some(PathBuf::from("/home/u/.x"))
        );
        assert_eq!(
            config_home_from_body(
                &body("CLAUDE_CONFIG_DIR=~/.x"),
                AgentKind::Claude,
                "/home/u"
            ),
            Some(PathBuf::from("/home/u/.x"))
        );
        assert_eq!(
            config_home_from_body(
                &body("CLAUDE_CONFIG_DIR=\"/plain\""),
                AgentKind::Claude,
                "/home/u"
            ),
            Some(PathBuf::from("/plain"))
        );
        assert_eq!(
            config_home_from_body(
                &body("CLAUDE_CONFIG_DIR=\"$SOME_VAR/.x\""),
                AgentKind::Claude,
                "/home/u"
            ),
            None,
            "unexpanded variables must not become a home"
        );
        assert_eq!(
            config_home_from_body("command claude \"$@\"", AgentKind::Claude, "/home/u"),
            None
        );
        assert_eq!(
            config_home_from_body(
                "CODEX_HOME=\"$HOME/.cx\" command codex",
                AgentKind::Codex,
                "/home/u"
            ),
            Some(PathBuf::from("/home/u/.cx"))
        );
        // A variable that only contains the name must not match.
        assert_eq!(
            config_home_from_body(
                "MY_CLAUDE_CONFIG_DIR=\"$HOME/.x\" command claude",
                AgentKind::Claude,
                "/home/u"
            ),
            None
        );
    }

    #[test]
    fn variant_models_orders_and_bridges() {
        let variants: Vec<Variant> = fixture_candidates()
            .iter()
            .filter_map(|c| classify(c, &[], &[], "/home/u"))
            .collect();
        let models = variant_models(&variants);
        assert_eq!(models.len(), 18);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            strings(&[
                "claude+agent-tools",
                "claude+client-tools",
                "codex+agent-tools",
                "codex+client-tools",
                "claude-lm-studio+agent-tools",
                "claude-lm-studio+client-tools",
                "claude-personal+agent-tools",
                "claude-personal+client-tools",
                "codex-baseline+agent-tools",
                "codex-baseline+client-tools",
                "codex-headroom+agent-tools",
                "codex-headroom+client-tools",
                "codex-lm-studio+agent-tools",
                "codex-lm-studio+client-tools",
                "codex-personal+agent-tools",
                "codex-personal+client-tools",
                "codex-pool+agent-tools",
                "codex-pool+client-tools",
            ])
        );
        for model in &models {
            let base = model
                .id
                .trim_end_matches(crate::config::AGENT_TOOLS_SUFFIX)
                .trim_end_matches(crate::config::CLIENT_TOOLS_SUFFIX);
            assert_eq!(model.command.as_deref(), Some(base));
        }
        let bridge = models
            .iter()
            .find(|m| m.id == "claude-personal+client-tools")
            .expect("model");
        assert_eq!(
            bridge.args,
            ["--tools", "", "--strict-mcp-config"].map(String::from)
        );
        let codex_bridge = models
            .iter()
            .find(|m| m.id == "codex-pool+client-tools")
            .expect("model");
        assert_eq!(
            codex_bridge.args,
            ["-s", "read-only", "-a", "never"].map(String::from)
        );
    }

    #[test]
    fn codex_integration_warnings_point_at_the_missing_home() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-disc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let variants = vec![
            Variant {
                name: "codex".to_string(),
                kind: AgentKind::Codex,
                config_home: None,
            },
            Variant {
                name: "codex-personal".to_string(),
                kind: AgentKind::Codex,
                config_home: Some(dir.clone()),
            },
        ];
        // Nothing installed anywhere: both variants warn, homes print with `~`.
        let warnings = codex_integration_warnings(&variants, "/home/u");
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            warnings[0],
            "codex variant codex: herdr codex integration not found in ~/.codex; run: \
             CODEX_HOME=~/.codex herdr integration install codex"
        );
        assert!(
            warnings[1].contains("codex variant codex-personal"),
            "got {warnings:?}"
        );

        std::fs::write(
            dir.join("hooks.json"),
            r#"{"hooks":{"Stop":[{"command":"herdr-agent-state"}]}}"#,
        )
        .expect("write hooks");
        let warnings = codex_integration_warnings(&variants, "/home/u");
        assert_eq!(warnings.len(), 1, "installed home no longer warns");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn discover_warns_on_unsupported_or_missing_shell() {
        let cfg = DiscoveryConfig {
            shell: Some("/bin/fish".to_string()),
            ..DiscoveryConfig::default()
        };
        let discovery = discover(&cfg, |_| None).await;
        assert!(discovery.variants.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert!(discovery.warnings[0].contains("unsupported shell"));

        let cfg = DiscoveryConfig::default();
        let discovery = discover(&cfg, |_| None).await;
        assert!(discovery.warnings[0].contains("$SHELL"));

        let cfg = DiscoveryConfig {
            enabled: false,
            ..DiscoveryConfig::default()
        };
        let discovery = discover(&cfg, |_| None).await;
        assert!(discovery.warnings.is_empty());
    }

    #[tokio::test]
    async fn discover_runs_a_real_bash_with_a_temp_home() {
        let home = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-disc-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        std::fs::write(
            home.join(".bashrc"),
            "claude-x() { command claude \"$@\"; }\ncodex-helper() { :; }\n",
        )
        .expect("write bashrc");
        let home_str = home.display().to_string();
        let cfg = DiscoveryConfig {
            shell: Some("/bin/bash".to_string()),
            ..DiscoveryConfig::default()
        };
        let discovery = discover(&cfg, |key| match key {
            "HOME" => Some(home_str.clone()),
            _ => None,
        })
        .await;
        assert!(
            discovery.warnings.is_empty(),
            "got {:?}",
            discovery.warnings
        );
        let names: Vec<&str> = discovery.variants.iter().map(|v| v.name.as_str()).collect();
        assert!(names.contains(&"claude-x"), "got {names:?}");
        assert!(!names.contains(&"codex-helper"), "got {names:?}");
        std::fs::remove_dir_all(&home).expect("cleanup");
    }
}
