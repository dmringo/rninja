#!/bin/bash
# Verify Ninja/rninja interoperability for no-op builds and header changes.
#
# Both tools use .ninja_log and .ninja_deps. rninja must read Ninja's logs so
# it can no-op after Ninja, and write compatible logs so Ninja can no-op after
# rninja. Header changes must still trigger a rebuild in either direction.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"

RNINJA="${RNINJA:-$PROJECT_ROOT/target/release/rninja}"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}=== Ninja NOP Compatibility Test ===${NC}"

if ! command -v cmake &> /dev/null || ! command -v ninja &> /dev/null; then
    echo -e "${YELLOW}SKIP: cmake or ninja not found${NC}"
    exit 0
fi

if [[ ! -x "$RNINJA" ]]; then
    cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>/dev/null
fi

TEST_DIR=$(mktemp -d)
trap "rm -rf '$TEST_DIR'" EXIT

SRC="$TEST_DIR/src"
mkdir -p "$SRC"

cat > "$SRC/CMakeLists.txt" << 'EOF'
cmake_minimum_required(VERSION 3.20)
project(nop CXX)
add_executable(app main.cpp)
EOF

cat > "$SRC/main.cpp" << 'EOF'
#include "value.h"
#include <cstdio>

int main() {
    std::printf("%d\n", VALUE);
    return 0;
}
EOF

cat > "$SRC/value.h" << 'EOF'
#pragma once
#define VALUE 1
EOF

run_ninja() {
    (cd "$1" && ninja)
}

run_rninja() {
    (cd "$1" && RNINJA_CACHE_ENABLED=0 "$RNINJA")
}

expect_rninja_noop() {
    local out
    out=$(cd "$1" && RNINJA_CACHE_ENABLED=0 "$RNINJA" -n 2>&1)
    if [[ "$out" != *"no work to do"* ]]; then
        echo -e "${RED}FAIL: rninja did not no-op in $1${NC}"
        echo "$out"
        exit 1
    fi
}

expect_ninja_noop() {
    local out
    out=$(cd "$1" && ninja -n 2>&1)
    if [[ "$out" != *"no work to do"* ]]; then
        echo -e "${RED}FAIL: ninja did not no-op in $1${NC}"
        echo "$out"
        exit 1
    fi
}

# Ninja first: rninja must read .ninja_deps/.ninja_log and no-op.
BUILD_NINJA="$TEST_DIR/build-ninja"
cmake -G Ninja -S "$SRC" -B "$BUILD_NINJA" > /dev/null
if ! grep -q "DEP_FILE" "$BUILD_NINJA/build.ninja"; then
    echo -e "${YELLOW}SKIP: compiler does not produce depfiles${NC}"
    exit 0
fi

echo -e "${YELLOW}Building with Ninja...${NC}"
run_ninja "$BUILD_NINJA"
expect_rninja_noop "$BUILD_NINJA"

echo -e "${YELLOW}Changing a header, then building with rninja...${NC}"
cat > "$SRC/value.h" << 'EOF'
#pragma once
#define VALUE 2
EOF
run_rninja "$BUILD_NINJA"
expect_ninja_noop "$BUILD_NINJA"

# rninja first: Ninja must read the logs written by rninja and no-op.
BUILD_RNINJA="$TEST_DIR/build-rninja"
cmake -G Ninja -S "$SRC" -B "$BUILD_RNINJA" > /dev/null

echo -e "${YELLOW}Building with rninja...${NC}"
run_rninja "$BUILD_RNINJA"
expect_ninja_noop "$BUILD_RNINJA"

echo -e "${YELLOW}Changing a header, then building with Ninja...${NC}"
cat > "$SRC/value.h" << 'EOF'
#pragma once
#define VALUE 3
EOF
run_ninja "$BUILD_RNINJA"
expect_rninja_noop "$BUILD_RNINJA"

echo -e "${GREEN}PASS: Ninja and rninja no-op after each other and track header changes${NC}"
