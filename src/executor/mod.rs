pub(crate) mod depfile;
mod runner;

use crate::buildlog::{get_ninja_mtime, BuildLog, MtimeCache};
use crate::cache::{Cache, CacheConfig};
use crate::depslog::DepsLog;
use crate::error::ExecError;
use crate::graph::{Graph, Node};
use crate::output::{JsonEvent, OutputMode};
use crate::progress::{ProgressMode, TreeProgress};
use crate::trace::BuildTrace;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{debug, info};

/// Collect environment variables that commonly affect build outputs.
///
/// This whitelist covers the most common compiler and linker environment
/// variables. A full implementation may need to be configurable per-project.
fn collect_relevant_env_vars() -> Vec<(&'static str, String)> {
    const KEYS: [&str; 12] = [
        "CC",
        "CXX",
        "LD",
        "AR",
        "RANLIB",
        "STRIP",
        "CFLAGS",
        "CXXFLAGS",
        "LDFLAGS",
        "CPPFLAGS",
        "RUSTFLAGS",
        "PATH",
    ];
    KEYS.iter()
        .filter_map(|&k| std::env::var(k).ok().map(|v| (k, v)))
        .collect()
}

/// Executor configuration
#[derive(Debug, Clone)]
pub struct Config {
    /// Number of parallel jobs
    pub parallelism: usize,
    /// Dry run mode (don't execute commands)
    pub dry_run: bool,
    /// Verbose mode (print all commands)
    pub verbose: bool,
    /// Maximum number of failed jobs before stopping (0 = unlimited)
    pub keep_going: usize,
    /// Kill already-running jobs once the failure limit is reached
    pub kill_on_failure: bool,
    /// Explain why targets are rebuilt
    pub explain: bool,
    /// Show stats at end
    pub stats: bool,
    /// Cache configuration
    pub cache_config: CacheConfig,
    /// Output mode (human or JSON)
    pub output_mode: OutputMode,
    /// Human-readable progress style
    pub progress: ProgressMode,
    /// Trace output file (None = disabled)
    pub trace_file: Option<PathBuf>,
    /// Tokio runtime handle (None means create one per run)
    pub runtime: Option<tokio::runtime::Handle>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            parallelism: num_cpus::get(),
            dry_run: false,
            verbose: false,
            keep_going: 1,
            kill_on_failure: false,
            explain: false,
            stats: false,
            cache_config: CacheConfig::from_env(),
            output_mode: OutputMode::Human,
            progress: ProgressMode::Plain,
            trace_file: None,
            runtime: None,
        }
    }
}

/// Result returned by a spawned build task.
type TaskResult = Result<(String, bool, Option<String>), (String, ExecError)>;

/// Handle for a spawned build task.
type TaskHandle = tokio::task::JoinHandle<TaskResult>;

/// Build statistics
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub started: usize,
    pub finished: usize,
    pub failed: usize,
    pub skipped: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub total_time: Duration,
}

impl Stats {
    pub fn print(&self, output_mode: OutputMode) {
        match output_mode {
            OutputMode::Human => {
                eprintln!();
                eprintln!("build statistics:");
                eprintln!("    started: {} edges", self.started);
                eprintln!("   finished: {} edges", self.finished);
                eprintln!("     failed: {} edges", self.failed);
                eprintln!("    skipped: {} edges", self.skipped);
                if self.cache_hits > 0 || self.cache_misses > 0 {
                    let total = self.cache_hits + self.cache_misses;
                    let hit_rate = if total > 0 {
                        100.0 * self.cache_hits as f64 / total as f64
                    } else {
                        0.0
                    };
                    eprintln!("  cache hit: {} ({:.1}%)", self.cache_hits, hit_rate);
                    eprintln!(" cache miss: {}", self.cache_misses);
                }
                eprintln!("       time: {:.3}s", self.total_time.as_secs_f64());
            }
            OutputMode::Json => {
                // JSON stats are included in BuildFinished event
            }
        }
    }
}

/// Tracks the current state of a build node
#[derive(Debug, Clone, PartialEq, Eq, Copy)]
enum NodeState {
    /// Waiting for dependencies
    Pending,
    /// Dependencies satisfied, waiting for semaphore
    Ready,
    /// Actively executing
    Running,
    /// Successfully completed
    Completed,
    /// Command failed
    Failed,
    /// Skipped because a dependency failed or build was cancelled
    Cancelled,
}

/// Error returned when an invalid state transition is attempted
#[derive(Debug)]
struct InvalidTransition {
    from: NodeState,
    to: NodeState,
}

