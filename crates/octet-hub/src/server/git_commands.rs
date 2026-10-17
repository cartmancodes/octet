//! The phone's git commands as the hub loop runs them: `changes`, `diff` and `commit`, the
//! listing each connection keeps of what its last `changes` of a session listed, and the turn
//! a commit waits on. Git itself runs in [`crate::git`].
use super::{ConnEvent, Hub, Job};
use crate::{
    git::{self, Listing, TURN_FIRST},
    sessions::Place,
};
use octet_core::remote::protocol::{Code, PhoneCommand, ack, changes_frame, diff_frame, refused};
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::watch;

/// The refusal for a `diff` of a path the connection's last `changes` for the session did not
/// list.
const ASK_FOR_CHANGES: &str = "Ask for the changes first";
/// The refusal for a commit to a session whose turn the hub cannot see.
const NOT_CONNECTED: &str = "The session is not connected";
/// The refusal for a git command naming a session the hub does not have.
const NO_SUCH_SESSION: &str = "No such session";
/// Sessions whose last listing one connection keeps; another lets the least recent go.
const LISTED_SESSIONS: usize = 8;

/// One connection's last listing of each session, for at most [`LISTED_SESSIONS`] sessions;
/// each holds at most [`git::FILES_LIMIT`] paths. Each `changes` takes a ticket as it starts, so
/// a slower, older answer never replaces a newer listing.
#[derive(Debug, Default)]
pub(super) struct Listings {
    /// By session: the ticket of the `changes` it came from, and the listing.
    by_session: HashMap<String, (u64, Listing)>,
    /// The last ticket given.
    tickets: u64,
}

impl Listings {
    /// The ticket of a `changes` starting now: later than any before.
    fn ticket(&mut self) -> u64 {
        self.tickets += 1;
        self.tickets
    }

    /// Keeps `listing` as `session`'s unless one of a later ticket is kept, and says whether it
    /// was; a ninth session lets the least recently listed go.
    fn keep(&mut self, session: String, ticket: u64, listing: Listing) -> bool {
        match self.by_session.get(&session) {
            Some((kept, _)) if *kept > ticket => return false,
            None if self.by_session.len() >= LISTED_SESSIONS => {
                let oldest = self
                    .by_session
                    .iter()
                    .min_by_key(|(_, (kept, _))| *kept)
                    .map(|(id, _)| id.clone());
                if let Some(oldest) = oldest {
                    self.by_session.remove(&oldest);
                }
            }
            // Its own older listing, or room for another session.
            _ => {}
        }
        self.by_session.insert(session, (ticket, listing));
        true
    }

    /// Forgets `session`'s listing, unless it came from a later ticket than `ticket`.
    fn forget(&mut self, session: &str, ticket: u64) {
        if self
            .by_session
            .get(session)
            .is_some_and(|(kept, _)| *kept < ticket)
        {
            self.by_session.remove(session);
        }
    }

    /// `session`'s last listing.
    fn get(&self, session: &str) -> Option<&Listing> {
        self.by_session.get(session).map(|(_, listing)| listing)
    }
}

impl Hub {
    /// Runs a git command: `changes`, `diff` or `commit` (any other was answered before).
    pub(super) fn git_command(&mut self, conn: u64, device: &str, req: u64, command: PhoneCommand) {
        match command {
            PhoneCommand::Changes { session } => self.changes(conn, req, session),
            PhoneCommand::Diff { session, path } => self.diff(conn, req, &session, &path),
            PhoneCommand::Commit { session, message } => {
                self.commit(conn, device, req, session, message);
            }
            _ => {}
        }
    }

    /// Lists what session `session`'s workspace changed, in the background; the answer keeps
    /// the listing for this connection (see [`on_listed`](Self::on_listed)).
    fn changes(&mut self, conn: u64, req: u64, session: String) {
        let Some(place) = self.sessions.place(&session) else {
            return self.refuse(conn, req, Code::Unavailable, NO_SUCH_SESSION);
        };
        let slot = match self.git.begin() {
            Ok(slot) => slot,
            Err((code, reason)) => return self.refuse(conn, req, code, &reason),
        };
        let Some(ticket) = self.conns.get_mut(&conn).map(|c| c.listings.ticket()) else {
            return;
        };
        let (git, events) = (self.git.clone(), self.events.clone());
        tokio::spawn(async move {
            let result = git.changes(&place.workspace).await;
            drop(slot);
            let done = ConnEvent::Listed {
                conn,
                req,
                ticket,
                session,
                result,
            };
            let _ = events.send(done).await;
        });
    }

