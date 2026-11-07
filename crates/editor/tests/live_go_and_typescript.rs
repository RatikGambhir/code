//! Drives real gopls and typescript-language-server through the same path
//! the app takes: detect the project around a file, plan its servers,
//! resolve and start the primary one, then ask for diagnostics,
//! completion and definitions.
//!
//! Each test is skipped (with a note) when its server is not installed.
//! Servers installed by Hangar are found through `HANGAR_LANGUAGE_SERVERS`
//! (a managed directory); with `HANGAR_TEST_INSTALL=1` as well, missing
//! servers are installed there first, which exercises the installer.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use cockpit_editor::{
    lsp::{plan, uri_to_path, LspClient, ManagedDir, ResolveError, Role, ServerConfig, ServerEvent, ServerPlan, ToolDirs},
    lsp_language_id_for_path,
    project::{FileContext, Runtime},
};
use futures::executor::block_on;
use lsp_types::{CompletionResponse, DiagnosticSeverity, GotoDefinitionResponse, Position};

fn tools() -> ToolDirs {
    ToolDirs {
        search: cockpit_common::shell_env::common_tool_dirs(),
        managed: std::env::var_os("HANGAR_LANGUAGE_SERVERS").map(ManagedDir::new),
    }
}

/// The primary server for `file`, installing it first when asked to.
fn primary_server(file: &Path, root: &Path) -> Option<(ServerPlan, ServerConfig)> {
    let language = lsp_language_id_for_path(file).expect("a language server language");
    let context = FileContext::detect(file, language, root);
    let primary = plan(&context).into_iter().find(|p| p.role() == Role::Primary)?;
    let tools = tools();
    match primary.resolve(&tools) {
        Ok(config) => Some((primary, config)),
        Err(ResolveError::NotInstalled { install: Some(install) })
            if std::env::var_os("HANGAR_TEST_INSTALL").is_some() && tools.managed.is_some() =>
        {
            let managed = tools.managed.clone().unwrap();
            let installed = managed.install(&[install], |name| tools.find(name)).expect("install succeeds");
            eprintln!("installed {installed:?} into {}", managed.root().display());
            let config = primary.resolve(&tools).expect("resolves after installing");
            Some((primary, config))
        }
        Err(err) => {
            eprintln!("skipping: {} unavailable ({err})", primary.spec.id);
            None
        }
    }
}