impl std::fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid node state transition: {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl NodeState {
    /// Attempt to transition from `self` to `new`, returning `Err` if invalid.
    fn transition(self, new: NodeState) -> Result<NodeState, InvalidTransition> {
        match (self, new) {
            // From Pending
            (NodeState::Pending, NodeState::Ready)
            | (NodeState::Pending, NodeState::Running)
            | (NodeState::Pending, NodeState::Completed)
            | (NodeState::Pending, NodeState::Cancelled) => Ok(new),
            // From Ready
            (NodeState::Ready, NodeState::Running)
            | (NodeState::Ready, NodeState::Failed)
            | (NodeState::Ready, NodeState::Cancelled) => Ok(new),
            // From Running
            (NodeState::Running, NodeState::Completed)
            | (NodeState::Running, NodeState::Failed)
            | (NodeState::Running, NodeState::Cancelled) => Ok(new),
            // Same state is idempotent
            (old, new) if old == new => Ok(new),
            // All other combinations are invalid
            (from, to) => Err(InvalidTransition { from, to }),
        }
    }
}

/// Shared state for parallel execution
struct BuildState {
    /// Node states (completed, failed, etc.) - single source of truth
    node_states: Mutex<HashMap<String, NodeState>>,
    /// Number of failures
    fail_count: AtomicUsize,
    /// Number of jobs that were running when the build was aborted
    cancelled_running: AtomicUsize,
    /// Whether the build is being aborted and running commands should not be
    /// reported as failures
    kill_requested: AtomicBool,
    /// PIDs of currently running child processes, keyed by target path
    processes: Mutex<HashMap<String, u32>>,
    /// Counter for progress display
    progress: AtomicUsize,
    /// Total number of nodes to build
    total: usize,
    /// Pool semaphores for limiting parallelism
    pools: HashMap<String, Arc<Semaphore>>,
    /// Default semaphore for job limiting
    job_semaphore: Arc<Semaphore>,
    /// Console pool (depth=1 by default)
    console_semaphore: Arc<Semaphore>,
    /// Cache hits counter
    cache_hits: AtomicUsize,
    /// Cache misses counter
    cache_misses: AtomicUsize,
    /// Output mode for progress reporting
    output_mode: OutputMode,
    /// Optional live dependency-tree progress renderer
    tree_progress: Option<Arc<TreeProgress>>,
    /// Build trace for chrome://tracing output
    trace: Arc<BuildTrace>,
    /// Ninja-compatible build log
    build_log: Mutex<BuildLog>,
    /// Ninja-compatible dependency log
    deps_log: Mutex<DepsLog>,
    /// Start of this build, used for Ninja-compatible log timestamps
    start: Instant,
}

impl BuildState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        parallelism: usize,
        total: usize,
        pools: &HashMap<String, usize>,
        output_mode: OutputMode,
        tree_progress: Option<Arc<TreeProgress>>,
        trace: Arc<BuildTrace>,
        build_log: BuildLog,
        deps_log: DepsLog,
    ) -> Self {
        let mut pool_sems = HashMap::new();
        for (name, depth) in pools {
            pool_sems.insert(name.clone(), Arc::new(Semaphore::new(*depth)));
        }

        Self {
            node_states: Mutex::new(HashMap::new()),
            fail_count: AtomicUsize::new(0),
            cancelled_running: AtomicUsize::new(0),
            kill_requested: AtomicBool::new(false),
            processes: Mutex::new(HashMap::new()),
            progress: AtomicUsize::new(0),
            total,
            pools: pool_sems,
            job_semaphore: Arc::new(Semaphore::new(parallelism)),
            console_semaphore: Arc::new(Semaphore::new(1)),
            cache_hits: AtomicUsize::new(0),
            cache_misses: AtomicUsize::new(0),
            output_mode,
            tree_progress,
            trace,
            build_log: Mutex::new(build_log),
            deps_log: Mutex::new(deps_log),
            start: Instant::now(),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn apply_transition(&self, path: &str, new: NodeState) {
        let mut states = self.node_states.lock();
        let current = states.get(path).copied().unwrap_or(NodeState::Pending);
        match current.transition(new) {
            Ok(NodeState::Failed) if current != NodeState::Failed => {
                states.insert(path.to_string(), NodeState::Failed);
                self.fail_count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(final_state) => {
                states.insert(path.to_string(), final_state);
            }
            Err(e) => {
                tracing::warn!("State transition error for {}: {}", path, e);
            }
        }
    }

    fn mark_ready(&self, path: &str) {
        self.apply_transition(path, NodeState::Ready);
    }

    fn mark_running(&self, path: &str) {
        self.apply_transition(path, NodeState::Running);
    }

    fn mark_completed(&self, path: &str) {
        self.apply_transition(path, NodeState::Completed);
    }

    fn mark_failed(&self, path: &str) {
        self.apply_transition(path, NodeState::Failed);
    }

    fn mark_cancelled(&self, path: &str) {
        self.apply_transition(path, NodeState::Cancelled);
    }

    /// Whether the configured failure limit has been reached. `0` means no
    /// limit. Checked both when scheduling new work and by already-spawned
    /// tasks before they start executing.
    fn should_stop(&self, keep_going: usize) -> bool {
        keep_going > 0 && self.fail_count.load(Ordering::SeqCst) >= keep_going
    }

    fn request_kill(&self) {
        self.kill_requested.store(true, Ordering::SeqCst);
    }

    fn kill_requested(&self) -> bool {
        self.kill_requested.load(Ordering::SeqCst)
    }

    fn record_build(&self, path: &str, command_hash: u64, start_ms: u64, end_ms: u64) {
        self.build_log
            .lock()
            .record(path, command_hash, start_ms, end_ms);
    }

    fn record_deps(&self, path: &str, deps: &[String]) {
        let Some(mtime) = get_ninja_mtime(Path::new(path)) else {
            return;
        };
        if let Err(e) = self.deps_log.lock().record(path, mtime, deps) {
            debug!("Failed to record dependencies for {}: {}", path, e);
        }
    }

    /// Re-record existing dependency data for a restored output so its mtime
    /// matches the newly written file.
    fn refresh_deps_mtime(&self, path: &str) {
        let deps = self
            .deps_log
            .lock()
            .get(path)
            .map(|entry| entry.deps.clone());
        if let Some(deps) = deps {
            self.record_deps(path, &deps);
        }
    }

    fn register_process(&self, path: &str, pid: u32) {
        self.processes.lock().insert(path.to_string(), pid);
    }

    fn unregister_process(&self, path: &str) {
        self.processes.lock().remove(path);
    }

    /// Terminate every registered child process tree. Called after the task
    /// handles have been aborted, so killed jobs are not reported as command
    /// failures.
    fn kill_running_processes(&self) {
        let pids: Vec<u32> = std::mem::take(&mut *self.processes.lock())
            .into_values()
            .collect();

        for pid in pids {
            kill_process_tree(pid);
        }
    }

    /// Mark every unfinished node as cancelled. Used when aborting the build
    /// and killing already-running jobs; completed and failed nodes are left
    /// alone.
    fn cancel_unfinished<'a>(&self, paths: impl Iterator<Item = &'a str>) {
        let mut states = self.node_states.lock();
        let mut cancelled_running = 0;

        for path in paths {
            match states.get(path).copied().unwrap_or(NodeState::Pending) {
                NodeState::Pending | NodeState::Ready => {
                    states.insert(path.to_string(), NodeState::Cancelled);
                }
                NodeState::Running => {
                    states.insert(path.to_string(), NodeState::Cancelled);
                    cancelled_running += 1;
                }
                NodeState::Completed | NodeState::Failed | NodeState::Cancelled => {}
            }
        }

        drop(states);
        if cancelled_running > 0 {
            self.cancelled_running
                .fetch_add(cancelled_running, Ordering::SeqCst);
        }
    }

    fn report_task_started(&self, path: &str) {
        if let Some(tree) = &self.tree_progress {
            tree.task_started(path);
        }
    }

    fn report_task_finished(&self, path: &str, success: bool) {
        if let Some(tree) = &self.tree_progress {
            tree.task_finished(path, success);
        }
    }

    fn write_output(&self, stdout: &[u8], stderr: &[u8]) {
        if let Some(tree) = &self.tree_progress {
            tree.write_output(stdout, stderr);
            return;
        }

        if !stdout.is_empty() {
            if let Err(e) = std::io::stdout().write_all(stdout) {
                debug!("Failed to write to stdout: {}", e);
            }
        }
        if !stderr.is_empty() {
            if let Err(e) = std::io::stderr().write_all(stderr) {
                debug!("Failed to write to stderr: {}", e);
            }
        }
    }

    fn report_permanent_error(&self, message: &str) {
        if let Some(tree) = &self.tree_progress {
            tree.permanent_error(message);
        } else {
            eprint!("{}", message);
        }
    }

    fn has_failed_dep(&self, deps: &[String]) -> bool {
        let states = self.node_states.lock();
        deps.iter().any(|d| {
            matches!(
                states.get(d).copied(),
                Some(NodeState::Failed) | Some(NodeState::Cancelled)
            )
        })
    }

    fn deps_ready(&self, deps: &[String], graph: &Graph) -> bool {
        let states = self.node_states.lock();
        deps.iter().all(|d| {
            // A dependency is satisfied only once it has actually finished.
            // `Ready`/`Running` mean the dependency is still in flight.
            matches!(states.get(d).copied(), Some(NodeState::Completed))
                || graph.get_node(d).map(|n| n.is_source).unwrap_or(true)
        })
    }

    fn next_progress(&self) -> usize {
        self.progress.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn get_pool_semaphore(&self, pool: Option<&str>) -> Arc<Semaphore> {
        match pool {
            Some("console") => self.console_semaphore.clone(),
            Some(name) => self
                .pools
                .get(name)
                .cloned()
                .unwrap_or(self.job_semaphore.clone()),
            None => self.job_semaphore.clone(),
        }
    }

    fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::SeqCst);
    }

    fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::SeqCst);
    }

    fn allocate_tid(&self) -> u32 {
        self.trace.allocate_tid()
    }

    #[allow(dead_code)]
    fn trace_begin(&self, target: &str, command: Option<&str>, tid: u32) -> u64 {
        self.trace.begin_target(target, command, tid)
    }

    fn trace_complete(
        &self,
        target: &str,
        start_us: u64,
        duration_us: u64,
        tid: u32,
        command: Option<&str>,
        cache_hit: bool,
    ) {
        self.trace
            .complete_target(target, start_us, duration_us, tid, command, cache_hit);
    }
}

