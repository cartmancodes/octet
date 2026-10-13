//! Reads whole journal records from files that are still growing.
use octet_core::remote::protocol::Cursor;
use serde_json::Value;
use std::{
    collections::VecDeque,
    io::SeekFrom,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader},
};

/// The most one `read` takes from the file.
const READ_CAP: u64 = 8 * 1024 * 1024;
/// The longest line kept while waiting for its newline; a longer one is skipped.
const LINE_CAP: usize = 16 * 1024 * 1024;

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
    /// `(device, inode)` of the file last read: another one is a replaced file.
    identity: Option<(u64, u64)>,
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
            identity: None,
        }
    }

    /// A tail that starts after the last whole line `path` has now, so it yields only what
    /// is written from here on; a file that cannot be read starts at its beginning.
    pub(crate) async fn at_end(path: PathBuf) -> Self {
        let mut tail = Self::new(path);
        tail.identity = tokio::fs::metadata(&tail.path)
            .await
            .ok()
            .map(|meta| (meta.dev(), meta.ino()));
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the tests read the count so far")
    )]
    pub(crate) fn skipped(&self) -> u64 {
        self.skipped
    }

    /// The records of every whole line written since the last call (at most `READ_CAP` bytes'
    /// worth; call again for more). The file is statted first, and left unopened when it has
    /// not grown. A file shorter than before was truncated, and one with another inode was
    /// replaced: either way the tail starts over at its beginning. Only a regular file is read.
    /// Cancel-safe: the tail changes only after the last await.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be opened or read, or is not a regular file.
    pub(crate) async fn read(&mut self) -> std::io::Result<Vec<Value>> {
        let meta = tokio::fs::metadata(&self.path).await?;
        let identity = Some((meta.dev(), meta.ino()));
        if meta.len() == self.offset && self.identity == identity {
            return Ok(Vec::new());
        }
        let mut file = File::open(&self.path).await?;
        let meta = file.metadata().await?;
        if !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let identity = Some((meta.dev(), meta.ino()));
        let restart =
            meta.len() < self.offset || self.identity.is_some_and(|old| Some(old) != identity);
        let from = if restart { 0 } else { self.offset };
        file.seek(SeekFrom::Start(from)).await?;
        let mut fresh = Vec::new();
        (&mut file).take(READ_CAP).read_to_end(&mut fresh).await?;
        if restart {
            self.partial.clear();
            self.discarding = false;
        }
        self.identity = identity;
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

/// Up to `limit` records of `journals` (oldest first) after `after`, as `(journal file name,
/// seq, event)`, and whether more follow. A cursor names a journal and a `seq`: the rest of
/// that journal, then every later journal; the caller pages by passing the last record it got
/// as the next cursor. No cursor, or a cursor naming no journal here (the phone's list is
/// stale), starts at the first record of the first journal. Journals are read a line at a time,
/// never whole; unreadable journals and half-written lines are passed over.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the server pages with replay_from")
)]
pub(crate) async fn replay(
    journals: &[PathBuf],
    after: Option<&Cursor>,
    limit: usize,
) -> (Vec<(String, u64, Value)>, bool) {
    let (records, more, _) = replay_from(journals, after, 0, limit).await;
    (records, more)
}

/// [`replay`], starting the cursor's journal at byte `offset` (the offset a previous page
/// returned; 0 for the first page), and also returning the offset just past the last record's
/// line, within that record's journal, so the next page reads on from there instead of from
/// the start. An offset past the end of the file (a replaced journal) reads it from the start.
pub(crate) async fn replay_from(
    journals: &[PathBuf],
    after: Option<&Cursor>,
    offset: u64,
    limit: usize,
) -> (Vec<(String, u64, Value)>, bool, u64) {
    let whole = Start {
        after: None,
        offset: 0,
    };
    let found = after.and_then(|cursor| {
        let at = journals
            .iter()
            .position(|j| file_name(j) == cursor.journal)?;
        Some((at, cursor.seq))
    });
    let (at, first) = match found {
        Some((at, seq)) => (
            at,
            Start {
                after: Some(seq),
                offset,
            },
        ),
        None => (0, whole),
    };
    let mut records = VecDeque::new();
    for (n, path) in journals.get(at..).unwrap_or_default().iter().enumerate() {
        let from = if n == 0 { first } else { whole };
        journal_records(path, from, limit, &mut records).await;
        if records.len() > limit {
            break;
        }
    }
    let more = records.len() > limit;
    records.truncate(limit);
    let end = records.back().map_or(first.offset, |r: &Found| r.3);
    (strip(records), more, end)
}

