use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use crate::dialog::Dialog;
use crate::openai::{AssistantReply, NormalizedMessage, NormalizedRequest};
use crate::question_hook::HookAnswer;

/// A cached reply stays retryable for this long.
const RETRY_WINDOW: Duration = Duration::from_secs(600);
/// LRU eviction only considers conversations idle for at least this long.
const EVICT_MIN_IDLE: Duration = Duration::from_secs(60);

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Normalize text for digesting: CRLF/CR to LF, trim trailing whitespace per line, trim outer.
pub fn normalize_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.lines().map(|line| line.trim_end()).collect();
    lines.join("\n").trim().to_string()
}

/// Digests of the User messages, in order. Tool outputs never participate.
pub fn user_digests(req: &NormalizedRequest) -> Vec<String> {
    req.messages
        .iter()
        .filter_map(|message| match message {
            NormalizedMessage::User(text) => Some(sha256_hex(&normalize_text(text))),
            _ => None,
        })
        .collect()
}

/// Stable digest of everything that defines the request work, excluding stream options.
pub fn request_digest(req: &NormalizedRequest) -> String {
    let messages: Vec<serde_json::Value> = req.messages.iter().map(message_json).collect();
    let value = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "tools": req.tools,
        "tool_choice": req.tool_choice,
    });
    sha256_hex(&serde_json::to_string(&value).unwrap_or_default())
}