/// The executor runs build commands respecting the dependency graph
pub struct Executor {
    config: Config,
    cache: Option<Cache>,
}

impl Executor {
    pub fn new(config: Config) -> Self {
        // Try to open the cache
        let cache = if config.cache_config.enabled {
            match Cache::open(config.cache_config.clone()) {
                Ok(c) => {
                    info!("Cache enabled at {}", c.cache_dir().display());
                    Some(c)
                }
                Err(e) => {
                    eprintln!("Warning: failed to open cache: {}", e);
                    None
                }
            }
        } else {
            None
        };

        Self { config, cache }
    }

    /// Run the build for given targets
    pub fn run(&self, graph: &Graph, targets: &[&str]) -> Result<Stats, ExecError> {
        // Use configured runtime handle if available, otherwise create one
        if let Some(handle) = &self.config.runtime {
            handle.block_on(self.run_async(graph, targets))
        } else {
            // Create a new runtime for this build
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(self.config.parallelism.min(num_cpus::get()))
                .enable_all()
                .build()
                .map_err(ExecError::SpawnError)?;

            rt.block_on(self.run_async(graph, targets))
        }
    }

    /// Handle the result of a completed task
    #[allow(clippy::type_complexity)]
    fn handle_task_result(
        result: Result<TaskResult, tokio::task::JoinError>,
        cache: &Option<Cache>,
        state: &BuildState,
    ) {
        match result {
            Ok(Ok((path, executed, cache_key))) => {
                // Store in cache if executed (not from cache)
                if executed {
                    if let (Some(c), Some(key)) = (cache, cache_key) {
                        let outputs = vec![Path::new(&path)];
                        if let Err(e) = c.store(&key, &outputs, "") {
                            debug!("Failed to cache {}: {}", path, e);
                        }
                    }
                }
            }
            Ok(Err((path, e))) => {
                let mut message = format!("FAILED: {}\n", path);
                if let ExecError::CommandFailed { command, code } = &e {
                    message.push_str(&format!("Command: {}\n", command));
                    message.push_str(&format!("Exit code: {}\n", code));
                }
                state.report_permanent_error(&message);
            }
            Err(e) if e.is_cancelled() => {
                // The build was aborted and this task was cancelled; there is
                // no command failure to report.
            }
            Err(e) => {
                state.report_permanent_error(&format!("Task error: {}\n", e));
            }
        }
    }

