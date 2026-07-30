mod dir;
mod stream;

use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context, Result, bail};
use bincode::Options;
use ouroboros::self_referencing;

pub use crate::db::dir::{Dir, Epoch, Rank};
pub use crate::db::stream::{Stream, StreamOptions};
use crate::{config, util};

#[self_referencing]
pub struct Database {
    path: PathBuf,
    bytes: Vec<u8>,
    #[borrows(bytes)]
    #[covariant]
    pub dirs: Vec<Dir<'this>>,
    dirty: bool,
}

impl Database {
    const VERSION: u32 = 3;

    pub fn open() -> Result<Self> {
        let data_dir = config::data_dir()?;
        Self::open_dir(data_dir)
    }

    pub fn open_dir(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref();
        let path = data_dir.join("db.zo");
        let path = fs::canonicalize(&path).unwrap_or(path);

        match fs::read(&path) {
            Ok(bytes) => Self::try_new(path, bytes, |bytes| Self::deserialize(bytes), false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Create data directory, but don't create any file yet. The file will be
                // created later by [`Database::save`] if any data is modified.
                fs::create_dir_all(data_dir).with_context(|| {
                    format!("unable to create data directory: {}", data_dir.display())
                })?;
                Ok(Self::new(path, Vec::new(), |_| Vec::new(), false))
            }
            Err(e) => {
                Err(e).with_context(|| format!("could not read from database: {}", path.display()))
            }
        }
    }

    pub fn save(&mut self) -> Result<()> {
        // Only write to disk if the database is modified.
        if !self.dirty() {
            return Ok(());
        }

        let bytes = Self::serialize(self.dirs())?;
        util::write(self.borrow_path(), bytes).context("could not write to database")?;
        self.with_dirty_mut(|dirty| *dirty = false);

        Ok(())
    }

    /// Increments the rank of a directory, or creates it if it does not exist.
    pub fn add(&mut self, path: impl AsRef<str> + Into<String>, by: Rank, now: Epoch) {
        self.with_dirs_mut(|dirs| match dirs.iter_mut().find(|dir| dir.path == path.as_ref()) {
            Some(dir) => dir.rank = (dir.rank + by).max(0.0),
            None => {
                dirs.push(Dir { path: path.into().into(), rank: by.max(0.0), last_accessed: now })
            }
        });
        self.with_dirty_mut(|dirty| *dirty = true);
    }

    /// Creates a new directory. This will create a duplicate entry if this
    /// directory is already in the database, it is expected that the user
    /// either does a check before calling this, or calls `dedup()`
    /// afterward.
    pub fn add_unchecked(&mut self, path: impl AsRef<str> + Into<String>, rank: Rank, now: Epoch) {
        self.with_dirs_mut(|dirs| {
            dirs.push(Dir { path: path.into().into(), rank, last_accessed: now })
        });
        self.with_dirty_mut(|dirty| *dirty = true);
    }

    /// Increments the rank and updates the last_accessed of a directory, or
    /// creates it if it does not exist.
    pub fn add_update(&mut self, path: impl AsRef<str> + Into<String>, by: Rank, now: Epoch) {
        self.with_dirs_mut(|dirs| match dirs.iter_mut().find(|dir| dir.path == path.as_ref()) {
            Some(dir) => {
                dir.rank = (dir.rank + by).max(0.0);
                dir.last_accessed = now;
            }
            None => {
                dirs.push(Dir { path: path.into().into(), rank: by.max(0.0), last_accessed: now })
            }
        });
        self.with_dirty_mut(|dirty| *dirty = true);
    }

    /// Removes the directory with `path` from the store. This does not preserve
    /// ordering, but is O(1).
    pub fn remove(&mut self, path: impl AsRef<str>) -> bool {
        match self.dirs().iter().position(|dir| dir.path == path.as_ref()) {
            Some(idx) => {
                self.swap_remove(idx);
                true
            }
            None => false,
        }
    }

    pub fn swap_remove(&mut self, idx: usize) {
        self.with_dirs_mut(|dirs| dirs.swap_remove(idx));
        self.with_dirty_mut(|dirty| *dirty = true);
    }