/// A record found by a replay, and the byte offset just past its line.
type Found = (String, u64, Value, u64);

/// Where to start reading one journal: after which `seq`, from which byte.
#[derive(Debug, Clone, Copy)]
struct Start {
    after: Option<u64>,
    offset: u64,
}

/// The records without their offsets.
fn strip(records: VecDeque<Found>) -> Vec<(String, u64, Value)> {
    records
        .into_iter()
        .map(|(journal, seq, event, _)| (journal, seq, event))
        .collect()
}

/// A journal's file name; what a cursor calls the journal.
pub(crate) fn file_name(journal: &Path) -> String {
    journal
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Appends to `out` the records of one journal from `from`, a line at a time, and stops once
/// `out` holds more than `keep`.
async fn journal_records(path: &Path, from: Start, keep: usize, out: &mut VecDeque<Found>) {
    let Ok(mut file) = File::open(path).await else {
        return;
    };
    let Ok(meta) = file.metadata().await else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    let mut position = if from.offset <= meta.len() {
        from.offset
    } else {
        0
    };
    if file.seek(SeekFrom::Start(position)).await.is_err() {
        return;
    }
    let name = file_name(path);
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    while let Some((whole, read)) = next_line(&mut reader, &mut line).await {
        position += read;
        let parsed = whole
            .then(|| serde_json::from_slice::<Value>(&line).ok())
            .flatten();
        let Some((seq, event)) = parsed.as_ref().and_then(event_of) else {
            continue;
        };
        if from.after.is_some_and(|after| seq <= after) {
            continue;
        }
        out.push_back((name.clone(), seq, event, position));
        if out.len() > keep {
            return;
        }
    }
}

/// Reads the next line of `reader` into `line` (newline removed), with the bytes it took.
/// `true` is a line to use, `false` one passed over because it is over `LINE_CAP`; `None` is
/// the end of the file, or a last line whose newline has not been written yet.
async fn next_line(reader: &mut BufReader<File>, line: &mut Vec<u8>) -> Option<(bool, u64)> {
    line.clear();
    let limit = LINE_CAP as u64 + 1;
    let read = (&mut *reader)
        .take(limit)
        .read_until(b'\n', line)
        .await
        .ok()?;
    if read == 0 {
        return None;
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        return Some((true, read as u64));
    }
    if line.len() as u64 <= LINE_CAP as u64 {
        return None;
    }
    // Over the cap: pass over the rest of the line.
    let mut taken = read as u64;
    loop {
        let buffered = reader.fill_buf().await.ok()?;
        if buffered.is_empty() {
            return None;
        }
        if let Some(end) = buffered.iter().position(|&b| b == b'\n') {
            reader.consume(end + 1);
            return Some((false, taken + end as u64 + 1));
        }
        let all = buffered.len();
        taken += all as u64;
        reader.consume(all);
    }
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

    const ALL: usize = usize::MAX;

    fn after(journal: &str, seq: u64) -> Cursor {
        Cursor {
            journal: journal.into(),
            seq,
        }
    }

    #[tokio::test]
    async fn replay_starts_after_the_cursor_and_crosses_journals() {
        let dir = temp("octet-hub-replay");
        let a = journal(dir.path(), "a.jsonl", 0..4);
        let b = journal(dir.path(), "b.jsonl", 0..3);
        let journals = [a, b.clone()];
        let (got, more) = replay(&journals, Some(&after("a.jsonl", 1)), 500).await;
        assert_eq!(
            marks(&got),
            ["a.jsonl2", "a.jsonl3", "b.jsonl0", "b.jsonl1", "b.jsonl2"]
        );
        assert!(!more);
        assert_eq!(got[0].2, serde_json::json!({"type": "t", "data": null}));
        // No cursor: everything, from the start of the first journal. A stale cursor: the same.
        let everything = [
            "a.jsonl0", "a.jsonl1", "a.jsonl2", "a.jsonl3", "b.jsonl0", "b.jsonl1", "b.jsonl2",
        ];
        assert_eq!(marks(&replay(&journals, None, 500).await.0), everything);
        let stale = after("gone.jsonl", 9);
        assert_eq!(
            marks(&replay(&journals, Some(&stale), 500).await.0),
            everything
        );
        let end = after("b.jsonl", 2);
        assert_eq!(
            replay(&journals, Some(&end), 500).await,
            (Vec::new(), false)
        );
        assert_eq!(replay(&[], None, 500).await, (Vec::new(), false));
        // A journal that is missing or half written does not stop the rest.
        let half = dir.path().join("c.jsonl");
        std::fs::write(&half, "{\"seq\":0,\"type\":\"t\"}\n{\"seq\":1,\"ty").unwrap();
        let all = [dir.path().join("none.jsonl"), b, half];
        let (got, _) = replay(&all, Some(&after("b.jsonl", 1)), 500).await;
        assert_eq!(marks(&got), ["b.jsonl2", "c.jsonl0"]);
    }

    #[tokio::test]
    async fn replay_without_a_cursor_pages_from_the_start() {
        let dir = temp("octet-hub-replay-long");
        let long = journal(dir.path(), "a.jsonl", 0..1200);
        let (got, more) = replay(std::slice::from_ref(&long), None, 500).await;
        assert_eq!(got.len(), 500);
        assert!(more, "the rest is a page away");
        assert_eq!(got.first().map(|r| r.1), Some(0));
        assert_eq!(got.last().map(|r| r.1), Some(499));
    }

    #[tokio::test]
    async fn pages_of_a_replay_add_up_to_one_replay() {
        let dir = temp("octet-hub-pages");
        let journals = [
            journal(dir.path(), "a.jsonl", 0..7),
            journal(dir.path(), "b.jsonl", 0..4),
            journal(dir.path(), "c.jsonl", 0..6),
        ];
        let first = after("a.jsonl", 0);
        let (whole, more) = replay(&journals, Some(&first), ALL).await;
        assert_eq!((whole.len(), more), (16, false));
        let mut paged = Vec::new();
        let mut cursor = Some(first);
        loop {
            let (page, more) = replay(&journals, cursor.as_ref(), 5).await;
            assert!(page.len() <= 5);
            let Some(last) = page.last() else { break };
            cursor = Some(after(&last.0, last.1));
            paged.extend(page);
            assert!(
                more || paged.len() == whole.len(),
                "a short page says there is more"
            );
            if !more {
                break;
            }
        }
        assert_eq!(marks(&paged), marks(&whole));
        // A page that ends exactly at the end of the records has nothing more.
        let (page, more) = replay(&journals, Some(&after("a.jsonl", 0)), 16).await;
        assert_eq!((page.len(), more), (16, false));
    }

    #[tokio::test]
    async fn a_long_journal_replays_in_pages() {
        let dir = temp("octet-hub-50k");
        let long = journal(dir.path(), "a.jsonl", 0..50_000);
        let journals = [long];
        let (page, more) = replay(&journals, Some(&after("a.jsonl", 0)), 500).await;
        assert_eq!((page.len(), more), (500, true));
        assert_eq!(page.first().map(|r| r.1), Some(1));
        let (page, more) = replay(&journals, Some(&after("a.jsonl", 40_000)), 500).await;
        assert_eq!((page.len(), more), (500, true));
        assert_eq!(page.last().map(|r| r.1), Some(40_500));
        let (page, more) = replay(&journals, Some(&after("a.jsonl", 49_700)), 500).await;
        assert_eq!((page.len(), more), (299, false));
        // No cursor: the first 500, however long the journal.
        let (page, more) = replay(&journals, None, 500).await;
        assert_eq!((page.len(), more), (500, true));
        assert_eq!(page.first().map(|r| r.1), Some(0));
    }

    #[tokio::test]
    async fn a_replaced_file_is_read_from_its_start() {
        let dir = temp("octet-hub-replaced");
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, "{\"seq\":0}\n").unwrap();
        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.read().await.unwrap().len(), 1);
        assert!(
            tail.read().await.unwrap().is_empty(),
            "unchanged: nothing to read"
        );
        // Another file, longer than the old one, takes its place.
        let other = dir.path().join("other.jsonl");
        std::fs::write(&other, "{\"seq\":10}\n{\"seq\":11}\n{\"seq\":12}\n").unwrap();
        std::fs::rename(&other, &path).unwrap();
        let got = tail.read().await.unwrap();
        let seqs: Vec<_> = got.iter().map(|r| r["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, [10, 11, 12]);
    }

    #[tokio::test]
    async fn a_tail_does_not_read_what_is_not_a_regular_file() {
        let mut tail = Tail::new(PathBuf::from("/dev/null"));
        assert!(tail.read().await.is_err());
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

    #[tokio::test]
    async fn a_page_resumes_at_its_byte_offset() {
        let dir = temp("octet-hub-resume");
        let a = journal(dir.path(), "a.jsonl", 0..10);
        let b = journal(dir.path(), "b.jsonl", 0..3);
        let line = |seq: u64| {
            format!(
                "{{\"format\":\"octet-preview-1\",\"type\":\"t\",\"data\":null,\"seq\":{seq}}}\n"
            )
            .len() as u64
        };
        let journals = [a.clone(), b];
        let (page, more, offset) = replay_from(&journals, Some(&after("a.jsonl", 0)), 0, 4).await;
        assert_eq!(
            marks(&page),
            ["a.jsonl1", "a.jsonl2", "a.jsonl3", "a.jsonl4"]
        );
        assert!(more);
        // The offset is just past the last record's line: lines 0..=4 of a.
        assert_eq!(offset, (0..5).map(line).sum::<u64>());
        // Resuming there reads on from that byte: a line before it, even with a later seq, is
        // not read again.
        let mut text = std::fs::read_to_string(&a).unwrap();
        let swapped = text.replacen("\"seq\":2}", "\"seq\":99}", 1);
        assert_ne!(swapped, text);
        text = swapped;
        std::fs::write(&a, &text).unwrap();
        let (page, more, offset) =
            replay_from(&journals, Some(&after("a.jsonl", 4)), offset, 4).await;
        assert_eq!(
            marks(&page),
            ["a.jsonl5", "a.jsonl6", "a.jsonl7", "a.jsonl8"]
        );
        assert!(more);
        // Across a journal boundary the offset is within the journal of the last record.
        let (page, more, offset) =
            replay_from(&journals, Some(&after("a.jsonl", 8)), offset, 2).await;
        assert_eq!(marks(&page), ["a.jsonl9", "b.jsonl0"]);
        assert!(more);
        assert_eq!(offset, line(0));
        let (page, more, _) = replay_from(&journals, Some(&after("b.jsonl", 0)), offset, 5).await;
        assert_eq!(marks(&page), ["b.jsonl1", "b.jsonl2"]);
        assert!(!more);
        // An offset past the end of a replaced, shorter file starts it over.
        let (page, _, _) = replay_from(&journals, Some(&after("b.jsonl", 0)), 1 << 40, 5).await;
        assert_eq!(marks(&page), ["b.jsonl1", "b.jsonl2"]);
    }
}
