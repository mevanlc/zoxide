use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use glob::Pattern;

use crate::cmd::{Run, Tidy};
use crate::db::{Database, Epoch, Rank};
use crate::error::BrokenPipeHandler;
use crate::util;

impl Run for Tidy {
    fn run(&self) -> Result<()> {
        let mut db = Database::open()?;
        let plan = self.plan(&db)?;

        let stderr = &mut io::stderr().lock();
        plan.write_warnings(stderr)?;
        let stdout = &mut io::stdout().lock();
        plan.write_report(stdout, self.dry_run)?;

        if !self.dry_run {
            plan.apply(&mut db);
        }
        db.save()
    }
}

#[derive(Debug)]
struct Entry {
    original_path: String,
    path: String,
    rank: Rank,
    last_accessed: Epoch,
    selected: bool,
    pruned: bool,
    normalized: bool,
}

#[derive(Debug)]
struct Probe {
    spelling: String,
    state: ProbeState,
}

#[derive(Debug)]
enum ProbeState {
    Live(FileId),
    Stale,
    Error(String),
}

#[derive(Debug)]
struct NormalizeReport {
    from: String,
    to: String,
}

#[derive(Debug)]
struct MergeReport {
    path: String,
    merged: Vec<String>,
    entries: usize,
}

#[derive(Debug)]
struct TidyPlan {
    entries: Vec<(String, Rank, Epoch)>,
    pruned: Vec<String>,
    normalized: Vec<NormalizeReport>,
    merges: Vec<MergeReport>,
    warnings: Vec<(String, String)>,
}

impl Tidy {
    fn dedupe_enabled(&self) -> bool {
        self.all || self.dedupe
    }

    fn normalize_enabled(&self) -> bool {
        self.all || self.normalize
    }

    fn prune_enabled(&self) -> bool {
        self.all || self.prune
    }

