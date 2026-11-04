//! The thread feed: which threads show, in what order, and what a collapsed
//! card says about each. The panel draws it.
//!
//! Rendering runs on every frame that changes anything (a cursor blink
//! included), so card summaries are cached per transcript revision and only
//! the open thread's transcript is walked in full.

use std::collections::{HashMap, HashSet};

use cockpit_agents::{
    protocol::{ItemStatus, TurnStatus},
    ApprovalState, Entry, EntryKind,
};
use gpui::SharedString;

use super::controller::ThreadSession;

/// Threads shown in the feed. Older ones stay in the thread picker and the
/// chat history, and show here again when opened.
pub const FEED_LIMIT: usize = 50;
/// Characters of preview on a collapsed card.
const PREVIEW_CHARS: usize = 160;
/// Cached cards for threads that left the feed, before they are dropped.
const CACHE_SLACK: usize = 16;

/// Indices into `threads` in feed order: oldest first, so new threads land
/// at the bottom and replies never move a card. The `FEED_LIMIT` newest,
/// plus `open` at the top when it is older than those.
pub fn order(threads: &[ThreadSession], open: Option<&str>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..threads.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        let (a, b) = (&threads[a], &threads[b]);
        (a.created_at, a.seq, &a.id).cmp(&(b.created_at, b.seq, &b.id))
    });
    let cut = order.len().saturating_sub(FEED_LIMIT);
    let older_open = open.and_then(|id| order[..cut].iter().copied().find(|&ix| threads[ix].id == id));
    order.drain(..cut);
    if let Some(ix) = older_open {
        order.insert(0, ix);
    }
    order
}

/// Where a thread stands, for its card's badge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CardStatus {
    NeedsApproval,
    Working,
    Queued,
    Loading,
    Done,
    Stopped,
    Failed,
    /// Nothing ran yet, or a saved thread that was not opened.
    Idle,
}

pub fn status(thread: &ThreadSession, needs_approval: bool, queued: usize) -> CardStatus {
    if needs_approval {
        CardStatus::NeedsApproval
    } else if thread.transcript.is_running() {
        CardStatus::Working
    } else if queued > 0 {
        CardStatus::Queued
    } else if thread.loading {
        CardStatus::Loading
    } else {
        match thread.transcript.last_status() {
            Some(TurnStatus::Completed) => CardStatus::Done,
            Some(TurnStatus::Interrupted) => CardStatus::Stopped,
            Some(TurnStatus::Failed) => CardStatus::Failed,
            Some(TurnStatus::InProgress | TurnStatus::Unknown) | None => CardStatus::Idle,
        }
    }
}

/// What a collapsed card says about a transcript.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CardFacts {
    /// The latest thing the agent said or did since the last prompt.
    pub preview: Option<String>,
    /// Commands and tool calls.
    pub steps: usize,
    /// Distinct files changed.
    pub files: usize,
    pub needs_approval: bool,
}

pub fn facts(entries: &[Entry]) -> CardFacts {
    let mut steps = 0;
    let mut files = HashSet::new();
    let mut needs_approval = false;
    for entry in entries {
        match &entry.kind {
            EntryKind::Command { .. } | EntryKind::Tool { .. } => steps += 1,
            EntryKind::FileChange { files: changed, .. } => files.extend(changed.iter().map(|f| f.path.as_str())),
            EntryKind::Approval { state: ApprovalState::Pending, .. } => needs_approval = true,
            _ => {}
        }
    }
    CardFacts {
        preview: preview(entries),
        steps,
        files: files.len(),
        needs_approval,
    }
}

