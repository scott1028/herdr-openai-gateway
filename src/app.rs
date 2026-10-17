//! Production wiring: config -> SocketHerdr -> Service -> axum, reaper, signals.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use uuid::Uuid;

use crate::config::Config;
use crate::herdr::{self, HerdrApi as _, SocketFlags, SocketHerdr};
use crate::service::{QuestionHookEnv, Service, ServiceEnv};
use crate::transcript::claude;

/// Idle conversations are reaped on this cadence.
const REAPER_INTERVAL: Duration = Duration::from_secs(30);
/// Tracked OpenCode clients are checked on this cadence.
const CLIENT_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// CLI flags for `serve`; CLI values win over config, config over the environment.
#[derive(Debug, Default)]
pub struct ServeArgs {
    pub config: Option<PathBuf>,
    pub listen: Option<String>,
    pub herdr_session: Option<String>,
    pub herdr_socket: Option<PathBuf>,
    pub log_requests: bool,
}

pub async fn serve(args: ServeArgs) -> ExitCode {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut config = match Config::load(args.config.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let listen = args.listen.unwrap_or_else(|| config.server.listen.clone());
    let addr: SocketAddr = match listen.parse() {
        Ok(addr) => addr,
        Err(_) => {
            tracing::error!("invalid listen address: {listen}");
            return ExitCode::FAILURE;
        }
    };
    let api_key = std::env::var(&config.server.api_key_env)
        .ok()
        .filter(|value| !value.is_empty());
    if let Err(err) = check_listen_policy(
        addr,
        config.server.allow_remote,
        api_key.is_some(),
        &config.server.api_key_env,
    ) {
        tracing::error!("{err}");
        return ExitCode::FAILURE;
    }
    let flags = SocketFlags {
        socket: args.herdr_socket.or_else(|| config.herdr.socket.clone()),
        session: args.herdr_session.or_else(|| config.herdr.session.clone()),
    };
    let socket_path = herdr::resolve_socket_path(&flags, |key| std::env::var(key).ok());
    let herdr = Arc::new(SocketHerdr::new(socket_path.clone()));
    match herdr.ping().await {
        Ok(pong) => tracing::info!(
            version = %pong.version,
            protocol = pong.protocol,
            socket = %socket_path.display(),
            "connected to herdr"
        ),
        // Without herdr there is nothing to serve: fail at startup with a message
        // that names the socket instead of running degraded.
        Err(err) => {
            tracing::error!("{err}");
            return ExitCode::FAILURE;
        }
    }
    let discovery =
        crate::discovery::discover(&config.discovery, |key| std::env::var(key).ok()).await;
    for warning in &discovery.warnings {
        tracing::warn!("{warning}");
    }
    for warning in crate::discovery::codex_integration_warnings(&discovery.variants, &home) {
        tracing::warn!("{warning}");
    }
    config.models = config.effective_models(&discovery.variants);
    for model in &config.models {
        tracing::info!(
            id = %model.id,
            kind = model.kind.herdr_label(),
            command = model.command.as_deref().unwrap_or(model.kind.herdr_label()),
            "offering model"
        );
    }
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!("cannot listen on {addr}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let listen_addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(err) => {
            tracing::error!("cannot read listener address: {err}");
            return ExitCode::FAILURE;
        }
    };
    let variant_config_homes: BTreeMap<String, PathBuf> = discovery
        .variants
        .iter()
        .filter_map(|variant| {
            let dir = variant.config_home.as_ref()?;
            Some([
                (
                    format!("{}{}", variant.name, crate::config::AGENT_TOOLS_SUFFIX),
                    dir.clone(),
                ),
                (
                    format!("{}{}", variant.name, crate::config::CLIENT_TOOLS_SUFFIX),
                    dir.clone(),
                ),
            ])
        })
        .flatten()
        .collect();
    let question_hook = QuestionHookEnv {
        url: question_hook_url(listen_addr),
        token: Uuid::new_v4().to_string(),
    };
    let env = ServiceEnv {
        home,
        transcript_roots: claude::claude_transcript_roots(&BTreeMap::new(), |key| {
            std::env::var(key).ok()
        }),
        codex_sessions_root: None,
        variant_config_homes,
        api_key,
        log_requests: args.log_requests,
        question_hook: Some(question_hook),
        proc_root: cfg!(target_os = "linux").then(|| PathBuf::from("/proc")),
        server_addr: Some(listen_addr),
    };
    let service = Service::new(config, herdr, env);
    service.startup_sweep().await;
    let reaper = tokio::spawn({
        let service = Arc::clone(&service);
        async move {
            loop {
                tokio::time::sleep(REAPER_INTERVAL).await;
                service.reap_expired().await;
                service.sweep_untagged_tabs().await;
            }
        }
    });
    let client_checker = tokio::spawn({
        let service = Arc::clone(&service);
        async move {
            loop {
                tokio::time::sleep(CLIENT_CHECK_INTERVAL).await;
                service.close_exited_clients().await;
            }
        }
    });
    tracing::info!("herdr-openai-gateway listening on http://{listen_addr}");
    let shutdown_service = Arc::clone(&service);
    let result = axum::serve(
        listener,
        crate::server::router(Arc::clone(&service))
            .into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        shutdown_service.cancel_all_hook_questions();
    })
    .await;
    reaper.abort();
    client_checker.abort();
    let failed = match &result {
        Ok(()) => false,
        Err(err) => {
            tracing::error!("server error: {err}");
            true
        }
    };
    service.shutdown().await;
    if failed {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Self-exec agents run local code: `allow_remote` needs a key even on loopback
/// (DNS rebinding), and a non-loopback bind needs both opt-in and a key.
fn check_listen_policy(
    addr: SocketAddr,
    allow_remote: bool,
    has_api_key: bool,
    api_key_env: &str,
) -> Result<(), String> {
    if allow_remote && !has_api_key {
        return Err(format!(
            "refusing server.allow_remote = true without an API key: put a key in \
             {api_key_env} first"
        ));
    }
    if !(addr.ip().is_loopback() || allow_remote) {
        return Err(format!(
            "refusing non-loopback listen {addr}: set server.allow_remote = true and put \
             a key in {api_key_env} first"
        ));
    }
    Ok(())
}

fn question_hook_url(addr: SocketAddr) -> String {
    let ip = if addr.ip().is_unspecified() {
        if addr.is_ipv4() {
            "127.0.0.1".parse().expect("IPv4 loopback")
        } else {
            "::1".parse().expect("IPv6 loopback")
        }
    } else {
        addr.ip()
    };
    format!(
        "http://{ip}/internal/claude-hooks/pre-tool-use",
        ip = SocketAddr::new(ip, addr.port())
    )
}

/// Resolves on Ctrl-C or SIGTERM so in-flight turns get their grace window.
async fn shutdown_signal() {
    let ctrl_c = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!("cannot install Ctrl-C handler: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                tracing::warn!("cannot install SIGTERM handler: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_url_uses_loopback_for_unspecified_listeners() {
        assert_eq!(
            question_hook_url("0.0.0.0:8787".parse::<SocketAddr>().unwrap()),
            "http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use"
        );
        assert_eq!(
            question_hook_url("[::]:8787".parse::<SocketAddr>().unwrap()),
            "http://[::1]:8787/internal/claude-hooks/pre-tool-use"
        );
    }

    #[test]
    fn listen_policy_requires_a_key_for_allow_remote() {
        let loopback: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        let remote: SocketAddr = "0.0.0.0:8787".parse().unwrap();
        let env = "MY_KEY_ENV";

        assert!(check_listen_policy(loopback, false, false, env).is_ok());
        assert!(check_listen_policy(loopback, false, true, env).is_ok());
        assert!(check_listen_policy(loopback, true, true, env).is_ok());
        assert!(check_listen_policy(remote, true, true, env).is_ok());

        // allow_remote without a key is refused even on loopback.
        let err = check_listen_policy(loopback, true, false, env).unwrap_err();
        assert!(err.contains(env), "{err}");
        let err = check_listen_policy(remote, true, false, env).unwrap_err();
        assert!(err.contains(env), "{err}");

        // Non-loopback without opt-in is refused, key or not.
        let err = check_listen_policy(remote, false, true, env).unwrap_err();
        assert!(err.contains(env), "{err}");
        assert!(check_listen_policy(remote, false, false, env).is_err());
    }

    #[tokio::test]
    async fn serve_exits_when_herdr_is_unreachable_at_startup() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-serve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, "").expect("write empty config");

        let args = ServeArgs {
            config: Some(config_path),
            listen: Some("127.0.0.1:0".to_string()),
            herdr_socket: Some(dir.join("missing-herdr.sock")),
            ..ServeArgs::default()
        };
        let code = tokio::time::timeout(Duration::from_secs(5), serve(args))
            .await
            .expect("serve must return promptly when herdr is unreachable");
        assert_eq!(code, ExitCode::FAILURE);
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }
}
