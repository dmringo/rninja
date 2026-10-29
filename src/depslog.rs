//! Reader/writer for Ninja's `.ninja_deps` dependency log.
//!
//! The format is a versioned stream of path records and dependency records.
//! Path records assign dense integer ids; dependency records reference those
//! ids and store the output's mtime so stale entries can be detected.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SIGNATURE: &[u8] = b"# ninjadeps\n";
const VERSION: i32 = 4;
const MAX_RECORD_SIZE: usize = (1 << 19) - 1;

/// Dependencies recorded for one output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepsEntry {
    /// Output mtime in Ninja's `TimeStamp` units.
    pub mtime: i64,
    /// Paths discovered by the compiler.
    pub deps: Vec<String>,
}

/// Ninja-compatible dependency log.
pub struct DepsLog {
    path: PathBuf,
    entries: HashMap<String, DepsEntry>,
    path_ids: HashMap<String, u32>,
    next_id: u32,
    file: Option<File>,
    valid: bool,
}

impl DepsLog {
    /// Open (or create lazily) `.ninja_deps` in `dir`.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(".ninja_deps");
        let mut log = Self {
            path,
            entries: HashMap::new(),
            path_ids: HashMap::new(),
            next_id: 0,
            file: None,
            valid: true,
        };

        if let Ok(bytes) = fs::read(&log.path) {
            if !log.parse(&bytes) {
                log.valid = false;
                log.entries.clear();
                log.path_ids.clear();
                log.next_id = 0;
            }
        }

        log
    }

    /// Get the recorded dependencies for an output, regardless of staleness.
    pub fn get(&self, output: &str) -> Option<&DepsEntry> {
        self.entries.get(output)
    }

    /// Get dependencies only if the recorded output mtime is current.
    pub fn get_if_current(&self, output: &str, current_mtime: i64) -> Option<&DepsEntry> {
        self.entries
            .get(output)
            .filter(|entry| entry.mtime == current_mtime)
    }

    /// Append a dependency record, creating path records as needed.
    pub fn record(&mut self, output: &str, mtime: i64, deps: &[String]) -> io::Result<()> {
        if let Some(existing) = self.entries.get(output) {
            if existing.mtime == mtime && existing.deps == deps {
                return Ok(());
            }
        }

        self.ensure_open()?;
        let output_id = self.ensure_id(output)?;
        let mut dep_ids = Vec::with_capacity(deps.len());
        for dep in deps {
            dep_ids.push(self.ensure_id(dep)?);
        }

        let size = 4u32 * (3 + dep_ids.len() as u32);
        if size as usize > MAX_RECORD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dependency record is too large",
            ));
        }

        let file = self.file.as_mut().unwrap();
        file.write_all(&(size | 0x8000_0000).to_le_bytes())?;
        file.write_all(&output_id.to_le_bytes())?;
        let mtime = mtime as u64;
        file.write_all(&(mtime as u32).to_le_bytes())?;
        file.write_all(&((mtime >> 32) as u32).to_le_bytes())?;
        for id in dep_ids {
            file.write_all(&id.to_le_bytes())?;
        }
        file.flush()?;

        self.entries.insert(
            output.to_string(),
            DepsEntry {
                mtime: mtime as i64,
                deps: deps.to_vec(),
            },
        );
        Ok(())
    }

    fn ensure_open(&mut self) -> io::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }

        let existing_len = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;

        if existing_len == 0 || !self.valid {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(SIGNATURE)?;
            file.write_all(&VERSION.to_le_bytes())?;
            file.flush()?;
            self.entries.clear();
            self.path_ids.clear();
            self.next_id = 0;
            self.valid = true;
        } else {
            file.seek(SeekFrom::End(0))?;
        }

        self.file = Some(file);
        Ok(())
    }

    fn ensure_id(&mut self, path: &str) -> io::Result<u32> {
        if let Some(&id) = self.path_ids.get(path) {
            return Ok(id);
        }

        let id = self.next_id;
        let bytes = path.as_bytes();
        let padding = (4 - bytes.len() % 4) % 4;
        let size = bytes.len() + padding + 4;
        if size > MAX_RECORD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dependency path is too long",
            ));
        }

        let file = self.file.as_mut().unwrap();
        file.write_all(&(size as u32).to_le_bytes())?;
        file.write_all(bytes)?;
        file.write_all(&vec![0u8; padding])?;
        file.write_all(&(!id).to_le_bytes())?;
        file.flush()?;

        self.path_ids.insert(path.to_string(), id);
        self.next_id += 1;
        Ok(id)
    }

    fn parse(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() < SIGNATURE.len() + 4 || &bytes[..SIGNATURE.len()] != SIGNATURE {
            return false;
        }

        let version = i32::from_le_bytes(
            bytes[SIGNATURE.len()..SIGNATURE.len() + 4]
                .try_into()
                .unwrap(),
        );
        if version != VERSION {
            return false;
        }

        let mut paths: Vec<String> = Vec::new();
        let mut path_ids = HashMap::new();
        let mut entries = HashMap::new();
        let mut offset = SIGNATURE.len() + 4;

        while offset + 4 <= bytes.len() {
            let raw_size = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let is_deps = raw_size & 0x8000_0000 != 0;
            let size = (raw_size & 0x7fff_ffff) as usize;

            if size == 0 || size > MAX_RECORD_SIZE || offset + size > bytes.len() {
                return false;
            }
            let payload = &bytes[offset..offset + size];
            offset += size;

            if is_deps {
                if size < 12 || !size.is_multiple_of(4) {
                    return false;
                }
                let output_id = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                if output_id >= paths.len() {
                    return false;
                }
                let low = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as u64;
                let high = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as u64;
                let mtime = ((high << 32) | low) as i64;

                let mut deps = Vec::with_capacity(size / 4 - 3);
                let (chunks, _) = payload[12..].as_chunks::<4>();
                for chunk in chunks {
                    let dep_id = u32::from_le_bytes(*chunk) as usize;
                    if dep_id >= paths.len() {
                        return false;
                    }
                    deps.push(paths[dep_id].clone());
                }

                entries.insert(paths[output_id].clone(), DepsEntry { mtime, deps });
            } else {
                if size < 5 {
                    return false;
                }
                let path_size = size - 4;
                let mut path_end = path_size;
                for _ in 0..3 {
                    if path_end > 0 && payload[path_end - 1] == 0 {
                        path_end -= 1;
                    }
                }
                if path_end == 0 {
                    return false;
                }

                let checksum = u32::from_le_bytes(payload[path_size..].try_into().unwrap());
                let expected_id = !checksum;
                let id = paths.len() as u32;
                if expected_id != id {
                    return false;
                }

                let path = String::from_utf8_lossy(&payload[..path_end]).into_owned();
                path_ids.insert(path.clone(), id);
                paths.push(path);
            }
        }

        if offset != bytes.len() {
            return false;
        }

        self.entries = entries;
        self.path_ids = path_ids;
        self.next_id = paths.len() as u32;
        true
    }
}

