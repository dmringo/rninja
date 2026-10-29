//! Build log for tracking output mtimes and enabling fast no-op detection
//!
//! This implements ninja's .ninja_log format for compatibility.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;
use tracing::warn;

/// Entry in the build log
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Start time in milliseconds relative to the build start (Ninja format)
    pub start_time_ms: u64,
    /// End time in milliseconds relative to the build start (Ninja format)
    pub end_time_ms: u64,
    /// Output mtime in Ninja's `TimeStamp` units
    pub mtime_ns: u64,
    /// Output path
    pub output: String,
    /// Ninja rapidhash of the command
    pub command_hash: u64,
}

/// Build log for fast up-to-date checking
pub struct BuildLog {
    path: PathBuf,
    entries: HashMap<String, LogEntry>,
    dirty: AtomicBool,
}

impl BuildLog {
    /// Open or create a build log
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(".ninja_log");
        let mut log = Self {
            path,
            entries: HashMap::new(),
            dirty: AtomicBool::new(false),
        };
        log.load();
        log
    }

    /// Load entries from disk
    fn load(&mut self) {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return,
        };

        let reader = BufReader::new(file);
        let mut lines = reader.lines();

        // Match Ninja's policy: only the current version is understood.
        // Older rninja logs (v5, decimal hashes) and future formats are
        // ignored rather than misparsed. The log is disposable; a later
        // build rewrites it in the current format.
        if let Some(Ok(header)) = lines.next() {
            let version = header
                .strip_prefix("# ninja log v")
                .and_then(|value| value.trim().parse::<u32>().ok());
            if version != Some(7) {
                return;
            }
        } else {
            return;
        }

        for line in lines {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };

            // Format: start_time end_time mtime_ns output command_hash
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 5 {
                if let (Ok(start), Ok(end), Ok(mtime), Ok(hash)) = (
                    parts[0].parse::<u64>(),
                    parts[1].parse::<u64>(),
                    parts[2].parse::<u64>(),
                    u64::from_str_radix(parts[4], 16),
                ) {
                    let output = parts[3].to_string();
                    self.entries.insert(
                        output.clone(),
                        LogEntry {
                            start_time_ms: start,
                            end_time_ms: end,
                            mtime_ns: mtime,
                            output,
                            command_hash: hash,
                        },
                    );
                }
            }
        }
    }

    /// Record a build
    pub fn record(&mut self, output: &str, command_hash: u64, start_ms: u64, end_ms: u64) {
        let mtime_ns = get_ninja_mtime(Path::new(output)).unwrap_or(0) as u64;

        self.entries.insert(
            output.to_string(),
            LogEntry {
                start_time_ms: start_ms,
                end_time_ms: end_ms,
                mtime_ns,
                output: output.to_string(),
                command_hash,
            },
        );
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Get recorded mtime for an output
    pub fn get_mtime(&self, output: &str) -> Option<u64> {
        self.entries.get(output).map(|e| e.mtime_ns)
    }

    /// Get command hash for an output
    pub fn get_command_hash(&self, output: &str) -> Option<u64> {
        self.entries.get(output).map(|e| e.command_hash)
    }

    /// Save log to disk
    pub fn save(&self) -> std::io::Result<()> {
        if !self.dirty.load(Ordering::Relaxed) && self.path.exists() {
            return Ok(());
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.path)?;

        writeln!(file, "# ninja log v7")?;

        for entry in self.entries.values() {
            writeln!(
                file,
                "{}\t{}\t{}\t{}\t{:x}",
                entry.start_time_ms,
                entry.end_time_ms,
                entry.mtime_ns,
                entry.output,
                entry.command_hash
            )?;
        }

        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Get all output names in the log
    pub fn entries(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    /// Check if output needs rebuild based on log
    pub fn needs_rebuild(&self, output: &str, command_hash: u64) -> bool {
        match self.entries.get(output) {
            Some(entry) => {
                // Command changed?
                if entry.command_hash != command_hash {
                    return true;
                }

                // Check if file mtime matches log
                let current_mtime = get_ninja_mtime(Path::new(output)).unwrap_or(0) as u64;
                if current_mtime == 0 {
                    return true; // File doesn't exist
                }

                // If mtime changed, needs rebuild
                current_mtime != entry.mtime_ns
            }
            None => true, // Not in log, needs build
        }
    }
}

impl Drop for BuildLog {
    fn drop(&mut self) {
        let _ = self.save();
    }
}

/// Mtime cache for avoiding repeated stat() calls
pub struct MtimeCache {
    cache: HashMap<PathBuf, Option<u64>>,
}

impl MtimeCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::with_capacity(1024),
        }
    }

    /// Get mtime in nanoseconds, cached
    pub fn get(&mut self, path: &Path) -> Option<u64> {
        if let Some(&cached) = self.cache.get(path) {
            return cached;
        }

        let mtime = get_mtime_ns(path);
        self.cache.insert(path.to_path_buf(), mtime);
        mtime
    }

    /// Invalidate cache entry (after build)
    pub fn invalidate(&mut self, path: &Path) {
        self.cache.remove(path);
    }

    /// Pre-populate cache with multiple paths (sequential)
    pub fn prefetch(&mut self, paths: &[&Path]) {
        for path in paths {
            if !self.cache.contains_key(*path) {
                let mtime = get_mtime_ns(path);
                self.cache.insert(path.to_path_buf(), mtime);
            }
        }
    }

    /// Pre-populate cache with multiple paths in parallel using rayon
    pub fn prefetch_parallel(&mut self, paths: &[PathBuf]) {
        use rayon::prelude::*;

        // Filter paths we haven't cached yet
        let uncached: Vec<&PathBuf> = paths
            .iter()
            .filter(|p| !self.cache.contains_key(*p))
            .collect();

        if uncached.is_empty() {
            return;
        }

        // Stat files in parallel
        let results: Vec<(PathBuf, Option<u64>)> = uncached
            .par_iter()
            .map(|p| ((*p).clone(), get_mtime_ns(p)))
            .collect();

        // Insert results into cache
        for (path, mtime) in results {
            self.cache.insert(path, mtime);
        }
    }

    /// Insert a known mtime (used for batch operations)
    pub fn insert(&mut self, path: PathBuf, mtime: Option<u64>) {
        self.cache.insert(path, mtime);
    }
}

