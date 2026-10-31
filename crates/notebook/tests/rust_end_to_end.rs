//! Builds and runs real cells against a small crate (needs `cargo`).

use std::{fs, path::Path, time::Duration};

use cockpit_notebook::{open, OpenRequest, Project, RunEvent, RunOutcome, Value};

const LIB: &str = r#"
pub mod text {
    /// Splits off the first word.
    pub fn first_word(input: &str) -> &str {
        input.split_whitespace().next().unwrap_or("")
    }

    #[derive(Debug)]
    pub struct Counts { pub words: usize, pub chars: usize }

    pub fn counts(input: &str) -> Counts {
        Counts { words: input.split_whitespace().count(), chars: input.chars().count() }
    }

    pub struct Opaque(pub u8);

    pub fn opaque() -> Opaque { Opaque(1) }

    pub async fn later(n: u32) -> u32 { n * 2 }

    pub fn shout(input: &str) {
        println!("{}!", input.to_uppercase());
        if input.is_empty() {
            panic!("nothing to shout");
        }
    }
}
"#;

fn run(kernel: &dyn cockpit_notebook::Kernel, code: &str) -> (Vec<RunEvent>, RunOutcome) {
    let (_handle, events) = kernel.run(code).expect("run starts");
    let mut seen = Vec::new();
    loop {
        let event = futures::executor::block_on(events.recv()).expect("run finishes");
        if let RunEvent::Finished(outcome) = event {
            return (seen, outcome);
        }
        seen.push(event);
    }
}

