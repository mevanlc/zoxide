# PLAN: `zoxide tidy` — repair and clean up database paths

## Problem

The database can contain paths that are no longer navigable, paths whose stored
spelling differs from the filesystem, and several spellings of the same
directory whose independent ranks compete. These are related maintenance jobs,
but none should be an implicit side effect of normal queries.

## Interface

```text
Usage: zoxide tidy [OPTIONS] <--dedupe|--normalize|--prune|--all> [pathglob]...

Actions:
  -d, --dedupe
      --normalize
  -p, --prune
  -a, --all

Options:
  -i, --assume-insensitive
  -n, --dry-run
```

At least one action is required. `--all` selects all three actions and conflicts
with the individual selectors. `--assume-insensitive` requires deduplication.
Path globs default to `*`, cross separators, and match the original full stored
path.

## Semantics

The command completes every filesystem probe needed by the selected actions
before changing the database, plans changes in the fixed order prune, normalize,
then dedupe, and saves once.

### Prune

A path is stale when following its final symlink no longer produces a directory.
Missing paths, files, and dangling symlinks are removed; symlinks resolving to
directories remain. Permission and other unexpected I/O errors leave an entry
untouched, emit a warning, and do not stop maintenance of other entries.

### Normalize

Each resolvable component is looked up in its parent and replaced in the stored
path with the directory entry's actual spelling. Traversal is root-to-leaf, so
an extant prefix is corrected even when a later component is missing. Lookup is
by directory-entry identity and never substitutes a symlink target for the
stored alias. No filesystem object is renamed.

If a normalized selected entry becomes byte-identical to another database row,
the exact group is merged even if another member did not match the glob. This is
an invariant of normalization: it must not create entries whose scores compete
under the same byte-exact path.

### Dedupe

Selected entries are proposed by:

```text
key(path) = nfc(full_casefold(path))
```

In normal mode, each proposal is partitioned by filesystem identity. Unix uses
device and inode from no-follow metadata, keeping distinct final symlinks
separate. Windows retains the existing handle-based behavior. Dead or unreadable
entries cannot join a filesystem-confirmed group.

`--assume-insensitive` skips identity checks and merges each complete textual
group. It is therefore able to merge dead entries, but can also merge distinct
directories on a case-sensitive filesystem.

The highest-ranked member supplies the fallback spelling, with database order
breaking ties. Confirmed groups use their complete on-disk spelling. Ranks are
summed and the newest access time is retained.

## Reporting

Prunes, source-to-target normalizations, and merge groups are printed in that
order, each sorted deterministically, followed by totals for every non-empty
category. Dry-run changes the verbs to `would prune`, `would normalize`, and
`would merge` and never dirties the database. A no-op prints `no changes needed`.

## Validation

- CLI action requirements, conflicts, short options, globs, and dry-run.
- Live, missing, non-directory, dangling-symlink, and live-symlink pruning.
- Whole-path and partial-prefix spelling correction without symlink resolution.
- Exact normalization collisions across the path-glob boundary.
- Filesystem-confirmed and assumed-insensitive deduplication.
- Score and access-time preservation, deterministic reporting, idempotence, and
  read-only dry runs.