    /// A `changes` finished: the connection keeps the listing, which replaces its last one of
    /// the session unless a later `changes` already did, before the phone hears it. A refusal
    /// forgets an older one.
    pub(super) fn on_listed(
        &mut self,
        conn: u64,
        req: u64,
        ticket: u64,
        session: String,
        result: Result<git::Changes, git::Refusal>,
    ) {
        let Some(state) = self.conns.get_mut(&conn) else {
            return;
        };
        let frame = listed_answer(&mut state.listings, req, ticket, session, result);
        self.send(conn, &frame);
    }

    /// One file's diff, in the background; `path` must be one this connection's last `changes`
    /// of the session listed.
    fn diff(&mut self, conn: u64, req: u64, session: &str, path: &str) {
        if self.sessions.place(session).is_none() {
            return self.refuse(conn, req, Code::Unavailable, NO_SUCH_SESSION);
        }
        let target = self
            .conns
            .get(&conn)
            .and_then(|state| state.listings.get(session))
            .and_then(|listing| listing.target(path));
        let Some(target) = target else {
            return self.refuse(conn, req, Code::Invalid, ASK_FOR_CHANGES);
        };
        let slot = match self.git.begin() {
            Ok(slot) => slot,
            Err((code, reason)) => return self.refuse(conn, req, code, &reason),
        };
        let (git, events) = (self.git.clone(), self.events.clone());
        tokio::spawn(async move {
            let frame = match git.diff(&target).await {
                Ok(diff) => diff_frame(req, &diff),
                Err((code, reason)) => refused(req, code, &reason),
            };
            drop(slot);
            let done = ConnEvent::Answer {
                conn,
                frame,
                job: Job::Git,
                audit: None,
            };
            let _ = events.send(done).await;
        });
    }

    /// Commits everything in session `session`'s workspace, in the background, while no turn
    /// runs and no approval waits: checked now, and again by the commit just before it stages,
    /// against the latest state (see [`update_turns`](Self::update_turns)). The audit names the
    /// device and the session, never the message: `commit`, or `commit failed` when git was
    /// refused after `add` ran.
    fn commit(&mut self, conn: u64, device: &str, req: u64, session: String, message: String) {
        let Some(place) = self.sessions.place(&session) else {
            return self.refuse(conn, req, Code::Unavailable, NO_SUCH_SESSION);
        };
        if let Err((code, reason)) = may_commit(Some(&place)) {
            return self.refuse(conn, req, code, reason);
        }
        let slot = match self.git.begin() {
            Ok(slot) => slot,
            Err((code, reason)) => return self.refuse(conn, req, code, &reason),
        };
        let clear = fresh_turn(&mut self.turns, &session);
        let (git, events, device) = (self.git.clone(), self.events.clone(), device.to_owned());
        tokio::spawn(async move {
            let commit = git.commit(&place.workspace, &message, &clear).await;
            drop((slot, clear));
            let (frame, what) = match commit.result {
                Ok(hash) if hash.is_empty() => (ack(req, "Committed"), Some("commit")),
                Ok(hash) => (ack(req, &format!("Committed {hash}")), Some("commit")),
                Err((code, reason)) => (
                    refused(req, code, &reason),
                    commit.added.then_some("commit failed"),
                ),
            };
            let done = ConnEvent::Answer {
                conn,
                frame,
                job: Job::Git,
                audit: what.map(|what| (device, session, what.to_owned())),
            };
            let _ = events.send(done).await;
        });
    }

    /// Tells each commit under way whether its session may still commit, from the latest
    /// state; a session no commit waits on any more is let go.
    pub(super) fn update_turns(&mut self) {
        let sessions = &self.sessions;
        self.turns.retain(|session, clear| {
            clear.send_replace(may_commit(sessions.place(session).as_ref()).is_ok());
            clear.receiver_count() > 0
        });
    }
}

/// The refusal for a `changes` answer that a later `changes` on the connection overtook.
const SUPERSEDED: &str = "Superseded; ask again";

