//! `@agent` mentions in the composer. `@claude fix the build` starts a new
//! Claude thread, the way mentioning someone in a chat channel pulls them in.
//! The OpenAI panel has two: `@codex` (the coding agent) and `@chatgpt`
//! (ChatGPT itself, on the user's plan).

use std::ops::Range;

use cockpit_agents::Provider;

/// Someone who can be mentioned.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Handle {
    Codex,
    ChatGpt,
    Claude,
}

impl Handle {
    pub const ALL: [Handle; 3] = [Handle::Codex, Handle::ChatGpt, Handle::Claude];

    /// What is typed after `@`.
    pub fn name(self) -> &'static str {
        match self {
            Handle::Codex => "codex",
            Handle::ChatGpt => "chatgpt",
            Handle::Claude => "claude",
        }
    }

    /// The panel (header switch) the handle belongs to.
    pub fn provider(self) -> Provider {
        match self {
            Handle::Codex | Handle::ChatGpt => Provider::OpenAi,
            Handle::Claude => Provider::Anthropic,
        }
    }

    /// One line for the suggestion list.
    pub fn description(self) -> &'static str {
        match self {
            Handle::Codex => "Start a Codex agent in this workspace",
            Handle::ChatGpt => "Ask ChatGPT, on your ChatGPT plan",
            Handle::Claude => "Start a Claude agent in this workspace",
        }
    }

    /// The handles offered in `provider`'s panel, the main one first.
    pub fn for_provider(provider: Provider) -> &'static [Handle] {
        match provider {
            Provider::OpenAi => &[Handle::Codex, Handle::ChatGpt],
            Provider::Anthropic => &[Handle::Claude],
        }
    }
}

/// The longest handle, to stop scanning early.
const MAX_HANDLE_LEN: usize = 7;

/// The main handle typed after `@` for `provider`.
pub fn handle(provider: Provider) -> &'static str {
    Handle::for_provider(provider)[0].name()
}

/// A message that mentions an agent.
#[derive(Debug, PartialEq, Eq)]
pub struct Mention<'a> {
    pub handle: Handle,
    /// What to send: the message without a leading mention.
    pub prompt: &'a str,
    /// Where `@handle` is in the text given to [`parse`] (for highlighting).
    pub range: Range<usize>,
}

/// Finds an `@claude`, `@codex` or `@chatgpt` mention anywhere in `text`. A
/// leading mention is dropped from the prompt (`@claude: fix it` sends
/// `fix it`); one inside a sentence stays, where it reads naturally.
pub fn parse(text: &str) -> Option<Mention<'_>> {
    let lead = text.len() - text.trim_start().len();
    let trimmed = text.trim();
    trimmed.match_indices('@').find_map(|(at, _)| {
        let (handle, end) = handle_at(trimmed, at)?;
        let prompt = if at == 0 {
            trimmed[end..].trim_start_matches([',', ':', ';']).trim()
        } else {
            trimmed
        };
        Some(Mention {
            handle,
            prompt,
            range: lead + at..lead + end,
        })
    })
}

/// The handle being typed just before `cursor` (a byte offset): `""` right
/// after an `@`, `"cl"` for `@cl`. `None` when the cursor is not in one.
pub fn partial_before_cursor(text: &str, cursor: usize) -> Option<&str> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let partial = &before[at + 1..];
    let fits = partial.len() <= MAX_HANDLE_LEN && partial.bytes().all(|b| b.is_ascii_alphabetic());
    (fits && starts_word(before, at)).then_some(partial)
}

/// `provider`'s handles that `partial` begins, the main one first.
pub fn completions(provider: Provider, partial: &str) -> Vec<Handle> {
    Handle::for_provider(provider)
        .iter()
        .copied()
        .filter(|handle| rest_of(*handle, partial).is_some())
        .collect()
}

/// The rest of `handle`'s name after `partial`, if `partial` begins it.
pub fn rest_of(handle: Handle, partial: &str) -> Option<&'static str> {
    let name = handle.name();
    let head = name.get(..partial.len())?;
    head.eq_ignore_ascii_case(partial).then(|| &name[partial.len()..])
}

/// The handle whose name follows the `@` at `text[at]`, and the byte offset
/// where the name ends.
fn handle_at(text: &str, at: usize) -> Option<(Handle, usize)> {
    if !starts_word(text, at) {
        return None;
    }
    let rest = &text[at + 1..];
    Handle::ALL.into_iter().find_map(|handle| {
        let name = handle.name();
        let typed = rest.get(..name.len())?;
        (typed.eq_ignore_ascii_case(name) && ends_handle(&rest[name.len()..]))
            .then_some((handle, at + 1 + name.len()))
    })
}

