//! Cells as rust-analyzer sees them: completion inside a cell works. Needs
//! rust-analyzer and takes a while, so it is opt-in:
//! `cargo test -p cockpit-notebook --test rust_analyzer_live -- --ignored`.

use std::{fs, time::Duration};

use cockpit_editor::lsp::{resolve_server, LspClient, ServerEvent};
use cockpit_notebook::{open, OpenRequest, Project};
use lsp_types::{CompletionResponse, Position};

const LIB: &str = "pub mod text {\n    pub fn counts(input: &str) -> usize { input.len() }\n}\n";

#[test]
#[ignore = "starts rust-analyzer"]
fn completes_inside_a_cell() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    fs::write(root.join("src/lib.rs"), LIB).unwrap();
    let project = Project { root: root.clone(), cargo_targets: cockpit_debugger::workspace_targets(&root).unwrap() };
    let path = root.join("src/lib.rs");
    let notebook = open(OpenRequest { path: &path, text: LIB, line: 1, project: &project }).unwrap();
    let analysis = notebook.analysis.clone().unwrap();

    let Some(mut config) = resolve_server("rust", &cockpit_common::shell_env::common_tool_dirs()).unwrap() else {
        eprintln!("rust-analyzer is not installed; skipping");
        return;
    };
    // Shallow merge is enough here: the notebook's settings win.
    let mut settings = config.settings.take().unwrap_or_default();
    for (key, value) in analysis.settings().unwrap().as_object().unwrap() {
        settings[key] = value.clone();
    }
    config.settings = Some(settings);

    futures::executor::block_on(async {
        let (client, events) = LspClient::start(&config, analysis.root()).await.unwrap();
        let (root_path, root_text) = analysis.root_document(&[0]).unwrap();
        client.did_open(&root_path, "rust", 1, &root_text).unwrap();
        let doc = analysis.cell_document(0);
        let cell = "use demo_lib::text::*;\nlet n = cou";
        client.did_open(&doc.path, "rust", 1, &format!("{}{cell}{}", doc.prefix, doc.suffix)).unwrap();

        // Wait for the project to load.
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        loop {
            assert!(std::time::Instant::now() < deadline, "rust-analyzer did not finish loading");
            let event = futures::future::select(
                Box::pin(events.recv()),
                Box::pin(async { std::thread::sleep(Duration::from_millis(200)) }),
            )
            .await;
            if let futures::future::Either::Left((Ok(ServerEvent::Status { quiescent: true, healthy, message }), _)) = event {
                assert!(healthy, "rust-analyzer is unhappy: {message:?}");
                break;
            }
        }

        let shift = doc.prefix.matches('\n').count() as u32;
        let response = client.completion(&doc.path, Position::new(shift + 1, 11), None).await.unwrap();
        let labels: Vec<String> = match response {
            Some(CompletionResponse::Array(items)) => items.into_iter().map(|i| i.label).collect(),
            Some(CompletionResponse::List(list)) => list.items.into_iter().map(|i| i.label).collect(),
            None => Vec::new(),
        };
        assert!(labels.iter().any(|l| l.starts_with("counts")), "no `counts` in {labels:?}");
        client.kill();
    });
}
