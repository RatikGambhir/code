//! Rust cells: the first cell written from a function's signature, and the
//! program a cell runs as.
//!
//! A cell is the body of `main`. Its last expression (if it does not end in
//! `;`) is the value shown after the run. It is reported from the same
//! statement it is computed in, so it may borrow locals and temporaries of
//! the cell.

use super::signature::{FnSignature, Receiver, Visibility};

/// The first cell for a function, and what to know before running it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scaffold {
    pub cell: String,
    pub notes: Vec<String>,
    /// The function's path (`demo::parse::Parser::next`).
    pub subject: String,
}

/// Writes a cell that calls `sig` with `todo!()` for every argument.
/// `crate_name` is the library's name in code, `file_modules` the module
/// path of the function's file within the crate.
pub fn scaffold(sig: &FnSignature, crate_name: &str, file_modules: &[String], tokio: bool) -> Scaffold {
    let mut module_path = vec![crate_name.to_string()];
    module_path.extend(file_modules.iter().cloned());
    module_path.extend(sig.modules.iter().cloned());
    let mut notes = Vec::new();

    let impl_block = sig.impl_block.as_ref();
    let self_type = impl_block.and_then(|b| b.self_type.clone());
    let mut generics = sig.generics.clone();
    if let Some(block) = impl_block {
        generics.extend(block.generics.iter().cloned());
    }

    let owner = match (&self_type, impl_block, &sig.in_trait) {
        (Some(ty), _, _) => Some(ty.clone()),
        (None, Some(block), _) => {
            notes.push(format!(
                "This method is on `{}`, which a notebook can't name for you: replace the `todo!()` for the receiver with a value of that type.",
                block.self_type_text
            ));
            None
        }
        (None, None, Some(trait_name)) => {
            notes.push(format!(
                "`{}` is a default method of the `{trait_name}` trait: call it on a type that implements `{trait_name}`.",
                sig.name
            ));
            None
        }
        (None, None, None) => None,
    };

    let subject = {
        let mut parts = module_path.clone();
        match (&owner, &sig.in_trait) {
            (Some(ty), _) => parts.push(ty.clone()),
            (None, Some(trait_name)) => parts.push(trait_name.clone()),
            _ => {}
        }
        parts.push(sig.name.clone());
        parts.join("::")
    };

    match sig.visibility {
        Visibility::Public => {}
        Visibility::Restricted | Visibility::Private => notes.push(format!(
            "`{}` isn't `pub`, so code outside `{crate_name}` can't call it and the build will fail. Make it `pub` (along with its module), or open a notebook on a public function that uses it.",
            sig.name
        )),
    }
    if let Some(trait_name) = impl_block.and_then(|b| b.trait_name.as_ref()) {
        notes.push(format!(
            "This is a `{trait_name}` method. If the cell says the method isn't found, add a `use` line for `{trait_name}`."
        ));
    }
    if !generics.is_empty() {
        notes.push(format!(
            "Generic parameters ({}) take their types from the values you pass.",
            generics.iter().map(|g| format!("`{g}`")).collect::<Vec<_>>().join(", ")
        ));
    }
    if sig.is_async && !tokio {
        notes.push(format!(
            "`{}` is async. The cell runs it with a minimal executor, so code that needs a Tokio runtime will panic.",
            sig.name
        ));
    }

    let mut lines = vec![format!("use {}::*;", module_path.join("::")), String::new()];
    let mut taken: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();

    let receiver_var = sig.receiver.map(|kind| {
        let base = owner.as_deref().map(|ty| snake_case(ty.rsplit("::").next().unwrap_or(ty)));
        let name = match base {
            Some(name) if !name.is_empty() && !taken.contains(&name.as_str()) && !is_keyword(&name) => name,
            _ => "receiver".to_string(),
        };
        let annotation = impl_block
            .filter(|_| owner.is_some())
            .map(|b| anonymize_lifetimes(&b.self_type_text))
            .filter(|ty| !mentions_any(ty, &generics));
        let mutability = if kind == Receiver::RefMut { "mut " } else { "" };
        lines.push(match annotation {
            Some(ty) => format!("let {mutability}{name}: {ty} = todo!();"),
            None => format!("let {mutability}{name} = todo!();"),
        });
        name
    });
    if let Some(name) = &receiver_var {
        taken.push(name);
    }

    let mut args = Vec::new();
    for param in &sig.params {
        let ty = match &owner {
            Some(owner) => replace_word(&param.ty, "Self", owner),
            None => param.ty.clone(),
        };
        let ty = anonymize_lifetimes(&ty);
        let (binding, arg) = binding_for(&param.name, &ty, &generics);
        lines.push(binding);
        args.push(arg);
    }
    if receiver_var.is_some() || !sig.params.is_empty() {
        lines.push(String::new());
    }

    let args = args.join(", ");
    let mut call = match (&receiver_var, &owner) {
        (Some(var), _) => format!("{var}.{}({args})", sig.name),
        (None, Some(ty)) => format!("{ty}::{}({args})", sig.name),
        (None, None) if impl_block.is_some() || sig.in_trait.is_some() => format!("Type::{}({args})", sig.name),
        (None, None) => format!("{}({args})", sig.name),
    };
    if sig.is_async {
        call = format!("__hangar::block_on({call})");
    }
    if sig.is_unsafe {
        call = format!("unsafe {{ {call} }}");
    }
    lines.push(call);

    Scaffold { cell: lines.join("\n"), notes, subject }
}

