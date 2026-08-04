# PLAN: `zoxide dedupe` — merge equivalent database entries

Status: draft (2026-07-30)

## Motivation

The database can accumulate multiple entries that refer to the same directory
under different spellings: case variants (`~/Projects` vs `~/projects`) and
Unicode normalization variants (NFC `hèh` vs NFD `hèh`). Each spelling
accrues rank independently, splitting the score that frecency ranking depends
on and cluttering `zoxide query -l` / `zoxide edit` output.

Normalization variants arise even on **case-sensitive** filesystems (see
[Empirical findings](#empirical-findings)), and `_ZO_RESOLVE_SYMLINKS` does not
prevent them: `realpath(3)` on macOS neither case-corrects nor renormalizes, so
resolved paths keep whatever spelling the shell handed us.

`zoxide dedupe` collapses each group of equivalent entries into a single entry
that receives the group's combined score. Groups of size 1 are untouched.

## CLI design

```
Merge database entries that refer to the same directory

Usage:
  zoxide dedupe [OPTIONS] [pathglob]...

Arguments:
  [pathglob]...  Globs selecting which entries to process; entries matching no
                glob are ignored. Patterns match against the full stored path
                (same matcher as _ZO_EXCLUDE_DIRS). Defaults to '*' to process
                the whole database. Wildcards are not required.

Options:
  -i, --assume-insensitive  Do not consult the filesystem; merge entries that
                            are textually equivalent (Unicode case fold + NFC).
                            Required to merge entries whose directories no
                            longer exist. Can merge genuinely distinct
                            directories on case-sensitive filesystems.
  -n, --dry-run             Show what would be merged without modifying the
                            database
```

Decisions baked into the above:

- **Name `dedupe`**: matches the existing internal `Database::dedup()` concept
  (byte-exact merge after imports); this command is its user-facing,
  equivalence-aware generalization. Not case-specific in name, because the
  duplicates it fixes are not always case variants (NFC/NFD).
- **Positional defaults to `'*'`.** With no selector, the whole database is
  processed. Explicit globs still restrict the working set.
- **No `--check-fs-case-sensitivity` flag.** Earlier drafts gated merging on a
  per-filesystem case-sensitivity probe. Testing showed that primitive is
  unsound — see [Why we probe paths, not filesystems](#why-we-probe-paths-not-filesystems).
  The default mode probes *per candidate group* instead; `--assume-insensitive`
  is the only switch, and it means "skip probing entirely."
- `-f`/`--force-*` naming rejected: `--force` conventionally overrides a
  safety refusal; this flag changes the comparison strategy.
- `_ZO_EXCLUDE_DIRS` is **not** applied: this is explicit database
  maintenance, and the user's globs are the selection mechanism.

## Semantics

### Two-stage algorithm: textual fold proposes, filesystem disposes

```
1. Load DB. Working set = entries matching any <pathglob>.
2. PROPOSE: group the working set by fold key
       key(path) = nfc(full_casefold(path))
   Groups of size 1 are dropped (the intentional no-op case).
3. CONFIRM (default mode): within each group, obtain each entry's filesystem
   identity (device, inode) without following symlinks. Partition the group by
   identity:
     - entries sharing an identity  -> merge into one survivor
     - entries with unique identity -> leave untouched (distinct directories)
     - entries that fail to stat    -> leave untouched (dead; see below)
   With --assume-insensitive: skip probing; the whole fold group merges.
4. MERGE: survivor.rank         = Σ group ranks
          survivor.last_accessed = max(group last_accessed)
   (same math as the existing byte-exact Database::dedup())
5. Report each merge (in all modes); skip save() under --dry-run.
```

Key properties, each load-bearing:

- **The fold key is deliberately generous** — NFC + *full* Unicode case
  folding (so `ß` ≡ `ss` ≡ `ẞ`), at least as broad as the broadest real
  filesystem (APFS). Over-grouping costs one wasted stat per entry and is
  corrected by the probe; under-grouping silently misses real duplicates.
- **The probe inherits the mounted filesystem's exact equivalence relation** —
  APFS's full fold, Paragon-NTFS's frozen `$UpCase`-style table, ext4's
  per-directory `+F` casefold — with zero tables or OS detection in zoxide.
  `stat("Aa")` resolving to `AA`'s inode *is* the filesystem answering the
  question directly.
- **The textual gate makes the probe safe.** Pure inode comparison would also
  merge aliases we must not touch (`/tmp` vs `/private/tmp` symlinks, macOS
  firmlinks). Those are never fold-equal, so they never form a candidate
  group; the probe only ever confirms case/normalization variants.

### Survivor selection (which spelling is kept)

1. Default mode, directory exists: the **on-disk spelling** (phase 2; see
   below). Until then, and as fallback:
2. The spelling of the group member with the highest `rank` (the spelling the
   user actually visits most).

On-disk spelling recovery is platform-specific — `fs::canonicalize` does *not*
case-correct on macOS (verified; it's `realpath`-based) but does on Windows
(`GetFinalPathNameByHandle`; pair with the existing `dunce` dep to strip the
`\\?\` prefix):

| Platform | Mechanism |
|---|---|
| macOS | `fcntl(F_GETPATH)` (verified to return stored spelling) |
| Windows | `dunce::canonicalize` |
| Other Unix | readdir the parent, find the entry with matching identity; fall back to highest-rank spelling |

### Edge-case decisions

| Case | Default mode | `--assume-insensitive` |
|---|---|---|
| `./AA` on disk, DB has `AA` + `Aa` | FS case-insensitive: `stat(Aa)` resolves to `AA`'s inode → merge. FS case-sensitive: `stat(Aa)` fails → `Aa` is a dead entry → skip | merge |
| `./AA` and `./Aa` both on disk (case-sensitive FS) | distinct inodes → never merged | merged (documented hazard of the flag) |
| Both entries dead | can't confirm → skip | merge |
| One live, one dead | dead one can't share an identity → skip the dead one, merge any confirmed live subset | merge all |
| Unicode fold / normalization dialects | filesystem's own behavior via probe; no configuration | fixed documented fold: NFC + full casefold |
| Fold-equal entries that are two distinct symlinks to the same target (case-sensitive FS) | identity is taken **without following symlinks** (lstat semantics) → distinct → skip | merged |
| Group members on different devices (`st_dev` differs) | identities differ → skip | merged |

Windows nuance for the symlink row: no-follow identity needs
`FILE_FLAG_OPEN_REPARSE_POINT`; if that's awkward through the chosen crate,
following symlinks on Windows only is acceptable (directory symlinks are rare
there, and the failure mode is a benign merge of two names that jump to the
same place).

## Why we probe paths, not filesystems

Empirical results that killed the per-filesystem check (see next section for
the data): case-sensitivity is not a property you can look up per filesystem —

- it isn't even *boolean* per volume: case-sensitive APFS still folds NFC/NFD;
- it isn't *uniform* across case-insensitive filesystems: APFS folds
  `ß`/`ss`/`ẞ`, Paragon-NTFS doesn't even fold `ß`/`ẞ`;
- it isn't a property of the *volume*: the same NTFS disk folds NFC/NFD under
  macOS Paragon but not under Windows — the relation belongs to the driver;
- it isn't even per-volume on Linux: ext4 casefold (`chattr +F`) is
  **per-directory**.

Any table or detection heuristic is wrong somewhere forever. Asking "are these
two specific paths the same directory right now, under the current mount?" is
always right, and that is exactly what the confirm-stage stat asks.

## Empirical findings

Tested 2026-07-29 on macOS (Darwin 25.5): APFS case-insensitive (`/tmp`),
APFS case-sensitive, NTFS via Paragon driver. ext4-casefold could not be
tested locally (OrbStack kernel lacks `CONFIG_UNICODE`); its column is from
kernel documentation.

Same directory (✓) or distinct (✗):

| Pair | APFS-CI | APFS-CS | NTFS (Paragon, macOS) | NTFS (Windows, doc) | ext4 default (doc) | ext4 `+F` (doc) |
|---|---|---|---|---|---|---|
| `AA` / `Aa` | ✓ | ✗ | ✓ | ✓ | ✗ | ✓ |
| `strasse` / `straße` | ✓ | ✗ | ✗ | ✗ | ✗ | ✓ |
| `ß` / `ẞ` (U+00DF/U+1E9E) | ✓ | ✗ | ✗ | ✗ | ✗ | ✓ |
| NFC / NFD (`hèh`) | ✓ | ✓ | ✓ | ✗ | ✗ | ✓ |

Verification method: `mkdir` errno (`EEXIST` ⇒ folded together) plus
`stat` inode equality across spellings. Also verified: Paragon-NTFS presents
stable, distinct `st_ino` values, so `(dev, ino)` identity works there;
`getcwd()`/`F_GETPATH` return the on-disk spelling on macOS while
`realpath()` does not.

## Implementation plan

### New dependencies

| Crate | Purpose | Notes |
|---|---|---|
| `unicode-normalization` | NFC for the fold key | small, ubiquitous |
| `caseless` | full Unicode case fold | thin layer over unicode-normalization |
| `same-file` | `(dev, ino)` / Windows file-index identity | BurntSushi; already solves Windows |
| `libc` (macOS target only) | `fcntl(F_GETPATH)` | phase 2 only |

### Changes by file

- `src/cmd/cmd.rs` — add `Dedupe` variant + struct (`pathglobs: Vec<String>`,
  `assume_insensitive: bool`, `dry_run: bool`). Build script regenerates shell
  completions from the clap definition automatically.
- `src/cmd/dedupe.rs` (new) — `impl Run for Dedupe`: glob filtering
  (`glob::Pattern`, same match options as `_ZO_EXCLUDE_DIRS` in
  `src/config.rs`), fold-key grouping, probing, reporting.
- `src/cmd/mod.rs` — register module + match arm.
- `src/db/mod.rs` — add `Database::merge_entries(survivor_idx, victim_idxs)`
  applying the rank/last_accessed math; refactor `dedup()` to share it.
- `src/util.rs` — `fold_key(path) -> String`; phase 2: `true_spelling(path)`.
- `man/man1/zoxide-dedupe.1` (new) — following existing per-command man pages.
- `README.md` — mention under the commands list if one exists there.

### Phases

1. **MVP**: command wiring, fold grouping, no-follow `(dev, ino)` probe,
   merge + report, `--dry-run`, highest-rank survivor spelling, tests, man
   page.
2. **True-spelling survivor**: `F_GETPATH` / `dunce::canonicalize` /
   readdir-walk; keep highest-rank as fallback.
3. (optional) Richer dead-entry handling (e.g. `--dead=skip|merge`) if users
   ask; not needed initially since `--assume-insensitive` covers it.

## Testing plan

Unit (pure, no FS):

- fold key: `AA`≡`aa`, `straße`≡`STRASSE`≡`strasse`, NFC≡NFD, negative cases.
- merge math: rank sum, last_accessed max, survivor choice by rank.
- grouping: singleton groups untouched; glob filtering in/out; `'*'` matches
  paths containing separators (glob crate default `MatchOptions`).

Integration (`tests/`, `tempfile`):

- **Idempotence**: running dedupe twice changes nothing the second time.
- `--dry-run` leaves `db.dirty() == false` and the file unmodified.
- Case-sensitive tmpdir (Linux CI, APFS-CS if available): default mode merges
  nothing when both spellings exist as real distinct dirs; merges nothing for
  dead pairs; `--assume-insensitive` merges both.
- Case-insensitive tmpdir (macOS CI `/tmp`): default mode merges case and
  NFC/NFD variants of a real dir; keeps the on-disk spelling (phase 2).
- Platform-gate the FS-behavior tests with `cfg` + runtime probe (create
  `AA`/stat `aa`) rather than assuming OS ⇒ behavior, since macOS can host
  case-sensitive volumes and vice versa.

## Open questions

- Should glob matching be case-insensitive when the *glob* itself is meant to
  select case-variant entries? Current answer: no — globs are byte-exact
  (consistent with `_ZO_EXCLUDE_DIRS`), and users select variants with
  wildcards (`'*rojects'`) or multiple globs.
- Windows path-separator variants (`C:\x` vs `C:/x`) and drive-letter case in
  the fold key: fold-key could normalize separators on Windows. Deferred until
  someone shows such entries occur in practice (zoxide stores native
  separators).
- Should `import` grow an opt-in to run this equivalence-aware dedupe after
  importing? Natural follow-up once the machinery exists.