fn wait_until_gone(path: &Path) -> bool {
    for _ in 0..250 {
        if !path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn runs_cells_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    fs::write(root.join("src/lib.rs"), LIB).unwrap();
    let project = Project { root: root.to_path_buf(), cargo_targets: cockpit_debugger::workspace_targets(root).unwrap() };

    let path = root.join("src/lib.rs");
    let line = LIB.lines().position(|l| l.contains("split_whitespace().next()")).unwrap();
    let notebook = open(OpenRequest { path: &path, text: LIB, line, project: &project }).expect("opens");
    assert_eq!(notebook.subject, "demo_lib::text::first_word");
    assert_eq!(
        notebook.cell,
        "use demo_lib::text::*;\n\nlet input: &str = todo!();\n\nfirst_word(input)"
    );
    let labels: Vec<&str> = notebook.functions.iter().map(|f| f.label.as_str()).collect();
    assert_eq!(labels, ["text::first_word", "text::counts", "text::opaque", "text::later", "text::shout"]);
    let kernel = notebook.kernel;
    // Rust notebooks can show their cells to rust-analyzer.
    let analysis = notebook.analysis.expect("rust has analysis");
    let doc = analysis.cell_document(0);
    assert!(doc.path.exists() && doc.path.ends_with("src/cell_0.rs"));
    let (root_path, root_text) = analysis.root_document(&[0, 2]).unwrap();
    assert!(root_path.ends_with("src/main.rs"));
    assert!(root_text.contains("mod cell_0;\nmod cell_2;\n"), "{root_text}");

    // Outside any function, the notebook is for the file.
    let file = open(OpenRequest { path: &path, text: LIB, line: 0, project: &project }).expect("opens for the file");
    assert_eq!(file.title, "lib.rs");
    assert_eq!(file.subject, "demo_lib");
    assert_eq!(file.signature, None);
    assert!(file.cell.starts_with("use demo_lib::*;\n"), "{}", file.cell);
    assert_eq!(file.functions.len(), 5);
    // Async functions say how they run, in the cell.
    assert!(file.functions[3].cell.starts_with("// `later` is async."), "{}", file.functions[3].cell);
    drop(file);

    // The value may borrow a local of the cell.
    let (_, outcome) = run(&*kernel, "use demo_lib::text::*;\nlet owned = String::from(\"hello world\");\nfirst_word(&owned)\n");
    assert_eq!(
        outcome,
        RunOutcome::Returned(Some(Value { type_name: "&str".into(), text: Some("\"hello\"".into()) }))
    );

    let (_, outcome) = run(&*kernel, "use demo_lib::text::*;\ncounts(\"a bc\")");
    let RunOutcome::Returned(Some(value)) = outcome else { panic!("{outcome:?}") };
    assert_eq!(value.type_name, "demo_lib::text::Counts");
    assert_eq!(value.text.as_deref(), Some("Counts {\n    words: 2,\n    chars: 4,\n}"));

    // Types without Debug are shown by name.
    let (_, outcome) = run(&*kernel, "demo_lib::text::opaque()");
    assert_eq!(
        outcome,
        RunOutcome::Returned(Some(Value { type_name: "demo_lib::text::Opaque".into(), text: None }))
    );

    let (_, outcome) = run(&*kernel, "__hangar::block_on(demo_lib::text::later(21))");
    assert_eq!(outcome, RunOutcome::Returned(Some(Value { type_name: "u32".into(), text: Some("42".into()) })));

    // Printed lines arrive; a statement at the end shows no value.
    let (events, outcome) = run(&*kernel, "demo_lib::text::shout(\"hi\");");
    assert_eq!(outcome, RunOutcome::Returned(None));
    assert!(events.contains(&RunEvent::Output { line: "HI!".into(), stderr: false }), "{events:?}");

    let (events, outcome) = run(&*kernel, "demo_lib::text::shout(\"\")");
    assert_eq!(outcome, RunOutcome::Failed("The cell panicked.".into()));
    assert!(events.iter().any(|e| matches!(e, RunEvent::Output { line, stderr: true } if line.contains("nothing to shout"))));

    // Cells can be formatted (when rustfmt is installed).
    if let Ok(Some(formatted)) = kernel.format("let a=1;a+1") {
        assert_eq!(formatted, "let a = 1;\na + 1");
    }

    // Compile errors point at the cell's lines.
    let (events, outcome) = run(&*kernel, "let a = 1;\nmissing + a");
    assert_eq!(outcome, RunOutcome::BuildFailed);
    let build: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Build(line) => Some(line.as_str()),
            _ => None,
        })
        .collect();
    assert!(build.iter().any(|l| l.trim() == "--> cell:2:1"), "{build:#?}");
    assert!(build.iter().any(|l| l.trim() == "2 | missing + a"), "{build:#?}");

    // Closing the notebook removes the throwaway project and its build files.
    let target = root.join("target");
    let scratch_parent = target.join("hangar-notebook");
    assert!(scratch_parent.exists());
    // Closing the tab drops both halves of the notebook.
    drop(kernel);
    drop(analysis);
    assert!(wait_until_gone(&scratch_parent), "the notebook's project is still there");
    let leftovers: Vec<String> = ["debug", "debug/deps", "debug/.fingerprint", "debug/incremental"]
        .iter()
        .flat_map(|sub| fs::read_dir(target.join(sub)).into_iter().flatten().flatten())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("hangar-notebook") || name.starts_with("hangar_notebook"))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    // The library's own build stays for the next notebook.
    let deps: Vec<String> = fs::read_dir(target.join("debug/deps"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(deps.iter().any(|name| name.starts_with("libdemo_lib")), "{deps:?}");
}

#[test]
fn explains_what_it_cannot_open() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    let main = "fn helper(x: u8) -> u8 { x }\nfn main() {}\n";
    fs::write(root.join("src/main.rs"), main).unwrap();
    let project = Project { root: root.to_path_buf(), cargo_targets: cockpit_debugger::workspace_targets(root).unwrap() };
    let path = root.join("src/main.rs");
    let request = |line| OpenRequest { path: &path, text: main, line, project: &project };
    let err = open(request(0)).err().unwrap();
    assert!(err.to_string().contains("library crates"), "{err}");
    // Outside a function, the message is about the file.
    let err = open(OpenRequest { text: "\n\nstruct A;", ..request(0) }).err().unwrap();
    assert!(err.to_string().starts_with("This file belongs to the `tool` binary"), "{err}");
}
