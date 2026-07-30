use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::{Context, Result};
use glob::Pattern;

use crate::cmd::{Dedupe, Run};
use crate::db::Database;
use crate::error::BrokenPipeHandler;
use crate::util;

impl Run for Dedupe {
    fn run(&self) -> Result<()> {
        let mut db = Database::open()?;
        self.dedupe(&mut db)?;
        db.save()
    }
}

struct Merge {
    survivor_idx: usize,
    victim_idxs: Vec<usize>,
    /// The spelling the merged entry will have.
    path: String,
    /// The spellings that will disappear from the database.
    merged: Vec<String>,
}

impl Dedupe {
    fn dedupe(&self, db: &mut Database) -> Result<()> {
        let patterns = self
            .pathglobs
            .iter()
            .map(|glob| Pattern::new(glob).with_context(|| format!("invalid glob: {glob}")))
            .collect::<Result<Vec<_>>>()?;

        // PROPOSE: group the selected entries by fold key. The key is
        // generous (full case fold + NFC), at least as broad as the broadest
        // real filesystem equivalence; the probe below corrects any
        // over-grouping.
        let mut proposed = HashMap::<String, Vec<usize>>::new();
        for (idx, dir) in db.dirs().iter().enumerate() {
            if patterns.iter().any(|pattern| pattern.matches(&dir.path)) {
                proposed.entry(util::fold_key(&dir.path)).or_default().push(idx);
            }
        }

        // CONFIRM: within each fold group, merge only the subsets that the
        // filesystem reports as the very same directory. This inherits the
        // mounted filesystem's exact equivalence relation, whatever it is,
        // and never merges distinct directories that happen to share a fold
        // key on a case-sensitive filesystem.
        let mut confirmed = Vec::new();
        for group in proposed.into_values() {
            if group.len() < 2 {
                continue;
            }
            if self.assume_insensitive {
                confirmed.push(group);
                continue;
            }
            // Entries that fail to stat are dead and are never merged in
            // this mode: the filesystem can no longer vouch for them.
            let mut by_identity = HashMap::<FileId, Vec<usize>>::new();
            for idx in group {
                if let Some(id) = file_id(&db.dirs()[idx].path) {
                    by_identity.entry(id).or_default().push(idx);
                }
            }
            confirmed.extend(by_identity.into_values().filter(|subset| subset.len() > 1));
        }

        // Pick each group's survivor and final spelling.
        let mut merges = Vec::new();
        for group in confirmed {
            let dirs = db.dirs();

            // The highest-rank member survives. Its spelling is the fallback
            // in case the on-disk spelling cannot be determined.
            let mut survivor_idx = group[0];
            for &idx in &group[1..] {
                if dirs[idx].rank > dirs[survivor_idx].rank {
                    survivor_idx = idx;
                }
            }

            let mut path = dirs[survivor_idx].path.to_string();
            if !self.assume_insensitive
                && let Some(spelling) = util::true_spelling(&path)
            {
                path = spelling;
            }

            let victim_idxs = group.iter().copied().filter(|&idx| idx != survivor_idx).collect();
            let mut merged = group
                .iter()
                .map(|&idx| dirs[idx].path.to_string())
                .filter(|spelling| *spelling != path)
                .collect::<Vec<_>>();
            merged.sort_unstable();
            merges.push(Merge { survivor_idx, victim_idxs, path, merged });
        }
        merges.sort_unstable_by(|merge1, merge2| merge1.path.cmp(&merge2.path));

        if !self.dry_run {
            // All indices refer to the pre-merge database: rewrite survivor
            // spellings before any entry is removed.
            for merge in &merges {
                db.set_path(merge.survivor_idx, merge.path.as_str());
            }
            let groups = merges
                .iter()
                .map(|merge| (merge.survivor_idx, merge.victim_idxs.clone()))
                .collect::<Vec<_>>();
            db.merge_entries(&groups);
        }

        let stdout = &mut io::stdout().lock();
        let verb = if self.dry_run { "would merge" } else { "merged" };
        for merge in &merges {
            writeln!(stdout, "{verb} into {}:", merge.path).pipe_exit("stdout")?;
            for spelling in &merge.merged {
                writeln!(stdout, "  {spelling}").pipe_exit("stdout")?;
            }
        }
        if merges.is_empty() {
            writeln!(stdout, "no duplicate entries found").pipe_exit("stdout")?;
        } else {
            let entries = merges.iter().map(|merge| merge.victim_idxs.len() + 1).sum::<usize>();
            let directories = if merges.len() == 1 { "directory" } else { "directories" };
            writeln!(stdout, "{verb} {entries} entries into {} {directories}", merges.len())
                .pipe_exit("stdout")?;
        }
        Ok(())
    }
}

