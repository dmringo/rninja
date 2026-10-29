//! Verify that concurrent rninja builds in one directory are rejected.

use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::Path;
use std::process::{Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
const SLOW_COMMAND: &str = "cmd /C \"ping -n 4 127.0.0.1 >nul & echo built> $out\"";
#[cfg(not(windows))]
const SLOW_COMMAND: &str = "sleep 3; echo built > $out";

fn write_manifest(dir: &Path) {
    fs::write(
        dir.join("build.ninja"),
        format!(
            "rule slow\n  command = {SLOW_COMMAND}\n  description = SLOW $out\n\n\
             build slow.txt: slow\n\
             default slow.txt\n"
        ),
    )
    .expect("write build.ninja");
}

fn run(dir: &Path) -> Output {
    cargo_bin_cmd!("rninja")
        .current_dir(dir)
        .env("RNINJA_CACHE_ENABLED", "0")
        .args(["-j", "1"])
        .output()
        .expect("run rninja")
}

#[test]
fn concurrent_invocations_are_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_manifest(dir.path());

    let mut first = std::process::Command::new(env!("CARGO_BIN_EXE_rninja"))
        .current_dir(dir.path())
        .env("RNINJA_CACHE_ENABLED", "0")
        .args(["-j", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn first rninja");

    let lock = dir.path().join(".rninja_lock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !lock.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !lock.exists() {
        let _ = first.kill();
        let _ = first.wait();
        panic!("first build did not create .rninja_lock");
    }

    let second = run(dir.path());
    assert!(
        !second.status.success(),
        "second build should have been rejected"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another rninja build"),
        "unexpected second-build output:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let status = first.wait().expect("wait for first rninja");
    assert!(status.success(), "first build failed: {status:?}");
}