impl Default for MtimeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Get a file mtime in Ninja's `TimeStamp` units.
///
/// On POSIX this is nanoseconds since the Unix epoch. On Windows it is the
/// FILETIME value rebased to the 2000 epoch, matching Ninja's
/// `TimeStampFromFileTime`.
pub fn get_ninja_mtime(path: &Path) -> Option<i64> {
    let metadata = std::fs::metadata(path).ok()?;

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Some(metadata.last_write_time() as i64 - 12_622_770_400i64 * 10_000_000)
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec())
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = metadata;
        None
    }
}

/// Get file mtime in nanoseconds since epoch
pub fn get_mtime_ns(path: &Path) -> Option<u64> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to get metadata for {}: {}", path.display(), e);
            return None;
        }
    };
    let mtime = match metadata.modified() {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to get mtime for {}: {}", path.display(), e);
            return None;
        }
    };
    let duration = match mtime.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d,
        Err(e) => {
            warn!("Failed to compute duration for {}: {}", path.display(), e);
            return None;
        }
    };
    Some(duration.as_nanos() as u64)
}

const RAPID_SEED: u64 = 0xbdd89aa982704029;
const RAPID_SECRET: [u64; 3] = [0x2d358dccaa6c78a5, 0x8bb84b93962eacc9, 0x4b33a62ed433d4a3];

fn rapid_mum(a: u64, b: u64) -> (u64, u64) {
    let product = (a as u128) * (b as u128);
    (product as u64, (product >> 64) as u64)
}

fn rapid_mix(a: u64, b: u64) -> u64 {
    let (low, high) = rapid_mum(a, b);
    low ^ high
}

fn read64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn read32(data: &[u8], offset: usize) -> u64 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as u64
}

fn read_small(data: &[u8], len: usize) -> u64 {
    ((data[0] as u64) << 56) | ((data[len >> 1] as u64) << 32) | (data[len - 1] as u64)
}

