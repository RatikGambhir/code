//! Formatting a cell with `rustfmt`. A cell is a function body, which
//! rustfmt cannot format on its own, so it is formatted inside a function
//! and taken out again.

use std::{
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{bail, Context as _, Result};

const OPEN: &str = "fn __hangar_cell() {";

/// `code` formatted by `rustfmt`, run in `dir` so the project's
/// `rustfmt.toml` and toolchain apply.
///
/// # Errors
///
/// When rustfmt cannot be run or rejects the code (a syntax error).
pub fn format_cell(code: &str, dir: &Path) -> Result<String> {
    let wrapped = format!("{OPEN}\n{code}\n}}\n");
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2021", "--emit", "stdout"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running rustfmt (install it with `rustup component add rustfmt`)")?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(wrapped.as_bytes())
        .context("writing to rustfmt")?;
    let output = child.wait_with_output().context("waiting for rustfmt")?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let first = message.lines().find(|l| !l.trim().is_empty()).unwrap_or("rustfmt failed");
        bail!("{}", first.trim_start_matches("error: ").trim());
    }
    unwrap(&String::from_utf8_lossy(&output.stdout)).context("rustfmt changed the cell's wrapper")
}

/// The body of the formatted wrapper function, one indent level out.
fn unwrap(formatted: &str) -> Option<String> {
    let formatted = formatted.trim_end();
    // An empty body comes back as `fn __hangar_cell() {}`.
    if formatted == format!("{OPEN}}}") {
        return Some(String::new());
    }
    let body = formatted.strip_prefix(OPEN)?.strip_suffix('}')?;
    let body = body.strip_prefix('\n').unwrap_or(body);
    let lines: Vec<&str> = body.lines().collect();
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let out: Vec<&str> = lines.iter().map(|l| if l.len() >= indent { &l[indent..] } else { l.trim_start() }).collect();
    Some(out.join("\n").trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rustfmt_available() -> bool {
        Command::new("rustfmt").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    #[test]
    fn formats_a_cell() {
        if !rustfmt_available() {
            eprintln!("rustfmt is not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir();
        let cell = "use demo::*;\nlet x=vec![1,2 ,3];\nlet total:i32=x.iter().sum();\nif total>3{println!(\"{}\",total)}\ntotal";
        assert_eq!(
            format_cell(cell, &dir).unwrap(),
            "use demo::*;\nlet x = vec![1, 2, 3];\nlet total: i32 = x.iter().sum();\nif total > 3 {\n    println!(\"{}\", total)\n}\ntotal"
        );
        assert_eq!(format_cell("", &dir).unwrap(), "");
        assert!(format_cell("let x = ;", &dir).is_err());
    }

    #[test]
    fn unwraps_the_body() {
        assert_eq!(unwrap("fn __hangar_cell() {\n    a();\n    b\n}\n").as_deref(), Some("a();\nb"));
        assert_eq!(unwrap("fn __hangar_cell() {}\n").as_deref(), Some(""));
        assert_eq!(unwrap("something else").as_deref(), None);
    }
}
