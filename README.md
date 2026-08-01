# a zoxide fork

This is a fork of [zoxide], a smarter `cd` command that remembers which
directories you use most frequently, so you can jump to them in just a few
keystrokes. See the [upstream repository][zoxide] for installation, shell
setup, and the full documentation.

## about this fork

This fork follows upstream zoxide, merging it in periodically, and adds:

- a `dedupe` subcommand that merges database entries naming the same directory
  under different spellings, so their scores stop competing; and
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

### `zoxide dedupe`

The database can accumulate several entries for one directory under different
spellings — `~/Projects` alongside `~/projects`, or the NFC and NFD encodings
of `~/café`. Each spelling accrues rank on its own, splitting the score that
ranking depends on. `zoxide dedupe` collapses each such group into a single
entry whose rank is the group's sum and whose access time is the group's most
recent.

```sh
zoxide dedupe -n '*'          # show what the whole database would merge
zoxide dedupe '*'             # merge it
zoxide dedupe "$HOME/src/*"   # restrict to one subtree
zoxide dedupe -i '*'          # skip the filesystem check (see below)
```

```console
$ zoxide dedupe -n '*'
would merge into /Users/alice/Projects:
  /Users/alice/projects
would merge 2 entries into 1 directory
```

When there is nothing to merge it prints `no duplicate entries found`.

Entries are first grouped by textual equivalence: full Unicode case folding
plus canonical normalization, so `ß`, `ss` and `ẞ` fold together, and the NFC
and NFD spellings of a name compare equal. Each group is then confirmed against
the filesystem, and only the entries the filesystem reports as the very same
directory are merged — on a case-sensitive filesystem where `Projects` and
`projects` are two distinct directories, nothing is merged. The surviving entry
takes the on-disk spelling when the platform can report it, and otherwise the
highest-ranked spelling in the group.

The `<pathglob>...` argument is required, and is matched against the full
stored path using the same glob matcher as `_ZO_EXCLUDE_DIRS`. `*` crosses path
separators, so `'*'` selects the whole database. Requiring an explicit selector
is deliberate: merging discards spellings and sums scores, and cannot be
undone. Quote the globs so the shell does not expand them first.

Options:

- `-n`, `--dry-run` — print what would be merged and leave the database
  untouched.
- `-i`, `--assume-insensitive` — skip the filesystem check and merge every
  textually equivalent group. This is the only way to merge entries whose
  directories no longer exist, which the default mode always leaves alone. It
  can also merge genuinely distinct directories on a case-sensitive filesystem,
  and it does not correct spellings, so try it with `--dry-run` first.

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
