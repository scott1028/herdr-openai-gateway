//! Conversation state that outlives the gateway process: which threadId runs in
//! which herdr tab, what its CLI has already seen, and this instance's identity.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedState {
    pub version: u32,
    /// `oa_owner` token value; tabs carrying it stay ours across restarts.
    pub owner_id: String,
    /// Question hook token baked into the env of tabs already running.
    pub hook_token: Option<String>,
    pub conversations: Vec<SavedConversation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedConversation {
    pub key: Option<String>,
    pub model_id: String,
    pub cwd: PathBuf,
    pub user_digests: Vec<String>,
    pub consumed: usize,
    /// CLI session to resume in a new tab, for a conversation whose tab is gone.
    pub resume_session: Option<String>,
    pub agent: Option<SavedAgent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedAgent {
    pub name: String,
    pub pane_id: String,
    pub tab_id: String,
    pub tab_label: String,
    pub workspace_id: String,
    pub session_id: Option<String>,
    pub ready: bool,
    pub managed: bool,
}

impl SavedState {
    pub fn new(owner_id: String, hook_token: Option<String>) -> Self {
        Self {
            version: STATE_VERSION,
            owner_id,
            hook_token,
            conversations: Vec::new(),
        }
    }
}

/// The state file plus an exclusive lock on its sibling `.lock` file, held for
/// the life of the process: two gateways sharing one file would steal each
/// other's tabs.
pub struct StateFile {
    path: PathBuf,
    _lock: File,
    /// Last text written, so an unchanged state never touches the disk.
    last_written: Mutex<Option<String>>,
}

impl StateFile {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
        }
        let lock_path = path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|err| format!("cannot open {}: {err}", lock_path.display()))?;
        lock.try_lock().map_err(|err| {
            format!(
                "cannot lock {} ({err}); is another gateway using the same state file?",
                lock_path.display()
            )
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            _lock: lock,
            last_written: Mutex::new(None),
        })
    }

    /// The saved state, or None when there is none or it cannot be used.
    pub fn load(&self) -> Option<SavedState> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                tracing::warn!(path = %self.path.display(), "cannot read the state file: {err}");
                return None;
            }
        };
        match serde_json::from_str::<SavedState>(&text) {
            Ok(state) if state.version == STATE_VERSION => Some(state),
            Ok(state) => {
                tracing::warn!(
                    path = %self.path.display(),
                    version = state.version,
                    "ignoring a state file of another version"
                );
                None
            }
            Err(err) => {
                tracing::warn!(path = %self.path.display(), "ignoring a broken state file: {err}");
                None
            }
        }
    }

    /// Write `state` atomically (temp file + rename, mode 0600) unless it is
    /// what the file already holds.
    pub fn save(&self, state: &SavedState) {
        let text = match serde_json::to_string_pretty(state) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!("cannot serialize the state: {err}");
                return;
            }
        };
        let mut last = self
            .last_written
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if last.as_deref() == Some(text.as_str()) {
            return;
        }
        match write_atomic(&self.path, &text) {
            Ok(()) => *last = Some(text),
            Err(err) => {
                tracing::warn!(path = %self.path.display(), "cannot write the state file: {err}")
            }
        }
    }
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}
