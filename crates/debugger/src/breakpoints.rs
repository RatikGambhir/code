//! Line breakpoints: what the user set, independent of any debug session,
//! and keeping them on their line while the file is edited.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::protocol::SourceBreakpoint;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineBreakpoint {
    /// Zero-based.
    pub line: usize,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Stop only when this expression is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Stop on the nth hit (`3`, `>= 3`, `% 2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    /// Log this (with `{expr}` interpolated) instead of stopping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

fn yes() -> bool {
    true
}

impl LineBreakpoint {
    pub fn new(line: usize) -> Self {
        Self { line, enabled: true, condition: None, hit_condition: None, log_message: None }
    }

    pub fn is_conditional(&self) -> bool {
        self.condition.is_some() || self.hit_condition.is_some()
    }

    pub fn to_source_breakpoint(&self) -> SourceBreakpoint {
        SourceBreakpoint {
            line: self.line as u32 + 1,
            condition: self.condition.clone(),
            hit_condition: self.hit_condition.clone(),
            log_message: self.log_message.clone(),
        }
    }
}

/// Every breakpoint in a workspace, by file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakpointSet {
    files: BTreeMap<PathBuf, Vec<LineBreakpoint>>,
}

impl BreakpointSet {
    pub fn for_file(&self, path: &Path) -> &[LineBreakpoint] {
        self.files.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn get(&self, path: &Path, line: usize) -> Option<&LineBreakpoint> {
        self.for_file(path).iter().find(|b| b.line == line)
    }

    pub fn get_mut(&mut self, path: &Path, line: usize) -> Option<&mut LineBreakpoint> {
        self.files.get_mut(path)?.iter_mut().find(|b| b.line == line)
    }

    pub fn files(&self) -> impl Iterator<Item = (&Path, &[LineBreakpoint])> {
        self.files.iter().map(|(p, b)| (p.as_path(), b.as_slice()))
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    /// Adds a breakpoint on `line`, or removes the one there. Returns whether
    /// one is there now.
    pub fn toggle(&mut self, path: &Path, line: usize) -> bool {
        let list = self.files.entry(path.to_path_buf()).or_default();
        let added = match list.iter().position(|b| b.line == line) {
            Some(ix) => {
                list.remove(ix);
                false
            }
            None => {
                list.push(LineBreakpoint::new(line));
                list.sort_by_key(|b| b.line);
                true
            }
        };
        if list.is_empty() {
            self.files.remove(path);
        }
        added
    }

    pub fn insert(&mut self, path: &Path, breakpoint: LineBreakpoint) {
        let list = self.files.entry(path.to_path_buf()).or_default();
        list.retain(|b| b.line != breakpoint.line);
        list.push(breakpoint);
        list.sort_by_key(|b| b.line);
    }

    pub fn remove(&mut self, path: &Path, line: usize) {
        if let Some(list) = self.files.get_mut(path) {
            list.retain(|b| b.line != line);
            if list.is_empty() {
                self.files.remove(path);
            }
        }
    }

    pub fn clear(&mut self) {
        self.files.clear();
    }

    /// Moves `path`'s breakpoints with an edit from `old` to `new` text.
    /// Returns whether any moved or went away.
    pub fn reanchor(&mut self, path: &Path, old: &str, new: &str) -> bool {
        let Some(list) = self.files.get_mut(path) else { return false };
        let before = list.clone();
        let edit = LineEdit::between(old, new);
        let mut moved: Vec<LineBreakpoint> = list
            .drain(..)
            .filter_map(|mut b| {
                b.line = edit.map(b.line)?;
                Some(b)
            })
            .collect();
        moved.sort_by_key(|b| b.line);
        moved.dedup_by_key(|b| b.line);
        let changed = moved != before;
        if moved.is_empty() {
            self.files.remove(path);
        } else {
            *list = moved;
        }
        changed
    }
}

/// One contiguous change between two versions of a text, in lines: the
/// lines before `start` and the last `suffix` lines are the same in both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub start: usize,
    pub old_len: usize,
    pub new_len: usize,
    pub suffix: usize,
}

impl LineEdit {
    pub fn between(old: &str, new: &str) -> Self {
        let old: Vec<&str> = old.split('\n').collect();
        let new: Vec<&str> = new.split('\n').collect();
        let start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let max_suffix = old.len().min(new.len()) - start;
        let suffix = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take(max_suffix)
            .take_while(|(a, b)| a == b)
            .count();
        Self { start, old_len: old.len(), new_len: new.len(), suffix }
    }

    /// Where old `line` is in the new text; `None` if it was deleted.
    pub fn map(&self, line: usize) -> Option<usize> {
        let old_changed_end = self.old_len - self.suffix;
        let new_changed_end = self.new_len - self.suffix;
        if line < self.start {
            Some(line)
        } else if line >= old_changed_end {
            Some(line + new_changed_end - old_changed_end)
        } else if new_changed_end > self.start {
            // An edited line stays where it is, within the edited lines.
            Some(line.min(new_changed_end - 1))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn toggles() {
        let mut set = BreakpointSet::default();
        let path = Path::new("/w/src/main.rs");
        assert!(set.toggle(path, 4));
        assert!(set.toggle(path, 1));
        assert_eq!(set.for_file(path).iter().map(|b| b.line).collect::<Vec<_>>(), [1, 4]);
        assert!(!set.toggle(path, 4));
        assert!(!set.toggle(path, 1));
        assert!(set.is_empty());
    }

    #[test]
    fn follows_inserted_and_deleted_lines() {
        let path = Path::new("/w/a.rs");
        let mut set = BreakpointSet::default();
        set.toggle(path, 5);
        let old = lines(10);

        // Two lines inserted above.
        let mut new: Vec<String> = old.split('\n').map(str::to_string).collect();
        new.insert(2, "a".into());
        new.insert(2, "b".into());
        let new = new.join("\n");
        assert!(set.reanchor(path, &old, &new));
        assert_eq!(set.for_file(path)[0].line, 7);

        // Typing on the breakpoint's own line keeps it there.
        let typed = new.replace("line 5", "line 5 changed");
        assert!(!set.reanchor(path, &new, &typed));
        assert_eq!(set.for_file(path)[0].line, 7);

        // Enter at the start of the line pushes it down.
        let pushed = typed.replace("line 5 changed", "\nline 5 changed");
        set.reanchor(path, &typed, &pushed);
        assert_eq!(set.for_file(path)[0].line, 8);

        // Deleting the line removes the breakpoint.
        let deleted = pushed.replace("line 5 changed\n", "");
        set.reanchor(path, &pushed, &deleted);
        assert!(set.for_file(path).is_empty());
    }

    #[test]
    fn edits_below_leave_it_alone() {
        let edit = LineEdit::between(&lines(10), &lines(12));
        assert_eq!(edit.map(3), Some(3));
        let edit = LineEdit::between("a\nb\nc", "a\nb\nc\n");
        assert_eq!(edit.map(2), Some(2));
    }

    #[test]
    fn round_trips_json() {
        let mut set = BreakpointSet::default();
        set.insert(
            Path::new("/w/a.rs"),
            LineBreakpoint { condition: Some("x > 2".into()), ..LineBreakpoint::new(3) },
        );
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(serde_json::from_str::<BreakpointSet>(&json).unwrap(), set);
        assert_eq!(set.get(Path::new("/w/a.rs"), 3).unwrap().to_source_breakpoint().line, 4);
    }
}