    fn plan(&self, db: &Database) -> Result<TidyPlan> {
        let patterns = self
            .pathglobs
            .iter()
            .map(|glob| Pattern::new(glob).with_context(|| format!("invalid glob: {glob}")))
            .collect::<Result<Vec<_>>>()?;

        let mut entries = db
            .dirs()
            .iter()
            .map(|dir| {
                let path = dir.path.to_string();
                let selected = patterns.iter().any(|pattern| pattern.matches(&path));
                Entry {
                    original_path: path.clone(),
                    path,
                    rank: dir.rank,
                    last_accessed: dir.last_accessed,
                    selected,
                    pruned: false,
                    normalized: false,
                }
            })
            .collect::<Vec<_>>();

        let mut needs_probe = vec![false; entries.len()];
        if self.prune_enabled() || self.normalize_enabled() {
            for (idx, entry) in entries.iter().enumerate() {
                needs_probe[idx] = entry.selected;
            }
        }
        if self.dedupe_enabled() && !self.assume_insensitive {
            let mut proposed = HashMap::<String, Vec<usize>>::new();
            for (idx, entry) in entries.iter().enumerate().filter(|(_, entry)| entry.selected) {
                proposed.entry(util::fold_key(&entry.path)).or_default().push(idx);
            }
            for group in proposed.into_values().filter(|group| group.len() > 1) {
                for idx in group {
                    needs_probe[idx] = true;
                }
            }
        }

        let probes = entries
            .iter()
            .zip(needs_probe)
            .map(|(entry, needed)| needed.then(|| probe_path(&entry.path)))
            .collect::<Vec<_>>();

        let mut warnings = probes
            .iter()
            .enumerate()
            .filter_map(|(idx, probe)| match probe.as_ref().map(|probe| &probe.state) {
                Some(ProbeState::Error(error)) => {
                    Some((entries[idx].original_path.clone(), error.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        warnings.sort_unstable();
        warnings.dedup();

        let mut pruned = Vec::new();
        if self.prune_enabled() {
            for (idx, entry) in entries.iter_mut().enumerate().filter(|(_, entry)| entry.selected) {
                if matches!(probes[idx].as_ref().map(|probe| &probe.state), Some(ProbeState::Stale))
                {
                    entry.pruned = true;
                    pruned.push(entry.original_path.clone());
                }
            }
        }
        pruned.sort_unstable();

        let mut normalized = Vec::new();
        if self.normalize_enabled() {
            for (idx, entry) in
                entries.iter_mut().enumerate().filter(|(_, entry)| entry.selected && !entry.pruned)
            {
                let Some(probe) = &probes[idx] else { continue };
                if matches!(probe.state, ProbeState::Error(_)) || probe.spelling == entry.path {
                    continue;
                }
                normalized.push(NormalizeReport {
                    from: entry.original_path.clone(),
                    to: probe.spelling.clone(),
                });
                entry.path.clone_from(&probe.spelling);
                entry.normalized = true;
            }
        }
        normalized.sort_unstable_by(|report1, report2| {
            (&report1.from, &report1.to).cmp(&(&report2.from, &report2.to))
        });

        let mut unions = UnionFind::new(entries.len());

        // Normalization must not create byte-identical rows. Exact collisions
        // are consolidated even when another member lies outside the glob.
        if self.normalize_enabled() {
            let mut exact = HashMap::<&str, Vec<usize>>::new();
            for (idx, entry) in entries.iter().enumerate().filter(|(_, entry)| !entry.pruned) {
                exact.entry(&entry.path).or_default().push(idx);
            }
            for group in exact
                .into_values()
                .filter(|group| group.len() > 1 && group.iter().any(|&idx| entries[idx].normalized))
            {
                union_group(&mut unions, &group);
            }
        }

        if self.dedupe_enabled() {
            let exact_groups = collect_groups(&mut unions, &entries);
            let mut proposed = HashMap::<String, Vec<usize>>::new();
            for group in exact_groups.iter().filter(|group| {
                group.iter().any(|&idx| entries[idx].selected) && !entries[group[0]].pruned
            }) {
                proposed.entry(util::fold_key(&entries[group[0]].path)).or_default().push(group[0]);
            }

            for group in proposed.into_values().filter(|group| group.len() > 1) {
                if self.assume_insensitive {
                    union_group(&mut unions, &group);
                    continue;
                }

                let mut by_identity = HashMap::<&FileId, Vec<usize>>::new();
                for root in group {
                    let members = &exact_groups[unions.find(root)];
                    if let Some(id) = group_file_id(members, &probes) {
                        by_identity.entry(id).or_default().push(root);
                    }
                }
                for same_file in by_identity.into_values().filter(|group| group.len() > 1) {
                    union_group(&mut unions, &same_file);
                }
            }
        }

        let groups = collect_groups(&mut unions, &entries);
        let mut output = vec![None; entries.len()];
        let mut merges = Vec::new();

        for group in groups.into_iter().filter(|group| !group.is_empty()) {
            let mut survivor = group[0];
            for &idx in &group[1..] {
                if entries[idx].rank > entries[survivor].rank {
                    survivor = idx;
                }
            }

            let mut path = entries[survivor].path.clone();
            if self.dedupe_enabled()
                && !self.assume_insensitive
                && group.len() > 1
                && let Some(spelling) = group_spelling(survivor, &group, &probes)
            {
                path = spelling.to_owned();
            }

            let rank = group.iter().map(|&idx| entries[idx].rank).sum();
            let last_accessed = group
                .iter()
                .map(|&idx| entries[idx].last_accessed)
                .max()
                .expect("group is non-empty");
            output[survivor] = Some((path.clone(), rank, last_accessed));

            if group.len() > 1 {
                let mut merged = group
                    .iter()
                    .copied()
                    .map(|idx| entries[idx].original_path.clone())
                    .filter(|spelling| *spelling != path)
                    .collect::<Vec<_>>();
                merged.sort_unstable();
                merges.push(MergeReport { path, merged, entries: group.len() });
            }
        }
        merges.sort_unstable_by(|merge1, merge2| merge1.path.cmp(&merge2.path));

        Ok(TidyPlan {
            entries: output.into_iter().flatten().collect(),
            pruned,
            normalized,
            merges,
            warnings,
        })
    }
}

impl TidyPlan {
    fn apply(self, db: &mut Database) {
        db.replace_dirs(self.entries);
    }

    fn write_warnings(&self, stderr: &mut impl Write) -> Result<()> {
        for (path, error) in &self.warnings {
            writeln!(stderr, "zoxide: warning: could not inspect {path}: {error}")
                .pipe_exit("stderr")?;
        }
        Ok(())
    }

    fn write_report(&self, stdout: &mut impl Write, dry_run: bool) -> Result<()> {
        let prune_verb = if dry_run { "would prune" } else { "pruned" };
        let normalize_verb = if dry_run { "would normalize" } else { "normalized" };
        let merge_verb = if dry_run { "would merge" } else { "merged" };

        for path in &self.pruned {
            writeln!(stdout, "{prune_verb} {path}").pipe_exit("stdout")?;
        }
        for report in &self.normalized {
            writeln!(stdout, "{normalize_verb} {} -> {}", report.from, report.to)
                .pipe_exit("stdout")?;
        }
        for merge in &self.merges {
            writeln!(stdout, "{merge_verb} into {}:", merge.path).pipe_exit("stdout")?;
            for path in &merge.merged {
                writeln!(stdout, "  {path}").pipe_exit("stdout")?;
            }
        }

        if self.pruned.is_empty() && self.normalized.is_empty() && self.merges.is_empty() {
            writeln!(stdout, "no changes needed").pipe_exit("stdout")?;
            return Ok(());
        }

        if !self.pruned.is_empty() {
            writeln!(
                stdout,
                "{prune_verb} {} {}",
                self.pruned.len(),
                plural(self.pruned.len(), "entry", "entries")
            )
            .pipe_exit("stdout")?;
        }
        if !self.normalized.is_empty() {
            writeln!(
                stdout,
                "{normalize_verb} {} {}",
                self.normalized.len(),
                plural(self.normalized.len(), "entry", "entries")
            )
            .pipe_exit("stdout")?;
        }
        if !self.merges.is_empty() {
            let entries = self.merges.iter().map(|merge| merge.entries).sum::<usize>();
            writeln!(
                stdout,
                "{merge_verb} {entries} {} into {} {}",
                plural(entries, "entry", "entries"),
                self.merges.len(),
                plural(self.merges.len(), "directory", "directories")
            )
            .pipe_exit("stdout")?;
        }
        Ok(())
    }
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn group_file_id<'a>(group: &[usize], probes: &'a [Option<Probe>]) -> Option<&'a FileId> {
    group.iter().find_map(|&idx| match probes[idx].as_ref().map(|probe| &probe.state) {
        Some(ProbeState::Live(id)) => Some(id),
        _ => None,
    })
}

fn group_spelling<'a>(
    survivor: usize,
    group: &[usize],
    probes: &'a [Option<Probe>],
) -> Option<&'a str> {
    std::iter::once(survivor).chain(group.iter().copied().filter(|&idx| idx != survivor)).find_map(
        |idx| match probes[idx].as_ref() {
            Some(Probe { spelling, state: ProbeState::Live(_) }) => Some(spelling.as_str()),
            _ => None,
        },
    )
}

fn probe_path(path: &str) -> Probe {
    match recover_spelling(Path::new(path)) {
        Ok((spelling, true)) => Probe { spelling, state: ProbeState::Stale },
        Ok((spelling, false)) => match fs::metadata(&spelling) {
            Ok(metadata) if metadata.is_dir() => match file_id(Path::new(&spelling)) {
                Ok(id) => Probe { spelling, state: ProbeState::Live(id) },
                Err(error) => {
                    Probe { spelling: path.to_owned(), state: ProbeState::Error(error.to_string()) }
                }
            },
            Ok(_) => Probe { spelling, state: ProbeState::Stale },
            Err(error) if is_missing(&error) => Probe { spelling, state: ProbeState::Stale },
            Err(error) => {
                Probe { spelling: path.to_owned(), state: ProbeState::Error(error.to_string()) }
            }
        },
        Err(error) => {
            Probe { spelling: path.to_owned(), state: ProbeState::Error(error.to_string()) }
        }
    }
}

/// Recovers every resolvable component's directory-entry spelling. The bool is
/// true when a component was definitively missing or not a directory, in which
/// case the untouched suffix is retained.
fn recover_spelling(path: &Path) -> io::Result<(String, bool)> {
    let mut corrected = PathBuf::new();
    let mut stale = false;

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => corrected.push(component.as_os_str()),
            Component::CurDir | Component::ParentDir => corrected.push(component.as_os_str()),
            Component::Normal(name) if stale => corrected.push(name),
            Component::Normal(name) => match true_component(&corrected, name) {
                Ok(actual) => corrected.push(actual),
                Err(error) if is_missing(&error) => {
                    corrected.push(name);
                    stale = true;
                }
                Err(error) => return Err(error),
            },
        }
    }

