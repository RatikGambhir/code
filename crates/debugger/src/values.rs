//! Presenting values: Rust-looking type names, inline values beside the
//! code, and the expression under the mouse.

use std::collections::{HashMap, HashSet};

/// LLDB without a Rust language plugin reports types the way the DWARF
/// spells them: `alloc::vec::Vec<int, alloc::alloc::Global>`. This shortens
/// them the way RustRover shows them: `Vec<i32>`.
pub fn pretty_type(name: &str) -> String {
    // `segment` is the word being read; `current` what is done at this
    // nesting level; `depth_args` the head and arguments of each open `<`.
    fn flush(segment: &mut String, out: &mut String) {
        let word = segment.trim();
        if !word.is_empty() {
            out.push_str(&short_path(word));
        }
        segment.clear();
    }
    let mut segment = String::new();
    let mut depth_args: Vec<Vec<String>> = Vec::new();
    let mut current = String::new();
    for c in name.chars() {
        match c {
            '<' => {
                flush(&mut segment, &mut current);
                depth_args.push(vec![std::mem::take(&mut current)]);
                current.clear();
            }
            ',' if !depth_args.is_empty() => {
                flush(&mut segment, &mut current);
                depth_args.last_mut().unwrap().push(std::mem::take(&mut current));
            }
            '>' if !depth_args.is_empty() => {
                flush(&mut segment, &mut current);
                let mut frame = depth_args.pop().unwrap();
                frame.push(std::mem::take(&mut current));
                let head = frame.remove(0);
                let args: Vec<String> = frame
                    .into_iter()
                    .map(|a| a.trim().to_string())
                    .filter(|a| !is_default_parameter(a))
                    .collect();
                current = if args.is_empty() { head } else { format!("{head}<{}>", args.join(", ")) };
            }
            _ => segment.push(c),
        }
    }
    flush(&mut segment, &mut current);
    current
}

/// Allocator and hasher parameters nobody writes out.
fn is_default_parameter(arg: &str) -> bool {
    matches!(arg, "Global" | "RandomState")
}

fn short_path(word: &str) -> String {
    // `&mut alloc::string::String` keeps its sigils.
    let (prefix, path) = split_sigils(word);
    let last = path.rsplit("::").next().unwrap_or(path);
    format!("{prefix}{}", c_to_rust(last))
}

fn split_sigils(word: &str) -> (&str, &str) {
    let mut end = 0;
    let bytes = word.as_bytes();
    loop {
        let rest = &word[end..];
        if rest.starts_with("&mut ") {
            end += 5;
        } else if rest.starts_with("*const ") {
            end += 7;
        } else if rest.starts_with("*mut ") {
            end += 5;
        } else if end < bytes.len() && (bytes[end] == b'&' || bytes[end] == b'[' || bytes[end] == b'(') {
            end += 1;
        } else {
            break;
        }
    }
    (&word[..end], &word[end..])
}

fn c_to_rust(name: &str) -> &str {
    match name {
        "int" => "i32",
        "unsigned int" => "u32",
        "long" | "long long" => "i64",
        "unsigned long" | "unsigned long long" => "u64",
        "short" => "i16",
        "unsigned short" => "u16",
        "signed char" => "i8",
        "unsigned char" => "u8",
        "float" => "f32",
        "double" => "f64",
        "_Bool" => "bool",
        "char32_t" => "char",
        "__int128" => "i128",
        "unsigned __int128" => "u128",
        other => other,
    }
}

/// A value shortened for one line.
pub fn one_line(value: &str, max_chars: usize) -> String {
    let flat: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    cockpit_common::truncate_chars(&flat, max_chars)
}