/// The newest entry worth a line, looking back only to the last prompt.
fn preview(entries: &[Entry]) -> Option<String> {
    entries.iter().rev().find_map(|entry| {
        let line = match &entry.kind {
            // Stop at the prompt: what came before answers an older one.
            EntryKind::User { .. } => return Some(None),
            EntryKind::Agent { text } => first_line(text).trim_start_matches(['#', '>', '-', '*', ' ']).to_string(),
            EntryKind::Command { command, status, .. } => {
                let verb = if *status == ItemStatus::InProgress { "Running" } else { "Ran" };
                format!("{verb} {}", first_line(command))
            }
            EntryKind::FileChange { files, status } => {
                let first = files.first()?;
                let verb = if *status == ItemStatus::InProgress { "Editing" } else { "Edited" };
                match files.len() {
                    1 => format!("{verb} {}", first.path),
                    n => format!("{verb} {} and {} more", first.path, n - 1),
                }
            }
            EntryKind::Tool { label, detail, .. } => match detail {
                Some(detail) => format!("{label} {}", first_line(detail)),
                None => label.clone(),
            },
            EntryKind::Approval { state: ApprovalState::Pending, .. } => "Waiting for your approval".into(),
            EntryKind::Error { message } => first_line(message).to_string(),
            EntryKind::Reasoning { .. } if entry.streaming => "Thinking…".into(),
            EntryKind::Approval { .. } | EntryKind::Reasoning { .. } | EntryKind::Plan { .. } | EntryKind::Notice { .. } => {
                return None;
            }
        };
        let line = line.trim();
        (!line.is_empty()).then(|| Some(cockpit_common::truncate_chars(line, PREVIEW_CHARS)))
    })?
}

fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// A card's summary, kept until its transcript changes.
pub struct Card {
    /// (revision, entry count) it was built from. The count catches a
    /// transcript replaced by a resume that happens to share a revision.
    key: (u64, usize),
    /// The thread id, shared so the card's element id costs no allocation
    /// per frame.
    pub thread_id: SharedString,
    pub preview: Option<SharedString>,
    /// "3 steps · 2 files".
    pub meta: Option<SharedString>,
    pub needs_approval: bool,
}

impl Card {
    fn new(thread: &ThreadSession) -> Self {
        let mut card = Self {
            key: (u64::MAX, 0),
            thread_id: SharedString::from(thread.id.clone()),
            preview: None,
            meta: None,
            needs_approval: false,
        };
        card.update(thread);
        card
    }

    fn update(&mut self, thread: &ThreadSession) {
        let key = (thread.transcript.revision(), thread.transcript.entries().len());
        if key == self.key {
            return;
        }
        let facts = facts(thread.transcript.entries());
        self.key = key;
        self.preview = facts.preview.map(SharedString::from);
        self.meta = meta(facts.steps, facts.files).map(SharedString::from);
        self.needs_approval = facts.needs_approval;
    }
}

fn meta(steps: usize, files: usize) -> Option<String> {
    let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (steps, files) {
        (0, 0) => None,
        (steps, 0) => Some(plural(steps, "step", "steps")),
        (0, files) => Some(plural(files, "file", "files")),
        (steps, files) => Some(format!("{} · {}", plural(steps, "step", "steps"), plural(files, "file", "files"))),
    }
}

/// Card summaries by thread id.
#[derive(Default)]
pub struct Cards(HashMap<String, Card>);

impl Cards {
    /// Brings the cards of the threads in `order` up to date.
    pub fn refresh(&mut self, threads: &[ThreadSession], order: &[usize]) {
        for thread in order.iter().map(|&ix| &threads[ix]) {
            match self.0.get_mut(&thread.id) {
                Some(card) => card.update(thread),
                None => {
                    self.0.insert(thread.id.clone(), Card::new(thread));
                }
            }
        }
        if self.0.len() > order.len() + CACHE_SLACK {
            let shown: HashSet<&str> = order.iter().map(|&ix| threads[ix].id.as_str()).collect();
            self.0.retain(|id, _| shown.contains(id.as_str()));
        }
    }

    pub fn get(&self, thread_id: &str) -> Option<&Card> {
        self.0.get(thread_id)
    }
}

#[cfg(test)]
mod tests {
    use cockpit_agents::{transcript::ChangedFile, Transcript};

    use super::*;

    fn thread(id: &str, created_at: i64, seq: u64) -> ThreadSession {
        ThreadSession {
            chatgpt: false,
            id: id.into(),
            title: id.to_string().into(),
            model: None,
            transcript: Transcript::new(),
            created_at,
            seq,
            updated_at: created_at,
            loaded: true,
            loading: false,
        }
    }

    fn entry(kind: EntryKind) -> Entry {
        Entry { id: String::new(), turn_id: None, kind, streaming: false }
    }

    fn command(command: &str, status: ItemStatus) -> Entry {
        entry(EntryKind::Command {
            command: command.into(),
            cwd: String::new(),
            output: String::new(),
            status,
            exit_code: None,
            duration_ms: None,
        })
    }