    async fn run_async(&self, graph: &Graph, targets: &[&str]) -> Result<Stats, ExecError> {
        let start = Instant::now();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let build_log = BuildLog::open(&cwd);
        let deps_log = DepsLog::open(&cwd);

        // Get all nodes in topological order
        let topo_order = graph
            .topo_order(targets)
            .map_err(|_| ExecError::SubcommandFailed)?;

        // Filter to nodes that need rebuilding
        let mut needs_build: HashSet<String> = HashSet::new();
        let mut needs_build_paths: HashSet<PathBuf> = HashSet::new();
        let mut mtime_cache = MtimeCache::new();

        // Mark nodes that need building. Walk in topological order (dependencies
        // first) so a dirty dependency propagates to its dependents.
        for node in topo_order.iter() {
            let mut needs_it =
                node.needs_rebuild() || node.deps.iter().any(|d| needs_build.contains(d));

            // Headers discovered by a previous compile are recorded in the
            // depfile, not in the manifest. Include them in the rebuild check,
            // and propagate generated dependencies that are themselves dirty.
            if !needs_it {
                needs_it = depfile_requires_rebuild(
                    node,
                    &cwd,
                    &needs_build_paths,
                    &deps_log,
                    &mut mtime_cache,
                );
            }

            // Phony edges have no command, so they are never scheduled. Treat
            // them as satisfied aliases rather than dirty nodes: otherwise
            // order-only phony dependencies (CMake emits one per target) would
            // force every dependent edge to rebuild.
            if needs_it && !node.is_source && !node.is_phony {
                needs_build.insert(node.path.clone());
                needs_build_paths.insert(absolute_path(&cwd, Path::new(&node.path)));
                if self.config.explain {
                    self.explain_rebuild(
                        node,
                        &needs_build,
                        &needs_build_paths,
                        &cwd,
                        &deps_log,
                        &mut mtime_cache,
                    );
                }
            }
        }

        // Collect nodes to build in order
        let work: Vec<&Node> = topo_order
            .iter()
            .filter(|n| needs_build.contains(&n.path))
            .copied()
            .collect();

        // Phony nodes are not scheduled, so expand them into their underlying
        // dependencies when deciding whether a job's dependencies are ready.
        // This is what makes CMake's `cmake_object_order_depends_*` phony
        // targets actually order generated headers before object compiles.
        let scheduling_deps: HashMap<String, Vec<String>> = work
            .iter()
            .map(|node| (node.path.clone(), graph.scheduling_deps(node)))
            .collect();

        let total = work.len();
        if total == 0 {
            match self.config.output_mode {
                OutputMode::Human => println!("ninja: no work to do."),
                OutputMode::Json => JsonEvent::NoWorkToDo.emit(),
            }
            return Ok(Stats {
                total_time: start.elapsed(),
                ..Default::default()
            });
        }

        // Collect pool depths from graph
        let pools = graph.pool_depths();

        // Emit build started event for JSON mode
        if self.config.output_mode == OutputMode::Json {
            JsonEvent::BuildStarted {
                total_targets: total,
                parallelism: self.config.parallelism,
            }
            .emit();
        }

        // Create build trace (enabled if trace_file is set)
        let trace = Arc::new(BuildTrace::new(self.config.trace_file.is_some()));
        trace.add_metadata("process_name", "rninja");

        // Create the optional live tree renderer. It falls back to the plain
        // output path when stderr is not a terminal.
        let tree_progress = if self.config.output_mode == OutputMode::Human
            && self.config.progress == ProgressMode::Tree
        {
            TreeProgress::new(graph, targets, &needs_build)
        } else {
            None
        };
        if let Some(tree) = &tree_progress {
            tree.start();
        }

        // Create shared state
        let state = Arc::new(BuildState::new(
            self.config.parallelism,
            total,
            &pools,
            self.config.output_mode,
            tree_progress,
            trace.clone(),
            build_log,
            deps_log,
        ));

        // Nodes that don't need rebuilding will never be scheduled, so mark
        // them as completed up-front. Otherwise `deps_ready` would wait for an
        // up-to-date dependency forever (e.g. when an output is missing but
        // all of its inputs are already built).
        for node in &topo_order {
            if !needs_build.contains(&node.path) {
                state.mark_completed(&node.path);
            }
        }

        // Execute nodes respecting dependencies
        let mut handles: Vec<TaskHandle> = Vec::new();
        let mut pending: Vec<&Node> = work.clone();

        while !pending.is_empty() || !handles.is_empty() {
            // Check if we should stop due to failures
            if state.should_stop(self.config.keep_going) {
                // Cancel remaining pending work
                for node in &pending {
                    state.mark_cancelled(&node.path);
                }

                if self.config.kill_on_failure {
                    // Flag first so tasks that finish while the tree is being
                    // killed are treated as cancelled, not failed. Then kill
                    // the process trees while the parent PIDs are still alive,
                    // and finally abort any tasks still waiting.
                    state.request_kill();
                    state.kill_running_processes();
                    for handle in &handles {
                        handle.abort();
                    }
                    // Catch any child that registered while the first sweep was
                    // running or while the tasks were being aborted.
                    state.kill_running_processes();
                    state.cancel_unfinished(work.iter().map(|node| node.path.as_str()));
                }
                break;
            }

            // Find nodes that are ready to execute
            let mut ready = Vec::new();
            let mut still_pending = Vec::new();

            for node in pending {
                let deps = scheduling_deps
                    .get(&node.path)
                    .map(Vec::as_slice)
                    .unwrap_or(node.deps.as_slice());

                if state.has_failed_dep(deps) {
                    // Skip nodes with failed dependencies. This is a
                    // cancellation, not a command failure, so it must not
                    // consume the keep-going budget.
                    state.mark_cancelled(&node.path);
                    continue;
                }

                if state.deps_ready(deps, graph) {
                    ready.push(node);
                } else {
                    still_pending.push(node);
                }
            }

            pending = still_pending;
            let spawned_any = !ready.is_empty();

            // Spawn tasks for ready nodes
            for node in ready {
                let state = state.clone();
                let config = self.config.clone();
                let path = node.path.clone();
                let command = node.command.clone();
                let description = node.description.clone();
                let is_phony = node.is_phony;
                let depfile = node.depfile.clone();
                let rspfile = node.rspfile.clone();
                let rspfile_content = node.rspfile_content.clone();
                let pool = node.pool.clone();
                let deps = node.deps.clone();
                let command_hash = node.command_hash;

                // Collect environment variables that affect the build
                let env_vars = collect_relevant_env_vars();

                // Compute cache key. Depfile-discovered headers are part of
                // the action inputs; without them a header change could
                // restore a stale object from the cache.
                let cache_key = if self.cache.is_some() && !is_phony {
                    command.as_deref().and_then(|cmd| {
                        let key = cache_key_for(cmd, &deps, &depfile, &env_vars);
                        if key.is_none() {
                            debug!(
                                "Skipping cache lookup for {}: depfile dependencies unavailable",
                                path
                            );
                        }
                        key
                    })
                } else {
                    None
                };

                // Check cache and try restore synchronously before spawning
                let mut restored = false;
                if let (Some(cache), Some(key)) = (&self.cache, &cache_key) {
                    if let Some(entry) = cache.lookup(key) {
                        match cache.restore(&entry) {
                            Ok(true) => {
                                state.record_cache_hit();
                                state.mark_completed(&path);
                                restored = true;
                            }
                            Ok(false) => {
                                debug!("Cache entry found but blob missing for {}", path);
                            }
                            Err(e) => {
                                debug!("Cache restore failed for {}: {}", path, e);
                            }
                        }
                    }
                }

                if restored {
                    // A cache hit is a successful build as far as Ninja's
                    // logs are concerned.
                    state.record_build(&path, command_hash, 0, 0);
                    if !depfile.is_empty() {
                        if let Ok(parsed) = depfile::parse(&depfile) {
                            let mut deps: Vec<String> = parsed.deps.into_iter().collect();
                            deps.sort();
                            state.record_deps(&path, &deps);
                        } else {
                            state.refresh_deps_mtime(&path);
                        }
                    } else {
                        state.refresh_deps_mtime(&path);
                    }

                    // Emit cache hit output and trace inline (no task spawn)
                    let idx = state.next_progress();
                    match state.output_mode {
                        OutputMode::Human => {
                            if let Some(tree) = &state.tree_progress {
                                tree.cache_hit(&path);
                            } else {
                                let desc = description.as_deref().unwrap_or("cached");
                                println!("[{}/{}] {} (cached)", idx, state.total, desc);
                                let _ = std::io::stdout().flush();
                            }
                        }
                        OutputMode::Json => {
                            JsonEvent::CacheHit {
                                target: &path,
                                index: idx,
                                total: state.total,
                            }
                            .emit();
                        }
                    }
                    let tid = state.allocate_tid();
                    let trace_start = state.trace.timestamp();
                    state.trace_complete(&path, trace_start, 0, tid, command.as_deref(), true);
                    continue;
                }

                // Not restored from cache; spawn task for execution
                state.mark_ready(&path);

                let handle = tokio::spawn(async move {
                    // Acquire semaphore permit
                    let sem = state.get_pool_semaphore(pool.as_deref());
                    let _permit = match sem.acquire().await {
                        Ok(permit) => permit,
                        Err(e) => {
                            tracing::error!("Failed to acquire semaphore: {}", e);
                            state.mark_failed(&path);
                            state.report_task_finished(&path, false);
                            return Err((
                                path,
                                ExecError::SpawnError(std::io::Error::other(format!(
                                    "semaphore error: {}",
                                    e
                                ))),
                            ));
                        }
                    };

                    // This task was spawned when its dependencies became
                    // ready, but it may have waited behind other jobs long
                    // enough for the failure limit to be reached. Do not start
                    // it in that case; it never executed, so it is cancelled
                    // rather than failed.
                    if state.should_stop(config.keep_going) {
                        state.mark_cancelled(&path);
                        return Ok((path, false, None));
                    }

                    state.mark_running(&path);
                    state.report_task_started(&path);

                    // Allocate thread ID for tracing
                    let tid = state.allocate_tid();
                    let trace_start = state.trace.timestamp();
                    let exec_start = Instant::now();
                    let build_start_ms = state.elapsed_ms();

                    let result = execute_node_async(
                        &path,
                        command.as_deref(),
                        description.as_deref(),
                        is_phony,
                        &depfile,
                        rspfile.as_deref(),
                        rspfile_content.as_deref(),
                        state.next_progress(),
                        state.total,
                        &config,
                        &state,
                    )
                    .await;

                    // Record trace event
                    let duration_us = exec_start.elapsed().as_micros() as u64;
                    state.trace_complete(
                        &path,
                        trace_start,
                        duration_us,
                        tid,
                        command.as_deref(),
                        false,
                    );

                    if state.kill_requested() {
                        state.mark_cancelled(&path);
                        return Ok((path, false, None));
                    }

                    match result {
                        Ok(executed) => {
                            if executed {
                                state.record_build(
                                    &path,
                                    command_hash,
                                    build_start_ms,
                                    state.elapsed_ms(),
                                );
                            }
                            state.mark_completed(&path);
                            state.report_task_finished(&path, true);

                            // The depfile only exists after the first compile.
                            // Recompute the key now so that build can still
                            // populate the cache with header-aware inputs.
                            let store_key = if executed {
                                cache_key_for(
                                    command.as_deref().unwrap_or_default(),
                                    &deps,
                                    &depfile,
                                    &env_vars,
                                )
                            } else {
                                cache_key
                            };

                            Ok((path, executed, store_key))
                        }
                        Err(e) => {
                            state.mark_failed(&path);
                            state.report_task_finished(&path, false);
                            Err((path, e))
                        }
                    }
                });

                handles.push(handle);
            }

            // If nothing is ready and we have pending handles, wait for some to complete
            if pending.is_empty() && handles.is_empty() {
                break;
            }

            if !handles.is_empty() && (pending.is_empty() || !spawned_any) {
                // Wait for at least one task to complete
                let (result, _idx, remaining) = futures::future::select_all(handles).await;
                handles = remaining;

                Self::handle_task_result(result, &self.cache, &state);
            }
        }

        // Wait for remaining handles
        for handle in handles {
            let result = handle.await;
            Self::handle_task_result(result, &self.cache, &state);
        }

        let fail_count = state.fail_count.load(Ordering::SeqCst);
        let finish_count = state.progress.load(Ordering::SeqCst);
        let cancelled_running = state.cancelled_running.load(Ordering::SeqCst);
        let cache_hits = state.cache_hits.load(Ordering::SeqCst);
        let cache_misses = state.cache_misses.load(Ordering::SeqCst);

        let completed = finish_count
            .saturating_sub(fail_count)
            .saturating_sub(cancelled_running);
        let stats = Stats {
            started: finish_count,
            finished: completed,
            failed: fail_count,
            skipped: total.saturating_sub(finish_count.saturating_sub(cancelled_running)),
            cache_hits,
            cache_misses,
            total_time: start.elapsed(),
        };

        if let Some(tree) = &state.tree_progress {
            tree.finish();

            // The tree is transient, so leave one permanent line confirming
            // that the build finished and how much work was done.
            if fail_count == 0 {
                let done = completed;
                let noun = if done == 1 { "task" } else { "tasks" };
                if cache_hits > 0 {
                    eprintln!(
                        "ninja: finished {} {} ({} from cache)",
                        done, noun, cache_hits
                    );
                } else {
                    eprintln!("ninja: finished {} {}", done, noun);
                }
            }
        }

        if self.config.stats {
            stats.print(self.config.output_mode);
        }

        // Emit build finished event for JSON mode
        if self.config.output_mode == OutputMode::Json {
            JsonEvent::BuildFinished {
                success: fail_count == 0,
                targets_built: finish_count.saturating_sub(cancelled_running),
                targets_total: total,
                duration_ms: stats.total_time.as_millis() as u64,
                cache_hits: if cache_hits > 0 {
                    Some(cache_hits)
                } else {
                    None
                },
                cache_misses: if cache_misses > 0 {
                    Some(cache_misses)
                } else {
                    None
                },
            }
            .emit();
        }

        // Write trace file if configured
        if let Some(ref trace_path) = self.config.trace_file {
            if let Err(e) = trace.write_to_file(trace_path) {
                eprintln!("Warning: failed to write trace file: {}", e);
            } else {
                info!("Trace written to {}", trace_path.display());
            }
        }

        if fail_count > 0 {
            Err(ExecError::BuildFailed(fail_count))
        } else {
            Ok(stats)
        }
    }