    let spelling = corrected
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "on-disk path is not UTF-8"))?
        .to_owned();
    if util::fold_key(&spelling) != util::fold_key(path.to_string_lossy()) {
        return Err(io::Error::other("on-disk spelling is not fold-equivalent to stored path"));
    }
    Ok((spelling, stale))
}

fn true_component(parent: &Path, name: &OsStr) -> io::Result<OsString> {
    let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
    let requested = parent.join(name);
    fs::symlink_metadata(&requested)?;

    let mut folded = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let actual = entry.file_name();
        if actual == name {
            return Ok(actual);
        }
        if let (Some(actual), Some(requested)) = (actual.to_str(), name.to_str())
            && util::fold_key(actual) == util::fold_key(requested)
        {
            folded.push(entry);
        }
    }

    let requested_id = file_id(&requested)?;
    for entry in folded {
        if file_id(&entry.path()).is_ok_and(|id| id == requested_id) {
            return Ok(entry.file_name());
        }
    }

    Err(io::Error::other(format!(
        "could not recover the on-disk spelling of {}",
        requested.display()
    )))
}

fn is_missing(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

/// A filesystem identity shared only by paths naming the same directory entry
/// under the current mount. Unix does not follow the final symlink.
#[cfg(unix)]
#[derive(Debug, Eq, Hash, PartialEq)]
struct FileId(u64, u64);

#[cfg(unix)]
fn file_id(path: &Path) -> io::Result<FileId> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)?;
    Ok(FileId(metadata.dev(), metadata.ino()))
}

