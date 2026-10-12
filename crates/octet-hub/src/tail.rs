//! Reads whole journal records from files that are still growing.
use octet_core::remote::protocol::Cursor;
use serde_json::Value;
use std::{
    collections::VecDeque,
    io::SeekFrom,
    path::{Path, PathBuf},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
};

/// The most one `read` takes from the file.
const READ_CAP: u64 = 8 * 1024 * 1024;
/// The longest line kept while waiting for its newline; a longer one is skipped.
const LINE_CAP: usize = 16 * 1024 * 1024;
/// A resuming phone without a usable cursor gets this many of the newest records.
const REPLAY_LAST: usize = 500;

/// Reads whole new lines from a growing file.
#[derive(Debug)]
pub(crate) struct Tail {
    path: PathBuf,
    /// Bytes of the file read so far, `partial` included.
    offset: u64,
    /// The start of a line whose newline has not arrived.
    partial: Vec<u8>,
    /// Unparsable or oversize lines passed over.
    skipped: u64,
    /// An oversize line is being passed over, up to its newline.
    discarding: bool,
}

impl Tail {
    /// A tail that starts at the beginning of `path`.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
            skipped: 0,
            discarding: false,
        }
    }

    /// A tail that starts after the last whole line `path` has now, so it yields only what
    /// is written from here on; a file that cannot be read starts at its beginning.
    pub(crate) async fn at_end(path: PathBuf) -> Self {
        let mut tail = Self::new(path);
        tail.offset = whole_lines_end(&tail.path).await.unwrap_or(0);
        tail
    }

    /// The file this tail follows.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// How many bytes of the file have been read.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// How many unparsable lines this tail has skipped.
    pub(crate) fn skipped(&self) -> u64 {
        self.skipped
    }

    /// The records of every whole line written since the last call (at most `READ_CAP` bytes'
    /// worth; call again for more). A file shorter than before was truncated: the tail starts
    /// over at its beginning. Cancel-safe: the tail changes only after the last await.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be opened or read.
    pub(crate) async fn read(&mut self) -> std::io::Result<Vec<Value>> {
        let mut file = File::open(&self.path).await?;
        let len = file.metadata().await?.len();
        let truncated = len < self.offset;
        let from = if truncated { 0 } else { self.offset };
        file.seek(SeekFrom::Start(from)).await?;
        let mut fresh = Vec::new();
        (&mut file).take(READ_CAP).read_to_end(&mut fresh).await?;
        if truncated {
            self.partial.clear();
            self.discarding = false;
        }
        self.offset = from + fresh.len() as u64;
        Ok(self.lines(&fresh))
    }

    /// The records of the lines `fresh` completes, keeping what is left over.
    fn lines(&mut self, fresh: &[u8]) -> Vec<Value> {
        let mut records = Vec::new();
        let mut rest = fresh;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let (line, after) = rest.split_at(end);
            rest = &after[1..];
            if self.discarding {
                self.discarding = false;
                continue;
            }
            let line = if self.partial.is_empty() {
                line
            } else {
                self.partial.extend_from_slice(line);
                &self.partial
            };
            if line.len() > LINE_CAP {
                self.skipped += 1;
            } else if !line.iter().all(u8::is_ascii_whitespace) {
                match serde_json::from_slice(line) {
                    Ok(value) => records.push(value),
                    Err(_) => self.skipped += 1,
                }
            }
            self.partial.clear();
        }
        if !self.discarding {
            self.partial.extend_from_slice(rest);
            if self.partial.len() > LINE_CAP {
                self.partial = Vec::new();
                self.discarding = true;
                self.skipped += 1;
            }
        }
        records
    }
}