    pub fn age(&mut self, max_age: Rank) {
        let mut dirty = false;
        self.with_dirs_mut(|dirs| {
            let total_age = dirs.iter().map(|dir| dir.rank).sum::<Rank>();
            if total_age > max_age {
                let factor = 0.9 * max_age / total_age;
                for idx in (0..dirs.len()).rev() {
                    let dir = &mut dirs[idx];
                    dir.rank *= factor;
                    if dir.rank < 1.0 {
                        dirs.swap_remove(idx);
                    }
                }
                dirty = true;
            }
        });
        self.with_dirty_mut(|dirty_prev| *dirty_prev |= dirty);
    }

    pub fn dedup(&mut self) {
        // Sort by path, so that equal paths are next to each other.
        self.sort_by_path();

        // Collect runs of byte-equal paths, then merge each run into its
        // first entry.
        let mut groups = Vec::new();
        let dirs = self.dirs();
        let mut idx = 0;
        while idx < dirs.len() {
            let mut end = idx + 1;
            while end < dirs.len() && dirs[end].path == dirs[idx].path {
                end += 1;
            }
            if end - idx > 1 {
                groups.push((idx, (idx + 1..end).collect()));
            }
            idx = end;
        }
        self.merge_entries(&groups);
    }

    /// Merges groups of entries. For each `(survivor_idx, victim_idxs)`
    /// group, the survivor's rank becomes the group's summed rank and its
    /// `last_accessed` the group's max; the victims are then removed. All
    /// indices refer to the database before any removal, and an entry may
    /// appear in at most one group.
    pub fn merge_entries(&mut self, groups: &[(usize, Vec<usize>)]) {
        if groups.iter().all(|(_, victim_idxs)| victim_idxs.is_empty()) {
            return;
        }

        self.with_dirs_mut(|dirs| {
            for (survivor_idx, victim_idxs) in groups {
                for &idx in victim_idxs {
                    let rank = dirs[idx].rank;
                    let last_accessed = dirs[idx].last_accessed;
                    let survivor = &mut dirs[*survivor_idx];
                    survivor.rank += rank;
                    survivor.last_accessed = survivor.last_accessed.max(last_accessed);
                }
            }

            // Removing in descending index order keeps the remaining victim
            // indices valid: `swap_remove` only disturbs positions at or
            // above the removed one.
            let mut victim_idxs = groups
                .iter()
                .flat_map(|(_, victim_idxs)| victim_idxs.iter().copied())
                .collect::<Vec<_>>();
            victim_idxs.sort_unstable_by(|idx1, idx2| idx2.cmp(idx1));
            for idx in victim_idxs {
                dirs.swap_remove(idx);
            }
        });
        self.with_dirty_mut(|dirty| *dirty = true);
    }

    /// Replaces the path of the entry at `idx`.
    pub fn set_path(&mut self, idx: usize, path: impl AsRef<str> + Into<String>) {
        if self.dirs()[idx].path == path.as_ref() {
            return;
        }
        self.with_dirs_mut(|dirs| dirs[idx].path = path.into().into());
        self.with_dirty_mut(|dirty| *dirty = true);
    }

    pub fn sort_by_path(&mut self) {
        self.with_dirs_mut(|dirs| dirs.sort_unstable_by(|dir1, dir2| dir1.path.cmp(&dir2.path)));
        self.with_dirty_mut(|dirty| *dirty = true);
    }

    pub fn sort_by_score(&mut self, now: Epoch) {
        self.with_dirs_mut(|dirs| {
            dirs.sort_unstable_by(|dir1: &Dir, dir2: &Dir| {
                dir1.score(now).total_cmp(&dir2.score(now))
            })
        });
        self.with_dirty_mut(|dirty| *dirty = true);
    }

    pub fn dirty(&self) -> bool {
        *self.borrow_dirty()
    }

    pub fn dirs(&self) -> &[Dir<'_>] {
        self.borrow_dirs()
    }