/// Windows handles follow symlinks; this retains the previous dedupe behavior.
#[cfg(windows)]
type FileId = same_file::Handle;

#[cfg(windows)]
fn file_id(path: &Path) -> io::Result<FileId> {
    same_file::Handle::from_path(path)
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Eq, Hash, PartialEq)]
struct FileId;

#[cfg(not(any(unix, windows)))]
fn file_id(_path: &Path) -> io::Result<FileId> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem identity is not supported on this platform",
    ))
}

#[derive(Debug)]
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self { parent: (0..len).collect() }
    }

    fn find(&mut self, idx: usize) -> usize {
        let parent = self.parent[idx];
        if parent != idx {
            self.parent[idx] = self.find(parent);
        }
        self.parent[idx]
    }

    fn union(&mut self, idx1: usize, idx2: usize) {
        let root1 = self.find(idx1);
        let root2 = self.find(idx2);
        if root1 != root2 {
            let (root, child) = if root1 < root2 { (root1, root2) } else { (root2, root1) };
            self.parent[child] = root;
        }
    }
}

fn union_group(unions: &mut UnionFind, group: &[usize]) {
    if let Some((&first, rest)) = group.split_first() {
        for &idx in rest {
            unions.union(first, idx);
        }
    }
}

/// Returns a vector indexed by union root. Pruned entries have no group.
fn collect_groups(unions: &mut UnionFind, entries: &[Entry]) -> Vec<Vec<usize>> {
    let mut groups = vec![Vec::new(); entries.len()];
    for (idx, _) in entries.iter().enumerate().filter(|(_, entry)| !entry.pruned) {
        let root = unions.find(idx);
        groups[root].push(idx);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tidy(
        db: &mut Database,
        dedupe: bool,
        normalize: bool,
        prune: bool,
        assume_insensitive: bool,
        dry_run: bool,
        pathglobs: &[&str],
    ) -> (String, String) {
        let command = Tidy {
            pathglobs: pathglobs.iter().map(|glob| glob.to_string()).collect(),
            dedupe,
            normalize,
            prune,
            all: false,
            assume_insensitive,
            dry_run,
        };
        let plan = command.plan(db).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        plan.write_warnings(&mut stderr).unwrap();
        plan.write_report(&mut stdout, dry_run).unwrap();
        if !dry_run {
            plan.apply(db);
        }
        (String::from_utf8(stdout).unwrap(), String::from_utf8(stderr).unwrap())
    }

    #[test]
    fn star_glob_crosses_separators() {
        assert!(Pattern::new("*").unwrap().matches("/foo/bar"));
    }

    #[test]
    fn all_enables_every_action() {
        let command = Tidy {
            pathglobs: vec!["*".to_owned()],
            dedupe: false,
            normalize: false,
            prune: false,
            all: true,
            assume_insensitive: false,
            dry_run: false,
        };

        assert!(command.dedupe_enabled());
        assert!(command.normalize_enabled());
        assert!(command.prune_enabled());
    }

    #[test]
    fn assume_insensitive_merges_variants() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/PROJECTS", 2.0, 100);
        db.add_unchecked("/foo/projects", 5.0, 50);
        db.add_unchecked("/foo/other", 1.0, 10);

        tidy(&mut db, true, false, false, true, false, &["*"]);

        assert_eq!(db.dirs().len(), 2);
        let dir = db.dirs().iter().find(|dir| dir.path == "/foo/projects").unwrap();
        assert!((dir.rank - 7.0).abs() < 0.01);
        assert_eq!(dir.last_accessed, 100);
    }

    #[test]
    fn glob_filters_selection() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/AA", 1.0, 100);
        db.add_unchecked("/foo/aa", 2.0, 200);

        tidy(&mut db, true, false, false, true, false, &["/bar/*"]);

        assert_eq!(db.dirs().len(), 2);
    }

    #[test]
    fn dry_run_is_readonly() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/AA", 1.0, 100);
        db.add_unchecked("/foo/aa", 2.0, 200);
        db.save().unwrap();

        let (stdout, _) = tidy(&mut db, true, false, false, true, true, &["*"]);

        assert!(stdout.contains("would merge 2 entries into 1 directory"));
        assert!(!db.dirty());
        assert_eq!(db.dirs().len(), 2);
    }

    #[test]
    fn idempotent() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked("/foo/AA", 1.0, 100);
        db.add_unchecked("/foo/aa", 2.0, 200);

        tidy(&mut db, true, false, false, true, false, &["*"]);
        db.save().unwrap();
        let (stdout, _) = tidy(&mut db, true, false, false, true, false, &["*"]);

        assert_eq!(stdout, "no changes needed\n");
        assert!(!db.dirty());
    }

    #[test]
    fn default_dedupe_skips_dead_entries() {
        let data_dir = tempfile::tempdir().unwrap();
        let root = data_dir.path().to_str().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(format!("{root}/gone-AA"), 1.0, 100);
        db.add_unchecked(format!("{root}/gone-aa"), 2.0, 200);

        tidy(&mut db, true, false, false, false, false, &["*"]);

        assert_eq!(db.dirs().len(), 2);
    }

    #[test]
    fn prune_removes_non_navigable_entries() {
        let data_dir = tempfile::tempdir().unwrap();
        let live = data_dir.path().join("live");
        let file = data_dir.path().join("file");
        let missing = data_dir.path().join("missing");
        fs::create_dir(&live).unwrap();
        fs::write(&file, "not a directory").unwrap();

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(live.to_string_lossy(), 1.0, 10);
        db.add_unchecked(file.to_string_lossy(), 2.0, 20);
        db.add_unchecked(missing.to_string_lossy(), 4.0, 30);

        let (stdout, _) = tidy(&mut db, false, false, true, false, false, &["*"]);

        assert_eq!(db.dirs().len(), 1);
        assert_eq!(db.dirs()[0].path, live.to_string_lossy());
        assert!(stdout.contains("pruned 2 entries"));
    }

    #[cfg(unix)]
    #[test]
    fn prune_keeps_live_directory_symlink_and_removes_dangling_one() {
        use std::os::unix::fs::symlink;

        let data_dir = tempfile::tempdir().unwrap();
        let target = data_dir.path().join("target");
        let live = data_dir.path().join("live-link");
        let dangling = data_dir.path().join("dangling-link");
        fs::create_dir(&target).unwrap();
        symlink(&target, &live).unwrap();
        symlink(data_dir.path().join("missing"), &dangling).unwrap();

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(live.to_string_lossy(), 1.0, 10);
        db.add_unchecked(dangling.to_string_lossy(), 2.0, 20);

        tidy(&mut db, false, false, true, false, false, &["*"]);

        assert_eq!(db.dirs().len(), 1);
        assert_eq!(db.dirs()[0].path, live.to_string_lossy());
    }

    #[cfg(unix)]
    #[test]
    fn normalization_preserves_symlink_alias() {
        use std::os::unix::fs::symlink;

        let data_dir = tempfile::tempdir().unwrap();
        let target = data_dir.path().join("target");
        let link = data_dir.path().join("link");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();

        let probe = probe_path(link.to_str().unwrap());

        assert!(matches!(probe.state, ProbeState::Live(_)));
        assert_eq!(probe.spelling, link.to_string_lossy());
    }

    #[test]
    fn normalization_corrects_every_resolvable_component() {
        let data_dir = tempfile::tempdir().unwrap();
        let parent = data_dir.path().join("TIDY-PARENT");
        let child = parent.join("TIDY-CHILD");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&child).unwrap();
        let alternate = data_dir.path().join("tidy-parent").join("tidy-child");

        // The behavior is meaningful only when this mount resolves alternate
        // spellings. Case-sensitive CI still exercises the traversal elsewhere.
        if fs::metadata(&alternate).is_err() {
            return;
        }

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(alternate.to_string_lossy(), 1.0, 10);
        tidy(&mut db, false, true, false, false, false, &["*"]);

        assert_eq!(db.dirs()[0].path, child.to_string_lossy());
    }

    #[test]
    fn normalization_corrects_extant_prefix_of_dead_path() {
        let data_dir = tempfile::tempdir().unwrap();
        let parent = data_dir.path().join("TIDY-PARENT");
        fs::create_dir(&parent).unwrap();
        let alternate = data_dir.path().join("tidy-parent").join("missing");
        if fs::symlink_metadata(data_dir.path().join("tidy-parent")).is_err() {
            return;
        }

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(alternate.to_string_lossy(), 1.0, 10);
        tidy(&mut db, false, true, false, false, false, &["*"]);

        assert_eq!(db.dirs()[0].path, parent.join("missing").to_string_lossy());
    }

    #[test]
    fn normalization_merges_exact_collision_across_glob_boundary() {
        let data_dir = tempfile::tempdir().unwrap();
        let actual = data_dir.path().join("TIDY-PATH");
        let alternate = data_dir.path().join("tidy-path");
        fs::create_dir(&actual).unwrap();
        if fs::metadata(&alternate).is_err() {
            return;
        }

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(alternate.to_string_lossy(), 2.0, 100);
        db.add_unchecked(actual.to_string_lossy(), 5.0, 200);
        tidy(&mut db, false, true, false, false, false, &[alternate.to_str().unwrap()]);

        assert_eq!(db.dirs().len(), 1);
        assert_eq!(db.dirs()[0].path, actual.to_string_lossy());
        assert!((db.dirs()[0].rank - 7.0).abs() < 0.01);
        assert_eq!(db.dirs()[0].last_accessed, 200);
    }

    #[test]
    fn normalization_does_not_merge_preexisting_exact_duplicates() {
        let data_dir = tempfile::tempdir().unwrap();
        let path = data_dir.path().join("already-normalized");
        fs::create_dir(&path).unwrap();

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(path.to_string_lossy(), 2.0, 100);
        db.add_unchecked(path.to_string_lossy(), 5.0, 200);
        let (stdout, _) = tidy(&mut db, false, true, false, false, false, &["*"]);

        assert_eq!(db.dirs().len(), 2);
        assert_eq!(stdout, "no changes needed\n");
    }

    #[test]
    fn default_dedupe_uses_filesystem_identity_and_spelling() {
        let data_dir = tempfile::tempdir().unwrap();
        let upper = data_dir.path().join("DEDUPE-CASE");
        let lower = data_dir.path().join("dedupe-case");
        fs::create_dir(&upper).unwrap();

        let mut db = Database::open_dir(data_dir.path()).unwrap();
        db.add_unchecked(upper.to_string_lossy(), 2.0, 100);
        db.add_unchecked(lower.to_string_lossy(), 5.0, 200);

        let folded = file_id(&lower).is_ok_and(|id| file_id(&upper).is_ok_and(|other| id == other));
        if folded {
            let (stdout, _) = tidy(&mut db, true, false, false, false, false, &["*"]);
            assert_eq!(db.dirs().len(), 1);
            assert_eq!(db.dirs()[0].path, upper.to_string_lossy());
            assert!((db.dirs()[0].rank - 7.0).abs() < 0.01);
            assert_eq!(db.dirs()[0].last_accessed, 200);
            assert!(stdout.contains(lower.to_str().unwrap()));
        } else {
            fs::create_dir(&lower).unwrap();
            tidy(&mut db, true, false, false, false, false, &["*"]);
            assert_eq!(db.dirs().len(), 2);
        }
    }

    #[test]
    fn unexpected_probe_error_warns_and_preserves_entry() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut db = Database::open_dir(data_dir.path()).unwrap();
        let path = format!("{}/bad\0path", data_dir.path().display());
        db.add_unchecked(path.as_str(), 1.0, 10);

        let (_, stderr) = tidy(&mut db, false, true, true, false, false, &["*"]);

        assert_eq!(db.dirs().len(), 1);
        assert_eq!(db.dirs()[0].path, path);
        assert!(stderr.contains("zoxide: warning: could not inspect"));
    }
}
