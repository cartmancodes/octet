//! Recent vendor sessions in a workspace, read from Octet's own journals.
use crate::Engine;
use octet_store::{JournalSummary, journal_stamp, read_summary};
use std::path::{Path, PathBuf};

/// The most journals [`recent_sessions`] reads, newest first, so a long
/// history does not slow the listing down.
const SCAN_LIMIT: usize = 400;

/// One resumable vendor session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSession {
    /// The provider that owns the session.
    pub engine: Engine,
    /// The newest journal for it.
    pub journal: PathBuf,
    /// What that journal says about it, except `first_prompt`, the oldest
    /// listed journal's of the same session (the session's own first), and
    /// `last_prompt`, from an older one when no newer one had a prompt
    /// (`None` when the latest is too far from its journal's end to read).
    pub summary: JournalSummary,
}

/// The newest [`SCAN_LIMIT`] journals in `directory`, newest first.
async fn newest_journals(directory: &Path) -> Vec<PathBuf> {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return Vec::new();
    };
    let mut journals = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if let Some(stamp) = journal_stamp(&path) {
            journals.push((stamp, path));
        }
    }
    journals.sort_unstable_by(|a, b| b.cmp(a));
    journals
        .into_iter()
        .take(SCAN_LIMIT)
        .map(|(_, path)| path)
        .collect()
}

/// Up to `limit` vendor sessions run in `workspace`, newest first, one entry
/// per session. Journals of other workspaces, the offline demo, sessions
/// the vendor never named, and files that are not journals are skipped.
pub async fn recent_sessions(
    directory: &Path,
    workspace: &Path,
    limit: usize,
) -> Vec<RecentSession> {
    recent_sessions_in(directory, &[workspace.to_path_buf()], limit, limit).await
}

/// Up to `total` vendor sessions run in any of `workspaces`, at most `per_workspace` in each,
/// newest first, one entry per session, from one pass over the newest journals. Skips what
/// [`recent_sessions`] skips.
pub async fn recent_sessions_in(
    directory: &Path,
    workspaces: &[PathBuf],
    per_workspace: usize,
    total: usize,
) -> Vec<RecentSession> {
    let mut found: Vec<RecentSession> = Vec::new();
    // For each found session, whether a journal read so far had a prompt.
    let mut prompted: Vec<bool> = Vec::new();
    for journal in newest_journals(directory).await {
        let Some(summary) = read_summary(&journal).await else {
            continue;
        };
        let Some(engine) = Engine::parse(&summary.engine).filter(|e| e.is_vendor()) else {
            continue;
        };
        if summary.session.is_empty() || !workspaces.contains(&summary.cwd) {
            continue;
        }
        // Reconnects write a journal per connection; the newest stands for
        // the session. Journals come newest first, so an older one's first
        // prompt is the session's, and its last one counts only while no
        // newer journal had a prompt (one may have, its latest unread).
        let has_prompt = summary.first_prompt.is_some() || summary.last_prompt.is_some();
        if let Some(index) = found.iter().position(|s| {
            s.engine == engine
                && s.summary.session == summary.session
                && s.summary.cwd == summary.cwd
        }) {
            let known = &mut found[index].summary;
            if !prompted[index] {
                known.last_prompt = summary.last_prompt;
            }
            if summary.first_prompt.is_some() {
                known.first_prompt = summary.first_prompt;
            }
            prompted[index] |= has_prompt;
            continue;
        }
        if found.len() == total {
            break;
        }
        let in_workspace = found
            .iter()
            .filter(|s| s.summary.cwd == summary.cwd)
            .count();
        if in_workspace == per_workspace {
            continue;
        }
        prompted.push(has_prompt);
        found.push(RecentSession {
            engine,
            journal,
            summary,
        });
    }
    found
}

