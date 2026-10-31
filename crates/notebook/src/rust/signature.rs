//! Finding the function at the cursor in a Rust file and reading its
//! signature. This is a line scanner, not a parser: it handles the way
//! functions are normally written (signatures over several lines, `impl`
//! blocks, inline modules) and gives up quietly on the rest.

use cockpit_debugger::cargo::{fn_name, strip_comments};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// `pub`
    Public,
    /// `pub(crate)`, `pub(super)`, `pub(in …)`
    Restricted,
    Private,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Receiver {
    /// `self`, `mut self`, `self: Box<Self>` and friends.
    Value,
    /// `&self`
    Ref,
    /// `&mut self`
    RefMut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    /// The binding name (`input`), or `argN` for patterns.
    pub name: String,
    /// The type as written (`&'a str`).
    pub ty: String,
}

/// The `impl` block a function is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplBlock {
    /// The type's name without generics (`Parser`), or `None` when it is not
    /// a plain path (`impl<T> Trait for &T`).
    pub self_type: Option<String>,
    /// The type as written (`Parser<'a>`).
    pub self_type_text: String,
    /// The trait, for `impl Trait for Type`.
    pub trait_name: Option<String>,
    /// Generic parameter names of the block (`T`, `'a` is left out).
    pub generics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnSignature {
    pub name: String,
    /// Zero-based line of the `fn`.
    pub line: usize,
    pub visibility: Visibility,
    pub is_async: bool,
    pub is_unsafe: bool,
    /// Generic parameter names (`T`); lifetimes are left out.
    pub generics: Vec<String>,
    pub receiver: Option<Receiver>,
    pub params: Vec<Param>,
    /// The return type, if not `()`.
    pub ret: Option<String>,
    /// Inline modules around the function, outermost first.
    pub modules: Vec<String>,
    pub impl_block: Option<ImplBlock>,
    /// Inside a `trait` definition (a default method).
    pub in_trait: Option<String>,
    /// The signature as written, on one line (`pub fn parse(input: &str) -> Token`).
    pub text: String,
}

#[derive(Clone, Debug)]
enum ScopeKind {
    Module(String),
    Impl(ImplBlock),
    Trait(String),
    /// Index into the function list.
    Function(usize),
    Other,
}

#[derive(Clone, Debug)]
struct Scope {
    kind: ScopeKind,
    /// Brace depth outside the scope.
    depth: i32,
}

/// A scope whose header was seen and whose `{` has not been reached.
#[derive(Clone, Debug)]
enum Pending {
    Module(String),
    /// The header text so far.
    Impl(String),
    Trait(String),
    /// The function index and its signature text so far.
    Function(usize, String),
}

struct Found {
    sig_start: usize,
    end: Option<usize>,
    nested_in_fn: bool,
    modules: Vec<String>,
    impl_block: Option<ImplBlock>,
    in_trait: Option<String>,
    text: String,
}

/// The function whose signature or body holds `cursor_line` (zero-based),
/// or whose signature starts just below it after only attributes and doc
/// comments. Functions nested in other functions are skipped for the outer
/// one, since they cannot be called from outside.
pub fn function_at(text: &str, cursor_line: usize) -> Option<FnSignature> {
    let lines: Vec<&str> = text.lines().collect();
    let found = scan(&lines);
    let callable = || found.iter().filter(|f| !f.nested_in_fn);
    let containing = callable().find(|f| f.sig_start <= cursor_line && f.end.is_some_and(|end| cursor_line <= end));
    let chosen = containing.or_else(|| {
        // On an attribute or doc comment above a function.
        callable().find(|f| {
            f.sig_start > cursor_line
                && lines[cursor_line..f.sig_start].iter().all(|line| {
                    let t = line.trim();
                    t.is_empty() || t.starts_with("#[") || t.starts_with("//")
                })
        })
    })?;
    to_signature(chosen)
}

/// Every function in the file that code outside it could call by path: not
/// nested in another function, in file order.
pub fn functions(text: &str) -> Vec<FnSignature> {
    let lines: Vec<&str> = text.lines().collect();
    scan(&lines).iter().filter(|f| !f.nested_in_fn).filter_map(to_signature).collect()
}

