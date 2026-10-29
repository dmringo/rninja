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
use std::thread;
use std::time::{Duration, Instant};

/// Minimum time between automatic lifecycle redraws. Forcing a redraw (command
/// output or an error) is not throttled.
const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

/// How often running-task timers are repainted. Lifecycle events still redraw
/// immediately (subject to [`REFRESH_INTERVAL`]); this is only needed so elapsed
/// times keep advancing while a long task runs.
const TIMER_INTERVAL: Duration = Duration::from_millis(250);

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
    started_at: Option<Instant>,
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
            started_at: None,
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
                self.nodes[index].started_at = Some(Instant::now());
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
            self.nodes[index].started_at = None;
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
        self.nodes[index].started_at = None;
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
        self.render_lines_at(width, max_lines, glyphs, Instant::now())
    }

    fn render_lines_at(
        &self,
        width: usize,
        max_lines: usize,
        glyphs: Glyphs,
        now: Instant,
    ) -> Vec<ProgressLine> {
        let visible = self.visibility();
        let total_visible = self.visible_count(&visible, self.root);
        let truncated = total_visible > max_lines;
        let limit = if truncated {
            max_lines.saturating_sub(1).max(1)
        } else {
            max_lines
        };

        let mut lines = Vec::with_capacity(total_visible.min(max_lines));
        if limit > 0 {
            self.render_node(
                self.root, "", None, limit, glyphs, now, &visible, &mut lines,
            );
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

    /// Compute which nodes should appear in the live tree. Queued work is
    /// culled; only running or failed tasks and their ancestors remain visible.
    fn visibility(&self) -> Vec<bool> {
        let mut visible = vec![false; self.nodes.len()];
        self.mark_visible(self.root, &mut visible);
        visible[self.root] = true;
        visible
    }

    fn mark_visible(&self, index: usize, visible: &mut [bool]) -> bool {
        let node = &self.nodes[index];
        if !node.active {
            return false;
        }

        let mut live = node.running || node.failed;
        for &child in &node.children {
            live |= self.mark_visible(child, visible);
        }
        visible[index] = live;
        live
    }

    fn has_running(&self) -> bool {
        self.nodes.iter().any(|node| node.active && node.running)
    }

    fn visible_count(&self, visible: &[bool], index: usize) -> usize {
        if !visible[index] {
            return 0;
        }

        let mut count = 1;
        for child in &self.nodes[index].children {
            count += self.visible_count(visible, *child);
        }
        count
    }

    #[allow(clippy::too_many_arguments)]
    fn render_node(
        &self,
        index: usize,
        prefix: &str,
        connector: Option<&str>,
        limit: usize,
        glyphs: Glyphs,
        now: Instant,
        visible: &[bool],
        lines: &mut Vec<ProgressLine>,
    ) {
        if lines.len() >= limit {
            return;
        }

        let node = &self.nodes[index];
        if !visible[index] {
            return;
        }

        let mut text = String::with_capacity(prefix.len() + 64);
        text.push_str(prefix);
        if let Some(connector) = connector {
            text.push_str(connector);
        }
        text.push_str(&format!(
            "[{}/{}] {}",
            node.completed,
            node.total,
            node.name.trim_end()
        ));
        if let Some(started_at) = node.started_at {
            let elapsed = now.saturating_duration_since(started_at);
            // Avoid adding a noisy "(0ms)" to every task as it starts; the
            // timer becomes useful once a task has been running for a while.
            if elapsed >= Duration::from_secs(1) {
                text.push_str(&format!(" ({})", format_elapsed(elapsed)));
            }
        }

        let kind = if node.failed {
            LineKind::Failed
        } else if node.running {
            LineKind::Running
        } else {
            LineKind::Branch
        };
        lines.push(ProgressLine { text, kind });

        let children: Vec<usize> = node
            .children
            .iter()
            .copied()
            .filter(|child| visible[*child])
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
            self.render_node(
                *child,
                &child_prefix,
                Some(connector),
                limit,
                glyphs,
                now,
                visible,
                lines,
            );
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
        started_at: None,
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

/// Format a running task's elapsed time compactly.
fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 1 {
        format!("{}ms", elapsed.as_millis())
    } else if secs < 60 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
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

    /// Draw the initial tree before the first task starts, then keep elapsed
    /// times advancing while tasks are running.
    pub fn start(self: &Arc<Self>) {
        {
            let mut state = self.inner.lock().unwrap();
            self.render(&mut state);
        }

        // The ticker holds only a weak reference, so it cannot keep the
        // renderer alive after the build finishes or is dropped.
        let weak = Arc::downgrade(self);
        thread::spawn(move || loop {
            thread::sleep(TIMER_INTERVAL);
            match weak.upgrade() {
                Some(tree) => {
                    if tree.tick() {
                        break;
                    }
                }
                None => break,
            }
        });
    }

    /// Repaint running timers. Returns true once the tree has finished.
    fn tick(&self) -> bool {
        let mut state = self.inner.lock().unwrap();
        if state.finished {
            return true;
        }
        if state.tree.has_running() {
            self.render(&mut state);
        }
        false
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
    fn renders_only_live_work_and_branch_counts() {
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

        // Nothing is executing yet, so all queued work is culled.
        assert_eq!(lines(&tree), vec!["[0/3] build"]);

        tree.cache_hit("obj1");
        assert_eq!(lines(&tree), vec!["[1/3] build"]);

        tree.task_started("obj2");
        assert_eq!(
            lines(&tree),
            vec![
                "[1/3] build",
                "`- [1/3] all",
                "   `- [1/3] LINK app",
                "      `- [0/1] CC obj2",
            ]
        );

        tree.task_finished("obj2", true);
        assert_eq!(lines(&tree), vec!["[2/3] build"]);

        tree.task_started("app");
        assert_eq!(
            lines(&tree),
            vec!["[2/3] build", "`- [2/3] all", "   `- [2/3] LINK app",]
        );

        tree.task_finished("app", true);
        assert_eq!(lines(&tree), vec!["[3/3] build"]);
    }

    #[test]
    fn culls_pending_siblings() {
        let mut tree = tree_for(
            r#"
rule cc
    command = cc $in -o $out
    description = CC $out

build a: cc a.c
build b: cc b.c
build all: phony a b
"#,
            &["all"],
            &["a", "b"],
        );

        tree.task_started("a");
        assert_eq!(
            lines(&tree),
            vec!["[0/2] build", "`- [0/2] all", "   `- [0/1] CC a",]
        );
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

        tree.task_started("shared");
        assert_eq!(
            lines(&tree),
            vec![
                "[0/3] build",
                "`- [0/3] all",
                "   `- [0/2] CC a",
                "      `- [0/1] CC shared",
            ]
        );

        tree.task_finished("shared", true);
        tree.task_started("a");
        tree.task_started("b");
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
    fn truncates_tall_running_trees() {
        let mut tree = tree_for(
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

        tree.task_started("a");
        tree.task_started("b");
        tree.task_started("c");

        let lines = tree.render_lines(100, 3, Glyphs::Ascii);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "[0/3] build");
        assert_eq!(lines[1].text, "`- [0/3] all");
        assert_eq!(lines[2].text, "   ... more");
    }

    #[test]
    fn shows_elapsed_time_for_running_tasks() {
        let mut tree = tree_for(
            r#"
rule cc
    command = cc $in -o $out
    description = CC $out

build a: cc a.c
build all: phony a
"#,
            &["all"],
            &["a"],
        );

        let base = Instant::now();
        tree.task_started("a");
        let index = tree.find("a").unwrap();
        tree.nodes[index].started_at = Some(base);

        let rendered =
            tree.render_lines_at(100, 100, Glyphs::Ascii, base + Duration::from_millis(1500));
        assert_eq!(rendered.last().unwrap().text, "   `- [0/1] CC a (1.5s)");
    }

    #[test]
    fn formats_elapsed_times_compactly() {
        assert_eq!(format_elapsed(Duration::from_millis(0)), "0ms");
        assert_eq!(format_elapsed(Duration::from_millis(250)), "250ms");
        assert_eq!(format_elapsed(Duration::from_millis(999)), "999ms");
        assert_eq!(format_elapsed(Duration::from_millis(1000)), "1.0s");
        assert_eq!(format_elapsed(Duration::from_millis(12_340)), "12.3s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m05s");
        assert_eq!(format_elapsed(Duration::from_secs(3723)), "1h02m");
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