    fn file_change(paths: &[&str]) -> Entry {
        entry(EntryKind::FileChange {
            files: paths
                .iter()
                .map(|p| ChangedFile { path: (*p).into(), change: "modified".into(), diff: String::new() })
                .collect(),
            status: ItemStatus::Completed,
        })
    }

    #[test]
    fn feed_is_oldest_first_and_stable_within_a_second() {
        // Most recently active first, as the controller keeps them.
        let threads = vec![thread("b", 10, 2), thread("c", 20, 3), thread("a", 10, 1)];
        let ids: Vec<&str> = order(&threads, None).iter().map(|&ix| threads[ix].id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn feed_keeps_the_newest_and_an_older_open_thread() {
        let threads: Vec<ThreadSession> = (0..FEED_LIMIT as i64 + 5).map(|n| thread(&format!("t{n}"), n, 0)).collect();

        let shown = order(&threads, None);
        assert_eq!(shown.len(), FEED_LIMIT);
        assert_eq!(threads[shown[0]].id, "t5", "the five oldest are left out");

        let shown = order(&threads, Some("t1"));
        assert_eq!(shown.len(), FEED_LIMIT + 1);
        assert_eq!(threads[shown[0]].id, "t1", "an open older thread shows first");
    }

    #[test]
    fn facts_count_steps_files_and_pending_approvals() {
        let entries = vec![
            entry(EntryKind::User { text: "fix it".into() }),
            command("cargo test", ItemStatus::Completed),
            file_change(&["src/a.rs", "src/b.rs"]),
            file_change(&["src/a.rs"]),
            entry(EntryKind::Tool { label: "Read".into(), detail: None, success: Some(true) }),
        ];
        let facts = facts(&entries);
        assert_eq!((facts.steps, facts.files, facts.needs_approval), (2, 2, false));
        assert_eq!(facts.preview.as_deref(), Some("Read"));
    }

    #[test]
    fn preview_shows_the_latest_line_since_the_last_prompt() {
        let answered = vec![
            entry(EntryKind::User { text: "fix it".into() }),
            command("cargo build\n--release", ItemStatus::InProgress),
            entry(EntryKind::Agent { text: "\n## Fixed the build\nDetails".into() }),
            entry(EntryKind::Notice { text: "Auto-approved".into() }),
        ];
        assert_eq!(facts(&answered).preview.as_deref(), Some("Fixed the build"));

        let running = &answered[..2];
        assert_eq!(facts(running).preview.as_deref(), Some("Running cargo build"));

        let waiting = vec![
            entry(EntryKind::Agent { text: "Done before".into() }),
            entry(EntryKind::User { text: "and now?".into() }),
        ];
        assert_eq!(facts(&waiting).preview, None, "an older answer is not this prompt's");
    }

    #[test]
    fn previews_are_cut_to_length() {
        let long = "x".repeat(PREVIEW_CHARS * 2);
        let preview = facts(&[entry(EntryKind::Agent { text: long })]).preview.unwrap_or_default();
        assert!(preview.chars().count() <= PREVIEW_CHARS + 1);
    }

    #[test]
    fn meta_counts_read_naturally() {
        assert_eq!(meta(0, 0), None);
        assert_eq!(meta(1, 0).as_deref(), Some("1 step"));
        assert_eq!(meta(0, 3).as_deref(), Some("3 files"));
        assert_eq!(meta(4, 1).as_deref(), Some("4 steps · 1 file"));
    }

    #[test]
    fn status_prefers_what_needs_the_user() {
        let mut t = thread("a", 0, 0);
        assert_eq!(status(&t, false, 0), CardStatus::Idle);
        assert_eq!(status(&t, false, 2), CardStatus::Queued);
        assert_eq!(status(&t, true, 2), CardStatus::NeedsApproval);
        t.loading = true;
        assert_eq!(status(&t, false, 0), CardStatus::Loading);
    }

    #[test]
    fn cards_rebuild_only_when_the_transcript_changes() {
        let mut threads = vec![thread("a", 0, 0)];
        let mut cards = Cards::default();
        cards.refresh(&threads, &[0]);
        assert_eq!(cards.get("a").map(|c| c.preview.clone()), Some(None));

        threads[0].transcript.push_error("boom");
        cards.refresh(&threads, &[0]);
        assert_eq!(cards.get("a").and_then(|c| c.preview.as_ref()).map(|p| p.as_ref()), Some("boom"));
    }
}