/// The `let` for one argument and how to pass it. References to sized types
/// bind an owned value and pass a borrow of it, so the cell can build it.
fn binding_for(name: &str, ty: &str, generics: &[String]) -> (String, String) {
    if ty.contains("impl ") || mentions_any(ty, generics) {
        return (format!("let {name} = todo!(); // {ty}"), name.to_string());
    }
    if let Some(referent) = ty.strip_prefix("&mut ") {
        let referent = referent.trim();
        if is_unsized(referent) {
            return (format!("let {name}: {ty} = todo!();"), name.to_string());
        }
        return (format!("let mut {name}: {referent} = todo!();"), format!("&mut {name}"));
    }
    if let Some(referent) = ty.strip_prefix('&') {
        let referent = referent.trim();
        if is_unsized(referent) {
            return (format!("let {name}: {ty} = todo!();"), name.to_string());
        }
        return (format!("let {name}: {referent} = todo!();"), format!("&{name}"));
    }
    (format!("let {name}: {ty} = todo!();"), name.to_string())
}

fn is_unsized(ty: &str) -> bool {
    let last = ty.rsplit("::").next().unwrap_or(ty);
    ty.starts_with('[') || ty.starts_with("dyn ") || matches!(last, "str" | "Path" | "OsStr" | "CStr")
}

/// Lifetimes other than `'static` become `'_`, which a `let` accepts; `&'_ T`
/// becomes `&T`.
fn anonymize_lifetimes(ty: &str) -> String {
    let mut out = String::with_capacity(ty.len());
    let mut chars = ty.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        while let Some(&next) = chars.peek() {
            if next.is_alphanumeric() || next == '_' {
                name.push(next);
                chars.next();
            } else {
                break;
            }
        }
        if name == "static" {
            out.push_str("'static");
        } else {
            out.push_str("'_");
        }
    }
    out.replace("&'_ ", "&")
}

fn mentions_any(ty: &str, words: &[String]) -> bool {
    words.iter().any(|w| contains_word(ty, w))
}

fn contains_word(text: &str, word: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

fn replace_word(text: &str, word: &str, with: &str) -> String {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (at, _) in text.match_indices(word) {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            out.push_str(&text[copied..at]);
            out.push_str(with);
            copied = at + word.len();
        }
    }
    out.push_str(&text[copied..]);
    out
}

fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "as" | "break" | "const" | "continue" | "crate" | "else" | "enum" | "extern" | "false" | "fn" | "for" | "if"
            | "impl" | "in" | "let" | "loop" | "match" | "mod" | "move" | "mut" | "pub" | "ref" | "return" | "self"
            | "static" | "struct" | "super" | "trait" | "true" | "type" | "unsafe" | "use" | "where" | "while"
            | "async" | "await" | "dyn" | "box" | "try" | "yield" | "gen"
    )
}

/// The program for a cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub source: String,
    pub lines: LineMap,
}

/// Which line of the cell a line of `main.rs` came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineMap {
    /// The 1-based line of `main.rs` holding the cell's first line.
    pub cell_first_line: usize,
    /// The line of `main.rs` where a line was added before the cell's last
    /// expression (to report its value). Lines after it are one further on.
    pub inserted_line: Option<usize>,
}

impl LineMap {
    /// The 1-based cell line for a 1-based `main.rs` line, or `None` outside
    /// the cell.
    pub fn cell_line(&self, main_line: usize) -> Option<usize> {
        // The added line itself stands for the line after it.
        let shift = usize::from(self.inserted_line.is_some_and(|inserted| main_line > inserted));
        let line = main_line.checked_sub(self.cell_first_line + shift)? + 1;
        Some(line)
    }
}

const PRELUDE: &str = "\
// Generated by Hangar for a notebook cell. It is rewritten on every run.
#![allow(unused, unreachable_code, clippy::all)]

use __hangar::{ShowDebug as _, ShowTypeName as _};