fn to_signature(found: &Found) -> Option<FnSignature> {
    parse_signature(&found.text).map(|parts| FnSignature {
        line: found.sig_start,
        modules: found.modules.clone(),
        impl_block: found.impl_block.clone(),
        in_trait: found.in_trait.clone(),
        text: collapse_whitespace(&found.text),
        ..parts
    })
}

fn scan(lines: &[&str]) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    let mut scopes: Vec<Scope> = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut depth: i32 = 0;
    let mut in_block_comment = false;

    for (line_ix, raw) in lines.iter().enumerate() {
        let code = strip_comments(raw, &mut in_block_comment);
        let trimmed = code.trim();
        if pending.is_none() && !trimmed.starts_with("#[") {
            pending = scope_header(trimmed, line_ix, &scopes, &mut found);
        }
        let header_start = match &pending {
            // The header begins at its keyword on the line it starts.
            Some(Pending::Function(ix, text)) if text.is_empty() && found[*ix].sig_start == line_ix => {
                code.len() - code.trim_start().len()
            }
            _ => 0,
        };
        for (byte, c) in code.char_indices() {
            match c {
                '{' => {
                    let kind = match pending.take() {
                        Some(Pending::Module(name)) => ScopeKind::Module(name),
                        Some(Pending::Impl(mut header)) => {
                            header.push_str(&code[..byte]);
                            ScopeKind::Impl(parse_impl_header(&header))
                        }
                        Some(Pending::Trait(name)) => ScopeKind::Trait(name),
                        Some(Pending::Function(ix, mut text)) => {
                            if byte >= header_start {
                                text.push_str(&code[header_start.min(byte)..byte]);
                            }
                            found[ix].text = text;
                            ScopeKind::Function(ix)
                        }
                        None => ScopeKind::Other,
                    };
                    scopes.push(Scope { kind, depth });
                    depth += 1;
                }
                '}' => {
                    depth -= 1;
                    while scopes.last().is_some_and(|s| s.depth >= depth) {
                        if let Some(Scope { kind: ScopeKind::Function(ix), .. }) = scopes.pop() {
                            found[ix].end = Some(line_ix);
                        }
                    }
                }
                ';' => {
                    // `mod x;`, or a trait method without a body.
                    match pending.take() {
                        // `[T; N]` in a parameter type is not the end.
                        Some(Pending::Function(ix, text))
                            if !brackets_balanced(&format!("{text}{}", &code[header_start.min(byte)..byte])) =>
                        {
                            pending = Some(Pending::Function(ix, text));
                        }
                        Some(Pending::Function(ix, mut text)) => {
                            text.push_str(&code[header_start.min(byte)..byte]);
                            found[ix].text = text;
                            found[ix].end = Some(line_ix);
                        }
                        Some(other @ (Pending::Impl(_) | Pending::Trait(_))) => pending = Some(other),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        // A header that continues on the next line.
        match &mut pending {
            Some(Pending::Function(ix, text)) => {
                let start = if found[*ix].sig_start == line_ix && text.is_empty() { header_start } else { 0 };
                text.push_str(&code[start..]);
                text.push(' ');
            }
            Some(Pending::Impl(header)) => {
                header.push_str(&code);
                header.push(' ');
            }
            _ => {}
        }
    }
    found
}

/// Starts a pending scope for a `mod`, `impl`, `trait` or `fn` header.
fn scope_header(trimmed: &str, line_ix: usize, scopes: &[Scope], found: &mut Vec<Found>) -> Option<Pending> {
    if trimmed.is_empty() {
        return None;
    }
    if fn_name(trimmed).is_some() {
        let nested_in_fn = scopes.iter().any(|s| matches!(s.kind, ScopeKind::Function(_)));
        let modules = scopes
            .iter()
            .filter_map(|s| match &s.kind {
                ScopeKind::Module(name) => Some(name.clone()),
                _ => None,
            })
            .collect();
        let impl_block = scopes.iter().rev().find_map(|s| match &s.kind {
            ScopeKind::Impl(block) => Some(block.clone()),
            _ => None,
        });
        let in_trait = scopes.iter().rev().find_map(|s| match &s.kind {
            ScopeKind::Trait(name) => Some(name.clone()),
            _ => None,
        });
        found.push(Found {
            sig_start: line_ix,
            end: None,
            nested_in_fn,
            modules,
            impl_block,
            in_trait,
            text: String::new(),
        });
        return Some(Pending::Function(found.len() - 1, String::new()));
    }
    let words = strip_visibility(trimmed);
    let words = words.strip_prefix("unsafe ").unwrap_or(words).trim_start();
    let words = words.strip_prefix("default ").unwrap_or(words).trim_start();
    if let Some(rest) = words.strip_prefix("mod ") {
        return Some(Pending::Module(identifier(rest)?.to_string()));
    }
    if words.starts_with("impl ") || words.starts_with("impl<") {
        return Some(Pending::Impl(String::new()));
    }
    if let Some(rest) = words.strip_prefix("trait ") {
        return Some(Pending::Trait(identifier(rest)?.to_string()));
    }
    None
}

/// Whether every `(` and `[` in `text` is closed.
fn brackets_balanced(text: &str) -> bool {
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

/// `trimmed` without a leading `pub`/`pub(…)`.
fn strip_visibility(trimmed: &str) -> &str {
    if let Some(rest) = trimmed.strip_prefix("pub(") {
        return rest.find(')').map_or(rest, |end| rest[end + 1..].trim_start());
    }
    trimmed.strip_prefix("pub ").map_or(trimmed, str::trim_start)
}

fn identifier(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let end = text.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(text.len());
    (end > 0).then(|| &text[..end])
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Reads `impl<…> [Trait for] Type<…> [where …]` (the text before `{`).
fn parse_impl_header(header: &str) -> ImplBlock {
    let header = collapse_whitespace(header);
    let header = header.trim_end_matches('{').trim();
    let after_impl = header.find("impl").map_or(header, |i| &header[i + 4..]);
    let (generics_text, rest) = split_leading_generics(after_impl.trim_start());
    let rest = rest.split(" where ").next().unwrap_or(rest).trim();
    let (trait_name, self_text) = match split_top_level(rest, " for ") {
        Some((t, s)) => (Some(t.trim().to_string()), s.trim()),
        None => (None, rest),
    };
    let self_type = plain_path(self_text);
    ImplBlock {
        self_type,
        self_type_text: self_text.to_string(),
        trait_name,
        generics: generic_names(generics_text),
    }
}

/// `<T: X, 'a>rest` → (`T: X, 'a`, `rest`).
fn split_leading_generics(text: &str) -> (&str, &str) {
    if !text.starts_with('<') {
        return ("", text);
    }
    match matching_close(text, 0) {
        Some(end) => (&text[1..end], text[end + 1..].trim_start()),
        None => ("", text),
    }
}

/// The names declared in a generics list (`T: Clone, 'a, const N: usize`).
fn generic_names(generics: &str) -> Vec<String> {
    split_commas(generics)
        .into_iter()
        .filter_map(|part| {
            let part = part.trim();
            if part.starts_with('\'') {
                return None;
            }
            let part = part.strip_prefix("const ").unwrap_or(part);
            identifier(part).map(str::to_string)
        })
        .collect()
}

/// `Parser<'a>` → `Parser`, `crate::a::B` → `crate::a::B`; `None` for
/// references, tuples, slices and trait objects.
fn plain_path(text: &str) -> Option<String> {
    let base = text.split('<').next().unwrap_or(text).trim();
    let ok = !base.is_empty()
        && base.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ':')
        && base.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && base != "dyn"
        && base != "impl";
    ok.then(|| base.to_string())
}

/// The index of the bracket closing the one at `open` (`<`, `(`, `[`, `{`),
/// counting all bracket kinds together and not counting the `>` of `->`.
fn matching_close(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' if i > 0 && bytes[i - 1] == b'-' => {}
            b'>' | b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits at commas outside brackets.
fn split_commas(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' if i > 0 && bytes[i - 1] == b'-' => {}
            b'>' | b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if !text[start..].trim().is_empty() {
        parts.push(&text[start..]);
    }
    parts
}

/// Splits at the first `sep` outside brackets.
fn split_top_level<'a>(text: &'a str, sep: &str) -> Option<(&'a str, &'a str)> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' if i > 0 && bytes[i - 1] == b'-' => {}
            b'>' | b')' | b']' | b'}' => depth -= 1,
            _ if depth == 0 && text[i..].starts_with(sep) => return Some((&text[..i], &text[i + sep.len()..])),
            _ => {}
        }
    }
    None
}

/// Reads `[vis] [const] [async] [unsafe] [extern "C"] fn name<…>(…) [-> T] [where …]`.
/// Module and impl context are filled in by the caller.
fn parse_signature(text: &str) -> Option<FnSignature> {
    let text = collapse_whitespace(text);
    let fn_at = find_word(&text, "fn")?;
    let qualifiers = &text[..fn_at];
    let visibility = if qualifiers.starts_with("pub(") {
        Visibility::Restricted
    } else if qualifiers.starts_with("pub ") {
        Visibility::Public
    } else {
        Visibility::Private
    };
    let has = |word: &str| qualifiers.split_whitespace().any(|w| w == word);
    let after_fn = text[fn_at + 2..].trim_start();
    let name = identifier(after_fn)?.to_string();
    let rest = after_fn[name.len()..].trim_start();
    let (generics_text, rest) = split_leading_generics(rest);
    if !rest.starts_with('(') {
        return None;
    }
    let close = matching_close(rest, 0)?;
    let params_text = &rest[1..close];
    let after_params = rest[close + 1..].trim_start();
    let ret = after_params.strip_prefix("->").map(|r| {
        let r = r.trim();
        split_top_level(r, " where ").map_or(r, |(before, _)| before).trim().to_string()
    });
    let ret = ret.filter(|r| r != "()" && !r.is_empty());

    let mut receiver = None;
    let mut params = Vec::new();
    for (index, part) in split_commas(params_text).into_iter().enumerate() {
        let part = part.trim();
        if index == 0 {
            if let Some(kind) = receiver_kind(part) {
                receiver = Some(kind);
                continue;
            }
        }
        let (pattern, ty) = match split_top_level(part, ":") {
            // `a::b` is a path, not a type ascription; only take a lone `:`.
            Some((pattern, ty)) if !ty.starts_with(':') => (pattern.trim(), ty.trim()),
            _ => continue,
        };
        let pattern = pattern.strip_prefix("mut ").unwrap_or(pattern).trim();
        let name = match identifier(pattern) {
            Some(id) if id == pattern && id != "_" => id.to_string(),
            _ => format!("arg{}", params.len() + 1),
        };
        params.push(Param { name, ty: ty.to_string() });
    }

    Some(FnSignature {
        name,
        line: 0,
        visibility,
        is_async: has("async"),
        is_unsafe: has("unsafe"),
        generics: generic_names(generics_text),
        receiver,
        params,
        ret,
        modules: Vec::new(),
        impl_block: None,
        in_trait: None,
        text: String::new(),
    })
}

fn receiver_kind(param: &str) -> Option<Receiver> {
    let p = collapse_whitespace(param);
    let p = p.as_str();
    if matches!(p, "self" | "mut self") || ["self:", "self :", "mut self:", "mut self :"].iter().any(|s| p.starts_with(s)) {
        return Some(Receiver::Value);
    }
    let rest = p.strip_prefix('&')?.trim_start();
    // Drop a lifetime: `&'a mut self`.
    let rest = if rest.starts_with('\'') { rest.split_once(' ')?.1 } else { rest };
    match rest.trim() {
        "self" => Some(Receiver::Ref),
        "mut self" => Some(Receiver::RefMut),
        _ => None,
    }
}

/// The byte index of `word` standing alone in `text`.
fn find_word(text: &str, word: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(found) = text[from..].find(word) {
        let at = from + found;
        let before_ok = at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let end = at + word.len();
        let after_ok = end == bytes.len() || !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_');
        if before_ok && after_ok {
            return Some(at);
        }
        from = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
use std::fmt;

/// Parses one line.
#[inline]
pub fn parse_line<'a>(input: &'a str, mut limit: usize) -> Result<Token<'a>, Error> {
    let s = "{";
    fn helper() {}
    todo!()
}

fn private_one() {}

pub(crate) async unsafe fn odd(
    (a, b): (u8, u8),
    _: Vec<HashMap<String, u32>>,
    f: impl Fn(u8) -> u8,
) where
    u8: Copy,
{
}

pub mod inner {
    pub struct Parser<'a> { text: &'a str }

    impl<'a> Parser<'a> {
        pub fn new(text: &'a str) -> Self { Parser { text } }

        pub fn next_token(&mut self) -> Option<Token<'a>> {
            None
        }
    }

    impl fmt::Display for Parser<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) }
    }
}

