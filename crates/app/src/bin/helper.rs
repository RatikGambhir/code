//! Chromium's helper processes (renderer, GPU, utility). Chromium launches
//! the `Cockpit Helper*.app` bundles with this executable inside; it never
//! runs on its own.

fn main() {
    std::process::exit(cockpit_browser::run_helper());
}
