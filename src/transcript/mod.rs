pub mod claude;
pub mod codex;

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;

/// Split a buffer into complete lines and the trailing partial remainder.
pub fn split_complete_lines(buf: &str) -> (Vec<&str>, &str) {
    match buf.rfind('\n') {
        Some(index) => (buf[..index].lines().collect(), &buf[index + 1..]),
        None => (Vec::new(), buf),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopKind {
    EndTurn,
    MaxTokens,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnScan {
    NotStarted,
    InProgress,
    Completed { text: String, stop: StopKind },
    Aborted { reason: String },
    UpstreamError { message: String },
    SwitchFile { session_id: String },
}

/// Incremental JSONL follower: keeps the offset and any incomplete trailing bytes.
#[derive(Debug, Default)]
pub struct JsonlCursor {
    pub path: Option<PathBuf>,
    pub offset: u64,
    pub partial: Vec<u8>,
}

impl JsonlCursor {
    /// Read lines appended since the last call. A missing file yields no lines.
    pub fn read_new_lines(&mut self) -> std::io::Result<Vec<String>> {
        let Some(path) = self.path.clone() else {
            return Ok(Vec::new());
        };
        let metadata = match path.metadata() {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        if metadata.len() < self.offset {
            // File rotated or truncated: restart from the beginning.
            self.offset = 0;
            self.partial.clear();
        }
        let mut file = std::fs::File::open(&path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        self.partial.extend_from_slice(&bytes);
        Ok(take_complete_lines(&mut self.partial))
    }
}

/// Move complete lines out of the partial buffer and decode them; keep trailing partial bytes
/// so a read that ends inside a multi-byte character cannot corrupt it.
fn take_complete_lines(partial: &mut Vec<u8>) -> Vec<String> {
    let Some(index) = partial.iter().rposition(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let complete: Vec<u8> = partial.drain(..=index).collect();
    complete[..complete.len() - 1]
        .split(|byte| *byte == b'\n')
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_complete_lines_across_partial_reads() {
        let (lines, rest) = split_complete_lines("{\"a\":1}\n{\"b\"");
        assert_eq!(lines, vec!["{\"a\":1}"]);
        assert_eq!(rest, "{\"b\"");

        let (lines, rest) = split_complete_lines(":2}\n{\"c\":3}\n");
        assert_eq!(lines, vec![":2}", "{\"c\":3}"]);
        assert_eq!(rest, "");

        let (lines, rest) = split_complete_lines("no newline yet");
        assert!(lines.is_empty());
        assert_eq!(rest, "no newline yet");

        let (lines, rest) = split_complete_lines("\n");
        assert!(lines.is_empty());
        assert_eq!(rest, "");
    }

    #[test]
    fn cursor_missing_file_is_empty() {
        let mut cursor = JsonlCursor {
            path: Some(PathBuf::from(
                "/nonexistent/herdr-openai-gateway-test.jsonl",
            )),
            ..JsonlCursor::default()
        };
        assert_eq!(cursor.read_new_lines().expect("io"), Vec::<String>::new());
    }

    #[test]
    fn cursor_follows_appends_and_keeps_partial_line() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-cursor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("log.jsonl");

        let mut cursor = JsonlCursor {
            path: Some(path.clone()),
            ..JsonlCursor::default()
        };
        std::fs::write(&path, "{\"n\":1}\n{\"n\":2}\n").expect("write 1");
        assert_eq!(cursor.read_new_lines().expect("read 1").len(), 2);

        std::fs::write(&path, "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n{\"n\":4").expect("write 2");
        let lines = cursor.read_new_lines().expect("read 2");
        assert_eq!(lines, vec!["{\"n\":3}".to_string()]);
        assert_eq!(cursor.partial, b"{\"n\":4");

        std::fs::write(&path, "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n").expect("write 3");
        let lines = cursor.read_new_lines().expect("read 3");
        assert_eq!(lines, vec!["{\"n\":4}".to_string()]);
        assert!(cursor.partial.is_empty());

        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn cursor_keeps_multibyte_char_split_across_appends() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-cjk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("log.jsonl");

        let line = "{\"text\":\"你好世界\"}";
        let bytes = line.as_bytes();
        // Split inside the first byte of the multi-byte character 你.
        let split = b"{\"text\":\"".len() + 1;

        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open for append");
        file.write_all(&bytes[..split]).expect("append part 1");
        file.flush().expect("flush 1");

        let mut cursor = JsonlCursor {
            path: Some(path.clone()),
            ..JsonlCursor::default()
        };
        assert_eq!(
            cursor.read_new_lines().expect("read 1"),
            Vec::<String>::new()
        );

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen for append");
        file.write_all(&bytes[split..]).expect("append part 2");
        file.write_all(b"\n").expect("append newline");
        file.flush().expect("flush 2");

        let lines = cursor.read_new_lines().expect("read 2");
        assert_eq!(lines, vec![line.to_string()]);
        assert!(cursor.partial.is_empty());

        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }
}