    fn explain_rebuild(
        &self,
        node: &Node,
        needs_build: &HashSet<String>,
        needs_build_paths: &HashSet<PathBuf>,
        cwd: &Path,
        deps_log: &DepsLog,
        mtime_cache: &mut MtimeCache,
    ) {
        if !Path::new(&node.path).exists() {
            println!("ninja explain: {} is missing", node.path);
            return;
        }

        let output_mtime = mtime_cache.get(Path::new(&node.path));

        for dep in &node.deps {
            if needs_build.contains(dep) {
                println!("ninja explain: {} is dirty", dep);
                return;
            }

            if let (Some(out_t), Some(dep_t)) = (output_mtime, mtime_cache.get(Path::new(dep))) {
                if dep_t > out_t {
                    println!("ninja explain: {} is newer than {}", dep, node.path);
                    return;
                }
            }
        }

        let current_ninja_mtime = get_ninja_mtime(Path::new(&node.path));
        let discovered: Option<Vec<String>> = if !node.depfile.is_empty() {
            match depfile::parse(&node.depfile) {
                Ok(parsed) => Some(parsed.deps.into_iter().collect()),
                Err(_) => match deps_log.get(&node.path) {
                    Some(entry) if current_ninja_mtime == Some(entry.mtime) => {
                        Some(entry.deps.clone())
                    }
                    Some(_) => {
                        println!(
                            "ninja explain: stored deps info is out of date for {}",
                            node.path
                        );
                        return;
                    }
                    None => None,
                },
            }
        } else {
            match deps_log.get(&node.path) {
                Some(entry) if current_ninja_mtime == Some(entry.mtime) => Some(entry.deps.clone()),
                Some(_) => {
                    println!(
                        "ninja explain: stored deps info is out of date for {}",
                        node.path
                    );
                    return;
                }
                None => None,
            }
        };

        if let Some(deps) = discovered {
            for dep in &deps {
                let dep_path = absolute_path(cwd, Path::new(dep.as_str()));

                if needs_build_paths.contains(&dep_path) {
                    println!("ninja explain: {} is dirty", dep);
                    return;
                }

                match (mtime_cache.get(&dep_path), output_mtime) {
                    (Some(dep_t), Some(out_t)) => {
                        if dep_t > out_t {
                            println!("ninja explain: {} is newer than {}", dep, node.path);
                            return;
                        }
                    }
                    (None, _) => {
                        println!("ninja explain: {} is missing", dep);
                        return;
                    }
                    (Some(_), None) => {
                        println!("ninja explain: {} is missing or unreadable", node.path);
                        return;
                    }
                }
            }
        } else if !node.depfile.is_empty() {
            println!(
                "ninja explain: depfile {} is missing or unreadable for {}",
                node.depfile, node.path
            );
            return;
        }

        println!("ninja explain: {} is dirty", node.path);
    }
}

