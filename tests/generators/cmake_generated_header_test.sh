#!/bin/bash
# Regression test for generated headers used as C includes.
#
# CMake wires custom-command outputs into object builds through phony
# intermediates:
#
#   object -> cmake_object_order_depends_target_app (phony)
#          -> gen_config (phony)
#          -> generated/config.h (custom command)
#
# rninja must wait for the header command even though the phony nodes have no
# command of their own. The header command sleeps briefly so a build that
# ignores the ordering reliably compiles the object too early.
#
# The test also changes the generated header's input and a normal included
# header. Both must recompile the object on the same run, using the compiler's
# depfile to discover the include dependency.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"

RNINJA="${RNINJA:-$PROJECT_ROOT/target/release/rninja}"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}=== CMake Generated Header Test ===${NC}"

if ! command -v cmake &> /dev/null || ! command -v ninja &> /dev/null; then
    echo -e "${YELLOW}SKIP: cmake or ninja not found${NC}"
    exit 0
fi

if [[ ! -x "$RNINJA" ]]; then
    cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>/dev/null
fi

TEST_DIR=$(mktemp -d)
trap "rm -rf '$TEST_DIR'" EXIT

# Use a path containing a space to exercise depfile and manifest escaping.
SRC="$TEST_DIR/project with space"
BUILD="$SRC/build"
mkdir -p "$SRC"

cat > "$SRC/CMakeLists.txt" << 'EOF'
cmake_minimum_required(VERSION 3.20)
project(gen_header C)

set(GENERATED_DIR "${CMAKE_CURRENT_BINARY_DIR}/generated")
set(GENERATED_HEADER "${GENERATED_DIR}/config.h")

add_custom_command(
    OUTPUT "${GENERATED_HEADER}"
    COMMAND ${CMAKE_COMMAND} -E make_directory "${GENERATED_DIR}"
    COMMAND ${CMAKE_COMMAND} -E sleep 2
    COMMAND ${CMAKE_COMMAND} -E copy "${CMAKE_CURRENT_SOURCE_DIR}/config.h.in" "${GENERATED_HEADER}"
    DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/config.h.in"
    COMMENT "Generating config.h"
    VERBATIM
)

add_custom_target(gen_config DEPENDS "${GENERATED_HEADER}")
add_executable(app main.c)
add_dependencies(app gen_config)
target_include_directories(app PRIVATE "${GENERATED_DIR}")
EOF

cat > "$SRC/main.c" << 'EOF'
#include "config.h"
#include "extra.h"
#include <stdio.h>

int main(void) {
    printf("%d\n", GENERATED_VALUE + EXTRA_VALUE);
    return 0;
}
EOF

cat > "$SRC/config.h.in" << 'EOF'
#pragma once
#define GENERATED_VALUE 42
EOF

cat > "$SRC/extra.h" << 'EOF'
#pragma once
#define EXTRA_VALUE 1
EOF

echo -e "${YELLOW}Configuring CMake project...${NC}"
cmake -G Ninja -S "$SRC" -B "$BUILD" > /dev/null

if ! grep -q "cmake_object_order_depends_target_app" "$BUILD/build.ninja"; then
    echo -e "${RED}FAIL: CMake did not emit cmake_object_order_depends_target_app${NC}"
    exit 1
fi

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;;
    *) EXE="" ;;
esac

CACHE_DIR="$TEST_DIR/cache"

run_rninja() {
    (cd "$BUILD" && RNINJA_CACHE_ENABLED=1 RNINJA_CACHE_DIR="$CACHE_DIR" \
        "$RNINJA" --no-daemon -j 4)
}

check_app() {
    local expected="$1"
    local actual
    actual=$("$BUILD/app$EXE")
    if [[ "$actual" != "$expected" ]]; then
        echo -e "${RED}FAIL: app output '$actual' != '$expected'${NC}"
        exit 1
    fi
}

echo -e "${YELLOW}Building with rninja (clean)...${NC}"
if ! run_rninja; then
    echo -e "${RED}FAIL: clean rninja build failed${NC}"
    exit 1
fi
check_app 43

if grep -q "DEP_FILE" "$BUILD/build.ninja"; then
    echo -e "${YELLOW}Rebuilding after generated header change...${NC}"
    cat > "$SRC/config.h.in" << 'EOF'
#pragma once
#define GENERATED_VALUE 43
EOF
    if ! run_rninja; then
        echo -e "${RED}FAIL: rninja build failed after generated header change${NC}"
        exit 1
    fi
    check_app 44

    echo -e "${YELLOW}Rebuilding after included header change...${NC}"
    cat > "$SRC/extra.h" << 'EOF'
#pragma once
#define EXTRA_VALUE 2
EOF
    if ! run_rninja; then
        echo -e "${RED}FAIL: rninja build failed after included header change${NC}"
        exit 1
    fi
    check_app 45

    echo -e "${YELLOW}Checking cache reuse after reverting headers...${NC}"
    cat > "$SRC/config.h.in" << 'EOF'
#pragma once
#define GENERATED_VALUE 42
EOF
    cat > "$SRC/extra.h" << 'EOF'
#pragma once
#define EXTRA_VALUE 1
EOF
    revert_output=$(run_rninja 2>&1)
    check_app 43
    if [[ "$revert_output" != *"(cached)"* ]]; then
        echo -e "${RED}FAIL: expected a cache hit after reverting headers${NC}"
        echo "$revert_output"
        exit 1
    fi
else
    echo -e "${YELLOW}SKIP: compiler does not produce depfiles${NC}"
fi

echo -e "${GREEN}PASS: generated and included headers were built before the object${NC}"
