use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use herdr_openai_server::app::{self, ServeArgs};
use herdr_openai_server::herdr::{self, HerdrApi as _, SocketFlags};

#[derive(Parser)]
#[command(
    name = "herdr-openai-server",
    about = "OpenAI-compatible API backed by agent CLIs running inside herdr panes",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the OpenAI-compatible API
    Serve {
        /// Path to config.toml (defaults to $XDG_CONFIG_HOME/herdr-openai-server/config.toml)
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Listen address, overrides server.listen from the config
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
        /// Herdr session name; resolves to <config>/herdr/sessions/<name>/herdr.sock
        #[arg(long, value_name = "NAME", conflicts_with = "herdr_socket")]
        herdr_session: Option<String>,
        /// Explicit path to the herdr API socket
        #[arg(long, value_name = "PATH", conflicts_with = "herdr_session")]
        herdr_socket: Option<PathBuf>,
        /// Log request headers (Authorization redacted) and body shapes
        #[arg(long)]
        log_requests: bool,
    },
    /// Ping the herdr server and report connectivity
    Check {
        /// Herdr session name; resolves to <config>/herdr/sessions/<name>/herdr.sock
        #[arg(long, value_name = "NAME", conflicts_with = "herdr_socket")]
        herdr_session: Option<String>,
        /// Explicit path to the herdr API socket
        #[arg(long, value_name = "PATH", conflicts_with = "herdr_session")]
        herdr_socket: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Command::Serve {
            config,
            listen,
            herdr_session,
            herdr_socket,
            log_requests,
        } => {
            let args = ServeArgs {
                config,
                listen,
                herdr_session,
                herdr_socket,
                log_requests,
            };
            app::serve(args).await
        }
        Command::Check {
            herdr_session,
            herdr_socket,
        } => {
            let config = herdr_openai_server::config::Config::load(None).unwrap_or_else(|err| {
                println!("config: {err}; using built-in defaults");
                herdr_openai_server::config::Config::builtin()
            });
            let flags = SocketFlags {
                socket: herdr_socket,
                session: herdr_session,
            };
            let socket_path = herdr::resolve_socket_path(&flags, |key| std::env::var(key).ok());
            println!("socket: {}", socket_path.display());
            let client = herdr::SocketHerdr::new(socket_path);
            let ping_ok = match client.ping().await {
                Ok(pong) => {
                    println!("herdr {} protocol {}: OK", pong.version, pong.protocol);
                    true
                }
                Err(err) => {
                    println!("FAIL: {err}");
                    false
                }
            };
            let discovery = herdr_openai_server::discovery::discover(&config.discovery, |key| {
                std::env::var(key).ok()
            })
            .await;
            for model in config.effective_models(&discovery.variants) {
                println!(
                    "model: {} {} {}",
                    model.id,
                    model.kind.herdr_label(),
                    model.command.as_deref().unwrap_or(model.kind.herdr_label())
                );
            }
            let home = std::env::var("HOME").unwrap_or_default();
            for warning in discovery.warnings.iter().chain(
                herdr_openai_server::discovery::codex_integration_warnings(
                    &discovery.variants,
                    &home,
                )
                .iter(),
            ) {
                println!("warning: {warning}");
            }
            if ping_ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