/// Workspaces of the newest journals in `directory`, newest first, without repeats.
pub async fn known_workspaces(directory: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for journal in newest_journals(directory).await {
        if let Some(summary) = read_summary(&journal).await
            && !found.contains(&summary.cwd)
        {
            found.push(summary.cwd);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A journal in `dir` for `engine` in `cwd`, naming `session`, with an
    /// optional first prompt.
    fn journal(
        dir: &Path,
        stamp: u64,
        engine: &str,
        cwd: &str,
        session: &str,
        prompt: Option<&str>,
    ) {
        let header = json!({"engine":engine,"cwd":cwd,"resume":null,"model":null,"mode":"ask"});
        let mut records = vec![("session", header), ("ready", json!(session))];
        if let Some(prompt) = prompt {
            records.push(("user", json!(prompt)));
        }
        octet_testkit::write_journal(dir, stamp, &records, "");
    }

    #[tokio::test]
    async fn recent_sessions_lists_newest_first_for_this_workspace() {
        let temp = octet_testkit::TempDir::new("octet-core-sessions");
        let dir = temp.path();
        std::fs::create_dir_all(dir).unwrap();
        journal(dir, 1, "codex", "/work", "t-1", Some("one"));
        journal(
            dir,
            2,
            "codex",
            "/elsewhere",
            "t-9",
            Some("other workspace"),
        );
        journal(dir, 3, "claude", "/work", "c-1", Some("two"));
        journal(dir, 4, "demo", "/work", "demo · offline", Some("demo"));
        journal(dir, 5, "codex", "/work", "t-1", None);
        journal(dir, 6, "claude", "/work", "", None);
        std::fs::write(dir.join("notes.txt"), "not a journal").unwrap();
        let found = recent_sessions(dir, Path::new("/work"), 20).await;
        let listed: Vec<(&str, &str, Option<&str>)> = found
            .iter()
            .map(|s| {
                (
                    s.engine.as_str(),
                    s.summary.session.as_str(),
                    s.summary.first_prompt.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("codex", "t-1", Some("one")),
                ("claude", "c-1", Some("two"))
            ]
        );
        assert!(found[0].journal.ends_with("session-5-1.jsonl"));
        assert_eq!(recent_sessions(dir, Path::new("/work"), 1).await.len(), 1);
        assert!(
            recent_sessions(&dir.join("missing"), Path::new("/work"), 20)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn recent_sessions_in_reads_once_for_many_workspaces_with_limits() {
        let temp = octet_testkit::TempDir::new("octet-core-sessions-in");
        let dir = temp.path();
        std::fs::create_dir_all(dir).unwrap();
        journal(dir, 1, "codex", "/a", "a-1", Some("a one"));
        journal(dir, 2, "codex", "/a", "a-2", Some("a two"));
        journal(dir, 3, "claude", "/b", "b-1", Some("b one"));
        journal(dir, 4, "codex", "/a", "a-3", Some("a three"));
        journal(dir, 5, "codex", "/c", "c-1", Some("not asked for"));
        journal(dir, 6, "codex", "/a", "a-2", None);
        let listed = |found: &[RecentSession]| -> Vec<String> {
            found.iter().map(|s| s.summary.session.clone()).collect()
        };
        let wanted = [PathBuf::from("/a"), PathBuf::from("/b")];
        // Newest journal first, one entry per session, nothing from /c.
        let all = recent_sessions_in(dir, &wanted, 20, 20).await;
        assert_eq!(listed(&all), ["a-2", "a-3", "b-1", "a-1"]);
        assert_eq!(all[0].summary.first_prompt.as_deref(), Some("a two"));
        // At most two per workspace.
        let some = recent_sessions_in(dir, &wanted, 2, 20).await;
        assert_eq!(listed(&some), ["a-2", "a-3", "b-1"]);
        // At most three in all.
        let few = recent_sessions_in(dir, &wanted, 20, 3).await;
        assert_eq!(listed(&few), ["a-2", "a-3", "b-1"]);
        assert!(recent_sessions_in(dir, &[], 20, 20).await.is_empty());
    }

    #[tokio::test]
    async fn a_reconnected_session_shows_its_first_and_latest_prompts() {
        let temp = octet_testkit::TempDir::new("octet-core-sessions-span");
        let dir = temp.path();
        std::fs::create_dir_all(dir).unwrap();
        // One session over three connections (a journal each).
        journal(dir, 1, "opencode", "/work", "ses-1", Some("Reply with OK."));
        journal(
            dir,
            2,
            "opencode",
            "/work",
            "ses-1",
            Some("Launch a subagent"),
        );
        let header =
            json!({"engine":"opencode","cwd":"/work","resume":"ses-1","model":null,"mode":"ask"});
        let records = [
            ("session", header),
            ("ready", json!("ses-1")),
            ("user", json!("third")),
            ("user", json!("fourth")),
        ];
        octet_testkit::write_journal(dir, 3, &records, "");
        let found = recent_sessions(dir, Path::new("/work"), 20).await;
        assert_eq!(found.len(), 1);
        let summary = &found[0].summary;
        assert!(found[0].journal.ends_with("session-3-1.jsonl"));
        assert_eq!(summary.first_prompt.as_deref(), Some("Reply with OK."));
        assert_eq!(summary.last_prompt.as_deref(), Some("fourth"));
    }

    #[tokio::test]
    async fn an_older_journals_prompt_is_not_the_latest_of_a_newer_one_with_prompts() {
        let temp = octet_testkit::TempDir::new("octet-core-sessions-unknown");
        let dir = temp.path();
        std::fs::create_dir_all(dir).unwrap();
        journal(dir, 1, "claude", "/work", "c-1", Some("older"));
        // The newest journal has prompts, but its latest is too far from its
        // end to be read: it is not known, and the older one's is not it.
        let header =
            json!({"engine":"claude","cwd":"/work","resume":"c-1","model":null,"mode":"ask"});
        let reply = "y".repeat(512 * 1024);
        let records = [
            ("session", header),
            ("ready", json!("c-1")),
            ("user", json!("newer")),
            ("text", json!(reply)),
            ("user", json!("newest")),
            ("text", json!(reply)),
        ];
        octet_testkit::write_journal(dir, 2, &records, "");
        let found = recent_sessions(dir, Path::new("/work"), 20).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].summary.first_prompt.as_deref(), Some("older"));
        assert_eq!(found[0].summary.last_prompt, None);
    }

    #[tokio::test]
    async fn known_workspaces_lists_each_once_newest_first() {
        let temp = octet_testkit::TempDir::new("octet-core-workspaces");
        let dir = temp.path();
        for cwd in ["/a", "/b", "/c", "/b"] {
            let mut journal = octet_store::Journal::create(dir).await.unwrap();
            let header =
                json!({"engine":"codex","cwd":cwd,"resume":null,"model":null,"mode":"ask"});
            journal.append("session", header, true).await.unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "not a journal").unwrap();
        assert_eq!(
            known_workspaces(dir).await,
            ["/b", "/c", "/a"].map(PathBuf::from)
        );
        assert!(known_workspaces(&dir.join("missing")).await.is_empty());
    }
}