/// Compute a cache key from the manifest inputs plus any headers recorded in
/// the depfile. Returns `None` when the depfile is unavailable or one of its
/// inputs is missing, in which case the caller must not use the cache.
fn cache_key_for(
    command: &str,
    deps: &[String],
    depfile: &str,
    env_vars: &[(&str, String)],
) -> Option<String> {
    let mut inputs = deps.to_vec();

    if !depfile.is_empty() {
        let parsed = depfile::parse(depfile).ok()?;
        for dep in parsed.deps {
            // A missing header makes the key ambiguous. Skipping the cache is
            // safer than risking a stale hit.
            if !Path::new(&dep).exists() {
                return None;
            }
            inputs.push(dep);
        }
    }

    inputs.sort();
    inputs.dedup();

    let input_paths: Vec<&Path> = inputs.iter().map(|p| Path::new(p.as_str())).collect();
    let env_refs: Vec<(&str, &str)> = env_vars
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();

    crate::cache::compute_action_key(command, &input_paths, &env_refs).ok()
}

/// Check whether a node's recorded depfile dependencies require a rebuild.
///
/// Compilers record headers that are not present in the manifest in a depfile.
/// A missing or malformed depfile means the dependency information is
/// unavailable, so the output cannot be trusted and must be rebuilt.
fn depfile_requires_rebuild(
    node: &Node,
    cwd: &Path,
    needs_build_paths: &HashSet<PathBuf>,
    deps_log: &DepsLog,
    mtime_cache: &mut MtimeCache,
) -> bool {
    let deps: Vec<String> = if !node.depfile.is_empty() {
        match depfile::parse(&node.depfile) {
            Ok(parsed) => parsed.deps.into_iter().collect(),
            Err(_) => {
                // Ninja deletes depfiles after storing them in .ninja_deps.
                let current = get_ninja_mtime(Path::new(&node.path));
                match current.and_then(|mtime| deps_log.get_if_current(&node.path, mtime)) {
                    Some(entry) => entry.deps.clone(),
                    None => return true,
                }
            }
        }
    } else {
        // Rules using deps=msvc have no depfile but may have a deps log entry.
        match deps_log.get(&node.path) {
            Some(entry) => {
                let current = get_ninja_mtime(Path::new(&node.path));
                if current != Some(entry.mtime) {
                    return true;
                }
                entry.deps.clone()
            }
            None => Vec::new(),
        }
    };

    if deps.is_empty() {
        return false;
    }

    let output_mtime = mtime_cache.get(&absolute_path(cwd, Path::new(&node.path)));
    for dep in &deps {
        // Depfiles may use paths relative to the build directory or absolute
        // paths, so normalize before comparing against scheduled outputs.
        let dep_path = absolute_path(cwd, Path::new(dep.as_str()));

        // A generated header that is already scheduled to be rebuilt makes
        // its dependents dirty even if the current file is still older.
        if needs_build_paths.contains(&dep_path) {
            return true;
        }

        match (mtime_cache.get(&dep_path), output_mtime) {
            (Some(dep_mtime), Some(output_mtime)) if dep_mtime <= output_mtime => {}
            _ => return true,
        }
    }

    false
}