pub fn generic<T: Clone, const N: usize>(items: [T; N]) -> Vec<T> { items.to_vec() }
"#;

    fn at(line_text: &str) -> FnSignature {
        let line = FILE.lines().position(|l| l.contains(line_text)).expect("line in fixture");
        function_at(FILE, line).unwrap_or_else(|| panic!("no function at {line_text:?}"))
    }

    #[test]
    fn reads_a_free_function() {
        let f = at("let s = ");
        assert_eq!(f.name, "parse_line");
        assert_eq!(f.visibility, Visibility::Public);
        assert_eq!(
            f.params,
            vec![
                Param { name: "input".into(), ty: "&'a str".into() },
                Param { name: "limit".into(), ty: "usize".into() }
            ]
        );
        assert_eq!(f.ret.as_deref(), Some("Result<Token<'a>, Error>"));
        assert!(f.generics.is_empty());
        assert!(f.modules.is_empty() && f.impl_block.is_none());
        assert_eq!(f.text, "pub fn parse_line<'a>(input: &'a str, mut limit: usize) -> Result<Token<'a>, Error>");
        // A nested fn belongs to the outer one; attributes above count too.
        assert_eq!(at("fn helper").name, "parse_line");
        assert_eq!(at("#[inline]").name, "parse_line");
        assert_eq!(at("/// Parses").name, "parse_line");
    }

    #[test]
    fn reads_multi_line_signatures_and_patterns() {
        let f = at("_: Vec<HashMap");
        assert_eq!(f.name, "odd");
        assert_eq!(f.visibility, Visibility::Restricted);
        assert!(f.is_async && f.is_unsafe);
        let names: Vec<_> = f.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["arg1", "arg2", "f"]);
        assert_eq!(f.params[2].ty, "impl Fn(u8) -> u8");
        assert_eq!(f.ret, None);
        assert_eq!(at("fn private_one").visibility, Visibility::Private);
    }

    #[test]
    fn reads_methods_in_modules() {
        let f = at("None");
        assert_eq!(f.name, "next_token");
        assert_eq!(f.receiver, Some(Receiver::RefMut));
        assert_eq!(f.modules, ["inner"]);
        let block = f.impl_block.unwrap();
        assert_eq!(block.self_type.as_deref(), Some("Parser"));
        assert_eq!(block.generics, Vec::<String>::new());
        assert_eq!(block.trait_name, None);

        let new = at("pub fn new");
        assert_eq!(new.receiver, None);
        assert_eq!(new.ret.as_deref(), Some("Self"));

        let display = at("fn fmt(");
        assert_eq!(display.impl_block.unwrap().trait_name.as_deref(), Some("fmt::Display"));
        assert_eq!(display.receiver, Some(Receiver::Ref));
    }

    #[test]
    fn reads_generics() {
        let f = at("pub fn generic");
        assert_eq!(f.generics, ["T", "N"]);
        assert_eq!(f.params[0].ty, "[T; N]");
    }

    #[test]
    fn nothing_outside_functions() {
        let line = FILE.lines().position(|l| l.contains("use std::fmt")).unwrap();
        assert_eq!(function_at(FILE, line), None);
    }

    #[test]
    fn lists_the_files_functions() {
        let names: Vec<String> = functions(FILE).into_iter().map(|f| f.name).collect();
        // `helper` is nested in `parse_line`, so it is not listed.
        assert_eq!(names, ["parse_line", "private_one", "odd", "new", "next_token", "fmt", "generic"]);
    }

    #[test]
    fn receivers() {
        assert_eq!(receiver_kind("self"), Some(Receiver::Value));
        assert_eq!(receiver_kind("mut self"), Some(Receiver::Value));
        assert_eq!(receiver_kind("&self"), Some(Receiver::Ref));
        assert_eq!(receiver_kind("&'a mut self"), Some(Receiver::RefMut));
        assert_eq!(receiver_kind("self: Box<Self>"), Some(Receiver::Value));
        assert_eq!(receiver_kind("selfish: u8"), None);
    }
}