/// Where to show each variable's value beside the code: the first line in
/// the function (from its `fn` line to `stop_line`) that mentions it. Lines
/// are zero-based; returns line → `name: value` texts.
pub fn inline_value_lines(
    text: &str,
    stop_line: usize,
    variables: &[(String, String)],
) -> HashMap<usize, Vec<String>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut result: HashMap<usize, Vec<String>> = HashMap::new();
    if stop_line >= lines.len() || variables.is_empty() {
        return result;
    }
    let start = (0..=stop_line)
        .rev()
        .take(400)
        .find(|&ix| is_fn_line(lines[ix]))
        .unwrap_or(stop_line.saturating_sub(60));
    let mut placed = HashSet::new();
    for ix in start..=stop_line {
        let words = identifiers(lines[ix]);
        for (name, value) in variables {
            if placed.contains(name.as_str()) || !words.contains(name.as_str()) {
                continue;
            }
            placed.insert(name.as_str());
            result.entry(ix).or_default().push(format!("{name}: {}", one_line(value, 60)));
        }
    }
    result
}

fn is_fn_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let trimmed = trimmed.split("//").next().unwrap_or("");
    trimmed.split_whitespace().take(5).any(|w| w == "fn") && trimmed.contains('(')
}

fn identifiers(line: &str) -> HashSet<&str> {
    let code = line.split("//").next().unwrap_or("");
    code.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty() && !w.starts_with(|c: char| c.is_ascii_digit()))
        .collect()
}

/// The expression under `offset` in `line` for a hover: the identifier there
/// with the field accesses before it (`self.config.port`).
pub fn expression_at(line: &str, offset: usize) -> Option<String> {
    let bytes = line.as_bytes();
    if offset > bytes.len() {
        return None;
    }
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut end = offset;
    while end < bytes.len() && is_ident(bytes[end]) {
        end += 1;
    }
    let mut start = offset.min(bytes.len());
    while start > 0 && (is_ident(bytes[start - 1]) || bytes[start - 1] == b'.') {
        start -= 1;
    }
    let expr = line[start..end].trim_matches('.');
    let first = expr.split('.').next()?;
    if expr.is_empty() || first.is_empty() || first.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    if is_keyword(expr) {
        return None;
    }
    Some(expr.to_string())
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "fn" | "let" | "mut" | "pub" | "use" | "mod" | "struct" | "enum" | "impl" | "trait" | "for" | "in"
            | "if" | "else" | "match" | "while" | "loop" | "return" | "break" | "continue" | "as" | "ref"
            | "where" | "type" | "const" | "static" | "crate" | "super" | "async" | "await" | "move"
            | "true" | "false" | "dyn" | "unsafe" | "extern"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_types() {
        assert_eq!(pretty_type("alloc::vec::Vec<int, alloc::alloc::Global>"), "Vec<i32>");
        assert_eq!(pretty_type("alloc::string::String"), "String");
        assert_eq!(
            pretty_type("std::collections::hash::map::HashMap<&str, int, std::hash::random::RandomState, alloc::alloc::Global>"),
            "HashMap<&str, i32>"
        );
        assert_eq!(pretty_type("core::option::Option<&str>"), "Option<&str>");
        assert_eq!(pretty_type("demo::Point"), "Point");
        assert_eq!(pretty_type("&mut alloc::vec::Vec<unsigned char, alloc::alloc::Global>"), "&mut Vec<u8>");
        assert_eq!(pretty_type("unsigned long"), "u64");
    }

    #[test]
    fn places_inline_values() {
        let text = "fn other() {}\n\nfn main() {\n    let name = String::new();\n    let n = 3;\n    println!(\"{name} {n}\");\n}";
        let vars = vec![("name".to_string(), "\"x\"".to_string()), ("n".to_string(), "3".to_string())];
        let lines = inline_value_lines(text, 5, &vars);
        assert_eq!(lines.get(&3), Some(&vec!["name: \"x\"".to_string()]));
        assert_eq!(lines.get(&4), Some(&vec!["n: 3".to_string()]));
        assert!(!lines.contains_key(&5));
        assert!(!lines.contains_key(&0));
    }

    #[test]
    fn finds_hover_expressions() {
        let line = "    let port = self.config.port + 1;";
        assert_eq!(expression_at(line, line.find("port +").unwrap() + 1).as_deref(), Some("self.config.port"));
        assert_eq!(expression_at(line, line.find("config").unwrap()).as_deref(), Some("self.config"));
        assert_eq!(expression_at(line, 5).as_deref(), None); // `let`
        assert_eq!(expression_at("x.0", 0).as_deref(), Some("x"));
    }
}
