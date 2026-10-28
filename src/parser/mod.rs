mod lexer;
mod manifest;

pub use manifest::{Build, Manifest, Pool, Rule};

use crate::error::ParseError;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Parse a ninja build file and return the manifest
pub fn parse(content: &str) -> Result<Manifest, ParseError> {
    parse_with_base(content, None)
}

/// Parse a ninja build file with a base directory for includes
pub fn parse_file(path: &Path) -> Result<Manifest, ParseError> {
    let content = std::fs::read_to_string(path).map_err(|e| ParseError::Syntax {
        line: 0,
        message: format!("failed to read {}: {}", path.display(), e),
    })?;

    let base_dir = path.parent().map(|p| p.to_path_buf());
    parse_with_base(&content, base_dir)
}

fn parse_with_base(content: &str, base_dir: Option<PathBuf>) -> Result<Manifest, ParseError> {
    let mut parser = Parser::new(content, base_dir);
    parser.parse()
}

struct Parser<'a> {
    lexer: lexer::Lexer<'a>,
    manifest: Manifest,
    current_line: usize,
    base_dir: Option<PathBuf>,
    included_files: HashSet<PathBuf>,
}

impl<'a> Parser<'a> {
    fn new(content: &'a str, base_dir: Option<PathBuf>) -> Self {
        Self {
            lexer: lexer::Lexer::new(content),
            manifest: Manifest::default(),
            current_line: 1,
            base_dir,
            included_files: HashSet::new(),
        }
    }

    fn parse(&mut self) -> Result<Manifest, ParseError> {
        while let Some(line) = self.lexer.next_line() {
            self.current_line = self.lexer.line_number();

            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(rest) = line.strip_prefix("rule ") {
                self.parse_rule(rest.trim())?;
            } else if let Some(rest) = line.strip_prefix("build ") {
                self.parse_build(rest.trim())?;
            } else if let Some(rest) = line.strip_prefix("default ") {
                self.parse_default(rest.trim())?;
            } else if let Some(rest) = line.strip_prefix("pool ") {
                self.parse_pool(rest.trim())?;
            } else if let Some(rest) = line.strip_prefix("include ") {
                self.parse_include(rest.trim())?;
            } else if let Some(rest) = line.strip_prefix("subninja ") {
                self.parse_subninja(rest.trim())?;
            } else if line.contains('=') {
                self.parse_variable(line)?;
            } else {
                return Err(ParseError::Syntax {
                    line: self.current_line,
                    message: format!("unexpected line: {}", line),
                });
            }
        }

        Ok(std::mem::take(&mut self.manifest))
    }

    fn parse_rule(&mut self, name: &str) -> Result<(), ParseError> {
        if self.manifest.rules.contains_key(name) {
            return Err(ParseError::DuplicateRule {
                name: name.to_string(),
            });
        }

        let mut rule = Rule {
            name: name.to_string(),
            ..Default::default()
        };

        // Parse indented variables
        while let Some(line) = self.lexer.peek_line() {
            if !line.starts_with(' ') && !line.starts_with('\t') {
                break;
            }
            let line = self.lexer.next_line().ok_or_else(|| ParseError::Syntax {
                line: self.current_line,
                message: "unexpected end of file while parsing rule".to_string(),
            })?;
            self.current_line = self.lexer.line_number();

            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                match key {
                    "command" => rule.command = Some(value.to_string()),
                    "description" => rule.description = Some(value.to_string()),
                    "depfile" => rule.depfile = Some(value.to_string()),
                    "deps" => rule.deps = Some(value.to_string()),
                    "generator" => rule.generator = value == "1" || value == "true",
                    "restat" => rule.restat = value == "1" || value == "true",
                    "rspfile" => rule.rspfile = Some(value.to_string()),
                    "rspfile_content" => rule.rspfile_content = Some(value.to_string()),
                    "pool" => rule.pool = Some(value.to_string()),
                    _ => {
                        // Store unknown variables for later expansion
                        rule.variables.insert(key.to_string(), value.to_string());
                    }
                }
            }
        }