/// `text[at]` begins a word: it starts the text or follows whitespace, so
/// `me@claude.ai` is not a mention.
fn starts_word(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_none_or(char::is_whitespace)
}

/// A handle ends at the end of the text, at whitespace, or at punctuation
/// that ends a sentence or clause (`@claude.ai` and `@claudette` do not).
fn ends_handle(after: &str) -> bool {
    let mut chars = after.chars();
    match chars.next() {
        None => true,
        Some(c) if c.is_whitespace() => true,
        Some(',' | ':' | ';' | '!' | '?') => true,
        Some('.') => chars.next().is_none_or(char::is_whitespace),
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention(text: &str) -> Option<(Handle, &str)> {
        parse(text).map(|m| (m.handle, m.prompt))
    }

    #[test]
    fn leading_mentions_are_removed_from_the_prompt() {
        assert_eq!(mention("@claude fix the build"), Some((Handle::Claude, "fix the build")));
        assert_eq!(mention("  @Claude: fix it "), Some((Handle::Claude, "fix it")));
        assert_eq!(mention("@codex, run the tests"), Some((Handle::Codex, "run the tests")));
        assert_eq!(mention("@chatgpt what is a monad?"), Some((Handle::ChatGpt, "what is a monad?")));
        assert_eq!(mention("@claude\nline one\nline two"), Some((Handle::Claude, "line one\nline two")));
    }

    #[test]
    fn mentions_know_where_they_are() {
        let text = "  @chatgpt hi";
        let m = parse(text).unwrap();
        assert_eq!(&text[m.range.clone()], "@chatgpt");
        let text = "ask @codex to look";
        assert_eq!(&text[parse(text).unwrap().range], "@codex");
    }

    #[test]
    fn a_bare_mention_has_an_empty_prompt() {
        assert_eq!(mention("@claude"), Some((Handle::Claude, "")));
        assert_eq!(mention("@chatgpt "), Some((Handle::ChatGpt, "")));
    }

    #[test]
    fn mentions_inside_a_sentence_keep_the_whole_message() {
        assert_eq!(
            mention("could @claude look at this?"),
            Some((Handle::Claude, "could @claude look at this?"))
        );
        assert_eq!(mention("thanks @claude."), Some((Handle::Claude, "thanks @claude.")));
    }

    #[test]
    fn addresses_domains_and_longer_words_are_not_mentions() {
        assert_eq!(mention("mail me@claude.ai"), None);
        assert_eq!(mention("see @claude.ai for docs"), None);
        assert_eq!(mention("@claudette hi"), None);
        assert_eq!(mention("@chatgpts hi"), None);
        assert_eq!(mention("no mention here"), None);
        assert_eq!(mention("@ claude"), None);
    }

    #[test]
    fn partial_handles_are_found_before_the_cursor() {
        assert_eq!(partial_before_cursor("@", 1), Some(""));
        assert_eq!(partial_before_cursor("hi @cl", 6), Some("cl"));
        assert_eq!(partial_before_cursor("hi @cl there", 6), Some("cl"));
        assert_eq!(partial_before_cursor("@chatgpt", 8), Some("chatgpt"), "seven letters fit");
        assert_eq!(partial_before_cursor("@claude fix", 11), None, "the handle is finished");
        assert_eq!(partial_before_cursor("me@cl", 5), None, "inside a word");
        assert_eq!(partial_before_cursor("é@", 1), None, "not a character boundary");
        assert_eq!(partial_before_cursor("no at sign", 10), None);
    }

    #[test]
    fn completions_follow_the_typed_prefix() {
        assert_eq!(completions(Provider::OpenAi, ""), vec![Handle::Codex, Handle::ChatGpt]);
        assert_eq!(completions(Provider::OpenAi, "c"), vec![Handle::Codex, Handle::ChatGpt]);
        assert_eq!(completions(Provider::OpenAi, "CH"), vec![Handle::ChatGpt]);
        assert_eq!(completions(Provider::OpenAi, "co"), vec![Handle::Codex]);
        assert_eq!(completions(Provider::OpenAi, "cl"), vec![]);
        assert_eq!(completions(Provider::Anthropic, "cl"), vec![Handle::Claude]);
        assert_eq!(rest_of(Handle::Claude, "CL"), Some("aude"));
        assert_eq!(rest_of(Handle::Claude, "claude"), Some(""));
        assert_eq!(rest_of(Handle::Codex, "codexx"), None);
    }
}
