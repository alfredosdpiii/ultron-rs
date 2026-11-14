//! What the language server says about files as they are on disk, with this process's environment:
//! `cargo run -p lsp --example check -- <file>...`. Waits while a server starts, installs or loads its project.

use lsp::{LspGuard, Report};
use std::time::{Duration, Instant};

fn main() {
    let guard = LspGuard::new(std::env::vars().collect());
    for file in std::env::args().skip(1) {
        let path = std::fs::canonicalize(&file).expect("a file");
        let text = std::fs::read_to_string(&path).expect("a text file");
        let started = Instant::now();
        let report = loop {
            let report = guard.diagnose(&path, &text, Duration::from_secs(4));
            if !matches!(report, Report::Starting { .. }) || started.elapsed() > Duration::from_secs(900) {
                break report;
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        println!("{file} ({:.1} s): {report:#?}", started.elapsed().as_secs_f64());
    }
}