/// Resolve a path against the build directory for comparisons. Depfiles and
/// manifests may mix relative and absolute forms of the same file.
fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    // Normalize `.` and `..` components lexically so equivalent spellings
    // compare equal without requiring the file to exist.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Execute a single node asynchronously.
///
/// Returns `Ok((path, executed, cache_key))` where:
/// - `executed` is `true` if the command was run, `false` if restored from cache
/// - `cache_key` is the action key if the command was executed
///
/// # Security Warning
///
/// Commands are executed via `sh -c` for shell compatibility with ninja build
/// files. This means shell injection is possible if build rules contain untrusted
/// input. Only use build files from trusted sources. Consider running rninja
/// with `--dry-run` for untrusted projects.
#[allow(clippy::too_many_arguments)]
async fn execute_node_async(
    path: &str,
    command: Option<&str>,
    description: Option<&str>,
    is_phony: bool,
    depfile: &str,
    rspfile: Option<&str>,
    rspfile_content: Option<&str>,
    idx: usize,
    total: usize,
    config: &Config,
    state: &BuildState,
) -> Result<bool, ExecError> {
    // Skip phony targets
    if is_phony {
        return Ok(false);
    }

    let command = match command {
        Some(cmd) => cmd,
        None => return Ok(false),
    };

    state.record_cache_miss();

    // Print status. Tree progress is updated by the executor when the task is
    // actually scheduled, so it is only printed here in plain mode. Verbose
    // tree mode emits command lines above the tree instead of using them as
    // tree labels, keeping long commands readable.
    match state.output_mode {
        OutputMode::Human => {
            if let Some(tree) = &state.tree_progress {
                if config.verbose {
                    // The tree already shows completed/total progress, so the
                    // command log does not repeat a different counter.
                    tree.write_status_line(command);
                }
            } else {
                let desc = description.unwrap_or(command);
                if config.verbose {
                    println!("[{}/{}] {}", idx, total, command);
                } else {
                    println!("[{}/{}] {}", idx, total, desc);
                }
                if let Err(e) = std::io::stdout().flush() {
                    debug!("Failed to flush stdout: {}", e);
                }
            }
        }
        OutputMode::Json => {
            JsonEvent::TargetStarted {
                target: path,
                index: idx,
                total,
                command: if config.verbose { Some(command) } else { None },
            }
            .emit();
        }
    }

    // Dry run - don't actually execute
    if config.dry_run {
        return Ok(false);
    }

    // Write response file if needed
    if let (Some(rsp), Some(content)) = (rspfile, rspfile_content) {
        tokio::fs::write(rsp, content).await?;
    }

    // Execute the command
    let output = spawn_command(command, path, state).await?;
    let success = output.status.success();

    // Print output if any, keeping the live progress tree intact. Tree mode
    // treats successful task output as noise: the tree already shows what is
    // running, so successful output is only shown with --verbose. Failed
    // tasks always surface their output so diagnostics are never hidden.
    if should_emit_output(state.tree_progress.is_some(), success, config.verbose) {
        state.write_output(&output.stdout, &output.stderr);
    }

    // Parse depfile if present
    if !depfile.is_empty() {
        if let Ok(parsed) = depfile::parse(depfile) {
            if success {
                let mut deps: Vec<String> = parsed.deps.into_iter().collect();
                deps.sort();
                state.record_deps(path, &deps);
            } else {
                debug!("Parsed depfile {}: {:?}", depfile, parsed.deps);
            }
        }
    }

    // Clean up response file
    if let Some(rsp) = rspfile {
        if let Err(e) = tokio::fs::remove_file(rsp).await {
            tracing::warn!("Failed to clean up response file {}: {}", rsp, e);
        }
    }

    if success {
        // Emit success event for JSON mode
        if state.output_mode == OutputMode::Json {
            JsonEvent::TargetFinished {
                target: path,
                index: idx,
                total,
                success: true,
                error: None,
            }
            .emit();
        }
        Ok(true) // Command was executed
    } else {
        let code = output.status.code().unwrap_or(-1);
        // Emit failure event for JSON mode
        if state.output_mode == OutputMode::Json {
            let error_msg = format!("exit code {}", code);
            JsonEvent::TargetFinished {
                target: path,
                index: idx,
                total,
                success: false,
                error: Some(&error_msg),
            }
            .emit();
        }
        Err(ExecError::CommandFailed {
            command: command.to_string(),
            code,
        })
    }
}

