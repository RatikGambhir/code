//! A small fuzzy matcher for quick-open and the command palette.
//!
//! Scores subsequence matches, rewarding consecutive characters, matches at
//! word boundaries and matches in the file name over the directory part.

/// Returns a score (higher is better) or `None` if `query` is not a
/// subsequence of `candidate`. Matching is case-insensitive.
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    // Paths and identifiers are nearly always ASCII; scoring their bytes
    // in place gives the same result without two allocations per
    // candidate (quick-open scores every file of the workspace per key).
    if query.is_ascii() && candidate.is_ascii() {
        return score_ascii(query.as_bytes(), candidate.as_bytes());
    }
    score_chars(query, candidate)
}

/// [`score`] for ASCII text.
fn score_ascii(query: &[u8], candidate: &[u8]) -> Option<i64> {
    let name_start = candidate.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    let mut score = 0i64;
    let mut position = 0usize;
    let mut previous_match: Option<usize> = None;
    for q in query.iter().map(u8::to_ascii_lowercase) {
        if q == b' ' {
            continue;
        }
        let found = position + candidate[position..].iter().position(|b| b.to_ascii_lowercase() == q)?;
        score += 10;
        if previous_match == Some(found.wrapping_sub(1)) {
            score += 15;
        }
        let at_boundary = found == 0
            || matches!(candidate[found - 1], b'/' | b'_' | b'-' | b'.' | b' ')
            || (candidate[found].is_ascii_uppercase() && candidate[found - 1].is_ascii_lowercase());
        if at_boundary {
            score += 20;
        }
        if found >= name_start {
            score += 5;
        }
        score -= (found - position) as i64;
        previous_match = Some(found);
        position = found + 1;
    }
    Some(score * 100 - candidate.len() as i64)
}

/// [`score`] for any text.
fn score_chars(query: &str, candidate: &str) -> Option<i64> {
    let candidate_chars: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = candidate_chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let name_start = candidate
        .rfind('/')
        .map(|i| candidate[..=i].chars().count())
        .unwrap_or(0);

    let mut score = 0i64;
    let mut position = 0usize;
    let mut previous_match: Option<usize> = None;
    for q in query.chars().flat_map(char::to_lowercase) {
        if q == ' ' {
            continue;
        }
        let found = (position..lower.len()).find(|&i| lower[i] == q)?;
        score += 10;
        if previous_match == Some(found.wrapping_sub(1)) {
            score += 15;
        }
        let at_boundary = found == 0
            || matches!(candidate_chars[found - 1], '/' | '_' | '-' | '.' | ' ')
            || (candidate_chars[found].is_uppercase() && candidate_chars[found - 1].is_lowercase());
        if at_boundary {
            score += 20;
        }
        if found >= name_start {
            score += 5;
        }
        // Gaps cost a little, so tighter matches win.
        score -= (found - position) as i64;
        previous_match = Some(found);
        position = found + 1;
    }
    // Shorter candidates win ties.
    Some(score * 100 - candidate_chars.len() as i64)
}

/// Ranks `candidates` against `query`, best first, keeping at most `limit`.
pub fn rank<'a, T>(
    query: &str,
    candidates: impl IntoIterator<Item = (&'a str, T)>,
    limit: usize,
) -> Vec<(T, i64)> {
    let scored: Vec<(T, i64)> = candidates
        .into_iter()
        .filter_map(|(text, item)| score(query, text).map(|s| (item, s)))
        .collect();
    best(scored, limit)
}

/// The `limit` best of `scored`, best first; equal scores keep their order.
/// Only the kept ones are sorted, not every match.
pub fn best<T>(scored: Vec<(T, i64)>, limit: usize) -> Vec<(T, i64)> {
    let mut keyed: Vec<(usize, (T, i64))> = scored.into_iter().enumerate().collect();
    let key = |(ix, (_, score)): &(usize, (T, i64))| (std::cmp::Reverse(*score), *ix);
    if keyed.len() > limit && limit > 0 {
        keyed.select_nth_unstable_by_key(limit - 1, key);
    }
    keyed.truncate(limit);
    keyed.sort_unstable_by_key(key);
    keyed.into_iter().map(|(_, scored)| scored).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_required() {
        assert!(score("mrs", "src/main.rs").is_some());
        assert!(score("xyz", "src/main.rs").is_none());
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn file_name_and_boundary_matches_rank_higher() {
        let ranked = rank(
            "main",
            [
                ("src/domain/ignore.rs", 1),
                ("src/main.rs", 2),
                ("crates/app/src/main.rs", 3),
            ],
            10,
        );
        assert_eq!(ranked[0].0, 2, "{ranked:?}");
        assert_eq!(ranked.last().unwrap().0, 1);
    }

    #[test]
    fn ascii_scoring_matches_the_general_path() {
        let candidates = [
            "src/main.rs",
            "crates/ui/src/BrowserView.rs",
            "a_b-c.d e/F",
            "README.md",
            "x",
            "",
        ];
        for query in ["m", "mrs", "bv", "BV", "a c", "readme", "zz", "ss", "/"] {
            for candidate in candidates {
                assert_eq!(
                    score_ascii(query.as_bytes(), candidate.as_bytes()),
                    score_chars(query, candidate),
                    "{query:?} in {candidate:?}"
                );
            }
        }
    }

    #[test]
    fn best_keeps_order_among_equals_and_limits() {
        let scored = vec![("a", 1), ("b", 3), ("c", 3), ("d", 2), ("e", 3)];
        assert_eq!(best(scored.clone(), 2), vec![("b", 3), ("c", 3)]);
        assert_eq!(best(scored.clone(), 10), vec![("b", 3), ("c", 3), ("e", 3), ("d", 2), ("a", 1)]);
        assert!(best(scored, 0).is_empty());
    }

    #[test]
    fn case_insensitive_and_camel_case_boundaries() {
        assert!(score("bv", "BrowserView.rs").unwrap() > score("bv", "obviously.rs").unwrap());
    }
}