/// Ninja's `rapidhash` command hash (used in `.ninja_log`).
fn rapidhash(data: &[u8]) -> u64 {
    let len = data.len();
    let mut seed =
        RAPID_SEED ^ rapid_mix(RAPID_SEED ^ RAPID_SECRET[0], RAPID_SECRET[1]) ^ (len as u64);
    let (mut a, mut b);

    if len <= 16 {
        if len >= 4 {
            let last = len - 4;
            a = (read32(data, 0) << 32) | read32(data, last);
            let delta = (len & 24) >> (len >> 3);
            b = (read32(data, delta) << 32) | read32(data, last - delta);
        } else if len > 0 {
            a = read_small(data, len);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut remaining = len;
        let mut offset = 0;

        if remaining > 48 {
            let mut see1 = seed;
            let mut see2 = seed;

            while remaining >= 96 {
                seed = rapid_mix(
                    read64(data, offset) ^ RAPID_SECRET[0],
                    read64(data, offset + 8) ^ seed,
                );
                see1 = rapid_mix(
                    read64(data, offset + 16) ^ RAPID_SECRET[1],
                    read64(data, offset + 24) ^ see1,
                );
                see2 = rapid_mix(
                    read64(data, offset + 32) ^ RAPID_SECRET[2],
                    read64(data, offset + 40) ^ see2,
                );
                seed = rapid_mix(
                    read64(data, offset + 48) ^ RAPID_SECRET[0],
                    read64(data, offset + 56) ^ seed,
                );
                see1 = rapid_mix(
                    read64(data, offset + 64) ^ RAPID_SECRET[1],
                    read64(data, offset + 72) ^ see1,
                );
                see2 = rapid_mix(
                    read64(data, offset + 80) ^ RAPID_SECRET[2],
                    read64(data, offset + 88) ^ see2,
                );
                offset += 96;
                remaining -= 96;
            }

            if remaining >= 48 {
                seed = rapid_mix(
                    read64(data, offset) ^ RAPID_SECRET[0],
                    read64(data, offset + 8) ^ seed,
                );
                see1 = rapid_mix(
                    read64(data, offset + 16) ^ RAPID_SECRET[1],
                    read64(data, offset + 24) ^ see1,
                );
                see2 = rapid_mix(
                    read64(data, offset + 32) ^ RAPID_SECRET[2],
                    read64(data, offset + 40) ^ see2,
                );
                offset += 48;
                remaining -= 48;
            }

            seed ^= see1 ^ see2;
        }

        if remaining > 16 {
            seed = rapid_mix(
                read64(data, offset) ^ RAPID_SECRET[2],
                read64(data, offset + 8) ^ seed ^ RAPID_SECRET[1],
            );
            if remaining > 32 {
                seed = rapid_mix(
                    read64(data, offset + 16) ^ RAPID_SECRET[2],
                    read64(data, offset + 24) ^ seed,
                );
            }
        }

        a = read64(data, offset + remaining - 16);
        b = read64(data, offset + remaining - 8);
    }

    a ^= RAPID_SECRET[1];
    b ^= seed;
    let (low, high) = rapid_mum(a, b);
    rapid_mix(low ^ RAPID_SECRET[0] ^ (len as u64), high ^ RAPID_SECRET[1])
}

/// Hash a command string for Ninja-compatible log storage.
pub fn hash_command(cmd: &str) -> u64 {
    rapidhash(cmd.as_bytes())
}

/// A node being checked, together with dependencies discovered from its
/// previous compile.
struct CheckNode<'a> {
    output: &'a str,
    node: &'a crate::graph::Node,
    depfile_deps: Vec<String>,
}

/// Fast check if all targets are up-to-date (no tokio needed)
/// Uses parallel stat() for large builds, sequential for small ones.
pub fn quick_uptodate_check(
    graph: &crate::graph::Graph,
    targets: &[&str],
    log: &BuildLog,
    deps_log: &crate::depslog::DepsLog,
    mtime_cache: &mut MtimeCache,
) -> bool {
    // Get all nodes we'd need to build
    let order = match graph.topo_order(targets) {
        Ok(o) => o,
        Err(_) => return false,
    };

    // Collect nodes to check (non-source, non-phony), including headers
    // recorded by the previous compile. A missing or malformed depfile means
    // we cannot prove the output is up-to-date, so fall through to the
    // executor.
    let mut nodes_to_check: Vec<CheckNode> = Vec::with_capacity(order.len());
    let mut total_paths = 0;

    for node in &order {
        if node.is_source || node.is_phony {
            continue;
        }

        // Ninja deletes depfiles after recording them in .ninja_deps, and
        // rules using deps=msvc have no depfile at all. Prefer the depfile
        // when present, otherwise use a current deps-log entry.
        let current_mtime = get_ninja_mtime(Path::new(&node.path));
        let log_deps = match deps_log.get(&node.path) {
            Some(entry) if Some(entry.mtime) == current_mtime => Some(entry.deps.clone()),
            Some(_) => return false, // stored deps are stale
            None => None,
        };

        let depfile_deps = if !node.depfile.is_empty() {
            match crate::executor::depfile::parse(&node.depfile) {
                Ok(parsed) => parsed.deps.into_iter().collect(),
                Err(_) => match log_deps {
                    Some(deps) => deps,
                    None => return false,
                },
            }
        } else {
            log_deps.unwrap_or_default()
        };

        total_paths += 1 + node.deps.len() + depfile_deps.len();
        nodes_to_check.push(CheckNode {
            output: node.path.as_str(),
            node,
            depfile_deps,
        });
    }

    // Use parallel stat() for large builds (> 2000 paths), sequential otherwise
    if total_paths > 2000 {
        quick_uptodate_check_parallel(&nodes_to_check, log)
    } else {
        quick_uptodate_check_sequential(&nodes_to_check, log, mtime_cache)
    }
}

