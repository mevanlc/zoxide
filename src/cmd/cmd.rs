#![allow(clippy::module_inception)]

use std::path::PathBuf;

use clap::builder::{IntoResettable, Resettable, StyledStr};
use clap::{Parser, Subcommand, ValueEnum, ValueHint};

struct HelpTemplate;

impl IntoResettable<StyledStr> for HelpTemplate {
    fn into_resettable(self) -> Resettable<StyledStr> {
        color_print::cstr!("\
{before-help}<bold><underline>{name} {version}</underline></bold>
{author}
https://github.com/ajeetdsouza/zoxide

{about}

{usage-heading}
{tab}{usage}

{all-args}{after-help}

<bold><underline>Environment variables:</underline></bold>
{tab}<bold>_ZO_DATA_DIR</bold>        {tab}Path for zoxide data files
{tab}<bold>_ZO_ECHO</bold>            {tab}Print the matched directory before navigating to it when set to 1
{tab}<bold>_ZO_EXCLUDE_DIRS</bold>    {tab}List of directory globs to be excluded
{tab}<bold>_ZO_FZF_OPTS</bold>        {tab}Custom flags to pass to fzf
{tab}<bold>_ZO_MATCH_TRAILING_SLASH</bold>{tab}Match trailing slash queries against directory ends when set to 1
{tab}<bold>_ZO_MAXAGE</bold>          {tab}Maximum total age after which entries start getting deleted
{tab}<bold>_ZO_RESOLVE_SYMLINKS</bold>{tab}Resolve symlinks when storing paths").into_resettable()
    }
}

#[derive(Debug, Parser)]
#[clap(
    about,
    author,
    help_template = HelpTemplate,
    disable_help_subcommand = true,
    propagate_version = true,
    version,
)]
pub enum Cmd {
    Add(Add),
    Dedupe(Dedupe),
    Edit(Edit),
    Import(Import),
    Init(Init),
    Query(Query),
    Remove(Remove),
}

/// Add a new directory or increment its rank
#[derive(Debug, Parser)]
#[clap(
    author,
    help_template = HelpTemplate,
)]
pub struct Add {
    #[clap(num_args = 1.., required = true, value_hint = ValueHint::DirPath)]
    pub paths: Vec<PathBuf>,

    /// The rank to increment the entry if it exists or initialize it with if it
    /// doesn't
    #[clap(short, long)]
    pub score: Option<f64>,
}

/// Merge database entries that refer to the same directory
#[derive(Debug, Parser)]
#[clap(
    author,
    help_template = HelpTemplate,
)]
pub struct Dedupe {
    /// Globs selecting which entries to process, matched against the full
    /// stored path. Defaults to '*' to process the whole database
    #[clap(default_value = "*", num_args = 1.., value_name = "pathglob", value_hint = ValueHint::DirPath)]
    pub pathglobs: Vec<String>,

    /// Do not consult the filesystem: merge all entries that are textually
    /// equivalent (Unicode case fold + NFC). Required to merge entries whose
    /// directories no longer exist; can merge genuinely distinct directories
    /// on case-sensitive filesystems
    #[clap(long, short = 'i')]
    pub assume_insensitive: bool,

    /// Show what would be merged without modifying the database
    #[clap(long, short = 'n')]
    pub dry_run: bool,
}

/// Edit the database
#[derive(Debug, Parser)]
#[clap(
    author,
    help_template = HelpTemplate,
)]
pub struct Edit {
    #[clap(subcommand)]
    pub cmd: Option<EditCommand>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum EditCommand {
    #[clap(hide = true)]
    Decrement { path: String },
    #[clap(hide = true)]
    Delete { path: String },
    #[clap(hide = true)]
    Increment { path: String },
    #[clap(hide = true)]
    Reload,
}

/// Import entries from another application
#[derive(Debug, Parser)]
#[clap(
    author,
    help_template = HelpTemplate,
)]
pub struct Import {
    #[clap(subcommand)]
    pub from: ImportFrom,

    /// Merge into existing database
    #[clap(long, global = true)]
    pub merge: bool,
}

#[derive(Subcommand, Clone, Debug)]
pub enum ImportFrom {
    /// Import from atuin
    Atuin,
    /// Import from autojump
    Autojump,
    /// Import from fasd
    Fasd,
    /// Import from z
    Z,
    /// Import from z.lua
    #[clap(name = "z.lua")]
    ZLua,
    /// Import from zsh-z
    #[clap(name = "zsh-z")]
    ZshZ,
}

/// Generate shell configuration
#[derive(Debug, Parser)]
#[clap(
    author,
    help_template = HelpTemplate,
)]
pub struct Init {
    #[clap(value_enum)]
    pub shell: InitShell,

    /// Binds a control key (e.g. '^g') to interactively select and insert a directory
    #[clap(long, value_name = "keyspec")]
    pub bind_fzf_insert: Option<String>,

    /// Prevents zoxide from defining the `z` and `zi` commands
    #[clap(long, alias = "no-aliases")]
    pub no_cmd: bool,

    /// Changes the prefix of the `z` and `zi` commands
    #[clap(long, default_value = "z")]
    pub cmd: String,

    /// Changes how often zoxide increments a directory's score
    #[clap(value_enum, long, default_value = "pwd")]
    pub hook: InitHook,
}

#[derive(ValueEnum, Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitHook {
    None,
    Prompt,
    Pwd,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum InitShell {
    Bash,
    Elvish,
    Fish,
    Nushell,
    #[clap(alias = "ksh")]
    Posix,
    Powershell,
    Tcsh,
    Xonsh,
    Zsh,
}

/// Search for a directory in the database
#[derive(Debug, Parser)]
#[clap(
    author,
    help_template = HelpTemplate,
)]
pub struct Query {
    pub keywords: Vec<String>,

    /// Show unavailable directories
    #[clap(long, short)]
    pub all: bool,

    /// Use interactive selection
    #[clap(long, short, conflicts_with = "list")]
    pub interactive: bool,

    /// List all matching directories
    #[clap(long, short, conflicts_with = "interactive")]
    pub list: bool,

    /// Print score with results
    #[clap(long, short)]
    pub score: bool,

    /// Exclude the current directory
    #[clap(long, value_hint = ValueHint::DirPath, value_name = "path")]
    pub exclude: Option<String>,

    /// Only search within this directory
    #[clap(long, value_hint = ValueHint::DirPath, value_name = "path")]
    pub base_dir: Option<String>,
}

/// Remove a directory from the database
#[derive(Debug, Parser)]
#[clap(
    author,
    help_template = HelpTemplate,
)]
pub struct Remove {
    #[clap(value_hint = ValueHint::DirPath)]
    pub paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_pathglobs_default_to_star() {
        let cmd = Dedupe::try_parse_from(["dedupe"]).unwrap();

        assert_eq!(cmd.pathglobs, ["*"]);
    }

    #[test]
    fn dedupe_supplied_pathglobs_replace_default() {
        let cmd = Dedupe::try_parse_from(["dedupe", "/foo/*", "/bar/*"]).unwrap();

        assert_eq!(cmd.pathglobs, ["/foo/*", "/bar/*"]);
    }
}