impl Drop for DepsLog {
    fn drop(&mut self) {
        if let Some(file) = &mut self.file {
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn round_trips_entries() {
        let dir = tempfile::tempdir().unwrap();
        let output = "obj/main.o";
        let header = "include/config.h";
        let source = "src/main.c";

        {
            let mut log = DepsLog::open(dir.path());
            log.record(output, 1234, &deps(&[header, source])).unwrap();
        }

        let log = DepsLog::open(dir.path());
        let entry = log.get(output).unwrap();
        assert_eq!(entry.mtime, 1234);
        assert_eq!(entry.deps, deps(&[header, source]));
        assert!(log.get_if_current(output, 1234).is_some());
        assert!(log.get_if_current(output, 9999).is_none());
    }

    #[test]
    fn updates_existing_entries() {
        let dir = tempfile::tempdir().unwrap();

        {
            let mut log = DepsLog::open(dir.path());
            log.record("out.o", 1, &deps(&["a.h"])).unwrap();
            log.record("out.o", 2, &deps(&["b.h"])).unwrap();
        }

        let log = DepsLog::open(dir.path());
        let entry = log.get("out.o").unwrap();
        assert_eq!(entry.mtime, 2);
        assert_eq!(entry.deps, deps(&["b.h"]));
    }

    #[test]
    fn writes_ninja_header_and_rewrites_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".ninja_deps"), b"not a deps log").unwrap();

        {
            let mut log = DepsLog::open(dir.path());
            log.record("out.o", 7, &deps(&["a.h"])).unwrap();
        }

        let bytes = fs::read(dir.path().join(".ninja_deps")).unwrap();
        assert_eq!(&bytes[..SIGNATURE.len()], SIGNATURE);
        assert_eq!(
            i32::from_le_bytes(
                bytes[SIGNATURE.len()..SIGNATURE.len() + 4]
                    .try_into()
                    .unwrap()
            ),
            VERSION
        );

        let log = DepsLog::open(dir.path());
        assert_eq!(log.get("out.o").unwrap().mtime, 7);
    }
}