fn message_json(message: &NormalizedMessage) -> serde_json::Value {
    match message {
        NormalizedMessage::System(text) => {
            serde_json::json!({"role": "system", "text": normalize_text(text)})
        }
        NormalizedMessage::User(text) => {
            serde_json::json!({"role": "user", "text": normalize_text(text)})
        }
        NormalizedMessage::Assistant { text, tool_calls } => serde_json::json!({
            "role": "assistant",
            "text": text.as_ref().map(|text| normalize_text(text)),
            "tool_calls": tool_calls,
        }),
        NormalizedMessage::Tool {
            call_id,
            name,
            content,
        } => serde_json::json!({
            "role": "tool",
            "call_id": call_id,
            "name": name,
            "content": normalize_text(content),
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConversationId(u64);

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "c{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct ConversationRecord {
    pub id: ConversationId,
    pub model_id: String,
    pub key: Option<String>,
    pub cwd: PathBuf,
    pub user_digests: Vec<String>,
    /// Non-system messages accounted for, including our last reply.
    pub consumed: usize,
    pub last_request: Option<String>,
    pub last_reply: Option<AssistantReply>,
    pub last_reply_at: Option<Instant>,
    pub inflight: Option<String>,
    /// User digests of the in-flight request, applied as abort semantics when it
    /// is superseded so its user message is never re-sent.
    pub inflight_users: Vec<String>,
    /// Non-system message count of the in-flight request.
    pub inflight_count: usize,
    /// Bumped whenever an in-flight turn is superseded; a turn task whose
    /// generation is older must not commit or fail the record.
    pub generation: u64,
    /// Number of dialog relays published for this conversation. Only ever
    /// increases: `answer_dialog` takes the pending entry out, but the next
    /// relay must still carry a higher seq for the answering request to see it.
    pub dialog_seq: u64,
    /// A dialog relayed to the client while the turn is in flight; the client's
    /// next answer-shaped request is matched against it.
    pub pending_dialog: Option<PendingDialog>,
    pub created: Instant,
    pub last_used: Instant,
}

/// A dialog the backing agent is blocked on, already relayed to the client.
#[derive(Debug, Clone)]
pub struct PendingDialog {
    pub dialog: Dialog,
    /// The relay as the client saw it (numbered text or `question` tool call).
    pub reply: AssistantReply,
    /// Tool call id when the relay used the `question` tool, else None.
    pub call_id: Option<String>,
    /// Monotonic per-conversation sequence, starting at 1; joiners wait for a
    /// dialog with a seq greater than the one they already received.
    pub seq: u64,
    /// User digests of the client history the relay was sent for.
    pub relayed_users: Vec<String>,
    /// Non-system message count of that history, including the relay itself.
    pub relayed_count: usize,
    /// True when the turn gave up waiting; a late answer gets an explanatory
    /// reply instead of keys.
    pub expired: bool,
    /// Original hook input and answer channel for a structured question relay.
    pub hook: Option<Arc<Mutex<Option<HookQuestion>>>>,
}

/// Original Claude question input and the waiting hook response channel.
#[derive(Debug)]
pub struct HookQuestion {
    /// Question input to preserve in Claude's updated tool input.
    pub tool_input: serde_json::Value,
    /// Receives the client's answer or dismissal.
    pub answer_tx: tokio::sync::oneshot::Sender<HookAnswer>,
}

/// The record's in-flight snapshot as it was before `answer_dialog` replaced
/// it; a failed answer apply puts it back together with the pending entry.
#[derive(Debug, Clone, Default)]
pub struct InflightSnapshot {
    pub request: Option<String>,
    pub users: Vec<String>,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Classification {
    /// bootstrap = request already carries an assistant message or more than one user message.
    New {
        bootstrap: bool,
    },
    Retry {
        id: ConversationId,
        reply: AssistantReply,
    },
    JoinInflight(ConversationId),
    /// The request answers a dialog relayed from this conversation's turn.
    DialogAnswer(ConversationId),
    Continue {
        id: ConversationId,
        delta: Vec<NormalizedMessage>,
    },
    Busy(ConversationId),
    Fork(ConversationId),
    SideRequest(ConversationId),
}

#[derive(Debug, Default)]
pub struct Registry {
    records: BTreeMap<ConversationId, ConversationRecord>,
    next_id: u64,
}

fn is_system(message: &NormalizedMessage) -> bool {
    matches!(message, NormalizedMessage::System(_))
}

fn is_assistant(message: &NormalizedMessage) -> bool {
    matches!(message, NormalizedMessage::Assistant { .. })
}

pub(crate) fn non_system_count(req: &NormalizedRequest) -> usize {
    req.messages.iter().filter(|m| !is_system(m)).count()
}

fn has_assistant_message(req: &NormalizedRequest) -> bool {
    req.messages.iter().any(is_assistant)
}

/// bootstrap when the request already carries history we must replay.
pub fn bootstrap_flag(req: &NormalizedRequest) -> bool {
    has_assistant_message(req)
        || req
            .messages
            .iter()
            .filter(|m| matches!(m, NormalizedMessage::User(_)))
            .count()
            > 1
}

impl Registry {
    pub fn classify(&self, req: &NormalizedRequest, now: Instant) -> Classification {
        let digest = request_digest(req);
        let users = user_digests(req);
        let count = non_system_count(req);
        if let Some(key) = &req.conversation_key {
            return match self.find_by_key(&req.model, key) {
                Some(record) => self.relate(record, req, &digest, &users, count, now, true),
                None => Classification::New {
                    bootstrap: bootstrap_flag(req),
                },
            };
        }
        let mut sole_continue = None;
        let mut continue_count = 0usize;
        for record in self
            .records
            .values()
            .filter(|record| record.model_id == req.model && record.key.is_none())
        {
            match self.relate(record, req, &digest, &users, count, now, false) {
                classification @ (Classification::Retry { .. }
                | Classification::JoinInflight(_)) => return classification,
                classification @ Classification::Continue { .. } => {
                    continue_count += 1;
                    sole_continue = Some(classification);
                }
                _ => {}
            }
        }
        if continue_count == 1 {
            if let Some(classification) = sole_continue {
                return classification;
            }
        }
        Classification::New {
            bootstrap: bootstrap_flag(req),
        }
    }

    fn relate(
        &self,
        record: &ConversationRecord,
        req: &NormalizedRequest,
        digest: &str,
        users: &[String],
        count: usize,
        now: Instant,
        keyed: bool,
    ) -> Classification {
        if record.inflight.as_deref() == Some(digest) {
            return Classification::JoinInflight(record.id);
        }
        // An answer-shaped request right after a relayed dialog answers it: the
        // history extends the relayed one by exactly one message, a user text
        // or the tool result for the relayed `question` call.
        if let Some(pending) = &record.pending_dialog {
            let last_matches = match req.messages.last() {
                Some(NormalizedMessage::User(_)) => true,
                Some(NormalizedMessage::Tool { call_id, .. }) => {
                    pending.call_id.as_deref() == Some(call_id.as_str())
                }
                _ => false,
            };
            if is_prefix(&pending.relayed_users, users)
                && count == pending.relayed_count + 1
                && last_matches
            {
                return Classification::DialogAnswer(record.id);
            }
        }
        if record.last_request.as_deref() == Some(digest) {
            let fresh = record
                .last_reply_at
                .is_some_and(|at| now.duration_since(at) <= RETRY_WINDOW);
            if fresh {
                if let Some(reply) = &record.last_reply {
                    return Classification::Retry {
                        id: record.id,
                        reply: reply.clone(),
                    };
                }
            }
        }
        if is_prefix(&record.user_digests, users) && count > record.consumed {
            if record.inflight.is_some() {
                return Classification::Busy(record.id);
            }
            let delta = delta_messages(req, record.consumed);
            if !delta.is_empty() {
                return Classification::Continue {
                    id: record.id,
                    delta,
                };
            }
        }
        if keyed {
            if has_assistant_message(req) {
                return Classification::Fork(record.id);
            }
            return Classification::SideRequest(record.id);
        }
        Classification::New { bootstrap: false }
    }

    pub fn create(
        &mut self,
        model_id: &str,
        key: Option<String>,
        cwd: PathBuf,
        now: Instant,
    ) -> ConversationId {
        let id = ConversationId(self.next_id);
        self.next_id += 1;
        self.records.insert(
            id,
            ConversationRecord {
                id,
                model_id: model_id.to_string(),
                key,
                cwd,
                user_digests: Vec::new(),
                consumed: 0,
                last_request: None,
                last_reply: None,
                last_reply_at: None,
                inflight: None,
                inflight_users: Vec::new(),
                inflight_count: 0,
                generation: 0,
                dialog_seq: 0,
                pending_dialog: None,
                created: now,
                last_used: now,
            },
        );
        id
    }

    /// Mark a request as in-flight and remember what it carried, so a later
    /// supersede can apply abort semantics without the request. Returns the
    /// record's generation, which the turn task must re-check before committing.
    pub fn begin(
        &mut self,
        id: ConversationId,
        request_digest: String,
        req: &NormalizedRequest,
    ) -> u64 {
        match self.records.get_mut(&id) {
            Some(record) => {
                // A new (non-answer) request supersedes an unanswered expired relay.
                if record
                    .pending_dialog
                    .as_ref()
                    .is_some_and(|pending| pending.expired)
                {
                    record.pending_dialog = None;
                }
                record.inflight = Some(request_digest);
                record.inflight_users = user_digests(req);
                record.inflight_count = non_system_count(req);
                record.generation
            }
            None => 0,
        }
    }

    /// Abandon an in-flight turn nobody waits for: apply the aborted request's
    /// digests like `abort` does, clear the marker, and bump the generation so
    /// the old turn's task skips its commit.
    pub fn supersede(&mut self, id: ConversationId, now: Instant) -> bool {
        let Some(record) = self.records.get_mut(&id) else {
            return false;
        };
        if record.inflight.is_none() {
            return false;
        }
        record.inflight = None;
        // The turn is abandoned, so its stale relay must not match later history.
        record.pending_dialog = None;
        record.user_digests = std::mem::take(&mut record.inflight_users);
        record.consumed = record.inflight_count;
        record.inflight_count = 0;
        record.generation += 1;
        record.last_used = now;
        true
    }

    /// Commit the finished turn: account for the in-flight request's snapshot
    /// (which `answer_dialog` may have updated to the answering request) plus
    /// this reply, and clear the marker. A non-expired pending dialog belongs to
    /// the finished turn and is dropped; an expired one survives until a
    /// different request begins.
    pub fn commit(&mut self, id: ConversationId, reply: AssistantReply, now: Instant) {
        if let Some(record) = self.records.get_mut(&id) {
            record.user_digests = std::mem::take(&mut record.inflight_users);
            record.consumed = record.inflight_count + 1;
            record.inflight_count = 0;
            record.last_request = record.inflight.take();
            record.last_reply = Some(reply);
            record.last_reply_at = Some(now);
            if record
                .pending_dialog
                .as_ref()
                .is_some_and(|pending| !pending.expired)
            {
                record.pending_dialog = None;
            }
            record.last_used = now;
        }
    }

    /// Record the digests without a reply so the same user message is never re-sent.
    pub fn abort(&mut self, id: ConversationId, req: &NormalizedRequest, now: Instant) {
        if let Some(record) = self.records.get_mut(&id) {
            record.user_digests = user_digests(req);
            record.consumed = non_system_count(req);
            record.inflight = None;
            record.last_used = now;
        }
    }

    /// Clear only the in-flight marker after a turn error; digests stay, so the
    /// same request classifies as Continue again and gets re-sent. A pending
    /// dialog belongs to the failed turn and is dropped unless it expired.
    pub fn fail(&mut self, id: ConversationId) {
        if let Some(record) = self.records.get_mut(&id) {
            record.inflight = None;
            if record
                .pending_dialog
                .as_ref()
                .is_some_and(|pending| !pending.expired)
            {
                record.pending_dialog = None;
            }
        }
    }

    /// Remember the dialog relayed to the client for this conversation's turn.
    pub fn set_dialog(&mut self, id: ConversationId, pending: PendingDialog) {
        if let Some(record) = self.records.get_mut(&id) {
            record.pending_dialog = Some(pending);
        }
    }

    /// Record a freshly relayed dialog for the turn's generation and return its
    /// seq. The per-conversation counter only ever increases, so a relay after
    /// an answer still carries a seq higher than what the answering request
    /// already saw. `None` when the record is gone or the turn was superseded.
    pub fn publish_dialog(
        &mut self,
        id: ConversationId,
        generation: u64,
        dialog: Dialog,
        reply: AssistantReply,
        call_id: Option<String>,
        hook: Option<Arc<Mutex<Option<HookQuestion>>>>,
    ) -> Option<u64> {
        let record = self.records.get_mut(&id)?;
        if record.generation != generation {
            return None;
        }
        record.dialog_seq += 1;
        let seq = record.dialog_seq;
        record.pending_dialog = Some(PendingDialog {
            dialog,
            reply,
            call_id,
            seq,
            relayed_users: record.inflight_users.clone(),
            relayed_count: record.inflight_count + 1,
            expired: false,
            hook,
        });
        Some(seq)
    }

    /// Mark the relayed dialog as expired: the turn stopped waiting for an answer.
    pub fn expire_dialog(&mut self, id: ConversationId) {
        if let Some(record) = self.records.get_mut(&id) {
            if let Some(pending) = &mut record.pending_dialog {
                pending.expired = true;
            }
        }
    }

    /// Expire a hook dialog only if it still belongs to the supplied hook.
    pub fn expire_hook_question(
        &mut self,
        id: ConversationId,
        hook: &Arc<Mutex<Option<HookQuestion>>>,
        drop_sender: bool,
    ) {
        let Some(pending) = self
            .records
            .get_mut(&id)
            .and_then(|record| record.pending_dialog.as_mut())
        else {
            return;
        };
        let Some(current_hook) = pending.hook.clone() else {
            return;
        };
        if !Arc::ptr_eq(&current_hook, hook) {
            return;
        }
        pending.expired = true;
        if drop_sender {
            current_hook
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
    }

    /// Drop senders for every pending Claude hook question.
    pub fn cancel_pending_hook_questions(&self) {
        for record in self.records.values() {
            let Some(hook) = record
                .pending_dialog
                .as_ref()
                .and_then(|pending| pending.hook.as_ref())
            else {
                continue;
            };
            hook.lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
    }

    /// Take the pending dialog when `req` answers it, and make `req` the
    /// in-flight request so the continuing turn's commit accounts for it.
    /// Returns the entry plus the in-flight snapshot it replaced, so a failed
    /// apply can put both back.
    pub fn answer_dialog(
        &mut self,
        id: ConversationId,
        request_digest: String,
        req: &NormalizedRequest,
    ) -> Option<(PendingDialog, InflightSnapshot)> {
        let record = self.records.get_mut(&id)?;
        let pending = record.pending_dialog.take()?;
        let snapshot = InflightSnapshot {
            request: record.inflight.take(),
            users: std::mem::take(&mut record.inflight_users),
            count: record.inflight_count,
        };
        record.inflight = Some(request_digest);
        record.inflight_users = user_digests(req);
        record.inflight_count = non_system_count(req);
        Some((pending, snapshot))
    }

    /// Put a failed dialog answer back: restore the pending entry AND the
    /// in-flight snapshot `answer_dialog` replaced, so an identical retry of
    /// the answer request classifies as `DialogAnswer` again and gets applied
    /// instead of joining the old request.
    pub fn restore_dialog_answer(
        &mut self,
        id: ConversationId,
        pending: PendingDialog,
        snapshot: InflightSnapshot,
    ) {
        if let Some(record) = self.records.get_mut(&id) {
            record.pending_dialog = Some(pending);
            record.inflight = snapshot.request;
            record.inflight_users = snapshot.users;
            record.inflight_count = snapshot.count;
        }
    }

    pub fn remove(&mut self, id: ConversationId) -> Option<ConversationRecord> {
        self.records.remove(&id)
    }

    pub fn get(&self, id: ConversationId) -> Option<&ConversationRecord> {
        self.records.get(&id)
    }

    pub fn expired(&self, now: Instant, idle_ttl: Duration) -> Vec<ConversationId> {
        self.records
            .values()
            .filter(|record| {
                record.inflight.is_none() && now.duration_since(record.last_used) >= idle_ttl
            })
            .map(|record| record.id)
            .collect()
    }

    /// Least-recently-used conversation with no in-flight turn and idle for at least 60 s.
    pub fn evict_candidate(&self, now: Instant) -> Option<ConversationId> {
        self.records
            .values()
            .filter(|record| {
                record.inflight.is_none() && now.duration_since(record.last_used) >= EVICT_MIN_IDLE
            })
            .min_by_key(|record| record.last_used)
            .map(|record| record.id)
    }

    pub fn count_for_model(&self, model_id: &str) -> usize {
        self.records
            .values()
            .filter(|record| record.model_id == model_id)
            .count()
    }

    pub fn count_total(&self) -> usize {
        self.records.len()
    }

    fn find_by_key(&self, model_id: &str, key: &str) -> Option<&ConversationRecord> {
        self.records
            .values()
            .find(|record| record.model_id == model_id && record.key.as_deref() == Some(key))
    }
}

/// `stored` is a (non-strict) prefix of `current`.
fn is_prefix(stored: &[String], current: &[String]) -> bool {
    stored.len() <= current.len() && stored.iter().zip(current).all(|(a, b)| a == b)
}

/// New messages after the conversation's consumed point, assistant messages dropped.
fn delta_messages(req: &NormalizedRequest, consumed: usize) -> Vec<NormalizedMessage> {
    let non_system: Vec<&NormalizedMessage> =
        req.messages.iter().filter(|m| !is_system(m)).collect();
    let consumed_is_reply = consumed > 0
        && non_system
            .get(consumed - 1)
            .is_some_and(|m| is_assistant(m));
    let start = if consumed_is_reply {
        consumed
    } else {
        // Never re-send consumed messages; also never start before the last assistant message.
        let after_last_assistant = match non_system.iter().rposition(|m| is_assistant(m)) {
            Some(index) => index + 1,
            None => 0,
        };
        consumed.max(after_last_assistant)
    };
    non_system
        .get(start..)
        .unwrap_or_default()
        .iter()
        .filter(|m| !is_assistant(m))
        .map(|m| (*m).clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{FunctionCall, ToolCall};

    fn base() -> Instant {
        Instant::now()
    }

    fn user(text: &str) -> NormalizedMessage {
        NormalizedMessage::User(text.to_string())
    }

    fn assistant(text: &str) -> NormalizedMessage {
        NormalizedMessage::Assistant {
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
        }
    }

    fn tool(call_id: &str, content: &str) -> NormalizedMessage {
        NormalizedMessage::Tool {
            call_id: call_id.to_string(),
            name: Some("read".to_string()),
            content: content.to_string(),
        }
    }

    fn req(model: &str, key: Option<&str>, messages: Vec<NormalizedMessage>) -> NormalizedRequest {
        NormalizedRequest {
            model: model.to_string(),
            messages,
            stream: false,
            include_usage: false,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: true,
            conversation_key: key.map(String::from),
            cwd_header: None,
            client_cwd: None,
            client_peer: None,
            opencode_session: false,
        }
    }

    fn reply(text: &str) -> AssistantReply {
        AssistantReply {
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            finish_reason: crate::openai::FinishReason::Stop,
        }
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/tmp/project")
    }

    #[test]
    fn keyed_new_then_continue_with_delta_of_new_user_message() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        assert_eq!(
            registry.classify(&first, now),
            Classification::New { bootstrap: false }
        );
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.commit(id, reply("hi"), now);

        let second = req(
            "claude",
            Some("k1"),
            vec![user("hello"), assistant("hi"), user("next question")],
        );
        match registry.classify(&second, now) {
            Classification::Continue { id: cid, delta } => {
                assert_eq!(cid, id);
                assert_eq!(delta, vec![user("next question")]);
            }
            other => panic!("expected continue, got {other:?}"),
        }
    }

    #[test]
    fn different_keys_with_same_first_message_stay_separate() {
        let now = base();
        let mut registry = Registry::default();
        registry.create("claude", Some("k1".to_string()), cwd(), now);
        let second = req("claude", Some("k2"), vec![user("same")]);
        assert!(matches!(
            registry.classify(&second, now),
            Classification::New { bootstrap: false }
        ));
        assert_eq!(registry.count_for_model("claude"), 1);
    }

    #[test]
    fn identical_request_after_commit_is_retry_with_cached_reply() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.commit(id, reply("cached answer"), now);

        match registry.classify(&first, now) {
            Classification::Retry { id: cid, reply } => {
                assert_eq!(cid, id);
                assert_eq!(reply.text.as_deref(), Some("cached answer"));
            }
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn retry_window_expiry() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.commit(id, reply("cached"), now);

        let later = now + Duration::from_secs(601);
        assert!(!matches!(
            registry.classify(&first, later),
            Classification::Retry { .. }
        ));
        let within = now + Duration::from_secs(599);
        assert!(matches!(
            registry.classify(&first, within),
            Classification::Retry { .. }
        ));
    }

    #[test]
    fn in_flight_identical_joins_and_different_is_busy() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);

        assert_eq!(
            registry.classify(&first, now),
            Classification::JoinInflight(id)
        );

        let different = req("claude", Some("k1"), vec![user("hello"), user("extra")]);
        assert_eq!(registry.classify(&different, now), Classification::Busy(id));
    }

    #[test]
    fn pruned_tool_output_still_continues() {
        let now = base();
        let mut registry = Registry::default();
        let first = req(
            "claude",
            Some("k1"),
            vec![user("read it"), tool("call_1", "full output text")],
        );
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.commit(id, reply("summarized"), now);

        // Same history but the tool output was pruned by the client afterwards.
        let second = req(
            "claude",
            Some("k1"),
            vec![
                user("read it"),
                tool("call_1", "pruned"),
                assistant("summarized"),
                user("and now?"),
            ],
        );
        match registry.classify(&second, now) {
            Classification::Continue { delta, .. } => {
                assert_eq!(delta, vec![user("and now?")]);
            }
            other => panic!("expected continue, got {other:?}"),
        }
    }

    #[test]
    fn keyless_unique_continuation_continues_but_ambiguous_starts_new() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", None, vec![user("hello")]);
        let id = registry.create("claude", None, cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.commit(id, reply("hi"), now);

        let follow = req(
            "claude",
            None,
            vec![user("hello"), assistant("hi"), user("more")],
        );
        assert!(matches!(
            registry.classify(&follow, now),
            Classification::Continue { .. }
        ));

        // A second keyless conversation with the same history makes it ambiguous.
        let other = req("claude", None, vec![user("hello")]);
        let id2 = registry.create("claude", None, cwd(), now);
        registry.begin(id2, request_digest(&other), &other);
        registry.commit(id2, reply("hi"), now);
        assert!(matches!(
            registry.classify(&follow, now),
            Classification::New { bootstrap: true }
        ));
    }

    #[test]
    fn keyed_diverged_history_forks_or_side_request() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.commit(id, reply("hi"), now);

        // Undo/compaction: history no longer extends ours and carries assistant messages.
        let forked = req(
            "claude",
            Some("k1"),
            vec![
                user("totally different"),
                assistant("old reply"),
                user("again"),
            ],
        );
        assert_eq!(registry.classify(&forked, now), Classification::Fork(id));

        // Title-generation style request: same key, no assistant messages.
        let side = req(
            "claude",
            Some("k1"),
            vec![user("summarize this chat title")],
        );
        assert_eq!(
            registry.classify(&side, now),
            Classification::SideRequest(id)
        );
    }

    #[test]
    fn abort_prevents_resending_the_same_user_message() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.abort(id, &first, now);

        let next = req("claude", Some("k1"), vec![user("hello"), user("follow up")]);
        match registry.classify(&next, now) {
            Classification::Continue { delta, .. } => {
                assert_eq!(delta, vec![user("follow up")]);
            }
            other => panic!("expected continue with only the new message, got {other:?}"),
        }
    }

    #[test]
    fn fail_clears_inflight_so_the_same_request_continues_again() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        assert_eq!(
            registry.classify(&first, now),
            Classification::JoinInflight(id)
        );
        registry.fail(id);
        // Same request, no reply committed: it continues and the prompt is re-sent.
        match registry.classify(&first, now) {
            Classification::Continue { id: cid, delta } => {
                assert_eq!(cid, id);
                assert_eq!(delta, vec![user("hello")]);
            }
            other => panic!("expected continue after fail, got {other:?}"),
        }
    }

    #[test]
    fn supersede_applies_abort_semantics_and_bumps_the_generation() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        let generation = registry.begin(id, request_digest(&first), &first);

        let next = req(
            "claude",
            Some("k1"),
            vec![user("hello"), user("new question")],
        );
        assert_eq!(registry.classify(&next, now), Classification::Busy(id));
        assert!(registry.supersede(id, now));

        // The aborted message counts as consumed: only the new text is a delta.
        match registry.classify(&next, now) {
            Classification::Continue { id: cid, delta } => {
                assert_eq!(cid, id);
                assert_eq!(delta, vec![user("new question")]);
            }
            other => panic!("expected continue after supersede, got {other:?}"),
        }
        let record = registry.get(id).expect("record");
        assert_eq!(record.inflight, None);
        assert_ne!(record.generation, generation, "the old turn is stale");
        // Nothing is in flight any more, so a second supersede does nothing.
        assert!(!registry.supersede(id, now));
    }

    #[test]
    fn expired_and_evict_candidate_respect_inflight_and_idle() {
        let now = base();
        let mut registry = Registry::default();
        let idle_old = registry.create("claude", Some("k1".to_string()), cwd(), now);
        let busy = registry.create("claude", Some("k2".to_string()), cwd(), now);
        let busy_request = req("claude", Some("k2"), vec![user("in flight")]);
        registry.begin(busy, request_digest(&busy_request), &busy_request);

        // Nothing is idle long enough yet.
        assert_eq!(
            registry.evict_candidate(now + Duration::from_secs(30)),
            None
        );
        assert!(registry
            .expired(now + Duration::from_secs(30), Duration::from_secs(60))
            .is_empty());

        if let Some(record) = registry.records.get_mut(&idle_old) {
            record.last_used = now;
        }
        assert_eq!(
            registry.evict_candidate(now + Duration::from_secs(61)),
            Some(idle_old)
        );
        let expired = registry.expired(now + Duration::from_secs(61), Duration::from_secs(60));
        assert_eq!(expired, vec![idle_old]);
    }

    #[test]
    fn counts_and_remove_and_get() {
        let now = base();
        let mut registry = Registry::default();
        let a = registry.create("claude", Some("ka".to_string()), cwd(), now);
        registry.create("claude", Some("kb".to_string()), cwd(), now);
        registry.create("codex", Some("kc".to_string()), cwd(), now);
        assert_eq!(registry.count_for_model("claude"), 2);
        assert_eq!(registry.count_total(), 3);
        assert!(registry.get(a).is_some());
        assert!(registry.remove(a).is_some());
        assert!(registry.get(a).is_none());
        assert_eq!(registry.count_total(), 2);
    }

    #[test]
    fn bootstrap_flag_on_new() {
        let now = base();
        let registry = Registry::default();
        let single = req("claude", Some("k"), vec![user("one")]);
        assert_eq!(
            registry.classify(&single, now),
            Classification::New { bootstrap: false }
        );
        let multi_user = req("claude", Some("k"), vec![user("one"), user("two")]);
        assert_eq!(
            registry.classify(&multi_user, now),
            Classification::New { bootstrap: true }
        );
        let with_assistant = req(
            "claude",
            Some("k"),
            vec![user("one"), assistant("reply"), user("two")],
        );
        assert_eq!(
            registry.classify(&with_assistant, now),
            Classification::New { bootstrap: true }
        );
    }

    #[test]
    fn digests_are_stable_and_text_normalized() {
        assert_eq!(
            normalize_text("a  \r\nb\t \r\n\r\n  c  "),
            "a\nb\n\n  c".to_string()
        );
        let a = req("claude", Some("k"), vec![user("hello world")]);
        let b = req("claude", Some("k"), vec![user("hello world  \r\n")]);
        assert_eq!(user_digests(&a), user_digests(&b));
        assert_eq!(request_digest(&a), request_digest(&b));
        let streamed = NormalizedRequest {
            stream: true,
            include_usage: true,
            ..a.clone()
        };
        assert_eq!(request_digest(&a), request_digest(&streamed));
    }

    #[test]
    fn conversation_id_displays_as_c_n() {
        let now = base();
        let mut registry = Registry::default();
        let id = registry.create("claude", None, cwd(), now);
        assert_eq!(id.to_string(), "c0");
    }

    #[test]
    fn delta_keeps_tool_messages_after_last_assistant() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("go")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.commit(id, reply("done"), now);

        let next = req(
            "claude",
            Some("k1"),
            vec![
                user("go"),
                NormalizedMessage::Assistant {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call_9".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "read".to_string(),
                            arguments: "{}".to_string(),
                        },
                    }],
                },
                tool("call_9", "file body"),
            ],
        );
        match registry.classify(&next, now) {
            Classification::Continue { delta, .. } => {
                assert!(matches!(delta[0], NormalizedMessage::Tool { .. }));
            }
            other => panic!("expected continue with tool delta, got {other:?}"),
        }
    }

    fn trust_dialog() -> Dialog {
        Dialog {
            question: "Accessing workspace:".to_string(),
            options: vec![
                "No, exit".to_string(),
                "Yes, I trust this folder".to_string(),
            ],
            cursor: 0,
        }
    }

    fn relayed(record_req: &NormalizedRequest, call_id: Option<&str>) -> PendingDialog {
        PendingDialog {
            dialog: trust_dialog(),
            reply: reply("Claude Code is asking"),
            call_id: call_id.map(String::from),
            seq: 1,
            relayed_users: user_digests(record_req),
            relayed_count: 2,
            expired: false,
            hook: None,
        }
    }

    #[test]
    fn answer_shaped_request_after_relay_classifies_as_dialog_answer() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.set_dialog(id, relayed(&first, None));

        let answer = req(
            "claude",
            Some("k1"),
            vec![user("hello"), assistant("Claude Code is asking"), user("2")],
        );
        assert_eq!(
            registry.classify(&answer, now),
            Classification::DialogAnswer(id)
        );
    }

    #[test]
    fn tool_answer_matches_only_the_relayed_call_id() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.set_dialog(id, relayed(&first, Some("call_abc")));

        let matching = req(
            "claude",
            Some("k1"),
            vec![
                user("hello"),
                assistant("asking"),
                tool("call_abc", "User has answered your questions"),
            ],
        );
        assert_eq!(
            registry.classify(&matching, now),
            Classification::DialogAnswer(id)
        );

        let other_call = req(
            "claude",
            Some("k1"),
            vec![
                user("hello"),
                assistant("asking"),
                tool("call_other", "some tool result"),
            ],
        );
        assert_eq!(
            registry.classify(&other_call, now),
            Classification::Busy(id)
        );
    }

    #[test]
    fn continuation_that_is_not_an_answer_stays_busy_while_the_turn_runs() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.set_dialog(id, relayed(&first, None));

        // Same key, but the count does not line up with the relay: not an answer.
        let different = req(
            "claude",
            Some("k1"),
            vec![user("hello"), user("something else")],
        );
        assert_eq!(registry.classify(&different, now), Classification::Busy(id));
    }

    #[test]
    fn identical_retry_while_a_dialog_is_pending_joins_the_turn() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.set_dialog(id, relayed(&first, None));
        assert_eq!(
            registry.classify(&first, now),
            Classification::JoinInflight(id)
        );
    }

    #[test]
    fn commit_accounts_for_the_answering_request_snapshot() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.set_dialog(id, relayed(&first, None));

        let answer = req(
            "claude",
            Some("k1"),
            vec![user("hello"), assistant("Claude Code is asking"), user("2")],
        );
        let (pending, snapshot) = registry
            .answer_dialog(id, request_digest(&answer), &answer)
            .expect("pending dialog");
        assert_eq!(pending.seq, 1);
        assert_eq!(
            snapshot.request.as_deref(),
            Some(request_digest(&first).as_str())
        );
        assert!(registry.get(id).expect("record").pending_dialog.is_none());

        registry.commit(id, reply("the answer"), now);
        let record = registry.get(id).expect("record");
        // The answering request plus the reply, not the original request.
        assert_eq!(record.consumed, 4);
        assert_eq!(record.user_digests, user_digests(&answer));
        assert_eq!(record.inflight, None);
        assert_eq!(
            record.last_request.as_deref(),
            Some(request_digest(&answer).as_str())
        );
    }

    #[test]
    fn restore_dialog_answer_lets_an_identical_retry_answer_again() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.set_dialog(id, relayed(&first, None));

        let answer = req(
            "claude",
            Some("k1"),
            vec![user("hello"), assistant("Claude Code is asking"), user("2")],
        );
        let (pending, snapshot) = registry
            .answer_dialog(id, request_digest(&answer), &answer)
            .expect("pending dialog");
        // A failed apply puts the entry and the snapshot back together.
        registry.restore_dialog_answer(id, pending, snapshot);
        let record = registry.get(id).expect("record");
        assert!(record.pending_dialog.is_some());
        assert_eq!(
            record.inflight.as_deref(),
            Some(request_digest(&first).as_str())
        );
        assert_eq!(record.inflight_users, user_digests(&first));
        assert_eq!(record.inflight_count, 1);

        // The identical retry answers again instead of joining the old request.
        assert_eq!(
            registry.classify(&answer, now),
            Classification::DialogAnswer(id)
        );
    }

    #[test]
    fn publish_dialog_seq_only_increases_across_answer_commit_and_fail() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);

        let seq = registry
            .publish_dialog(id, 0, trust_dialog(), reply("asking"), None, None)
            .expect("published");
        assert_eq!(seq, 1);

        let answer = req(
            "claude",
            Some("k1"),
            vec![user("hello"), assistant("asking"), user("2")],
        );
        let (pending, snapshot) = registry
            .answer_dialog(id, request_digest(&answer), &answer)
            .expect("pending dialog");
        registry.restore_dialog_answer(id, pending, snapshot);

        // The next relay continues the counter even though the pending entry
        // was taken out and put back.
        let seq = registry
            .publish_dialog(id, 0, trust_dialog(), reply("asking again"), None, None)
            .expect("published");
        assert_eq!(seq, 2);
        registry.commit(id, reply("the answer"), now);
        assert_eq!(registry.get(id).expect("record").dialog_seq, 2);
        registry.fail(id);
        assert_eq!(registry.get(id).expect("record").dialog_seq, 2);
        // A superseded turn's generation never publishes.
        assert!(registry
            .publish_dialog(id, 7, trust_dialog(), reply("stale"), None, None)
            .is_none());
    }

    #[test]
    fn commit_drops_a_live_dialog_but_keeps_an_expired_one() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);

        let mut pending = relayed(&first, None);
        pending.expired = true;
        registry.set_dialog(id, pending);
        registry.commit(id, reply("explanation"), now);
        assert!(registry.get(id).expect("record").pending_dialog.is_some());

        // A different request beginning drops the expired entry.
        let next = req(
            "claude",
            Some("k1"),
            vec![user("hello"), assistant("explanation"), user("next")],
        );
        registry.begin(id, request_digest(&next), &next);
        assert!(registry.get(id).expect("record").pending_dialog.is_none());
    }

    #[test]
    fn fail_drops_a_live_dialog_but_keeps_an_expired_one() {
        let now = base();
        let mut registry = Registry::default();
        let first = req("claude", Some("k1"), vec![user("hello")]);
        let id = registry.create("claude", Some("k1".to_string()), cwd(), now);
        registry.begin(id, request_digest(&first), &first);
        registry.set_dialog(id, relayed(&first, None));
        registry.fail(id);
        assert!(registry.get(id).expect("record").pending_dialog.is_none());

        let mut pending = relayed(&first, None);
        pending.expired = true;
        registry.set_dialog(id, pending);
        registry.fail(id);
        assert!(registry.get(id).expect("record").pending_dialog.is_some());
    }
}