/// Sequential up-to-date check for smaller builds
#[inline]
fn quick_uptodate_check_sequential(
    nodes_to_check: &[CheckNode],
    log: &BuildLog,
    mtime_cache: &mut MtimeCache,
) -> bool {
    for check in nodes_to_check {
        let output = check.output;
        let node = check.node;

        // Check output exists
        let output_path = Path::new(output);
        let output_mtime = match mtime_cache.get(output_path) {
            Some(m) => m,
            None => return false, // Output doesn't exist
        };

        // Check command hash hasn't changed (uses pre-computed hash)
        if node.command.is_some() {
            if let Some(logged_hash) = log.get_command_hash(output) {
                if logged_hash != node.command_hash {
                    return false; // Command changed
                }
            }
        }

        // Check all manifest inputs are older than output
        for dep in &node.deps {
            let dep_path = Path::new(dep.as_str());
            if let Some(dep_mtime) = mtime_cache.get(dep_path) {
                if dep_mtime > output_mtime {
                    return false; // Input is newer than output
                }
            }
        }

        // Depfile dependencies are real inputs; unlike phony paths, they must
        // exist and be no newer than the output.
        for dep in &check.depfile_deps {
            match mtime_cache.get(Path::new(dep.as_str())) {
                Some(dep_mtime) if dep_mtime <= output_mtime => {}
                _ => return false,
            }
        }
    }

    true
}

/// Parallel up-to-date check for large builds
fn quick_uptodate_check_parallel(nodes_to_check: &[CheckNode], log: &BuildLog) -> bool {
    use rayon::prelude::*;

    // Collect all unique paths we need to stat
    let mut all_paths: Vec<&str> = Vec::with_capacity(nodes_to_check.len() * 3);
    for check in nodes_to_check {
        all_paths.push(check.output);
        for dep in &check.node.deps {
            all_paths.push(dep.as_str());
        }
        for dep in &check.depfile_deps {
            all_paths.push(dep.as_str());
        }
    }

    // Parallel stat() all paths at once
    let mtimes: HashMap<&str, Option<u64>> = all_paths
        .par_iter()
        .map(|&path| (path, get_mtime_ns(Path::new(path))))
        .collect();

    // Now check each node using cached mtimes
    for check in nodes_to_check {
        let output = check.output;
        let node = check.node;

        // Check output exists
        let output_mtime = match mtimes.get(output) {
            Some(Some(m)) => *m,
            _ => return false,
        };

        // Check command hash hasn't changed
        if node.command.is_some() {
            if let Some(logged_hash) = log.get_command_hash(output) {
                if logged_hash != node.command_hash {
                    return false;
                }
            }
        }

        // Check all manifest inputs are older than output
        for dep in &node.deps {
            if let Some(Some(dep_mtime)) = mtimes.get(dep.as_str()) {
                if *dep_mtime > output_mtime {
                    return false;
                }
            }
        }

        // Depfile dependencies must exist and be no newer than the output.
        for dep in &check.depfile_deps {
            match mtimes.get(dep.as_str()) {
                Some(Some(dep_mtime)) if *dep_mtime <= output_mtime => {}
                _ => return false,
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_mtime_cache() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "content").unwrap();

        let mut cache = MtimeCache::new();

        let mtime1 = cache.get(&file);
        assert!(mtime1.is_some());

        let mtime2 = cache.get(&file);
        assert_eq!(mtime1, mtime2);
    }

    #[test]
    fn test_build_log_roundtrip() {
        let dir = tempdir().unwrap();

        {
            let mut log = BuildLog::open(dir.path());
            log.record("output.o", 12345, 1000, 2000);
            log.save().unwrap();
        }

        {
            let log = BuildLog::open(dir.path());
            assert_eq!(log.get_command_hash("output.o"), Some(12345));
        }
    }

    #[test]
    fn ignores_old_build_log_versions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".ninja_log"),
            "# ninja log v5\n0\t1\t2\tout.o\t3039\n",
        )
        .unwrap();

        let log = BuildLog::open(dir.path());
        assert!(log.get_command_hash("out.o").is_none());
        assert!(log.get_mtime("out.o").is_none());
    }

    #[test]
    fn test_hash_command() {
        // Reference values from Ninja's rapidhash implementation.
        assert_eq!(hash_command(""), 0x5a6ef77074ebc84b);
        assert_eq!(hash_command("hello"), 0x1e5eabe786f5dbba);
        assert_eq!(hash_command("gcc -c foo.c -o foo.o"), 0x671fc54922edb6f9);
        assert_eq!(hash_command("cmd /C \"echo hi\""), 0x20458de11cc85e86);

        let h1 = hash_command("gcc -c foo.c -o foo.o");
        let h2 = hash_command("gcc -c foo.c -o foo.o");
        let h3 = hash_command("gcc -c bar.c -o bar.o");

        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }
}
