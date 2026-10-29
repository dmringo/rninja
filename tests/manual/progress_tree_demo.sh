#!/usr/bin/env bash
#
# Manual demo for the `--progress tree` renderer.
#
# Generates a small nested build graph, then runs rninja against it:
#
#   tests/manual/progress_tree_demo.sh [extra rninja args...]
#
# The graph has three levels of dependencies, a shared dependency, and two
# nested phony aliases, so the live tree shows branch counts and pruning.
# Only tasks that are actually executing (plus their ancestors) are drawn, so
# the initial view is just the root. The full shape, reached once everything
# along these paths is running, is:
#
#   [0/9] build
#   `- [0/9] check
#      `- [0/9] all
#         `- [0/9] GEN bin/app.txt
#            |- [0/5] GEN lib/core.txt
#            |  `- [0/4] GEN obj/a.txt
#            |     |- [0/2] GEN obj/shared.txt
#            |     |  `- [0/1] GEN src/one.txt
#            |     `- [0/1] GEN src/two.txt
#            `- [0/3] GEN lib/extra.txt
#               `- [0/2] GEN obj/b.txt
#                  `- [0/1] GEN src/deep/three.txt
#
# Queued tasks stay hidden until they start, and finished tasks disappear.
# Running tasks show their elapsed execution time once they have been running
# for at least a second, for example `GEN obj/shared.txt (1.5s)`.
#
# `obj/shared.txt` is a shared dependency: it appears once, under obj/a.txt,
# and is not duplicated under obj/b.txt.
#
# Tasks are silent on success, like a normal compiler build. Task output is
# buffered while the tree is drawn and only shown when a task fails; pass -v to
# also show successful output and a full command line above the tree for each
# task. The tree labels stay as descriptions. The tree is cleared at the end
# and replaced by a single summary line:
#
#   ninja: finished 9 tasks
#
# This script is manual-only (it needs a terminal and takes a few seconds), so
# it is not part of tests/run_all_tests.sh.
#
# Environment:
#   J=4                    parallelism passed to rninja
#   KEEP=1                 keep the generated project directory on exit
#   RNINJA_CACHE_ENABLED   defaults to 0 so every run executes the commands
#   DEMO_NOISY=1           make each successful task print a line; combine
#                          with -v to see it above the tree
#   DEMO_FAIL=1            make obj/b.txt fail after writing to stdout and
#                          stderr, to exercise failure-only output
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

BIN="$ROOT/target/release/rninja"
if [ -x "$BIN.exe" ]; then
    BIN="$BIN.exe"
elif [ -x "$BIN" ]; then
    :
elif [ -x "$ROOT/target/debug/rninja.exe" ]; then
    BIN="$ROOT/target/debug/rninja.exe"
elif [ -x "$ROOT/target/debug/rninja" ]; then
    BIN="$ROOT/target/debug/rninja"
else
    echo "rninja not built; running cargo build --release..." >&2
    (cd "$ROOT" && cargo build --release --quiet)
    if [ -x "$ROOT/target/release/rninja.exe" ]; then
        BIN="$ROOT/target/release/rninja.exe"
    else
        BIN="$ROOT/target/release/rninja"
    fi
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/rninja-progress-XXXXXX")"
cleanup() {
    if [ "${KEEP:-0}" != "1" ]; then
        rm -rf "$WORK"
    else
        echo "kept demo project at: $WORK" >&2
    fi
}
trap cleanup EXIT

case "$(uname -s 2>/dev/null || echo unknown)" in
    MINGW* | MSYS* | CYGWIN*) windows=1 ;;
    *) windows=0 ;;
esac

# Each task sleeps briefly so the tree stays visible. DEMO_NOISY=1 makes the
# helper print a line on success (visible with -v); DEMO_FAIL=1 makes obj/b.txt
# fail after printing to both streams, demonstrating failure-only output.
if [ "$windows" = 1 ]; then
    cat >"$WORK/task.cmd" <<'EOF'
@echo off
if not exist "%~dp1" mkdir "%~dp1"
ping -n 2 127.0.0.1 >nul
if "%DEMO_NOISY%"=="1" echo building %1
if "%DEMO_FAIL%"=="1" if "%~1"=="obj/b.txt" (
    echo simulated stdout from failing %1
    echo simulated stderr from failing %1 1>&2
    exit /b 1
)
echo built %1> %1
EOF
    RULE_COMMAND='cmd /C task.cmd $out'
    mkdir -p "$WORK/src/deep"
    : >"$WORK/src/one.in"
    : >"$WORK/src/two.in"
    : >"$WORK/src/deep/three.in"
else
    cat >"$WORK/task.sh" <<'EOF'
#!/bin/sh
mkdir -p "$(dirname "$1")"
sleep 0.4
if [ "${DEMO_NOISY:-0}" = "1" ]; then
    echo "building $1"
fi
if [ "${DEMO_FAIL:-0}" = "1" ] && [ "$1" = "obj/b.txt" ]; then
    echo "simulated stdout from failing $1"
    echo "simulated stderr from failing $1" >&2
    exit 1
fi
echo "built $1" >"$1"
EOF
    chmod +x "$WORK/task.sh"
    RULE_COMMAND='sh task.sh $out'
    mkdir -p "$WORK/src/deep"
    : >"$WORK/src/one.in"
    : >"$WORK/src/two.in"
    : >"$WORK/src/deep/three.in"
fi

{
    echo "rule gen"
    echo "  command = $RULE_COMMAND"
    echo "  description = GEN \$out"
    echo
    echo "build src/one.txt: gen src/one.in"
    echo "build src/two.txt: gen src/two.in"
    echo "build src/deep/three.txt: gen src/deep/three.in"
    echo "build obj/shared.txt: gen src/one.txt"
    echo "build obj/a.txt: gen obj/shared.txt src/two.txt"
    echo "build obj/b.txt: gen obj/shared.txt src/deep/three.txt"
    echo "build lib/core.txt: gen obj/a.txt"
    echo "build lib/extra.txt: gen obj/b.txt"
    echo "build bin/app.txt: gen lib/core.txt lib/extra.txt"
    echo "build all: phony bin/app.txt"
    echo "build check: phony all"
    echo "default check"
} >"$WORK/build.ninja"

export RNINJA_CACHE_ENABLED="${RNINJA_CACHE_ENABLED:-0}"
cd "$WORK"

J="${J:-4}"

# Git Bash/MSYS terminals do not always expose a Windows console handle, so
# route through msys `script` when it is available to get a real pty.
if [ "$windows" = 1 ] && command -v script >/dev/null 2>&1; then
    cmd="$BIN --progress tree -j $J"
    for arg in "$@"; do
        cmd="$cmd $(printf '%q' "$arg")"
    done
    exec script -q -c "$cmd" /dev/null
fi

exec "$BIN" --progress tree -j "$J" "$@"
