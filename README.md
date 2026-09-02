# a zoxide fork

This is a fork of [zoxide], a smarter `cd` command that remembers which
directories you use most frequently, so you can jump to them in just a few
keystrokes. See the [upstream repository][zoxide] for installation, shell
setup, and the full documentation.

## about this fork

This fork follows upstream zoxide, merging it in periodically, and adds:

- a `tidy` subcommand that prunes unavailable directories, corrects stored path
  spellings, and merges duplicate entries so their scores stop competing; and
- `_ZO_MATCH_TRAILING_SLASH`, which lets a trailing slash in a query match the
  end of a directory path; and
- `zoxide init --bind-fzf-insert`, which binds a control key to insert a
  directory selected from zoxide's interactive picker.

Everything else behaves like upstream zoxide as of the most recent merge.

### `zoxide init --bind-fzf-insert`

For Bash, Zsh, Fish, and Nushell, `zoxide init` can bind a control key to open
the same interactive directory picker as `zi`, then insert the selected path
into the command line instead of changing directories:

```sh
eval "$(zoxide init zsh --bind-fzf-insert '^g')"
```

The key specification uses caret notation and must name an ASCII control letter
(case-insensitive). Each shell follows fzf's Ctrl-T insertion behavior and
native path escaping. The option is rejected for other shells. Bash also rejects
`^z`, which fzf's Bash 3 compatibility mechanism reserves for switching Readline
keymaps.

### `zoxide tidy`

The database can retain directories that have disappeared, store paths using a
spelling different from the filesystem's, or accumulate several entries for one
directory. `zoxide tidy` performs one or more explicit maintenance actions:

```sh
zoxide tidy -d                 # deduplicate the whole database
zoxide tidy --normalize       # correct stored paths to on-disk spelling
zoxide tidy -p                # remove paths that are no longer navigable
zoxide tidy -a -n             # preview all three actions
zoxide tidy -d "$HOME/src/*"  # restrict maintenance to one subtree
```

```console
$ zoxide tidy --normalize --dedupe --dry-run
would normalize /Users/alice/projects -> /Users/alice/Projects
would merge into /Users/alice/Projects:
  /Users/alice/projects
would normalize 1 entry
would merge 2 entries into 1 directory
```

At least one of `--dedupe`, `--normalize`, `--prune`, or `--all` is required.
When nothing would change, the command prints `no changes needed`.

`--prune` removes stored paths that no longer resolve to directories. This
includes missing paths, paths replaced by files, and dangling symlinks; symlinks
that still resolve to directories are retained. Unexpected filesystem errors
leave the affected entry untouched and produce a warning.

`--normalize` checks each path component against its directory entry and rewrites
the database path using the filesystem's spelling. It never renames anything on
the filesystem. Existing parent components are corrected even when a later
component is missing, and symlink aliases are preserved rather than replaced by
their targets. If normalization makes entries byte-identical, their ranks are
combined and their latest access time is kept.

`--dedupe` handles different spellings of the same directory — `~/Projects`
alongside `~/projects`, or the NFC and NFD encodings of `~/café`. Each spelling
otherwise accrues rank on its own, splitting the score that ranking depends on.

Entries are first grouped by textual equivalence: full Unicode case folding
plus canonical normalization, so `ß`, `ss` and `ẞ` fold together, and the NFC
and NFD spellings of a name compare equal. Each group is then confirmed against
the filesystem, and only the entries the filesystem reports as the very same
directory are merged — on a case-sensitive filesystem where `Projects` and
`projects` are two distinct directories, nothing is merged. The surviving entry
takes the on-disk spelling when the platform can report it, and otherwise the
highest-ranked spelling in the group. Ranks are summed and the most recent
access time is kept.

The optional `<pathglob>...` arguments are matched against the full stored path
using the same glob matcher as `_ZO_EXCLUDE_DIRS`. With no arguments, the glob
defaults to `*`, which crosses path separators and selects the whole database.
Quote any supplied globs so the shell does not expand them first.

Action and behavior options:

- `-d`, `--dedupe` — merge different spellings of the same filesystem entry.
- `--normalize` — rewrite stored paths using their on-disk spelling.
- `-p`, `--prune` — remove paths that no longer resolve to directories.
- `-a`, `--all` — prune, normalize, and deduplicate.
- `-n`, `--dry-run` — print what would change and leave the database untouched.
- `-i`, `--assume-insensitive` — skip the filesystem check and merge every
  textually equivalent group. This lets dedupe merge entries whose directories
  no longer exist, which filesystem-confirmed dedupe leaves alone. It can also
  merge genuinely distinct directories on a case-sensitive filesystem, and it
  does not itself correct spellings, so try it with `--dry-run` first. It
  requires `--dedupe` or `--all`.

### `_ZO_MATCH_TRAILING_SLASH`

Upstream, a trailing slash on the last query keyword is matched literally, so
it only ever matches a directory with something below it: `z lat/` finds
`~/p/my/lat/tools` but never `~/p/my/lat` itself.

Setting `_ZO_MATCH_TRAILING_SLASH=1` also lets that slash match the end of a
stored path, as if every directory carried an implicit trailing slash. `z lat/`
then matches:

- `~/p/my/lat`, where the slash matches the end of the path — upstream does
  not match this; and
- `~/p/my/lat/tools`, one component below the keyword, as upstream does.

It still does not match `~/p/my/latest`, because the slash forbids matching
part of a component, nor `~/p/my/lat/tools/more`, which is more than one
component below the keyword.

The variable must be set to exactly `1`; any other value leaves matching
unchanged, and it is unset by default. Only the last keyword is affected —
earlier keywords match as upstream, and the `z foo /` idiom is unchanged. The
setting applies to everything that queries the database: `z`, `zi`, and
`zoxide query`.

## building

zoxide requires Rust 1.88.0 or newer:

```sh
cargo build --release
```

## license

MIT, unchanged from upstream — copyright 2020 Ajeet D'Souza. See [LICENSE].

[license]: LICENSE
[zoxide]: https://github.com/ajeetdsouza/zoxide