/// A filesystem identity that two paths share iff they name the very same
/// object under the current mount. Symlinks are not followed, so two distinct
/// symlinks to one target remain distinct.
#[cfg(unix)]
type FileId = (u64, u64);

#[cfg(unix)]
fn file_id(path: &str) -> Option<FileId> {
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

/// A filesystem identity that two paths share iff they name the very same
/// object under the current mount. The handle follows symlinks; the failure
/// mode is a benign merge of two links that lead to the same directory.
#[cfg(windows)]
type FileId = same_file::Handle;

#[cfg(windows)]
fn file_id(path: &str) -> Option<FileId> {
    same_file::Handle::from_path(path).ok()
}

/// File identity cannot be determined on this platform: nothing is ever
/// confirmed, so the default mode merges nothing and --assume-insensitive is
/// required.
#[cfg(not(any(unix, windows)))]
type FileId = ();

#[cfg(not(any(unix, windows)))]
fn file_id(_path: &str) -> Option<FileId> {
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn dedupe(pathglobs: &[&str], assume_insensitive: bool, dry_run: bool, db: &mut Database) {
        let cmd = Dedupe {
            pathglobs: pathglobs.iter().map(|glob| glob.to_string()).collect(),
            assume_insensitive,
            dry_run,
        };
        cmd.dedupe(db).unwrap();
    }

    #[test]
    fn star_glob_crosses_separators() {
        // The whole-database selector relies on this glob crate behavior.
        assert!(Pattern::new("*").unwrap().matches("/foo/bar"));
    }

    #[test]
    fn assume_insensitive_merges_variants() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/PROJECTS", 2.0, 100);
        db.add_unchecked("/foo/projects", 5.0, 50);
        db.add_unchecked("/foo/other", 1.0, 10);
        dedupe(&["*"], true, false, &mut db);

        assert_eq!(db.dirs().len(), 2);
        // The highest-rank spelling survives with summed rank and max
        // last_accessed.
        let dir = db.dirs().iter().find(|dir| dir.path == "/foo/projects").unwrap();
        assert!((dir.rank - 7.0).abs() < 0.01);
        assert_eq!(dir.last_accessed, 100);
        assert!(db.dirs().iter().any(|dir| dir.path == "/foo/other"));
    }

    #[test]
    fn glob_filters_selection() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/AA", 1.0, 100);
        db.add_unchecked("/foo/aa", 2.0, 200);
        dedupe(&["/bar/*"], true, false, &mut db);
        assert_eq!(db.dirs().len(), 2);
    }

    #[test]
    fn dry_run_is_readonly() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/AA", 1.0, 100);
        db.add_unchecked("/foo/aa", 2.0, 200);
        db.save().unwrap();

        dedupe(&["*"], true, true, &mut db);
        assert!(!db.dirty());
        assert_eq!(db.dirs().len(), 2);
    }

    #[test]
    fn idempotent() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/AA", 1.0, 100);
        db.add_unchecked("/foo/aa", 2.0, 200);

        dedupe(&["*"], true, false, &mut db);
        assert_eq!(db.dirs().len(), 1);
        db.save().unwrap();

        dedupe(&["*"], true, false, &mut db);
        assert!(!db.dirty());
    }

    #[test]
    fn default_mode_skips_dead_entries() {
        let data_dir = tempfile::tempdir().unwrap();
        let root = data_dir.path().to_str().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(format!("{root}/gone-AA"), 1.0, 100);
        db.add_unchecked(format!("{root}/gone-aa"), 2.0, 200);
        dedupe(&["*"], false, false, &mut db);
        assert_eq!(db.dirs().len(), 2);
    }

    #[test]
    fn default_mode_uses_fs_identity() {
        let data_dir = tempfile::tempdir().unwrap();
        let root = data_dir.path().to_str().unwrap();
        let upper = format!("{root}/DEDUPE-CASE");
        let lower = format!("{root}/dedupe-case");
        fs::create_dir(&upper).unwrap();

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(upper.as_str(), 2.0, 100);
        db.add_unchecked(lower.as_str(), 5.0, 200);

        // Probe the filesystem's actual behavior instead of assuming
        // OS ⇒ behavior: macOS can host case-sensitive volumes and vice
        // versa.
        let folded = file_id(&lower).is_some_and(|id| Some(id) == file_id(&upper));
        if folded {
            dedupe(&["*"], false, false, &mut db);
            assert_eq!(db.dirs().len(), 1);
            let dir = &db.dirs()[0];
            assert_eq!(util::fold_key(&dir.path), util::fold_key(&upper));
            assert!((dir.rank - 7.0).abs() < 0.01);
            assert_eq!(dir.last_accessed, 200);
        } else {
            // Case-sensitive: both spellings exist as distinct directories
            // and must not be merged.
            fs::create_dir(&lower).unwrap();
            dedupe(&["*"], false, false, &mut db);
            assert_eq!(db.dirs().len(), 2);
        }
    }
}