        self.manifest.rules.insert(name.to_string(), rule);
        Ok(())
    }

    fn parse_build(&mut self, line: &str) -> Result<(), ParseError> {
        // Format: outputs [| implicit_outputs]: rule inputs | implicit_deps || order_only_deps
        //
        // Colons can appear in output paths when escaped as `$:` (for example
        // CMake's Ninja Multi-Config aliases such as `all$:Debug`), so find
        // the first colon that is not part of an escape sequence.
        let (outputs_part, rest) =
            split_at_unescaped(line, ':').ok_or_else(|| ParseError::Syntax {
                line: self.current_line,
                message: "build line missing ':'".to_string(),
            })?;

        let (explicit_outputs, implicit_outputs) =
            split_at_unescaped(outputs_part, '|').map_or((outputs_part, ""), |(a, b)| (a, b));

        let outputs = self.split_paths(explicit_outputs);
        let implicit_outputs = self.split_paths(implicit_outputs);

        if outputs.is_empty() {
            return Err(ParseError::Syntax {
                line: self.current_line,
                message: "build line has no outputs".to_string(),
            });
        }

        let rest = rest.trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let rule = parts.next().unwrap_or("").to_string();
        let deps_str = parts.next().unwrap_or("");

        // Parse dependencies: inputs | implicit || order_only
        let (explicit, rest) = deps_str
            .split_once("||")
            .map_or((deps_str, ""), |(a, b)| (a, b));
        let order_only: Vec<String> = self.split_paths(rest);

        let (explicit, implicit_str) = explicit.split_once('|').unwrap_or((explicit, ""));
        let inputs: Vec<String> = self.split_paths(explicit);
        let implicit: Vec<String> = self.split_paths(implicit_str);

        let mut build = Build {
            outputs,
            implicit_outputs,
            rule,
            inputs,
            implicit_deps: implicit,
            order_only_deps: order_only,
            ..Default::default()
        };

        // Parse indented variables (build-specific overrides)
        while let Some(line) = self.lexer.peek_line() {
            if !line.starts_with(' ') && !line.starts_with('\t') {
                break;
            }
            let line = self.lexer.next_line().ok_or_else(|| ParseError::Syntax {
                line: self.current_line,
                message: "unexpected end of file while parsing build".to_string(),
            })?;
            self.current_line = self.lexer.line_number();

            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                build
                    .variables
                    .insert(key.trim().to_string(), value.trim().to_string());
            }
        }

        self.manifest.builds.push(build);
        Ok(())
    }

    fn parse_default(&mut self, line: &str) -> Result<(), ParseError> {
        let targets = self.split_paths(line);
        self.manifest.defaults.extend(targets);
        Ok(())
    }

    fn parse_pool(&mut self, name: &str) -> Result<(), ParseError> {
        let mut pool = Pool {
            name: name.to_string(),
            depth: 1,
        };

        // Parse indented variables
        while let Some(line) = self.lexer.peek_line() {
            if !line.starts_with(' ') && !line.starts_with('\t') {
                break;
            }
            let line = self.lexer.next_line().ok_or_else(|| ParseError::Syntax {
                line: self.current_line,
                message: "unexpected end of file while parsing pool".to_string(),
            })?;
            self.current_line = self.lexer.line_number();

            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "depth" {
                    pool.depth = value.trim().parse().unwrap_or(1);
                }
            }
        }

        self.manifest.pools.insert(name.to_string(), pool);
        Ok(())
    }

    fn parse_include(&mut self, path: &str) -> Result<(), ParseError> {
        let path = self.expand_path(path);
        let full_path = self.resolve_path(&path);

        // Check for circular includes
        if self.included_files.contains(&full_path) {
            return Err(ParseError::CircularInclude {
                path: full_path.display().to_string(),
            });
        }

        // Read and parse the included file
        let content = std::fs::read_to_string(&full_path).map_err(|e| ParseError::Syntax {
            line: self.current_line,
            message: format!("failed to include {}: {}", full_path.display(), e),
        })?;

        self.included_files.insert(full_path.clone());
        self.manifest.includes.push(path);

        // Parse included content into the same manifest (same scope).
        //
        // Ninja resolves include/subninja paths relative to the directory the
        // build was started in, not relative to the including file. Keep using
        // the top-level base directory so nested includes work (CMake's
        // `CMakeFiles/impl-Debug.ninja` includes `CMakeFiles/common.ninja`).
        let mut sub_parser = Parser {
            lexer: lexer::Lexer::new(&content),
            manifest: std::mem::take(&mut self.manifest),
            current_line: 1,
            base_dir: self.base_dir.clone(),
            included_files: self.included_files.clone(),
        };

        self.manifest = sub_parser.parse()?;
        self.included_files = sub_parser.included_files;

        Ok(())
    }

    fn parse_subninja(&mut self, path: &str) -> Result<(), ParseError> {
        let path = self.expand_path(path);
        let full_path = self.resolve_path(&path);

        // Check for circular includes
        if self.included_files.contains(&full_path) {
            return Err(ParseError::CircularInclude {
                path: full_path.display().to_string(),
            });
        }

        // Read and parse the subninja file
        let content = std::fs::read_to_string(&full_path).map_err(|e| ParseError::Syntax {
            line: self.current_line,
            message: format!("failed to include subninja {}: {}", full_path.display(), e),
        })?;

        self.included_files.insert(full_path.clone());
        self.manifest.subninjas.push(path);

        // Parse subninja content with separate scope (new variables, but shares rules/pools/builds)
        let mut sub_manifest = Manifest {
            rules: self.manifest.rules.clone(),
            pools: self.manifest.pools.clone(),
            ..Default::default()
        };

        let mut sub_parser = Parser {
            lexer: lexer::Lexer::new(&content),
            manifest: sub_manifest,
            current_line: 1,
            base_dir: self.base_dir.clone(),
            included_files: self.included_files.clone(),
        };

        sub_manifest = sub_parser.parse()?;
        self.included_files = sub_parser.included_files;

        // Merge results back: builds, defaults, new rules, new pools
        self.manifest.builds.extend(sub_manifest.builds);
        self.manifest.defaults.extend(sub_manifest.defaults);
        for (k, v) in sub_manifest.rules {
            self.manifest.rules.entry(k).or_insert(v);
        }
        for (k, v) in sub_manifest.pools {
            self.manifest.pools.entry(k).or_insert(v);
        }

        Ok(())
    }

    fn parse_variable(&mut self, line: &str) -> Result<(), ParseError> {
        if let Some((key, value)) = line.split_once('=') {
            self.manifest
                .variables
                .insert(key.trim().to_string(), value.trim().to_string());
        }
        Ok(())
    }

    /// Split a whitespace-separated path list and expand each path.
    ///
    /// Whitespace escaped with `$` (for example `foo$ bar`) is part of the
    /// path and must not split the token.
    fn split_paths(&self, s: &str) -> Vec<String> {
        let mut paths = Vec::new();
        let mut current = String::new();
        let mut chars = s.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '$' {
                current.push(c);
                match chars.next() {
                    Some('{') => {
                        current.push('{');
                        for c in chars.by_ref() {
                            current.push(c);
                            if c == '}' {
                                break;
                            }
                        }
                    }
                    Some(next) => current.push(next),
                    None => {}
                }
            } else if c.is_whitespace() {
                if !current.is_empty() {
                    paths.push(self.expand_path(&current));
                    current.clear();
                }
            } else {
                current.push(c);
            }
        }

        if !current.is_empty() {
            paths.push(self.expand_path(&current));
        }

        paths
    }

    /// Expand manifest-level variables in a path and process ninja escapes.
    ///
    /// Paths in build files may contain escaped characters: `$ ` is a space,
    /// `$:` is a colon, and `$$` is a literal dollar sign. CMake's Ninja
    /// Multi-Config generator emits paths such as `C$:/path/to/file` and
    /// target aliases such as `all$:Debug`.
    fn expand_path(&self, path: &str) -> String {
        let mut result = String::with_capacity(path.len());
        let mut chars = path.chars().peekable();

        while let Some(c) = chars.next() {
            if c != '$' {
                result.push(c);
                continue;
            }

            match chars.peek().copied() {
                Some('$') => {
                    chars.next();
                    result.push('$');
                }
                Some(':') => {
                    chars.next();
                    result.push(':');
                }
                Some(' ') => {
                    chars.next();
                    result.push(' ');
                }
                Some('{') => {
                    chars.next();
                    let mut name = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '}' {
                            closed = true;
                            break;
                        }
                        name.push(c);
                    }
                    if closed {
                        if let Some(value) = self.manifest.variables.get(&name) {
                            result.push_str(&unescape_path(value));
                        }
                    } else {
                        result.push_str("${");
                        result.push_str(&name);
                    }
                }
                Some(c) if is_variable_char(c) => {
                    let mut name = String::new();
                    while let Some(&c) = chars.peek() {
                        if is_variable_char(c) {
                            name.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Some(value) = self.manifest.variables.get(&name) {
                        result.push_str(&unescape_path(value));
                    }
                }
                Some(c) => {
                    chars.next();
                    result.push('$');
                    result.push(c);
                }
                None => result.push('$'),
            }
        }

        result
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(base) = &self.base_dir {
            base.join(p)
        } else {
            p.to_path_buf()
        }
    }
}