/// What the phone is told of a finished `changes`: the frame, once `listings` keeps its
/// listing; busy when a later `changes` on the connection already gave a newer one (the phone
/// waits on every req, so it hears); or the refusal.
fn listed_answer(
    listings: &mut Listings,
    req: u64,
    ticket: u64,
    session: String,
    result: Result<git::Changes, git::Refusal>,
) -> Value {
    match result {
        Ok(changes) => {
            if listings.keep(session, ticket, changes.listing) {
                changes_frame(req, &changes.info)
            } else {
                refused(req, Code::Busy, SUPERSEDED)
            }
        }
        Err((code, reason)) => {
            listings.forget(&session, ticket);
            refused(req, code, &reason)
        }
    }
}

/// A receiver for a commit to `session` starting now, just after its turn was found clear: the
/// sender says so again, so a false left from an earlier commit cannot refuse this one.
fn fresh_turn(
    turns: &mut HashMap<String, watch::Sender<bool>>,
    session: &str,
) -> watch::Receiver<bool> {
    let sender = turns
        .entry(session.to_owned())
        .or_insert_with(|| watch::channel(true).0);
    sender.send_replace(true);
    sender.subscribe()
}

/// Whether a session at `place` may commit: it is there, its bridge said its state, its phase
/// is idle or stopped, and no approval waits.
fn may_commit(place: Option<&Place>) -> Result<(), (Code, &'static str)> {
    let Some(place) = place else {
        return Err((Code::Unavailable, NO_SUCH_SESSION));
    };
    match place.phase.as_deref() {
        None => Err((Code::NotConnected, NOT_CONNECTED)),
        Some("idle" | "stopped") if place.approvals == 0 => Ok(()),
        Some(_) => Err((Code::TurnOpen, TURN_FIRST)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_keeps_the_last_listing_of_eight_sessions() {
        let mut listings = Listings::default();
        for n in 0..LISTED_SESSIONS {
            let ticket = listings.ticket();
            assert!(listings.keep(format!("s{n}"), ticket, Listing::default()));
        }
        // Listing s0 again makes it the most recent, so s1 goes first.
        let ticket = listings.ticket();
        listings.keep("s0".into(), ticket, Listing::default());
        let ticket = listings.ticket();
        listings.keep("s8".into(), ticket, Listing::default());
        assert_eq!(listings.by_session.len(), LISTED_SESSIONS);
        assert!(listings.get("s0").is_some());
        assert!(listings.get("s1").is_none());
        assert!(listings.get("s8").is_some());
        let ticket = listings.ticket();
        listings.forget("s8", ticket);
        assert!(listings.get("s8").is_none());
    }

    #[test]
    fn an_older_listing_never_replaces_a_newer_one() {
        let mut listings = Listings::default();
        let (older, newer) = (listings.ticket(), listings.ticket());
        assert!(listings.keep("s".into(), newer, Listing::default()));
        assert!(!listings.keep("s".into(), older, Listing::default()));
        assert_eq!(listings.by_session["s"].0, newer);
        // Nor does an older refusal forget it.
        listings.forget("s", older);
        assert!(listings.get("s").is_some());
    }

    #[test]
    fn an_answer_that_lost_to_a_newer_listing_is_refused() {
        let mut listings = Listings::default();
        let (older, newer) = (listings.ticket(), listings.ticket());
        let changes = || git::Changes {
            info: octet_core::remote::protocol::ChangesInfo::default(),
            listing: Listing::default(),
        };
        let frame = listed_answer(&mut listings, 2, newer, "s".into(), Ok(changes()));
        assert_eq!(frame["type"], "changes", "{frame}");
        let frame = listed_answer(&mut listings, 1, older, "s".into(), Ok(changes()));
        assert_eq!(frame, refused(1, Code::Busy, SUPERSEDED));
        assert_eq!(listings.by_session["s"].0, newer);
        // A refusal is the phone's answer whichever listing is kept.
        let refusal = (Code::Unavailable, "No".to_owned());
        let frame = listed_answer(&mut listings, 3, older, "s".into(), Err(refusal));
        assert_eq!(frame, refused(3, Code::Unavailable, "No"));
        assert!(listings.get("s").is_some());
    }

    #[test]
    fn a_new_commit_starts_with_its_turn_clear() {
        let mut turns = HashMap::new();
        let (stale, held) = watch::channel(false);
        turns.insert("s".to_owned(), stale);
        let clear = fresh_turn(&mut turns, "s");
        assert!(*clear.borrow());
        assert!(*held.borrow(), "every commit waiting on it sees the same");
        let other = fresh_turn(&mut turns, "t");
        assert!(*other.borrow());
    }
}
