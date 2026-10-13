use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

use super::{
    decode_response, decode_result, parse_pong, AgentInfo, AgentInfoResult, AgentStart,
    AgentStarted, HerdrApi, HerdrError, PaneReadResult, Pong, ReadSource, RequestEnvelope,
    TabCreate, TabCreated, TabInfo, TabInfoResult, TabListResult, WaitSpec, WorkspaceCreated,
    WorkspaceInfo, WorkspaceListResult,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Client-side guard added on top of the server-enforced `timeout_ms` for wait calls.
const WAIT_GUARD_MS: u64 = 30_000;

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// Talks to the herdr server over its NDJSON API socket: one request per connection,
/// mirroring herdr's own client in `src/api/client.rs`.
pub struct SocketHerdr {
    socket_path: PathBuf,
}

impl SocketHerdr {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, HerdrError> {
        self.request_guarded(method, params, Some(REQUEST_TIMEOUT))
            .await
    }

    /// Wait calls are bounded by the server's `timeout_ms`; the client only adds a generous
    /// guard so a hung socket cannot outlive the server-side wait. No guard without `timeout_ms`.
    async fn request_wait(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout_ms: Option<u64>,
    ) -> Result<serde_json::Value, HerdrError> {
        let guard = timeout_ms.map(|ms| Duration::from_millis(ms + WAIT_GUARD_MS));
        self.request_guarded(method, params, guard).await
    }

    async fn request_guarded(
        &self,
        method: &str,
        params: serde_json::Value,
        guard: Option<Duration>,
    ) -> Result<serde_json::Value, HerdrError> {
        let id = format!(
            "herdr-openai-server-{}",
            REQUEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let envelope = RequestEnvelope {
            id: &id,
            method,
            params: &params,
        };
        let line = serde_json::to_string(&envelope)
            .map_err(|err| HerdrError::Protocol(format!("failed to encode request: {err}")))?;
        match guard {
            Some(duration) => match tokio::time::timeout(duration, self.exchange(&line)).await {
                Ok(result) => result,
                Err(_) => Err(HerdrError::Timeout),
            },
            None => self.exchange(&line).await,
        }
    }

    /// Attach the socket this client tried so the message says where to look.
    fn unavailable(&self, source: io::Error) -> HerdrError {
        HerdrError::Unavailable {
            socket: self.socket_path.clone(),
            source,
        }
    }

    async fn exchange(&self, line: &str) -> Result<serde_json::Value, HerdrError> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|err| self.unavailable(err))?;
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|err| self.unavailable(err))?;
        writer
            .write_all(b"\n")
            .await
            .map_err(|err| self.unavailable(err))?;
        writer.flush().await.map_err(|err| self.unavailable(err))?;

        let mut response = String::new();
        let read = BufReader::new(reader)
            .read_line(&mut response)
            .await
            .map_err(|err| self.unavailable(err))?;
        if read == 0 || response.trim().is_empty() {
            return Err(HerdrError::Protocol(
                "server closed without a response".into(),
            ));
        }
        decode_response(&response)
    }
}

impl HerdrApi for SocketHerdr {
    async fn ping(&self) -> Result<Pong, HerdrError> {
        let result = self.request("ping", serde_json::json!({})).await?;
        parse_pong(result)
    }

    async fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, HerdrError> {
        let result = self
            .request("workspace.list", serde_json::json!({}))
            .await?;
        Ok(decode_result::<WorkspaceListResult>(result, "workspace_list")?.workspaces)
    }

    async fn workspace_create(
        &self,
        label: &str,
        cwd: &Path,
        focus: bool,
    ) -> Result<WorkspaceCreated, HerdrError> {
        let params = serde_json::json!({
            "label": label,
            "cwd": cwd.display().to_string(),
            "focus": focus,
        });
        let result = self.request("workspace.create", params).await?;
        decode_result(result, "workspace_created")
    }

    async fn tab_list(&self, workspace_id: &str) -> Result<Vec<TabInfo>, HerdrError> {
        let params = serde_json::json!({"workspace_id": workspace_id});
        let result = self.request("tab.list", params).await?;
        Ok(decode_result::<TabListResult>(result, "tab_list")?.tabs)
    }

    async fn tab_create(&self, params: TabCreate<'_>) -> Result<TabCreated, HerdrError> {
        let mut body = serde_json::Map::new();
        body.insert(
            "workspace_id".to_string(),
            serde_json::Value::String(params.workspace_id.to_string()),
        );
        body.insert("focus".to_string(), serde_json::Value::Bool(params.focus));
        if let Some(cwd) = params.cwd {
            body.insert(
                "cwd".to_string(),
                serde_json::Value::String(cwd.display().to_string()),
            );
        }
        if let Some(label) = params.label {
            body.insert(
                "label".to_string(),
                serde_json::Value::String(label.to_string()),
            );
        }
        if !params.env.is_empty() {
            body.insert("env".to_string(), serde_json::json!(params.env));
        }
        let result = self
            .request("tab.create", serde_json::Value::Object(body))
            .await?;
        decode_result(result, "tab_created")
    }

