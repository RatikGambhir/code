//! Drives a real rust-analyzer over a small Cargo project: completion,
//! go-to-definition across files, references and hover.
//!
//! Skipped (with a note) when rust-analyzer is not installed; install it with
//! `rustup component add rust-analyzer`.

use std::time::{Duration, Instant};

use cockpit_editor::lsp::{resolve_server, uri_to_path, LspClient, ServerEvent};
use futures::executor::block_on;
use lsp_types::{CompletionResponse, GotoDefinitionResponse, Position};

const LIB: &str = "pub struct Greeter {\n    pub name: String,\n}\n\nimpl Greeter {\n    pub fn greet(&self) -> String {\n        format!(\"hi {}\", self.name)\n    }\n}\n";
const MAIN: &str = "use demo::Greeter;\n\nfn main() {\n    let g = Greeter { name: \"é😀\".into() };\n    println!(\"{}\", g.greet());\n    g.\n}\n";

#[test]
fn rust_analyzer_answers_ide_requests() {
    let dirs = cockpit_common::shell_env::common_tool_dirs();
    let config = match resolve_server("rust", &dirs) {
        Ok(Some(config)) => config,
        other => {
            eprintln!("skipping: rust-analyzer unavailable ({other:?})");
            return;
        }
    };

    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(root.join("src/main.rs"), MAIN).unwrap();
    let main = root.join("src/main.rs");

    let (client, events) = block_on(LspClient::start(&config, &root)).expect("rust-analyzer starts");
    // Columns are counted in characters on both sides.
    let encoding = client.capabilities().and_then(|c| c.position_encoding.clone());
    assert_eq!(encoding.as_ref().map(|e| e.as_str()), Some("utf-32"));

    client.did_open(&main, "rust", 1, MAIN).unwrap();
    wait_until_loaded(&events, Duration::from_secs(180));

    // `g.` on line 5: the method and the field are offered.
    let completion = block_on(client.completion(&main, Position::new(5, 6), None)).unwrap();
    let labels: Vec<String> = match completion {
        Some(CompletionResponse::Array(items)) => items.into_iter().map(|i| i.label).collect(),
        Some(CompletionResponse::List(list)) => list.items.into_iter().map(|i| i.label).collect(),
        None => Vec::new(),
    };
    assert!(labels.iter().any(|l| l.starts_with("greet")), "completions: {labels:?}");
    assert!(labels.iter().any(|l| l == "name"), "completions: {labels:?}");

    // `greet` in `g.greet()`.
    let definition = block_on(client.definition(&main, Position::new(4, 23))).unwrap();
    let (path, line) = match definition {
        Some(GotoDefinitionResponse::Link(links)) => {
            (uri_to_path(&links[0].target_uri).unwrap(), links[0].target_selection_range.start.line)
        }
        Some(GotoDefinitionResponse::Scalar(loc)) => (uri_to_path(&loc.uri).unwrap(), loc.range.start.line),
        Some(GotoDefinitionResponse::Array(locs)) => (uri_to_path(&locs[0].uri).unwrap(), locs[0].range.start.line),
        None => panic!("no definition for greet"),
    };
    assert_eq!(path, root.join("src/lib.rs"));
    assert_eq!(line, 5, "greet is declared on line 6 of lib.rs");

    // `Greeter` in `use demo::Greeter;`: declared in lib.rs, used three times.
    let references = block_on(client.references(&main, Position::new(0, 12))).unwrap().unwrap_or_default();
    assert!(references.len() >= 3, "references: {references:?}");
    assert!(references.iter().any(|r| uri_to_path(&r.uri).as_deref() == Some(root.join("src/lib.rs").as_path())));

    // Hover on `into` after "é😀": with UTF-32 columns the emoji is one column.
    let hover = block_on(client.hover(&main, Position::new(3, 34))).unwrap();
    let text = format!("{:?}", hover.expect("hover on into"));
    assert!(text.contains("into"), "hover: {text}");

    block_on(client.shutdown()).unwrap();
}

/// Waits for rust-analyzer to report that the workspace has loaded.
fn wait_until_loaded(events: &async_channel::Receiver<ServerEvent>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(ServerEvent::Status { quiescent: true, healthy, message }) => {
                assert!(healthy, "rust-analyzer reported a problem: {message:?}");
                return;
            }
            Ok(ServerEvent::Exited) => panic!("rust-analyzer exited"),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    panic!("rust-analyzer did not finish loading in {timeout:?}");
}