fn write(root: &Path, path: &str, text: &str) -> PathBuf {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

fn labels(response: Option<CompletionResponse>) -> Vec<String> {
    match response {
        Some(CompletionResponse::Array(items)) => items.into_iter().map(|i| i.label).collect(),
        Some(CompletionResponse::List(list)) => list.items.into_iter().map(|i| i.label).collect(),
        None => Vec::new(),
    }
}

fn definition_target(response: Option<GotoDefinitionResponse>) -> (PathBuf, u32) {
    match response {
        Some(GotoDefinitionResponse::Link(links)) => {
            (uri_to_path(&links[0].target_uri).unwrap(), links[0].target_selection_range.start.line)
        }
        Some(GotoDefinitionResponse::Scalar(loc)) => (uri_to_path(&loc.uri).unwrap(), loc.range.start.line),
        Some(GotoDefinitionResponse::Array(locs)) => (uri_to_path(&locs[0].uri).unwrap(), locs[0].range.start.line),
        None => panic!("no definition"),
    }
}

/// Waits for an error diagnostic in `file` whose message contains `needle`.
fn wait_for_error(events: &async_channel::Receiver<ServerEvent>, file: &Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(ServerEvent::Diagnostics(params)) if uri_to_path(&params.uri).as_deref() == Some(file) => {
                for diagnostic in params.diagnostics {
                    if diagnostic.severity == Some(DiagnosticSeverity::ERROR) && diagnostic.message.contains(needle) {
                        return;
                    }
                    seen.push(diagnostic.message);
                }
            }
            Ok(ServerEvent::Exited) => panic!("the server exited"),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    panic!("no error containing {needle:?} within {timeout:?}; saw {seen:?}");
}

/// Asks until the answer is non-empty: servers answer with nothing while
/// they are still loading the project.
fn retry<T>(timeout: Duration, mut ask: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(answer) = ask() {
            return answer;
        }
        assert!(Instant::now() < deadline, "no answer within {timeout:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

const GO_MAIN: &str = "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(Greet(\"go\"))\n\tunused := 1\n}\n";
const GO_GREET: &str = "package main\n\n// Greet says hi.\nfunc Greet(name string) string {\n\treturn \"hi \" + name\n}\n";

#[test]
fn gopls_answers_through_the_planned_module_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "go.work", "go 1.21\n\nuse ./svc\n");
    write(&root, "svc/go.mod", "module example.com/svc\n\ngo 1.21\n");
    let main = write(&root, "svc/main.go", GO_MAIN);
    write(&root, "svc/greet.go", GO_GREET);

    let Some((primary, config)) = primary_server(&main, &root) else { return };
    assert_eq!(primary.spec.id, "gopls");
    assert_eq!(primary.root, root, "gopls runs at the go.work directory");

    let (client, events) = block_on(LspClient::start(&config, &primary.root)).expect("gopls starts");
    client.did_open(&main, "go", 1, GO_MAIN).unwrap();
    wait_for_error(&events, &main, "declared and not used", Duration::from_secs(120));

    // After `fmt.` in `fmt.Println` on line 6: the package's members.
    let completions = retry(Duration::from_secs(30), || {
        let labels = labels(block_on(client.completion(&main, Position::new(5, 5), None)).ok()?);
        (!labels.is_empty()).then_some(labels)
    });
    assert!(completions.iter().any(|l| l == "Println"), "completions: {completions:?}");

    // `Greet` in `Greet("go")` is declared in greet.go.
    let (path, line) = definition_target(block_on(client.definition(&main, Position::new(5, 15))).unwrap());
    assert_eq!((path, line), (root.join("svc/greet.go"), 3));

    block_on(client.shutdown()).unwrap();
}

const TS_INDEX: &str = "import { greet } from \"./greet\";\n\nconst count: number = \"one\";\nconsole.log(greet(\"ts\"), count);\ngreet(\"x\").toUpp\n";
const TS_GREET: &str = "/** Says hi. */\nexport function greet(name: string): string {\n  return `hi ${name}`;\n}\n";

#[test]
fn typescript_answers_in_a_bun_project() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "package.json", r#"{"name": "demo", "type": "module", "scripts": {"dev": "bun run src/index.ts"}}"#);
    write(&root, "bun.lock", "{}");
    write(&root, "tsconfig.json", r#"{"compilerOptions": {"strict": true, "module": "esnext", "target": "es2022", "moduleResolution": "bundler", "noEmit": true}}"#);
    let index = write(&root, "src/index.ts", TS_INDEX);
    write(&root, "src/greet.ts", TS_GREET);

    let context = FileContext::detect(&index, "typescript", &root);
    let js = context.js.as_ref().unwrap();
    assert_eq!((js.runtime, js.root.as_path()), (Runtime::Bun, root.as_path()));

    let Some((primary, config)) = primary_server(&index, &root) else { return };
    assert_eq!(primary.spec.id, "typescript");

    let (client, events) = block_on(LspClient::start(&config, &primary.root)).expect("the TypeScript server starts");
    client.did_open(&index, "typescript", 1, TS_INDEX).unwrap();
    wait_for_error(&events, &index, "is not assignable to type 'number'", Duration::from_secs(60));

    // `.toUpp` on line 5: string methods.
    let completions = retry(Duration::from_secs(30), || {
        let labels = labels(block_on(client.completion(&index, Position::new(4, 16), None)).ok()?);
        (!labels.is_empty()).then_some(labels)
    });
    assert!(completions.iter().any(|l| l == "toUpperCase"), "completions: {completions:?}");

    // `greet` in the call on line 4 is declared in greet.ts.
    let (path, line) = definition_target(block_on(client.definition(&index, Position::new(3, 14))).unwrap());
    assert_eq!((path, line), (root.join("src/greet.ts"), 1));

    block_on(client.shutdown()).unwrap();
}

/// A project still on TypeScript 6 keeps tsserver, through
/// typescript-language-server. The project's TypeScript is linked from the
/// managed directory's classic copy (installed first when asked to).
#[test]
fn typescript_six_projects_keep_tsserver() {
    let tools = tools();
    let Some(managed) = tools.managed.clone() else {
        eprintln!("skipping: HANGAR_LANGUAGE_SERVERS is not set");
        return;
    };
    if managed.classic_typescript_lib().is_none() && std::env::var_os("HANGAR_TEST_INSTALL").is_some() {
        let classic = cockpit_editor::lsp::Install::Npm(&["typescript-classic@npm:typescript@6", "typescript-language-server"]);
        managed.install(&[classic], |name| tools.find(name)).expect("install succeeds");
    }
    let Some(classic_lib) = managed.classic_typescript_lib() else {
        eprintln!("skipping: no classic TypeScript in the managed directory");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "package.json", r#"{"devDependencies": {"typescript": "^6"}}"#);
    write(&root, "package-lock.json", "{}");
    std::fs::create_dir_all(root.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(classic_lib.parent().unwrap(), root.join("node_modules/typescript")).unwrap();
    let index = write(&root, "src/index.ts", TS_INDEX);
    write(&root, "src/greet.ts", TS_GREET);

    let Some((primary, config)) = primary_server(&index, &root) else { return };
    assert_eq!(primary.spec.id, "typescript-classic");
    assert_eq!(config.name, "typescript-language-server");

    let (client, events) = block_on(LspClient::start(&config, &primary.root)).expect("typescript-language-server starts");
    client.did_open(&index, "typescript", 1, TS_INDEX).unwrap();
    wait_for_error(&events, &index, "is not assignable to type 'number'", Duration::from_secs(60));
    let (path, line) = definition_target(block_on(client.definition(&index, Position::new(3, 14))).unwrap());
    assert_eq!((path, line), (root.join("src/greet.ts"), 1));
    block_on(client.shutdown()).unwrap();
}
