//! OpenAI-compatible API backed by agent CLIs running inside herdr panes.

pub mod agents;
#[cfg(unix)]
pub mod app;
/// Linux `/proc` lookup for connected client processes.
pub mod client_process;
pub mod config;
pub mod conversation;
pub mod dialog;
pub mod discovery;
pub mod herdr;
pub mod openai;
pub mod prompt;
/// Claude PreToolUse question hook payload conversion.
pub mod question_hook;
pub mod server;
pub mod service;
pub mod toolcall;
pub mod transcript;
