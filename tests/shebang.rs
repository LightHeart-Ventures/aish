//! TASK-18 — end-to-end shebang support.
//!
//! A file that begins with `#!/usr/bin/env aish` (or `#!<absolute path to
//! aish>`) and is marked executable runs as a program: the kernel execs aish
//! with the script path, script mode (TASK-17) runs the body, and the leading
//! `#!` line is skipped because it is a `#` comment. These tests drive the REAL
//! compiled binary through the OS shebang path — something a unit test can't
//! reach.
//!
//! Hermetic by construction: a throwaway `HOME` and working directory (so no
//! real `~/.aish` or project `.mcp.json` is touched and MCP connects to
//! nothing), plus a dummy `ANTHROPIC_API_KEY` so the default Claude backend
//! *constructs* without a real credential. The script bodies only run `echo`,
//! so no model/API call is ever made — the key never reaches the wire.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path to the binary cargo built for this test run.
const AISH_BIN: &str = env!("CARGO_BIN_EXE_aish");

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// A fresh, empty scratch dir that doubles as `HOME` and the working directory.
fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("aish-shebang-{tag}-{}-{}", std::process::id(), nanos()));
    fs::create_dir_all(&p).expect("create scratch dir");
    p
}

/// Write `body` to `path` and mark it executable (rwxr-xr-x).
fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod +x");
}

/// Run an executable file directly (relying on its shebang) inside a hermetic
/// environment. `extra_path`, when set, is prepended to `PATH` so a
/// `#!/usr/bin/env aish` line can resolve `aish`.
fn run_executable(file: &Path, home: &Path, extra_path: Option<&Path>) -> std::process::Output {
    let mut cmd = Command::new(file);
    cmd.current_dir(home)
        .env("HOME", home)
        // Dummy credential: lets the Claude backend construct; the script body
        // never calls the model, so it's never used on the wire.
        .env("ANTHROPIC_API_KEY", "sk-ant-shebang-test-not-used");
    if let Some(dir) = extra_path {
        let base = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{}", dir.display(), base));
    }
    cmd.output().expect("execute shebang script")
}

#[test]
fn absolute_path_shebang_runs_the_script() {
    let home = scratch("abs");
    let script = home.join("hello.aish");
    write_executable(
        &script,
        &format!("#!{AISH_BIN}\n# a comment line, skipped\n\necho shebang-abs-ok\n"),
    );

    let out = run_executable(&script, &home, None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("shebang-abs-ok"),
        "script body did not run via an absolute-path shebang.\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "exit status: {:?}", out.status.code());

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn env_shebang_resolves_aish_on_path() {
    let home = scratch("env");
    // `#!/usr/bin/env aish` resolves `aish` via PATH, so expose the built binary
    // under the name `aish` in a dir we prepend to PATH.
    let bindir = home.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    let linked = bindir.join("aish");
    // A symlink keeps it cheap; fall back to a copy if symlinking is unavailable.
    if std::os::unix::fs::symlink(AISH_BIN, &linked).is_err() {
        fs::copy(AISH_BIN, &linked).expect("stage aish on PATH");
        let mut perms = fs::metadata(&linked).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&linked, perms).unwrap();
    }

    let script = home.join("hello.aish");
    write_executable(&script, "#!/usr/bin/env aish\necho shebang-env-ok\n");

    let out = run_executable(&script, &home, Some(&bindir));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("shebang-env-ok"),
        "script body did not run via a `/usr/bin/env aish` shebang.\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    let _ = fs::remove_dir_all(&home);
}