fn main() -> ::std::result::Result<(), ::std::boxed::Box<dyn ::std::error::Error>> {
";

/// `main.rs` for `cell`. With `tokio`, async calls run on a Tokio runtime.
pub fn program(cell: &str, tokio: bool) -> Program {
    let mut source = String::from(PRELUDE);
    let cell_first_line = PRELUDE.lines().count() + 1;
    let mut inserted_line = None;
    match split_tail(cell) {
        Some((start, end)) => {
            let line_start = cell[..start].rfind('\n').map_or(0, |n| n + 1);
            if cell[line_start..start].trim().is_empty() {
                // The wrapper goes on lines of its own, so compiler messages
                // about the expression show it as written.
                source.push_str(&cell[..line_start]);
                inserted_line = Some(cell_first_line + cell[..line_start].matches('\n').count());
                source.push_str("(&&__hangar::Show(&(\n");
                source.push_str(&cell[line_start..end]);
                source.push_str("\n))).show();");
            } else {
                source.push_str(&cell[..start]);
                source.push_str("(&&__hangar::Show(&(");
                source.push_str(&cell[start..end]);
                source.push_str("))).show();");
            }
            source.push_str(&cell[end..]);
        }
        None => source.push_str(cell),
    }
    if !source.ends_with('\n') {
        source.push('\n');
    }
    source.push_str("    #[allow(unreachable_code)]\n    ::std::result::Result::Ok(())\n}\n");
    source.push_str(&helpers(tokio));
    Program { source, lines: LineMap { cell_first_line, inserted_line } }
}

/// The `__hangar` module: reporting the cell's value, and `block_on`.
pub(crate) fn helpers(tokio: bool) -> String {
    let block_on = if tokio {
        "
    pub fn block_on<F: ::std::future::Future>(future: F) -> F::Output {
        ::tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect(\"starting a Tokio runtime\")
            .block_on(future)
    }"
    } else {
        "
    struct ThreadWaker(::std::thread::Thread);

    impl ::std::task::Wake for ThreadWaker {
        fn wake(self: ::std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }

    pub fn block_on<F: ::std::future::Future>(future: F) -> F::Output {
        let mut future = ::std::pin::pin!(future);
        let waker = ::std::task::Waker::from(::std::sync::Arc::new(ThreadWaker(::std::thread::current())));
        let mut cx = ::std::task::Context::from_waker(&waker);
        loop {
            if let ::std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
            ::std::thread::park();
        }
    }"
    };
    format!(
        "
mod __hangar {{
    pub struct Show<'a, T>(pub &'a T);

    /// Picked for values that implement `Debug` (method lookup tries
    /// `&Show` before `Show`).
    pub trait ShowDebug {{
        fn show(&self);
    }}

    impl<T: ::std::fmt::Debug> ShowDebug for &Show<'_, T> {{
        fn show(&self) {{
            report(::std::any::type_name::<T>(), Some(format!(\"{{:#?}}\", self.0)));
        }}
    }}

    pub trait ShowTypeName {{
        fn show(&self);
    }}

    impl<T> ShowTypeName for Show<'_, T> {{
        fn show(&self) {{
            report(::std::any::type_name::<T>(), None);
        }}
    }}

    fn report(type_name: &str, text: Option<String>) {{
        let Some(path) = ::std::env::var_os(\"{RESULT_ENV}\") else {{ return }};
        let body = match text {{
            Some(text) => format!(\"{{type_name}}\\ndebug\\n{{text}}\"),
            None => format!(\"{{type_name}}\\nopaque\\n\"),
        }};
        let _ = ::std::fs::write(path, body);
    }}
{block_on}
}}
"
    )
}

/// The variable the program writes its result file's path to.
pub const RESULT_ENV: &str = "HANGAR_NOTEBOOK_RESULT";

/// Reads the result file a cell's program wrote.
pub fn parse_result(text: &str) -> Option<crate::Value> {
    let (type_name, rest) = text.split_once('\n')?;
    let (kind, value) = rest.split_once('\n').unwrap_or((rest, ""));
    if type_name == "()" {
        return None;
    }
    Some(crate::Value {
        type_name: type_name.to_string(),
        text: (kind == "debug").then(|| value.to_string()),
    })
}

/// The byte range of the cell's last expression: what follows the last
/// statement, if it does not end in `;` and is not an item. Strings,
/// characters and comments are skipped; a `}` ends a statement unless what
/// follows continues the expression (`.`, `?`, `else`, an operator).
pub fn split_tail(cell: &str) -> Option<(usize, usize)> {
    let bytes = cell.as_bytes();
    let mut depth = 0i32;
    let mut statement_end = 0usize;
    // Where the current statement's first and last significant bytes are.
    let mut first_sig: Option<usize> = None;
    let mut last_sig_end = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        // Comments.
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i = skip_block_comment(bytes, i);
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let end = match b {
            b'"' => skip_string(bytes, i),
            b'r' if is_raw_string(bytes, i) => skip_raw_string(bytes, i),
            b'b' if bytes.get(i + 1) == Some(&b'"') => skip_string(bytes, i + 1),
            b'\'' => skip_char_or_lifetime(bytes, i),
            _ => {
                match b {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' => depth -= 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                i + 1
            }
        };
        if first_sig.is_none() {
            first_sig = Some(start);
        }
        last_sig_end = end;
        i = end;
        let ends_statement = depth == 0
            && match b {
                b';' => true,
                b'}' => !continues_expression(&cell[end..]),
                _ => false,
            };
        if ends_statement {
            statement_end = end;
            first_sig = None;
        }
    }
    let start = first_sig?;
    if start < statement_end || last_sig_end <= start {
        return None;
    }
    let tail = &cell[start..last_sig_end];
    let item_words = [
        "fn ", "struct ", "enum ", "impl ", "impl<", "use ", "mod ", "trait ", "const ", "static ", "type ", "let ",
        "pub ", "#[", "macro_rules!", "extern ", "unsafe fn ", "async fn ",
    ];
    if item_words.iter().any(|w| tail.starts_with(w)) {
        return None;
    }
    Some((start, last_sig_end))
}

/// Whether `rest` (after a `}`) carries on the same expression.
fn continues_expression(rest: &str) -> bool {
    let rest = skip_trivia(rest);
    let starts_word = |w: &str| {
        rest.strip_prefix(w)
            .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    };
    rest.starts_with(['.', '?', ';', ')', ']', ',', '+', '-', '*', '/', '%', '&', '|', '^', '<', '>', '=', '!'])
        || starts_word("else")
        || starts_word("as")
}

fn skip_trivia(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim_start();
        if let Some(rest) = trimmed.strip_prefix("//") {
            text = rest.find('\n').map_or("", |n| &rest[n..]);
        } else if trimmed.starts_with("/*") {
            let end = skip_block_comment(trimmed.as_bytes(), 0);
            text = &trimmed[end..];
        } else {
            return trimmed;
        }
    }
}

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0;
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// From the opening `"`, past the closing one.
fn skip_string(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

fn is_raw_string(bytes: &[u8], at: usize) -> bool {
    let preceded_by_ident = at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
    let mut i = at + 1;
    while bytes.get(i) == Some(&b'#') {
        i += 1;
    }
    !preceded_by_ident && bytes.get(i) == Some(&b'"')
}

fn skip_raw_string(bytes: &[u8], at: usize) -> usize {
    let mut i = at + 1;
    let mut hashes = 0;
    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    i += 1; // the opening quote
    while i < bytes.len() {
        if bytes[i] == b'"' && bytes[i + 1..].iter().take(hashes).filter(|&&b| b == b'#').count() == hashes {
            return i + 1 + hashes;
        }
        i += 1;
    }
    bytes.len()
}

/// A char literal (`'a'`, `'\n'`, `'\u{1F600}'`) or a lifetime (`'a`).
fn skip_char_or_lifetime(bytes: &[u8], open: usize) -> usize {
    if bytes.get(open + 1) == Some(&b'\\') {
        let mut i = open + 2;
        while i < bytes.len() && bytes[i] != b'\'' {
            i += 1;
        }
        return (i + 1).min(bytes.len());
    }
    // One character, which may be several bytes.
    let mut i = open + 1;
    if i < bytes.len() {
        i += 1;
        while i < bytes.len() && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
            i += 1;
        }
    }
    if bytes.get(i) == Some(&b'\'') {
        return i + 1;
    }
    // A lifetime: the quote and its name.
    let mut i = open + 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust::signature::function_at;

    fn tail(cell: &str) -> Option<&str> {
        split_tail(cell).map(|(s, e)| &cell[s..e])
    }

    #[test]
    fn finds_the_last_expression() {
        assert_eq!(tail("let a = 1;\na + 1\n"), Some("a + 1"));
        assert_eq!(tail("let a = 1;\na + 1; // done\n"), None);
        assert_eq!(tail("parse(\"x;\") // the answer\n"), Some("parse(\"x;\")"));
        assert_eq!(tail("for i in 0..3 {\n    println!(\"{i}\");\n}\nlast(i)"), Some("last(i)"));
        assert_eq!(
            tail("let x = if a { 1 } else { 2 };\nmatch x {\n    1 => 'a',\n    _ => '}',\n}.to_string()"),
            Some("match x {\n    1 => 'a',\n    _ => '}',\n}.to_string()")
        );
        assert_eq!(tail("fn helper() -> u8 { 1 }\nhelper()"), Some("helper()"));
        assert_eq!(tail("fn helper() -> u8 { 1 }"), None);
        assert_eq!(tail("struct P { x: u8 }"), None);
        assert_eq!(tail("let r = r#\"a \" ; b\"#;\nr.len()"), Some("r.len()"));
        assert_eq!(tail("fn f<'a>(s: &'a str) -> &'a str { s }\nf(\"x\")"), Some("f(\"x\")"));
        assert_eq!(tail("/* only a comment */"), None);
        assert_eq!(tail(""), None);
    }

    #[test]
    fn wraps_the_tail_and_keeps_lines() {
        let cell = "use demo::*;\nlet s = String::from(\"hi\");\nparse(&s) // value\n";
        let program = program(cell, false);
        let lines: Vec<&str> = program.source.lines().collect();
        let first = program.lines.cell_first_line;
        assert_eq!(lines[first - 1], "use demo::*;");
        assert_eq!(&lines[first + 1..first + 4], ["(&&__hangar::Show(&(", "parse(&s)", "))).show(); // value"]);
        // Each cell line maps back from where it ended up.
        assert_eq!(program.lines.cell_line(first), Some(1));
        assert_eq!(program.lines.cell_line(first + 1), Some(2));
        assert_eq!(program.lines.cell_line(first + 3), Some(3));
        assert_eq!(program.lines.cell_line(first - 1), None);
        // Code before the expression on its line keeps the wrapper inline.
        let inline = super::program("let a = 1; a + 1", false);
        assert!(inline.source.contains("let a = 1; (&&__hangar::Show(&(a + 1))).show();"));
        assert_eq!(inline.lines.inserted_line, None);
        assert!(program.source.contains("Ok(())"));
        assert!(!program.source.contains("tokio"));
        assert!(super::program("f().await", true).source.contains("::tokio::runtime"));
    }

    #[test]
    fn reads_results() {
        assert_eq!(
            parse_result("demo::Token\ndebug\nToken {\n    a: 1,\n}"),
            Some(crate::Value { type_name: "demo::Token".into(), text: Some("Token {\n    a: 1,\n}".into()) })
        );
        assert_eq!(
            parse_result("demo::Opaque\nopaque\n"),
            Some(crate::Value { type_name: "demo::Opaque".into(), text: None })
        );
        assert_eq!(parse_result("()\ndebug\n()"), None);
    }

    fn scaffold_for(source: &str, needle: &str) -> Scaffold {
        let line = source.lines().position(|l| l.contains(needle)).unwrap();
        let sig = function_at(source, line).unwrap();
        scaffold(&sig, "demo", &["parse".to_string()], false)
    }

    #[test]
    fn scaffolds_a_free_function() {
        let s = scaffold_for(
            "pub fn parse_line<'a>(input: &'a str, config: &Config, out: &mut Vec<u8>, n: usize) -> Token<'a> {}",
            "parse_line",
        );
        assert_eq!(
            s.cell,
            "use demo::parse::*;\n\nlet input: &str = todo!();\nlet config: Config = todo!();\nlet mut out: Vec<u8> = todo!();\nlet n: usize = todo!();\n\nparse_line(input, &config, &mut out, n)"
        );
        assert_eq!(s.subject, "demo::parse::parse_line");
        assert!(s.notes.is_empty());
    }

    #[test]
    fn scaffolds_methods_generics_and_async() {
        let source = "mod lexer {\n    impl<'a> Lexer<'a> {\n        pub async fn next(&mut self, max: Option<usize>) -> Option<Token<'a>> {}\n        pub fn new(text: &'a str) -> Self {}\n    }\n}\nfn private<T: Clone>(items: Vec<T>, f: impl Fn(T)) {}\n";
        let s = scaffold_for(source, "async fn next");
        assert_eq!(
            s.cell,
            "use demo::parse::lexer::*;\n\nlet mut lexer: Lexer<'_> = todo!();\nlet max: Option<usize> = todo!();\n\n__hangar::block_on(lexer.next(max))"
        );
        assert_eq!(s.subject, "demo::parse::lexer::Lexer::next");
        assert_eq!(s.notes.len(), 1, "{:?}", s.notes);

        let s = scaffold_for(source, "pub fn new");
        assert!(s.cell.ends_with("let text: &str = todo!();\n\nLexer::new(text)"), "{}", s.cell);

        let s = scaffold_for(source, "fn private");
        assert!(s.cell.contains("let items = todo!(); // Vec<T>\nlet f = todo!(); // impl Fn(T)\n"), "{}", s.cell);
        assert_eq!(s.notes.len(), 2, "{:?}", s.notes);
    }

    #[test]
    fn words_and_lifetimes() {
        assert_eq!(anonymize_lifetimes("&'a Foo<'b, 'static>"), "&Foo<'_, 'static>");
        assert_eq!(replace_word("Vec<Self> + SelfLike", "Self", "Lexer"), "Vec<Lexer> + SelfLike");
        assert_eq!(snake_case("HttpServer"), "http_server");
        assert!(contains_word("Vec<T>", "T") && !contains_word("Token", "T"));
    }
}
