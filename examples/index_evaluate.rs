//! Reproducible index workload; uses an isolated store and no network dependencies.
use anyhow::{Result, ensure};
use rust_repo_intelligence::Service;
use serde_json::json;
use std::{fs, time::Instant};

fn main() -> Result<()> {
    let module_count: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(64);
    ensure!(
        module_count > 0 && 2048 % module_count == 0,
        "module count must divide 2048"
    );
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join("src"))?;
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='index-workload'\nversion='0.1.0'\nedition='2024'\n",
    )?;
    let mut modules = String::new();
    for module in 0..module_count {
        modules.push_str(&format!("pub mod module_{module};\n"));
        let mut source = String::new();
        for function in 0..(2048 / module_count) {
            source.push_str(&format!("pub fn function_{module}_{function}(value: usize) -> usize {{ value + {function} }}\n"));
        }
        fs::write(root.path().join(format!("src/module_{module}.rs")), source)?;
    }
    fs::write(root.path().join("src/lib.rs"), modules)?;
    let mut service = Service::open(root.path())?;
    let mut full = Vec::new();
    let mut unchanged = Vec::new();
    let mut document = Vec::new();
    let mut phases = Vec::new();
    for sample in 0..3 {
        let started = Instant::now();
        let summary = service.refresh(Some("full"))?;
        full.push(started.elapsed().as_secs_f64() * 1000.0);
        phases.push(json!({"mode":"full","timings":summary["timings"]}));
        ensure!(
            summary["counts"]["nodes"] == 2048 + module_count,
            "lost indexed symbols: {summary}"
        );
        let started = Instant::now();
        let summary = service.refresh(None)?;
        unchanged.push(started.elapsed().as_secs_f64() * 1000.0);
        ensure!(summary["mode"] == "unchanged", "no-op published: {summary}");
        fs::write(
            root.path().join("README.md"),
            format!("# Workload {sample}\n"),
        )?;
        let started = Instant::now();
        let summary = service.refresh(None)?;
        document.push(started.elapsed().as_secs_f64() * 1000.0);
        phases.push(json!({"mode":"document","timings":summary["timings"]}));
        ensure!(
            summary["changed_inputs"] == 1 && summary["counts"]["nodes"] == 2048 + module_count,
            "document edit changed symbols: {summary}"
        );
    }
    println!(
        "{}",
        json!({"workload":{"files":module_count + 1,"functions":2048,"samples":3},"full_ms":full,"unchanged_ms":unchanged,"document_ms":document,"phases":phases})
    );
    Ok(())
}
