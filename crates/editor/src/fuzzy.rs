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
    let mut scored: Vec<(T, i64)> = candidates
        .into_iter()
        .filter_map(|(text, item)| score(query, text).map(|s| (item, s)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.truncate(limit);
    scored
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
    fn case_insensitive_and_camel_case_boundaries() {
        assert!(score("bv", "BrowserView.rs").unwrap() > score("bv", "obviously.rs").unwrap());
    }
}