/// The offset just after the last newline of `path`; 0 if it has none.
async fn whole_lines_end(path: &Path) -> std::io::Result<u64> {
    const CHUNK: u64 = 8192;
    let mut file = File::open(path).await?;
    let mut end = file.metadata().await?.len();
    let mut chunk = vec![0; usize::try_from(CHUNK).unwrap_or(8192)];
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let size = usize::try_from(end - start).unwrap_or(chunk.len());
        file.seek(SeekFrom::Start(start)).await?;
        file.read_exact(&mut chunk[..size]).await?;
        if let Some(at) = chunk[..size].iter().rposition(|&b| b == b'\n') {
            return Ok(start + at as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// Every record of `journals` (oldest first) after `after`, as `(journal file name, seq,
/// event)`. A cursor names a journal and a `seq`: the rest of that journal, then every later
/// journal. No cursor, or a cursor naming no journal here (the phone's list is stale), gives
/// the newest journal's last 500 records. Unreadable journals and half-written lines are
/// passed over.
pub(crate) async fn replay(
    journals: &[PathBuf],
    after: Option<&Cursor>,
) -> Vec<(String, u64, Value)> {
    let found = after.and_then(|cursor| {
        let at = journals
            .iter()
            .position(|j| file_name(j) == cursor.journal)?;
        Some((at, cursor.seq))
    });
    let mut records = Vec::new();
    if let Some((at, seq)) = found {
        for (n, path) in journals[at..].iter().enumerate() {
            records.extend(journal_records(path, (n == 0).then_some(seq), usize::MAX).await);
        }
    } else if let Some(newest) = journals.last() {
        records.extend(journal_records(newest, None, REPLAY_LAST).await);
    }
    records
}

/// A journal's file name; what a cursor calls the journal.
pub(crate) fn file_name(journal: &Path) -> String {
    journal
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The records of one journal after `after`, the last `keep` of them.
async fn journal_records(
    path: &Path,
    after: Option<u64>,
    keep: usize,
) -> VecDeque<(String, u64, Value)> {
    let name = file_name(path);
    let mut tail = Tail::new(path.to_owned());
    let mut kept = VecDeque::new();
    loop {
        let before = tail.offset;
        let Ok(records) = tail.read().await else {
            break;
        };
        for (seq, event) in records.iter().filter_map(event_of) {
            if after.is_none_or(|after| seq > after) {
                kept.push_back((name.clone(), seq, event));
                if kept.len() > keep {
                    kept.pop_front();
                }
            }
        }
        if tail.offset <= before {
            break;
        }
    }
    kept
}

/// A journal record as the phone sees it: its `seq` and the event `{type, data}`.
pub(crate) fn event_of(record: &Value) -> Option<(u64, Value)> {
    let seq = record.get("seq")?.as_u64()?;
    let kind = record.get("type")?.as_str()?;
    let data = record.get("data").cloned().unwrap_or(Value::Null);
    Some((seq, serde_json::json!({ "type": kind, "data": data })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fmt::Write as _, io::Write};

    fn temp(name: &str) -> octet_testkit::TempDir {
        let dir = octet_testkit::TempDir::new(name);
        std::fs::create_dir_all(dir.path()).unwrap();
        dir
    }

    #[tokio::test]
    async fn the_tailer_waits_for_whole_lines() {
        let dir = temp("octet-hub-tail");
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, "{\"seq\":0}\n{\"seq\":1").unwrap();
        let mut tail = Tail::new(path.clone());
        let first = tail.read().await.unwrap();
        assert_eq!(first, vec![serde_json::json!({"seq": 0})]);
        assert!(tail.read().await.unwrap().is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"}\nnot json\n{\"seq\":2}\n").unwrap();
        let more = tail.read().await.unwrap();
        assert_eq!(
            more,
            vec![serde_json::json!({"seq": 1}), serde_json::json!({"seq": 2})]
        );
        assert_eq!(tail.skipped(), 1);
        // A truncated file starts over without a panic or a stale half line.
        file.set_len(0).unwrap();
        assert!(tail.read().await.unwrap().is_empty());
        assert_eq!(tail.offset, 0);
        file.write_all(b"{\"seq\":9}\n").unwrap();
        assert_eq!(
            tail.read().await.unwrap(),
            vec![serde_json::json!({"seq": 9})]
        );
    }

    fn journal(dir: &Path, name: &str, seqs: std::ops::Range<u64>) -> PathBuf {
        let path = dir.join(name);
        let mut text = String::new();
        for seq in seqs {
            let record = r#"{"format":"octet-preview-1","type":"t","data":null"#;
            writeln!(text, "{record},\"seq\":{seq}}}").unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    fn marks(records: &[(String, u64, Value)]) -> Vec<String> {
        records.iter().map(|(j, s, _)| format!("{j}{s}")).collect()
    }

    #[tokio::test]
    async fn replay_starts_after_the_cursor_and_crosses_journals() {
        let dir = temp("octet-hub-replay");
        let a = journal(dir.path(), "a.jsonl", 0..4);
        let b = journal(dir.path(), "b.jsonl", 0..3);
        let journals = [a, b.clone()];
        let after = |journal: &str, seq| {
            Some(Cursor {
                journal: journal.into(),
                seq,
            })
        };
        let got = replay(&journals, after("a.jsonl", 1).as_ref()).await;
        assert_eq!(
            marks(&got),
            ["a.jsonl2", "a.jsonl3", "b.jsonl0", "b.jsonl1", "b.jsonl2"]
        );
        assert_eq!(got[0].2, serde_json::json!({"type": "t", "data": null}));
        // No cursor: the newest journal only. A stale cursor: the same.
        let newest = ["b.jsonl0", "b.jsonl1", "b.jsonl2"];
        assert_eq!(marks(&replay(&journals, None).await), newest);
        assert_eq!(
            marks(&replay(&journals, after("gone.jsonl", 9).as_ref()).await),
            newest
        );
        assert!(
            replay(&journals, after("b.jsonl", 2).as_ref())
                .await
                .is_empty()
        );
        assert!(replay(&[], None).await.is_empty());
        // A journal that is missing or half written does not stop the rest.
        let half = dir.path().join("c.jsonl");
        std::fs::write(&half, "{\"seq\":0,\"type\":\"t\"}\n{\"seq\":1,\"ty").unwrap();
        let all = [dir.path().join("none.jsonl"), b, half];
        let got = replay(&all, after("b.jsonl", 1).as_ref()).await;
        assert_eq!(marks(&got), ["b.jsonl2", "c.jsonl0"]);
    }

    #[tokio::test]
    async fn replay_without_a_cursor_keeps_the_last_500_records() {
        let dir = temp("octet-hub-replay-long");
        let long = journal(dir.path(), "a.jsonl", 0..1200);
        let got = replay(&[long], None).await;
        assert_eq!(got.len(), 500);
        assert_eq!(got.first().map(|r| r.1), Some(700));
        assert_eq!(got.last().map(|r| r.1), Some(1199));
    }

    #[tokio::test]
    async fn a_tail_at_the_end_yields_only_what_comes_next() {
        let dir = temp("octet-hub-at-end");
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, "{\"seq\":0}\n{\"seq\":1}\n{\"seq\":2").unwrap();
        let mut tail = Tail::at_end(path.clone()).await;
        assert_eq!(tail.offset, 20);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"}\n{\"seq\":3}\n").unwrap();
        let got = tail.read().await.unwrap();
        assert_eq!(
            got,
            vec![serde_json::json!({"seq": 2}), serde_json::json!({"seq": 3})]
        );
    }
}
