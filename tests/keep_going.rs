//! End-to-end coverage for `-k`/`--keep-going` failure limits.
//!
//! Every edge in these manifests fails immediately, so the number of
//! `FAILED:` messages equals the number of commands that actually ran.

use assert_cmd::cargo::cargo_bin_cmd;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
const FAIL_COMMAND: &str = "cmd /C exit /b 1";
#[cfg(not(windows))]
const FAIL_COMMAND: &str = "false";

#[cfg(windows)]
const SLOW_COMMAND: &str = "cmd /C \"ping -n 6 127.0.0.1 >nul\"";
#[cfg(not(windows))]
const SLOW_COMMAND: &str = "sleep 5";

fn write_manifest(dir: &Path, manifest: &str) {
    fs::write(dir.join("build.ninja"), manifest).expect("write build.ninja");
}

fn independent_manifest(count: usize) -> String {
    let mut manifest =
        format!("rule fail\n  command = {FAIL_COMMAND}\n  description = FAIL $out\n\n");
    for i in 1..=count {
        writeln!(manifest, "build out{i}.txt: fail").expect("write edge");
    }
    let outputs = (1..=count)
        .map(|i| format!("out{i}.txt"))
        .collect::<Vec<_>>()
        .join(" ");
    writeln!(manifest, "\nbuild all: phony {outputs}").expect("write phony");
    writeln!(manifest, "default all").expect("write default");
    manifest
}

fn chain_manifest() -> String {
    format!(
        "rule fail\n  command = {FAIL_COMMAND}\n  description = FAIL $out\n\n\
         build a: fail\n\
         build b: fail a\n\
         build c: fail b\n\
         build all: phony c\n\
         default all\n"
    )
}

fn kill_on_failure_manifest() -> String {
    format!(
        "rule fail\n  command = {FAIL_COMMAND}\n  description = FAIL $out\n\
         rule slow\n  command = {SLOW_COMMAND}\n  description = SLOW $out\n\n\
         build fail.txt: fail\n\
         build slow.txt: slow\n\
         build all: phony fail.txt slow.txt\n\
         default all\n"
    )
}

fn run(dir: &Path, args: &[&str]) -> Output {
    cargo_bin_cmd!("rninja")
        .current_dir(dir)
        .env("RNINJA_CACHE_ENABLED", "0")
        .args(args)
        .output()
        .expect("run rninja")
}

/// Run rninja, but fail the test instead of hanging forever if it does not
/// exit. This keeps the dependent-cancellation regression from wedging CI.
fn run_with_timeout(dir: &Path, args: &[&str], timeout: Duration) -> Output {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rninja"))
        .current_dir(dir)
        .env("RNINJA_CACHE_ENABLED", "0")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rninja");

    let start = Instant::now();
    loop {
        match child.try_wait().expect("wait for rninja") {
            Some(_) => return child.wait_with_output().expect("collect rninja output"),
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                let output = child.wait_with_output().expect("collect timed-out output");
                panic!(
                    "rninja {:?} did not exit within {timeout:?}\nstdout:\n{}\nstderr:\n{}",
                    args,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            None => thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn failures(output: &Output) -> usize {
    String::from_utf8_lossy(&output.stderr)
        .matches("FAILED: ")
        .count()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn default_keep_going_stops_after_first_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_manifest(dir.path(), &independent_manifest(8));

    let output = run(dir.path(), &["-j", "1"]);

    assert!(!output.status.success());
    assert_eq!(
        failures(&output),
        1,
        "default -k should stop after one failure:\n{}",
        stderr(&output)
    );
}

#[test]
fn keep_going_two_stops_after_two_failures() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_manifest(dir.path(), &independent_manifest(8));

    let output = run(dir.path(), &["-j", "1", "-k", "2"]);

    assert!(!output.status.success());
    assert_eq!(
        failures(&output),
        2,
        "-k 2 should stop after two failures:\n{}",
        stderr(&output)
    );
}

#[test]
fn keep_going_zero_runs_everything() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_manifest(dir.path(), &independent_manifest(8));

    let output = run(dir.path(), &["-j", "1", "-k", "0"]);

    assert!(!output.status.success());
    assert_eq!(
        failures(&output),
        8,
        "-k 0 should keep going after every failure:\n{}",
        stderr(&output)
    );
}

#[test]
fn parallel_keep_going_allows_only_in_flight_failures() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_manifest(dir.path(), &independent_manifest(8));

    let output = run(dir.path(), &["-j", "4"]);

    assert!(!output.status.success());
    let failed = failures(&output);
    assert!(
        (1..=4).contains(&failed),
        "with -j 4 at most four already-running jobs may fail, got {failed}:\n{}",
        stderr(&output)
    );
}

#[test]
fn kill_after_conflicts_with_keep_going() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_manifest(dir.path(), &independent_manifest(1));

    let output = run(dir.path(), &["-k", "1", "-K", "1"]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("cannot be used with"),
        "expected a conflict error:\n{}",
        stderr(&output)
    );
}

#[test]
fn kill_on_failure_terminates_running_jobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_manifest(dir.path(), &kill_on_failure_manifest());

    let start = Instant::now();
    let output = run(dir.path(), &["-j", "2", "-K", "1"]);
    let elapsed = start.elapsed();

    assert!(!output.status.success());
    assert_eq!(
        failures(&output),
        1,
        "only the failing job should be reported:\n{}",
        stderr(&output)
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "--kill-on-failure should not wait for the slow job; took {elapsed:?}:\n{}",
        stderr(&output)
    );
}

#[test]
fn failed_dependencies_are_cancelled_without_hanging() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_manifest(dir.path(), &chain_manifest());

    for args in [&["-j", "1", "-k", "0"][..], &["-j", "1", "-k", "2"][..]] {
        let output = run_with_timeout(dir.path(), args, Duration::from_secs(10));

        assert!(!output.status.success(), "args: {args:?}");
        assert_eq!(
            failures(&output),
            1,
            "only the root command should fail for {args:?}:\n{}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("1 targets failed"),
            "skipped dependents must not count as failures for {args:?}:\n{}",
            stderr(&output)
        );
    }
}
