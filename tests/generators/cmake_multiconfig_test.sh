#!/bin/bash
# Test rninja with CMake's Ninja Multi-Config generator
#
# Ninja Multi-Config manifests differ from single-config ones in ways that
# exercise several parser features:
#   - nested includes (build.ninja -> CMakeFiles/impl-Debug.ninja -> common.ninja)
#   - escaped colons in target names (all$:Debug)
#   - escaped spaces in paths and implicit outputs
#   - build-specific variables and undefined variables (${LAUNCHER})
#   - multiple configurations via -f build-<Config>.ninja

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"

RNINJA="${RNINJA:-$PROJECT_ROOT/target/release/rninja}"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}=== CMake Ninja Multi-Config Test ===${NC}"

if ! command -v cmake &> /dev/null || ! command -v ninja &> /dev/null; then
    echo -e "${YELLOW}SKIP: cmake or ninja not found${NC}"
    exit 0
fi

if ! cmake --help 2>/dev/null | grep -q "Ninja Multi-Config"; then
    echo -e "${YELLOW}SKIP: CMake does not support Ninja Multi-Config${NC}"
    exit 0
fi

if [[ ! -x "$RNINJA" ]]; then
    cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>/dev/null
fi

TEST_DIR=$(mktemp -d)
trap "rm -rf '$TEST_DIR'" EXIT

# Use a path containing a space to exercise `$ ` path escapes.
SRC="$TEST_DIR/project with space"
BUILD="$SRC/build"
mkdir -p "$SRC"

cat > "$SRC/CMakeLists.txt" << 'EOF'
cmake_minimum_required(VERSION 3.20)
project(multiconfig C)
add_library(mylib STATIC lib.c "extra file.c")
add_executable(myapp main.c)
target_link_libraries(myapp mylib)
EOF

cat > "$SRC/lib.c" << 'EOF'
int add(int a, int b) { return a + b; }
EOF

cat > "$SRC/extra file.c" << 'EOF'
int mul(int a, int b) { return a * b; }
EOF

cat > "$SRC/main.c" << 'EOF'
#include <stdio.h>
extern int add(int a, int b);
int main(void) { printf("%d\n", add(1, 2)); return 0; }
EOF

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;;
    *) EXE="" ;;
esac

echo -e "${YELLOW}Configuring Ninja Multi-Config project...${NC}"
cmake -G "Ninja Multi-Config" -S "$SRC" -B "$BUILD" > /dev/null

for manifest in build.ninja build-Debug.ninja build-Release.ninja; do
    if [[ ! -f "$BUILD/$manifest" ]]; then
        echo -e "${RED}FAIL: CMake did not generate $manifest${NC}"
        exit 1
    fi
done

cd "$BUILD"

echo -e "${YELLOW}Building Debug with rninja...${NC}"
RNINJA_CACHE_ENABLED=0 "$RNINJA" --no-daemon > /dev/null

if [[ ! -x "Debug/myapp$EXE" ]]; then
    echo -e "${RED}FAIL: Debug/myapp$EXE not found${NC}"
    exit 1
fi
if [[ "$(./Debug/myapp$EXE)" != "3" ]]; then
    echo -e "${RED}FAIL: Debug executable produced wrong output${NC}"
    exit 1
fi
echo -e "${GREEN}PASS: Debug build${NC}"

echo -e "${YELLOW}Building Release with rninja...${NC}"
RNINJA_CACHE_ENABLED=0 "$RNINJA" --no-daemon -f build-Release.ninja > /dev/null

if [[ ! -x "Release/myapp$EXE" ]]; then
    echo -e "${RED}FAIL: Release/myapp$EXE not found${NC}"
    exit 1
fi
if [[ "$(./Release/myapp$EXE)" != "3" ]]; then
    echo -e "${RED}FAIL: Release executable produced wrong output${NC}"
    exit 1
fi
echo -e "${GREEN}PASS: Release build${NC}"

echo -e "${YELLOW}Checking no-op rebuilds...${NC}"
noop=$(RNINJA_CACHE_ENABLED=0 "$RNINJA" --no-daemon 2>&1)
if [[ "$noop" != *"no work to do"* ]]; then
    echo -e "${RED}FAIL: Expected no work to do, got: $noop${NC}"
    exit 1
fi
echo -e "${GREEN}PASS: No-op rebuild${NC}"

echo -e "${YELLOW}Checking incremental rebuild (dependency ordering)...${NC}"
touch "$SRC/lib.c"
RNINJA_CACHE_ENABLED=0 "$RNINJA" --no-daemon -f build-Release.ninja > /dev/null
if [[ "$(./Release/myapp$EXE)" != "3" ]]; then
    echo -e "${RED}FAIL: Release executable produced wrong output after rebuild${NC}"
    exit 1
fi
echo -e "${GREEN}PASS: Incremental rebuild${NC}"

echo -e "${YELLOW}Comparing expanded commands with ninja...${NC}"
# Ninja writes CRLF on Windows while rninja writes LF; normalize before
# comparing, and sort because the tools may report edges in a different order.
ninja -f build-Debug.ninja -t commands all \
    | tr -d '\r' | LC_ALL=C sort > "$TEST_DIR/ninja.cmds"
RNINJA_CACHE_ENABLED=0 "$RNINJA" --no-daemon -f build-Debug.ninja -t commands all \
    | tr -d '\r' | LC_ALL=C sort > "$TEST_DIR/rninja.cmds"
if ! diff -u "$TEST_DIR/ninja.cmds" "$TEST_DIR/rninja.cmds"; then
    echo -e "${RED}FAIL: Expanded commands differ from ninja${NC}"
    exit 1
fi
echo -e "${GREEN}PASS: Commands match ninja${NC}"

echo -e "${YELLOW}Checking clean preserves sources and generated manifests...${NC}"
RNINJA_CACHE_ENABLED=0 "$RNINJA" --no-daemon -t clean > /dev/null
for file in "$SRC/CMakeLists.txt" "$SRC/lib.c" "$SRC/extra file.c" \
            "$BUILD/build.ninja" "$BUILD/build-Debug.ninja"; do
    if [[ ! -f "$file" ]]; then
        echo -e "${RED}FAIL: clean removed $file${NC}"
        exit 1
    fi
done
if [[ -e "Debug/myapp$EXE" ]]; then
    echo -e "${RED}FAIL: clean did not remove Debug/myapp$EXE${NC}"
    exit 1
fi
echo -e "${GREEN}PASS: Clean${NC}"

echo -e "${YELLOW}Rebuilding after clean...${NC}"
RNINJA_CACHE_ENABLED=0 "$RNINJA" --no-daemon > /dev/null
if [[ "$(./Debug/myapp$EXE)" != "3" ]]; then
    echo -e "${RED}FAIL: Debug executable produced wrong output after clean${NC}"
    exit 1
fi

echo -e "${GREEN}CMake Ninja Multi-Config test PASSED${NC}"
