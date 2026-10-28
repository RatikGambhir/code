//! Workspace text search, respecting `.gitignore`.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{Context as _, Result};
use regex::{Regex, RegexBuilder};

#[derive(Clone, Debug, Default)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Treat the query as a regular expression rather than literal text.
    pub regex: bool,
    pub max_results: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    /// Relative to the search root.
    pub path: PathBuf,
    /// 1-based.
    pub line_number: usize,
    pub line_text: String,
    /// Byte range of the match within `line_text`.
    pub range: std::ops::Range<usize>,
}

pub fn build_regex(query: &str, options: &SearchOptions) -> Result<Regex> {
    let mut pattern = if options.regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    if options.whole_word {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .build()
        .context("invalid search pattern")
}

/// Searches every non-ignored text file under `root`. `cancel` lets the UI
/// abandon a search when the query changes.
pub fn search(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<SearchMatch>> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let regex = build_regex(query, options)?;
    let max_results = if options.max_results == 0 {
        1000
    } else {
        options.max_results
    };

    let mut results = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walker.filter_map(Result::ok) {
        if cancel.load(Ordering::Relaxed) || results.len() >= max_results {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(0) > 4 * 1024 * 1024 {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if crate::document::looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_path_buf();
        for (index, line) in text.lines().enumerate() {
            if let Some(m) = regex.find(line) {
                // Keep very long lines (minified files) readable in the list.
                let (line_text, range) = clip_line(line, m.range(), 240);
                results.push(SearchMatch {
                    path: relative.clone(),
                    line_number: index + 1,
                    line_text,
                    range,
                });
                if results.len() >= max_results {
                    break;
                }
            }
        }
    }
    Ok(results)
}

/// Shortens `line` around `range` to roughly `max` bytes, on char boundaries.
fn clip_line(line: &str, range: std::ops::Range<usize>, max: usize) -> (String, std::ops::Range<usize>) {
    if line.len() <= max {
        return (line.to_string(), range);
    }
    let mut start = range.start.saturating_sub(max / 3);
    while !line.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + max).min(line.len()).max(range.end.min(line.len()));
    while !line.is_char_boundary(end) {
        end += 1;
    }
    let clipped = line[start..end].to_string();
    (clipped, range.start - start..range.end - start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run(root: &Path, query: &str, options: SearchOptions) -> Vec<SearchMatch> {
        search(root, query, &options, Arc::new(AtomicBool::new(false))).unwrap()
    }

    #[test]
    fn finds_matches_and_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/gen.rs"), "fn needle() {}\n").unwrap();
        fs::write(root.join("lib.rs"), "fn hay() {}\nfn Needle() {}\n").unwrap();
        fs::write(root.join("blob.bin"), b"needle\0").unwrap();

        let results = run(root, "needle", SearchOptions::default());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, PathBuf::from("lib.rs"));
        assert_eq!(results[0].line_number, 2);
        assert_eq!(&results[0].line_text[results[0].range.clone()], "Needle");

        let sensitive = run(root, "needle", SearchOptions { case_sensitive: true, ..Default::default() });
        assert!(sensitive.is_empty());
    }

    #[test]
    fn whole_word_and_regex() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "cat\ncatalog\nconcat\n").unwrap();
        let words = run(dir.path(), "cat", SearchOptions { whole_word: true, ..Default::default() });
        assert_eq!(words.len(), 1);
        let regex = run(dir.path(), "^con", SearchOptions { regex: true, ..Default::default() });
        assert_eq!(regex.len(), 1);
        assert_eq!(regex[0].line_text, "concat");
        // Literal mode escapes regex syntax.
        assert!(run(dir.path(), "c.t", SearchOptions::default()).is_empty());
    }

    #[test]
    fn invalid_regex_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let options = SearchOptions { regex: true, ..Default::default() };
        assert!(search(dir.path(), "(", &options, Arc::new(AtomicBool::new(false))).is_err());
    }

    #[test]
    fn long_lines_are_clipped_around_the_match() {
        let line = format!("{}needle{}", "x".repeat(1000), "y".repeat(1000));
        let start = line.find("needle").unwrap();
        let (clipped, range) = clip_line(&line, start..start + 6, 240);
        assert!(clipped.len() <= 260);
        assert_eq!(&clipped[range], "needle");
    }
}