    async fn tab_rename(&self, tab_id: &str, label: &str) -> Result<TabInfo, HerdrError> {
        let params = serde_json::json!({"tab_id": tab_id, "label": label});
        let result = self.request("tab.rename", params).await?;
        Ok(decode_result::<TabInfoResult>(result, "tab_info")?.tab)
    }

    async fn tab_close(&self, tab_id: &str) -> Result<(), HerdrError> {
        let params = serde_json::json!({"tab_id": tab_id});
        let result = self.request("tab.close", params).await?;
        decode_result::<serde_json::Value>(result, "ok")?;
        Ok(())
    }

    async fn agent_get(&self, target: &str) -> Result<AgentInfo, HerdrError> {
        let params = serde_json::json!({"target": target});
        let result = self.request("agent.get", params).await?;
        Ok(decode_result::<AgentInfoResult>(result, "agent_info")?.agent)
    }

    async fn pane_send_input(
        &self,
        pane_id: &str,
        text: &str,
        keys: &[&str],
    ) -> Result<(), HerdrError> {
        let params = serde_json::json!({"pane_id": pane_id, "text": text, "keys": keys});
        let result = self.request("pane.send_input", params).await?;
        decode_result::<serde_json::Value>(result, "ok")?;
        Ok(())
    }

    async fn agent_rename(&self, target: &str, name: &str) -> Result<AgentInfo, HerdrError> {
        let params = serde_json::json!({"target": target, "name": name});
        let result = self.request("agent.rename", params).await?;
        Ok(decode_result::<AgentInfoResult>(result, "agent_info")?.agent)
    }

    async fn agent_start(&self, params: AgentStart<'_>) -> Result<AgentStarted, HerdrError> {
        let body = serde_json::json!({
            "name": params.name,
            "kind": params.kind,
            "pane_id": params.pane_id,
            "args": params.args,
            "timeout_ms": params.timeout_ms,
        });
        let result = self.request("agent.start", body).await?;
        decode_result(result, "agent_started")
    }

    async fn agent_prompt(
        &self,
        target: &str,
        text: &str,
        wait: Option<WaitSpec>,
    ) -> Result<AgentInfo, HerdrError> {
        let timeout_ms = wait.as_ref().and_then(|spec| spec.timeout_ms);
        let mut body = serde_json::json!({"target": target, "text": text});
        if let Some(spec) = &wait {
            body["wait"] = spec.until_param();
        }
        let result = self.request_wait("agent.prompt", body, timeout_ms).await?;
        Ok(decode_result::<AgentInfoResult>(result, "agent_prompted")?.agent)
    }

    async fn agent_wait(&self, target: &str, wait: WaitSpec) -> Result<AgentInfo, HerdrError> {
        let mut body = wait.until_param();
        body["target"] = serde_json::Value::String(target.to_string());
        let result = self
            .request_wait("agent.wait", body, wait.timeout_ms)
            .await?;
        Ok(decode_result::<AgentInfoResult>(result, "agent_info")?.agent)
    }

    async fn agent_send_keys(&self, target: &str, keys: &[&str]) -> Result<(), HerdrError> {
        let params = serde_json::json!({"target": target, "keys": keys});
        let result = self.request("agent.send_keys", params).await?;
        decode_result::<serde_json::Value>(result, "ok")?;
        Ok(())
    }

    async fn agent_read(
        &self,
        target: &str,
        source: ReadSource,
        lines: Option<u32>,
    ) -> Result<String, HerdrError> {
        let params =
            serde_json::json!({"target": target, "source": source.as_str(), "lines": lines});
        let result = self.request("agent.read", params).await?;
        Ok(decode_result::<PaneReadResult>(result, "pane_read")?
            .read
            .text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::AgentStatus;

    #[tokio::test]
    async fn missing_socket_is_unavailable() {
        let herdr = SocketHerdr::new(PathBuf::from("/nonexistent/herdr-test.sock"));
        let err = herdr.ping().await.expect_err("must fail");
        assert!(matches!(err, HerdrError::Unavailable { .. }));
        assert!(
            err.to_string().contains("/nonexistent/herdr-test.sock"),
            "message must name the socket: {err}"
        );
    }

    #[tokio::test]
    async fn wait_call_reports_unavailable_not_timeout() {
        let herdr = SocketHerdr::new(PathBuf::from("/nonexistent/herdr-test.sock"));
        let wait = WaitSpec {
            until: vec![AgentStatus::Idle],
            timeout_ms: Some(1),
        };
        let err = herdr.agent_wait("nope", wait).await.expect_err("must fail");
        assert!(matches!(err, HerdrError::Unavailable { .. }));
    }
}
