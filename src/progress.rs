//! Optional live progress display inspired by Zig's `std.Progress`.
//!
//! The tree is built once from the dependency graph and then updated as the
//! executor starts and finishes tasks. Rendering is kept in this module so the
//! scheduler only has to report lifecycle events.

use crate::graph::Graph;
use crate::output::Styles;
use console::{truncate_str, Term};
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Minimum time between automatic lifecycle redraws. Forcing a redraw (command
/// output or an error) is not throttled.
const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

/// How progress should be displayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProgressMode {
    /// One line per task, matching rninja's historical output.
    #[default]
    Plain,
    /// A live dependency tree drawn in place on the terminal.
    Tree,
}

/// Tree drawing characters. Windows consoles are not guaranteed to render
/// UTF-8 box drawing characters, so use an ASCII approximation there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Glyphs {
    Unicode,
    Ascii,
}

impl Glyphs {
    fn for_platform() -> Self {
        if cfg!(windows) {
            Self::Ascii
        } else {
            Self::Unicode
        }
    }

    fn tee(self) -> &'static str {
        match self {
            Self::Unicode => "├─ ",
            Self::Ascii => "|- ",
        }
    }

    fn line(self) -> &'static str {
        match self {
            Self::Unicode => "│  ",
            Self::Ascii => "|  ",
        }
    }

    fn langle(self) -> &'static str {
        match self {
            Self::Unicode => "└─ ",
            Self::Ascii => "`- ",
        }
    }

    fn ellipsis(self) -> &'static str {
        match self {
            Self::Unicode => "…",
            Self::Ascii => "...",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Branch,
    Pending,
    Running,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProgressLine {
    text: String,
    kind: LineKind,
}

#[derive(Debug, Clone)]
struct ProgressNode {
    name: String,
    parent: Option<usize>,
    children: Vec<usize>,
    is_task: bool,
    total: usize,
    completed: usize,
    active: bool,
    running: bool,
    failed: bool,
}

/// The dependency tree used by the live progress display.
#[derive(Debug)]
struct ProgressTree {
    nodes: Vec<ProgressNode>,
    root: usize,
    index_by_path: HashMap<String, usize>,
}

impl ProgressTree {
    /// Build a spanning tree of the scheduled work below `targets`.
    ///
    /// Only scheduled build edges and phony aliases are included. Shared
    /// dependencies are shown under the first branch that reaches them so the
    /// result is a tree rather than a duplicated DAG.
    fn from_graph(graph: &Graph, targets: &[&str], scheduled: &HashSet<String>) -> Option<Self> {
        let mut nodes = vec![ProgressNode {
            name: "build".to_string(),
            parent: None,
            children: Vec::new(),
            is_task: false,
            total: 0,
            completed: 0,
            active: true,
            running: false,
            failed: false,
        }];
        let mut index_by_path = HashMap::new();

        for target in targets {
            if let Some((index, created)) = add_node(
                target,
                Some(0),
                graph,
                scheduled,
                &mut nodes,
                &mut index_by_path,
            ) {
                if created {
                    nodes[0].children.push(index);
                }
            }
        }

        if nodes[0].children.is_empty() {
            return None;
        }

        compute_totals(&mut nodes, 0);

        for node in &mut nodes {
            node.active = node.total > 0;
        }
        nodes[0].active = true;

        // Drop phony aliases with no scheduled work below them.
        for index in 0..nodes.len() {
            let children = std::mem::take(&mut nodes[index].children);
            nodes[index].children = children
                .into_iter()
                .filter(|&child| nodes[child].total > 0)
                .collect();
        }

        Some(Self {
            nodes,
            root: 0,
            index_by_path,
        })
    }

    fn task_started(&mut self, path: &str) {
        if let Some(index) = self.find(path) {
            if self.nodes[index].active {
                self.nodes[index].running = true;
            }
        }
    }

    fn task_finished(&mut self, path: &str, success: bool) {
        let Some(index) = self.find(path) else {
            return;
        };

        if !self.nodes[index].is_task {
            return;
        }

        if success {
            self.complete(index);
        } else {
            self.nodes[index].running = false;
            self.nodes[index].failed = true;
        }
    }

    fn cache_hit(&mut self, path: &str) {
        if let Some(index) = self.find(path) {
            self.complete(index);
        }
    }

    fn complete(&mut self, index: usize) {
        if !self.nodes[index].active {
            return;
        }

        self.nodes[index].running = false;
        self.nodes[index].active = false;
        self.nodes[index].completed = self.nodes[index].total;

        // A completed task counts toward every branch above it. Phony aliases
        // disappear once all of their scheduled descendants are done.
        let mut parent = self.nodes[index].parent;
        while let Some(parent_index) = parent {
            self.nodes[parent_index].completed += 1;
            if parent_index != self.root
                && !self.nodes[parent_index].is_task
                && self.nodes[parent_index].completed >= self.nodes[parent_index].total
            {
                self.nodes[parent_index].active = false;
            }
            parent = self.nodes[parent_index].parent;
        }
    }

    fn find(&self, path: &str) -> Option<usize> {
        self.index_by_path.get(path).copied()
    }

    fn render_lines(&self, width: usize, max_lines: usize, glyphs: Glyphs) -> Vec<ProgressLine> {
        let visible = self.visible_count(self.root);
        let truncated = visible > max_lines;
        let limit = if truncated {
            max_lines.saturating_sub(1).max(1)
        } else {
            max_lines
        };

        let mut lines = Vec::with_capacity(visible.min(max_lines));
        if limit > 0 {
            self.render_node(self.root, "", None, limit, glyphs, &mut lines);
        }

        if truncated && lines.len() < max_lines {
            lines.push(ProgressLine {
                text: format!("   {} more", glyphs.ellipsis()),
                kind: LineKind::Branch,
            });
        }

        for line in &mut lines {
            if width > 0 {
                line.text = truncate_str(&line.text, width, glyphs.ellipsis()).into_owned();
            }
        }

        lines
    }

    fn visible_count(&self, index: usize) -> usize {
        if !self.nodes[index].active {
            return 0;
        }

        let mut count = 1;
        for child in &self.nodes[index].children {
            count += self.visible_count(*child);
        }
        count
    }

    fn render_node(
        &self,
        index: usize,
        prefix: &str,
        connector: Option<&str>,
        limit: usize,
        glyphs: Glyphs,
        lines: &mut Vec<ProgressLine>,
    ) {
        if lines.len() >= limit {
            return;
        }

        let node = &self.nodes[index];
        if !node.active {
            return;
        }

        let mut text = String::with_capacity(prefix.len() + 64);
        text.push_str(prefix);
        if let Some(connector) = connector {
            text.push_str(connector);
        }
        text.push_str(&format!(
            "[{}/{}] {}",
            node.completed, node.total, node.name
        ));

        let kind = if node.failed {
            LineKind::Failed
        } else if node.running {
            LineKind::Running
        } else if node.is_task {
            LineKind::Pending
        } else {
            LineKind::Branch
        };
        lines.push(ProgressLine { text, kind });

        let children: Vec<usize> = node
            .children
            .iter()
            .copied()
            .filter(|child| self.nodes[*child].active)
            .collect();

        let child_prefix = if index == self.root {
            String::new()
        } else if connector == Some(glyphs.langle()) {
            format!("{}{}", prefix, "   ")
        } else {
            format!("{}{}", prefix, glyphs.line())
        };

        for (position, child) in children.iter().enumerate() {
            if lines.len() >= limit {
                return;
            }
            let connector = if position + 1 == children.len() {
                glyphs.langle()
            } else {
                glyphs.tee()
            };
            self.render_node(*child, &child_prefix, Some(connector), limit, glyphs, lines);
        }
    }
}

fn add_node(
    path: &str,
    parent: Option<usize>,
    graph: &Graph,
    scheduled: &HashSet<String>,
    nodes: &mut Vec<ProgressNode>,
    index_by_path: &mut HashMap<String, usize>,
) -> Option<(usize, bool)> {
    if let Some(&index) = index_by_path.get(path) {
        return Some((index, false));
    }

    let graph_node = graph.get_node(path)?;
    if graph_node.is_source {
        return None;
    }

    let is_task = scheduled.contains(path) && !graph_node.is_phony;
    if !is_task && !graph_node.is_phony {
        return None;
    }

    let name = graph_node
        .description
        .as_deref()
        .or(graph_node.command.as_deref())
        .unwrap_or(path)
        .to_string();

    let index = nodes.len();
    nodes.push(ProgressNode {
        name,
        parent,
        children: Vec::new(),
        is_task,
        total: 0,
        completed: 0,
        active: true,
        running: false,
        failed: false,
    });
    index_by_path.insert(path.to_string(), index);

    let deps = graph_node.deps.clone();
    for dep in deps {
        if let Some((child, created)) =
            add_node(&dep, Some(index), graph, scheduled, nodes, index_by_path)
        {
            if created {
                nodes[index].children.push(child);
            }
        }
    }

    Some((index, true))
}

fn compute_totals(nodes: &mut [ProgressNode], index: usize) -> usize {
    let mut total = if nodes[index].is_task { 1 } else { 0 };
    let children = nodes[index].children.clone();
    for child in children {
        total += compute_totals(nodes, child);
    }
    nodes[index].total = total;
    total
}

/// Write a captured stream, ensuring it ends on a fresh line so the tree can
/// be redrawn below it. A missing trailing newline is common for tools that
/// write progress indicators or partial diagnostics.
fn write_stream(mut stream: impl Write, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    let _ = stream.write_all(bytes);
    if !bytes.ends_with(b"\n") {
        let _ = stream.write_all(b"\n");
    }
    let _ = stream.flush();
}

struct RenderState {
    tree: ProgressTree,
    last_lines: usize,
    last_render: Option<Instant>,
    finished: bool,
}

/// Thread-safe live tree renderer.
pub struct TreeProgress {
    inner: Mutex<RenderState>,
    term: Term,
    styles: Styles,
    glyphs: Glyphs,
}

impl TreeProgress {
    /// Create a tree renderer for the current build, if stderr is an
    /// interactive terminal and there is scheduled work to display.
    pub fn new(graph: &Graph, targets: &[&str], scheduled: &HashSet<String>) -> Option<Arc<Self>> {
        if !io::stderr().is_terminal() {
            return None;
        }

        let tree = ProgressTree::from_graph(graph, targets, scheduled)?;

        Some(Arc::new(Self {
            inner: Mutex::new(RenderState {
                tree,
                last_lines: 0,
                last_render: None,
                finished: false,
            }),
            term: Term::stderr(),
            styles: Styles::default(),
            glyphs: Glyphs::for_platform(),
        }))
    }

    /// Draw the initial tree before the first task starts.
    pub fn start(&self) {
        let mut state = self.inner.lock().unwrap();
        self.render(&mut state);
    }

    pub fn task_started(&self, path: &str) {
        let mut state = self.inner.lock().unwrap();
        state.tree.task_started(path);
        self.render_throttled(&mut state);
    }

    pub fn task_finished(&self, path: &str, success: bool) {
        let mut state = self.inner.lock().unwrap();
        state.tree.task_finished(path, success);
        self.render_throttled(&mut state);
    }

    pub fn cache_hit(&self, path: &str) {
        let mut state = self.inner.lock().unwrap();
        state.tree.cache_hit(path);
        self.render_throttled(&mut state);
    }

    /// Write command output above the tree without corrupting it.
    ///
    /// Each stream is terminated with a newline if it does not already end in
    /// one, so the redrawn tree always starts on a fresh line.
    pub fn write_output(&self, stdout: &[u8], stderr: &[u8]) {
        if stdout.is_empty() && stderr.is_empty() {
            return;
        }

        let mut state = self.inner.lock().unwrap();
        self.clear(&mut state);

        write_stream(io::stdout().lock(), stdout);
        write_stream(io::stderr().lock(), stderr);

        self.render(&mut state);
    }

    /// Print a permanent status line (such as a verbose command line) above
    /// the tree, then redraw the tree below it.
    pub fn write_status_line(&self, line: &str) {
        let mut state = self.inner.lock().unwrap();
        self.clear(&mut state);

        {
            let mut err = io::stderr().lock();
            let _ = err.write_all(line.as_bytes());
            if !line.ends_with('\n') {
                let _ = err.write_all(b"\n");
            }
            let _ = err.flush();
        }

        self.render(&mut state);
    }

    /// Print a permanent message (such as a task failure) above the tree.
    pub fn permanent_error(&self, message: &str) {
        let mut state = self.inner.lock().unwrap();
        self.clear(&mut state);

        let mut err = io::stderr().lock();
        let _ = err.write_all(message.as_bytes());
        let _ = err.flush();

        self.render(&mut state);
    }

    /// Remove the tree and leave the terminal at the start of the cleared area.
    pub fn finish(&self) {
        let mut state = self.inner.lock().unwrap();
        state.finished = true;
        self.clear(&mut state);
    }

    /// Redraw at most once per [`REFRESH_INTERVAL`] so large builds do not
    /// spend all of their time repainting the tree.
    fn render_throttled(&self, state: &mut RenderState) {
        if let Some(last) = state.last_render {
            if last.elapsed() < REFRESH_INTERVAL {
                return;
            }
        }
        self.render(state);
    }

    fn render(&self, state: &mut RenderState) {
        if state.finished {
            return;
        }
        state.last_render = Some(Instant::now());

        let (rows, cols) = self.term.size();
        let rows = if rows == 0 { 24 } else { rows as usize };
        let width = if cols == 0 { 80 } else { cols as usize };
        let max_lines = rows.saturating_sub(2).max(1);
        let lines = state.tree.render_lines(width, max_lines, self.glyphs);

        if lines.is_empty() {
            self.clear(state);
            return;
        }

        self.clear(state);
        for line in &lines {
            let text = match line.kind {
                LineKind::Running => self.styles.progress.apply_to(&line.text).to_string(),
                LineKind::Failed => self.styles.error.apply_to(&line.text).to_string(),
                _ => line.text.clone(),
            };
            let _ = self.term.write_line(&text);
        }
        state.last_lines = lines.len();
        let _ = self.term.flush();
    }

    fn clear(&self, state: &mut RenderState) {
        if state.last_lines == 0 {
            return;
        }

        let _ = self.term.clear_last_lines(state.last_lines);
        state.last_lines = 0;
        let _ = self.term.flush();
    }
}

impl Drop for TreeProgress {
    fn drop(&mut self) {
        if let Ok(mut state) = self.inner.lock() {
            self.clear(&mut state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn tree_for(manifest: &str, targets: &[&str], scheduled: &[&str]) -> ProgressTree {
        let manifest = parser::parse(manifest).unwrap();
        let graph = Graph::from_manifest(&manifest).unwrap();
        let scheduled: HashSet<String> = scheduled.iter().map(|s| s.to_string()).collect();
        ProgressTree::from_graph(&graph, targets, &scheduled).unwrap()
    }

    fn lines(tree: &ProgressTree) -> Vec<String> {
        tree.render_lines(100, 100, Glyphs::Ascii)
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    #[test]
    fn renders_dependency_tree_with_branch_counts() {
        let mut tree = tree_for(
            r#"
rule cc
    command = cc $in -o $out
    description = CC $out
rule link
    command = link $in -o $out
    description = LINK $out

build obj1: cc src1.c
build obj2: cc src2.c
build app: link obj1 obj2
build all: phony app
"#,
            &["all"],
            &["obj1", "obj2", "app"],
        );

        assert_eq!(
            lines(&tree),
            vec![
                "[0/3] build",
                "`- [0/3] all",
                "   `- [0/3] LINK app",
                "      |- [0/1] CC obj1",
                "      `- [0/1] CC obj2",
            ]
        );

        tree.cache_hit("obj1");
        assert_eq!(
            lines(&tree),
            vec![
                "[1/3] build",
                "`- [1/3] all",
                "   `- [1/3] LINK app",
                "      `- [0/1] CC obj2",
            ]
        );

        tree.task_started("obj2");
        tree.task_finished("obj2", true);
        assert_eq!(
            lines(&tree),
            vec!["[2/3] build", "`- [2/3] all", "   `- [2/3] LINK app",]
        );

        tree.task_started("app");
        tree.task_finished("app", true);
        assert_eq!(lines(&tree), vec!["[3/3] build"]);
    }

    #[test]
    fn shared_dependencies_are_shown_only_once() {
        let mut tree = tree_for(
            r#"
rule cc
    command = cc $in -o $out
    description = CC $out

build shared: cc shared.c
build a: cc shared
build b: cc shared
build all: phony a b
"#,
            &["all"],
            &["shared", "a", "b"],
        );

        assert_eq!(
            lines(&tree),
            vec![
                "[0/3] build",
                "`- [0/3] all",
                "   |- [0/2] CC a",
                "   |  `- [0/1] CC shared",
                "   `- [0/1] CC b",
            ]
        );

        tree.cache_hit("shared");
        assert_eq!(
            lines(&tree),
            vec![
                "[1/3] build",
                "`- [1/3] all",
                "   |- [1/2] CC a",
                "   `- [0/1] CC b",
            ]
        );
    }

    #[test]
    fn truncates_tall_trees() {
        let tree = tree_for(
            r#"
rule cc
    command = cc $in -o $out
    description = CC $out

build a: cc a.c
build b: cc b.c
build c: cc c.c
build all: phony a b c
"#,
            &["all"],
            &["a", "b", "c"],
        );

        let lines = tree.render_lines(100, 3, Glyphs::Ascii);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "[0/3] build");
        assert_eq!(lines[1].text, "`- [0/3] all");
        assert_eq!(lines[2].text, "   ... more");
    }

    #[test]
    fn writes_unterminated_output_on_its_own_line() {
        let mut buf = Vec::new();
        write_stream(&mut buf, b"partial line");
        assert_eq!(buf, b"partial line\n");

        let mut buf = Vec::new();
        write_stream(&mut buf, b"already terminated\n");
        assert_eq!(buf, b"already terminated\n");

        let mut buf = Vec::new();
        write_stream(&mut buf, b"");
        assert!(buf.is_empty());
    }
}
