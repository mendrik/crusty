#[cfg(unix)]
#[test]
fn observatory_mcp_regressions() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/mcp_regressions.py"
        ))
        .env(
            "CRUSTY_TEST_BINARY",
            env!("CARGO_BIN_EXE_rust-repo-intelligence"),
        )
        .output()
        .expect("python3 is required for the isolated MCP protocol regression");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
