use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub type FileId = u64;
pub type Level = u8;

pub(crate) const NUM_LEVELS: usize = 7;

/// Returns the canonical path for an SSTable file with the given ID.
/// Zero-padded to 20 digits so lexicographic order matches numeric order.
pub fn sst_path(dir: &Path, file_id: FileId) -> PathBuf {
    dir.join(format!("{:020}.sst", file_id))
}

#[derive(Clone, Debug, PartialEq)]
pub struct SstableMetadata {
    pub file_id:      FileId,
    pub level:        Level,
    pub smallest_key: String,
    pub largest_key:  String,
}

#[derive(Clone, Debug)]
pub enum VersionEdit {
    AddFile {
        level:        Level,
        file_id:      FileId,
        smallest_key: String,
        largest_key:  String,
    },
    RemoveFile {
        level:   Level,
        file_id: FileId,
    },
}

// ---------------------------------------------------------------------------
// VersionState — in-memory snapshot of all live SSTable files
// ---------------------------------------------------------------------------

/// Copy-on-Write snapshot of every live SSTable, organised by level.
///
/// L0 files are stored in flush order (oldest first); L1+ files are kept
/// sorted by `smallest_key` so binary search across a level is always valid.
#[derive(Clone, Debug)]
pub struct VersionState {
    levels: Vec<Vec<SstableMetadata>>,
}

impl VersionState {
    pub fn new() -> Self {
        Self {
            levels: vec![Vec::new(); NUM_LEVELS],
        }
    }

    pub fn apply(&mut self, edit: &VersionEdit) {
        match edit {
            VersionEdit::AddFile { level, file_id, smallest_key, largest_key } => {
                let lvl = *level as usize;
                if lvl >= self.levels.len() {
                    self.levels.resize(lvl + 1, Vec::new());
                }
                let meta = SstableMetadata {
                    file_id:      *file_id,
                    level:        *level,
                    smallest_key: smallest_key.clone(),
                    largest_key:  largest_key.clone(),
                };
                if lvl == 0 {
                    self.levels[0].push(meta);
                } else {
                    let pos = self.levels[lvl]
                        .partition_point(|m| m.smallest_key < *smallest_key);
                    self.levels[lvl].insert(pos, meta);
                }
            }
            VersionEdit::RemoveFile { level, file_id } => {
                let lvl = *level as usize;
                if lvl < self.levels.len() {
                    self.levels[lvl].retain(|m| m.file_id != *file_id);
                }
            }
        }
    }

    pub fn files_at_level(&self, level: usize) -> &[SstableMetadata] {
        self.levels.get(level).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn all_file_ids(&self) -> impl Iterator<Item = FileId> + '_ {
        self.levels.iter().flatten().map(|m| m.file_id)
    }

    /// Files at `level` whose key range overlaps `[lo, hi]`.
    pub fn overlapping_files<'a>(
        &'a self,
        level: usize,
        lo: &str,
        hi: &str,
    ) -> Vec<&'a SstableMetadata> {
        self.files_at_level(level)
            .iter()
            .filter(|m| m.smallest_key.as_str() <= hi && m.largest_key.as_str() >= lo)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// SharedVersion — the CoW wrapper used by LsmEngine
// ---------------------------------------------------------------------------

/// Thread-safe CoW container for the current `VersionState`.
///
/// Readers clone the inner `Arc` under a brief read-lock; writers clone the
/// whole `VersionState`, mutate the copy, and swap the `Arc` under a
/// write-lock — giving readers an atomic, wait-free view.
pub struct SharedVersion(RwLock<Arc<VersionState>>);

impl SharedVersion {
    pub fn new() -> Self {
        Self(RwLock::new(Arc::new(VersionState::new())))
    }

    /// Returns a point-in-time snapshot. Callers can read freely without
    /// holding any lock.
    pub fn snapshot(&self) -> Arc<VersionState> {
        Arc::clone(&self.0.read().unwrap())
    }

    /// Atomically apply one or more edits to the version state.
    pub fn apply(&self, edits: &[VersionEdit]) {
        let mut guard = self.0.write().unwrap();
        let mut next = (**guard).clone();
        for edit in edits {
            next.apply(edit);
        }
        *guard = Arc::new(next);
    }
}