    fn serialize(dirs: &[Dir<'_>]) -> Result<Vec<u8>> {
        (|| -> bincode::Result<_> {
            // Preallocate buffer with combined size of sections.
            let buffer_size =
                bincode::serialized_size(&Self::VERSION)? + bincode::serialized_size(&dirs)?;
            let mut buffer = Vec::with_capacity(buffer_size as usize);

            // Serialize sections into buffer.
            bincode::serialize_into(&mut buffer, &Self::VERSION)?;
            bincode::serialize_into(&mut buffer, &dirs)?;

            Ok(buffer)
        })()
        .context("could not serialize database")
    }

    fn deserialize(bytes: &[u8]) -> Result<Vec<Dir<'_>>> {
        // Assume a maximum size for the database. This prevents bincode from throwing
        // strange errors when it encounters invalid data.
        const MAX_SIZE: u64 = 32 << 20; // 32 MiB
        let deserializer = &mut bincode::options().with_fixint_encoding().with_limit(MAX_SIZE);

        // Split bytes into sections.
        let version_size = deserializer.serialized_size(&Self::VERSION).unwrap() as _;
        if bytes.len() < version_size {
            bail!("could not deserialize database: corrupted data");
        }
        let (bytes_version, bytes_dirs) = bytes.split_at(version_size);

        // Deserialize sections.
        let version = deserializer.deserialize(bytes_version)?;
        let dirs = match version {
            Self::VERSION => {
                deserializer.deserialize(bytes_dirs).context("could not deserialize database")?
            }
            version => {
                bail!("unsupported version (got {version}, supports {})", Self::VERSION)
            }
        };

        Ok(dirs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add() {
        let data_dir = tempfile::tempdir().unwrap();
        let path = if cfg!(windows) { r"C:\foo\bar" } else { "/foo/bar" };
        let now = 946684800;

        {
            let mut db = Database::open_dir(data_dir.path()).unwrap();
            db.add(path, 1.0, now);
            db.add(path, 1.0, now);
            db.save().unwrap();
        }

        {
            let db = Database::open_dir(data_dir.path()).unwrap();
            assert_eq!(db.dirs().len(), 1);

            let dir = &db.dirs()[0];
            assert_eq!(dir.path, path);
            assert!((dir.rank - 2.0).abs() < 0.01);
            assert_eq!(dir.last_accessed, now);
        }
    }

    #[test]
    fn dedup() {
        let data_dir = tempfile::tempdir().unwrap();
        let (path1, path2) =
            if cfg!(windows) { (r"C:\foo\bar", r"C:\foo\baz") } else { ("/foo/bar", "/foo/baz") };

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(path1, 1.0, 100);
        db.add_unchecked(path2, 2.0, 300);
        db.add_unchecked(path1, 4.0, 200);
        db.add_unchecked(path1, 8.0, 50);
        db.dedup();

        assert_eq!(db.dirs().len(), 2);
        let dir1 = db.dirs().iter().find(|dir| dir.path == path1).unwrap();
        assert!((dir1.rank - 13.0).abs() < 0.01);
        assert_eq!(dir1.last_accessed, 200);
        let dir2 = db.dirs().iter().find(|dir| dir.path == path2).unwrap();
        assert!((dir2.rank - 2.0).abs() < 0.01);
        assert_eq!(dir2.last_accessed, 300);
    }

    #[test]
    fn merge_entries() {
        let data_dir = tempfile::tempdir().unwrap();

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/a", 1.0, 100);
        db.add_unchecked("/foo/b", 2.0, 400);
        db.add_unchecked("/foo/c", 4.0, 200);
        db.add_unchecked("/foo/d", 8.0, 300);

        // Merge b and d into a; c untouched.
        db.merge_entries(&[(0, vec![1, 3])]);

        assert_eq!(db.dirs().len(), 2);
        let dir_a = db.dirs().iter().find(|dir| dir.path == "/foo/a").unwrap();
        assert!((dir_a.rank - 11.0).abs() < 0.01);
        assert_eq!(dir_a.last_accessed, 400);
        let dir_c = db.dirs().iter().find(|dir| dir.path == "/foo/c").unwrap();
        assert!((dir_c.rank - 4.0).abs() < 0.01);
        assert_eq!(dir_c.last_accessed, 200);
    }

    #[test]
    fn merge_entries_empty_stays_clean() {
        let data_dir = tempfile::tempdir().unwrap();

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.merge_entries(&[]);
        assert!(!db.dirty());
    }

    #[test]
    fn remove() {
        let data_dir = tempfile::tempdir().unwrap();
        let path = if cfg!(windows) { r"C:\foo\bar" } else { "/foo/bar" };
        let now = 946684800;

        {
            let mut db = Database::open_dir(data_dir.path()).unwrap();
            db.add(path, 1.0, now);
            db.save().unwrap();
        }

        {
            let mut db = Database::open_dir(data_dir.path()).unwrap();
            assert!(db.remove(path));
            db.save().unwrap();
        }

        {
            let mut db = Database::open_dir(data_dir.path()).unwrap();
            assert!(db.dirs().is_empty());
            assert!(!db.remove(path));
            db.save().unwrap();
        }
    }
}