/// Whether captured command output should be written to the terminal.
///
/// Plain mode matches Ninja and always prints output. Tree mode keeps the
/// display quiet by default: output from successful tasks is hidden unless
/// `--verbose` is set, while failed tasks always print their output.
fn should_emit_output(tree_mode: bool, success: bool, verbose: bool) -> bool {
    !tree_mode || !success || verbose
}

/// Spawn a build command, matching ninja's platform behavior.
///
/// On Unix, commands are run through `sh -c`. On Windows, ninja passes the
/// command line directly to `CreateProcess`, so we do the same. Wrapping the
/// command in an extra `cmd /C` breaks commands that already invoke
/// `cmd.exe /C "..."` (as CMake does for linking) because the nested quotes
/// get escaped twice.
async fn spawn_command(
    command: &str,
    path: &str,
    state: &BuildState,
) -> std::io::Result<std::process::Output> {
    #[cfg(windows)]
    let mut cmd = {
        let (program, rest) = split_windows_program(command);
        let mut cmd = Command::new(program);
        if !rest.is_empty() {
            // Pass the remainder verbatim so existing quoting is preserved.
            cmd.raw_arg(rest);
        }
        cmd
    };

    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        // Put the shell in its own process group so --kill-on-failure can
        // terminate the whole tree with kill(-pid).
        cmd.process_group(0);
        cmd
    };

    // Capture both streams so the executor can apply its success/failure
    // output policy. Without this, children inherit rninja's terminal and
    // successful stderr would leak through even in tree mode.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // If the executor aborts this task (for example, with
    // --kill-on-failure), dropping the future terminates the direct child.
    cmd.kill_on_drop(true);

    let child = cmd.spawn()?;
    if let Some(pid) = child.id() {
        state.register_process(path, pid);
    }

    let result = child.wait_with_output().await;
    state.unregister_process(path);
    result
}

/// Terminate a child process and its descendants. This is best effort:
/// Windows uses `taskkill /T`, while Unix kills the process group created by
/// `spawn_command`.
fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(unix)]
    {
        // Negative PID targets the process group created in spawn_command.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

/// Split a Windows command line into the program path and its arguments.
///
/// The program may be quoted if it contains spaces. Everything after the
/// program is returned unchanged so it can be passed through verbatim.
#[cfg(windows)]
fn split_windows_program(command: &str) -> (&str, &str) {
    let command = command.trim_start();

    if let Some(rest) = command.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (&rest[..end], &rest[end + 1..]);
        }
    }

    match command.find(char::is_whitespace) {
        Some(i) => (&command[..i], &command[i..]),
        None => (command, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_node_can_be_marked_completed() {
        // Nodes that are already up-to-date are marked completed before
        // scheduling; they never pass through Ready/Running.
        assert_eq!(
            NodeState::Pending.transition(NodeState::Completed).unwrap(),
            NodeState::Completed
        );
    }

    #[test]
    fn test_completed_node_cannot_run_again() {
        assert!(NodeState::Completed.transition(NodeState::Running).is_err());
    }

    #[test]
    fn test_skipped_node_is_cancelled_not_failed() {
        // Nodes skipped because a dependency failed never ran a command, so
        // they must not consume the keep-going failure budget.
        assert_eq!(
            NodeState::Pending.transition(NodeState::Cancelled).unwrap(),
            NodeState::Cancelled
        );
        assert!(NodeState::Pending.transition(NodeState::Failed).is_err());
    }

    #[test]
    fn test_ready_node_can_fail_when_scheduling_fails() {
        // A spawned task can fail before it starts (for example, if the
        // semaphore is closed), so Ready -> Failed is a valid fatal path.
        assert_eq!(
            NodeState::Ready.transition(NodeState::Failed).unwrap(),
            NodeState::Failed
        );
    }

    #[test]
    fn test_cache_key_includes_depfile_headers() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.c");
        let header = dir.path().join("config.h");
        let depfile = dir.path().join("main.o.d");

        std::fs::write(&source, "int main(void) { return 0; }").unwrap();
        std::fs::write(&header, "#define VALUE 1\n").unwrap();
        std::fs::write(
            &depfile,
            format!("main.o: {} {}\n", source.display(), header.display()),
        )
        .unwrap();

        let deps = vec![source.to_string_lossy().into_owned()];
        let depfile = depfile.to_str().unwrap();

        let first = cache_key_for("cc", &deps, depfile, &[]).unwrap();

        std::fs::write(&header, "#define VALUE 2\n").unwrap();
        let second = cache_key_for("cc", &deps, depfile, &[]).unwrap();
        assert_ne!(first, second, "header content must affect the cache key");

        std::fs::remove_file(&header).unwrap();
        assert!(
            cache_key_for("cc", &deps, depfile, &[]).is_none(),
            "missing depfile inputs must disable the cache"
        );
    }

    #[test]
    fn test_spawn_command_captures_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let state = BuildState::new(
            1,
            1,
            &HashMap::new(),
            OutputMode::Human,
            None,
            Arc::new(BuildTrace::new(false)),
            BuildLog::open(dir.path()),
            DepsLog::open(dir.path()),
        );

        #[cfg(windows)]
        let command = "cmd /C \"echo stdout-value & echo stderr-value 1>&2\"";
        #[cfg(not(windows))]
        let command = "echo stdout-value; echo stderr-value 1>&2";

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let output = runtime
            .block_on(spawn_command(command, "test", &state))
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "stdout-value"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            "stderr-value"
        );
    }

    #[test]
    fn test_task_output_policy() {
        // Plain mode always prints output, matching Ninja.
        assert!(should_emit_output(false, true, false));
        assert!(should_emit_output(false, false, false));

        // Tree mode hides successful output unless verbose.
        assert!(!should_emit_output(true, true, false));
        assert!(should_emit_output(true, true, true));

        // Failures always print their output so diagnostics are visible.
        assert!(should_emit_output(true, false, false));
        assert!(should_emit_output(true, false, true));
    }
}