/// Split `s` at the first occurrence of `sep` that is not escaped with `$`.
fn split_at_unescaped(s: &str, sep: char) -> Option<(&str, &str)> {
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '$' {
            // Skip the escaped character (or an entire `${...}` reference).
            match chars.next() {
                Some((_, '{')) => {
                    for (_, c) in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                    }
                }
                Some(_) => {}
                None => break,
            }
        } else if c == sep {
            return Some((&s[..i], &s[i + c.len_utf8()..]));
        }
    }
    None
}

/// Whether `c` may appear in a ninja variable name.
fn is_variable_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Process ninja escape sequences in a path value (`$$`, `$ ` and `$:`).
fn unescape_path(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '$' {
            result.push(c);
            continue;
        }

        match chars.peek().copied() {
            Some('$') => {
                chars.next();
                result.push('$');
            }
            Some(' ') => {
                chars.next();
                result.push(' ');
            }
            Some(':') => {
                chars.next();
                result.push(':');
            }
            _ => result.push('$'),
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_rule() {
        let content = r#"
rule cc
    command = gcc -c $in -o $out
    description = Compiling $in
"#;
        let manifest = parse(content).unwrap();
        assert!(manifest.rules.contains_key("cc"));
        let rule = &manifest.rules["cc"];
        assert_eq!(rule.command, Some("gcc -c $in -o $out".to_string()));
    }

    #[test]
    fn test_parse_build() {
        let content = r#"
rule cc
    command = gcc -c $in -o $out

build foo.o: cc foo.c | header.h
"#;
        let manifest = parse(content).unwrap();
        assert_eq!(manifest.builds.len(), 1);
        let build = &manifest.builds[0];
        assert_eq!(build.outputs, vec!["foo.o"]);
        assert_eq!(build.rule, "cc");
        assert_eq!(build.inputs, vec!["foo.c"]);
        assert_eq!(build.implicit_deps, vec!["header.h"]);
    }

    #[test]
    fn test_parse_default() {
        let content = r#"
default foo bar
"#;
        let manifest = parse(content).unwrap();
        assert_eq!(manifest.defaults, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_pool() {
        let content = r#"
pool link_pool
    depth = 4
"#;
        let manifest = parse(content).unwrap();
        assert!(manifest.pools.contains_key("link_pool"));
        assert_eq!(manifest.pools["link_pool"].depth, 4);
    }

    #[test]
    fn test_parse_variables() {
        let content = r#"
builddir = out
cflags = -Wall

rule cc
    command = gcc $cflags -c $in -o $out

build $builddir/foo.o: cc foo.c
"#;
        let manifest = parse(content).unwrap();
        assert_eq!(manifest.variables.get("builddir"), Some(&"out".to_string()));
        assert_eq!(manifest.builds[0].outputs, vec!["out/foo.o"]);
    }

    #[test]
    fn test_parse_escaped_colons_in_outputs() {
        // CMake's Ninja Multi-Config generator uses `$:` in target names.
        let content = r#"
build all$:Debug: phony Debug/app.exe
build all: phony all$:Debug
default all
"#;
        let manifest = parse(content).unwrap();

        assert_eq!(manifest.builds[0].outputs, vec!["all:Debug"]);
        assert_eq!(manifest.builds[0].inputs, vec!["Debug/app.exe"]);
        assert_eq!(manifest.builds[1].outputs, vec!["all"]);
        assert_eq!(manifest.builds[1].inputs, vec!["all:Debug"]);
        assert_eq!(manifest.defaults, vec!["all"]);
    }

    #[test]
    fn test_parse_escaped_spaces_and_dollars_in_paths() {
        let content = r#"
build foo$ bar$$baz: phony C$:/src/a$ b.cpp
"#;
        let manifest = parse(content).unwrap();
        let build = &manifest.builds[0];

        assert_eq!(build.outputs, vec!["foo bar$baz"]);
        assert_eq!(build.inputs, vec!["C:/src/a b.cpp"]);
    }

    #[test]
    fn test_parse_implicit_outputs() {
        let content = r#"
build out | imp1 imp2: phony
"#;
        let manifest = parse(content).unwrap();
        let build = &manifest.builds[0];

        assert_eq!(build.outputs, vec!["out"]);
        assert_eq!(build.implicit_outputs, vec!["imp1", "imp2"]);
    }

    #[test]
    fn test_nested_include_is_relative_to_build_dir() {
        let dir = tempfile::tempdir().unwrap();
        let build_dir = dir.path();
        std::fs::create_dir_all(build_dir.join("CMakeFiles")).unwrap();

        std::fs::write(
            build_dir.join("build.ninja"),
            "include CMakeFiles/impl.ninja\n",
        )
        .unwrap();
        std::fs::write(
            build_dir.join("CMakeFiles/impl.ninja"),
            "include CMakeFiles/common.ninja\nbuild all: phony helper\n",
        )
        .unwrap();
        std::fs::write(
            build_dir.join("CMakeFiles/common.ninja"),
            "build helper: phony\n",
        )
        .unwrap();

        let manifest = parse_file(&build_dir.join("build.ninja")).unwrap();

        assert!(manifest
            .builds
            .iter()
            .any(|b| b.outputs == vec!["helper".to_string()]));
        assert!(manifest
            .builds
            .iter()
            .any(|b| b.outputs == vec!["all".to_string()]));
    }
}
