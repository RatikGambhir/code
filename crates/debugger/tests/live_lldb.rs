//! Debugs a small Rust program with the real `lldb-dap`, through the same
//! steps the IDE takes. Skipped (passing) when `lldb-dap` is not installed.
//!
//! `cargo test -p cockpit-debugger --test live_lldb -- --nocapture`

use std::{path::Path, time::Duration};

use cockpit_debugger::{
    breakpoints::LineBreakpoint, cargo::find_runnables, find_lldb_dap, protocol::FunctionBreakpoint,
    values::pretty_type, workspace_targets, Build, BuildEvent, DapClient, Event, RunConfig, RustToolchain,
};
use futures::executor::block_on;
use serde_json::json;

const MAIN: &str = r#"use std::collections::HashMap;

struct Point { x: i32, y: i32 }

fn add(a: i32, b: i32) -> i32 {
    let sum = a + b;
    sum
}

fn main() {
    let name = String::from("hangar");
    let nums = vec![1, 2, 3];
    let mut map = HashMap::new();
    map.insert("a", 1);
    let p = Point { x: 3, y: 4 };
    let total = add(p.x, p.y);
    println!("total={total} {name} {} {}", nums.len(), map.len());
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add(1, 2), 3);
    }
}
"#;

fn next_event(events: &async_channel::Receiver<Event>, mut wanted: impl FnMut(&Event) -> bool) -> Event {
    loop {
        let event = events.recv_blocking().expect("the adapter's event stream ended");
        if let Event::Output(out) = &event {
            eprint!("[{}] {}", out.category.as_deref().unwrap_or("?"), out.output);
        }
        if wanted(&event) {
            return event;
        }
    }
}

fn build(config: &RunConfig, root: &Path) -> std::path::PathBuf {
    let (_build, events) = Build::start(config, root).unwrap();
    loop {
        match events.recv_blocking().unwrap() {
            BuildEvent::Line(line) => eprintln!("cargo: {line}"),
            BuildEvent::Finished(result) => return result.expect("build"),
        }
    }
}

#[test]
fn debugs_a_rust_program() {
    // A hung adapter fails the test rather than the whole run.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(180));
        eprintln!("timed out");
        std::process::exit(1);
    });
    let adapter = match find_lldb_dap() {
        Ok(adapter) => adapter,
        Err(err) => {
            eprintln!("skipping: {err}");
            return;
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    let main_rs = root.join("src/main.rs");
    std::fs::write(&main_rs, MAIN).unwrap();

    let targets = workspace_targets(&root).unwrap();
    let runnables = find_runnables(&main_rs, MAIN, &targets);
    assert_eq!(runnables.iter().map(|r| r.line).collect::<Vec<_>>(), [9, 20, 22]);
    let program = build(&runnables[0].config, &root);
    assert!(program.exists());

    let (client, events) = DapClient::spawn(&adapter, &root).unwrap();
    let caps = block_on(client.initialize()).unwrap();
    assert!(caps.supports_configuration_done_request);

    let toolchain = RustToolchain::detect(&root);
    let launch = client.launch(json!({
        "program": program,
        "args": [],
        "cwd": root,
        "env": ["RUST_BACKTRACE=1"],
        "initCommands": toolchain.init_commands(),
        "stopOnEntry": false,
    }));
    next_event(&events, |e| matches!(e, Event::Initialized));
    // Zero-based line 5: `    let sum = a + b;`.
    let bps = block_on(client.set_breakpoints(&main_rs, &[LineBreakpoint::new(5).to_source_breakpoint()])).unwrap();
    assert!(bps[0].verified, "{bps:?}");
    block_on(client.set_function_breakpoints(&[FunctionBreakpoint { name: "rust_panic".into(), condition: None }])).unwrap();
    block_on(client.configuration_done()).unwrap();
    block_on(launch).unwrap();

    let Event::Stopped(stopped) = next_event(&events, |e| matches!(e, Event::Stopped(_))) else { unreachable!() };
    assert_eq!(stopped.reason, "breakpoint");
    let thread = stopped.thread_id.unwrap();
    let frames = block_on(client.stack_trace(thread, 20)).unwrap();
    assert!(frames[0].name.ends_with("add"), "{}", frames[0].name);
    assert_eq!(frames[0].line, 6);
    assert!(frames[1].name.ends_with("main"));

    // Locals of `main` (the caller frame).
    let scopes = block_on(client.scopes(frames[1].id)).unwrap();
    let locals = block_on(client.variables(scopes[0].variables_reference)).unwrap();
    let get = |name: &str| locals.iter().find(|v| v.name == name).unwrap_or_else(|| panic!("no {name} in {locals:?}"));
    assert_eq!(get("name").value, "\"hangar\"");
    assert_eq!(pretty_type(get("nums").type_name.as_deref().unwrap()), "Vec<i32>");
    let children = block_on(client.variables(get("nums").variables_reference)).unwrap();
    assert_eq!(children.iter().map(|c| c.value.as_str()).collect::<Vec<_>>(), ["1", "2", "3"]);

    let eval = block_on(client.evaluate("p.x + p.y", Some(frames[1].id), "watch")).unwrap();
    assert_eq!(eval.result, "7");

    block_on(client.step_out(thread)).unwrap();
    let Event::Stopped(stepped) = next_event(&events, |e| matches!(e, Event::Stopped(_))) else { unreachable!() };
    assert_eq!(stepped.reason, "step");
    let frames = block_on(client.stack_trace(thread, 1)).unwrap();
    assert!(frames[0].name.ends_with("main"));

    block_on(client.continue_(thread)).unwrap();
    let mut saw_output = false;
    let exit = next_event(&events, |e| {
        if let Event::Output(out) = e {
            saw_output |= out.output.contains("total=7");
        }
        matches!(e, Event::Exited { .. } | Event::Terminated)
    });
    assert!(matches!(exit, Event::Exited { exit_code: 0 } | Event::Terminated), "{exit:?}");
    assert!(saw_output, "the program's output should arrive as output events");
    let _ = block_on(client.disconnect(true));
}
