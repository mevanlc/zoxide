use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use askama::Template;

use crate::cmd::{Init, InitShell, Run};
use crate::config;
use crate::error::BrokenPipeHandler;
use crate::shell::{
    Bash, Elvish, Fish, FzfInsertBinding, Nushell, Opts, Posix, Powershell, Tcsh, Xonsh, Zsh,
};

impl Run for Init {
    fn run(&self) -> Result<()> {
        let bind_fzf_insert = fzf_insert_binding(&self.shell, self.bind_fzf_insert.as_deref())?;
        let cmd = if self.no_cmd { None } else { Some(self.cmd.as_str()) };
        let echo = config::echo();
        let resolve_symlinks = config::resolve_symlinks();
        let opts = &Opts { cmd, hook: self.hook, echo, resolve_symlinks, bind_fzf_insert };

        let source = match self.shell {
            InitShell::Bash => Bash(opts).render(),
            InitShell::Elvish => Elvish(opts).render(),
            InitShell::Fish => Fish(opts).render(),
            InitShell::Nushell => Nushell(opts).render(),
            InitShell::Posix => Posix(opts).render(),
            InitShell::Powershell => Powershell(opts).render(),
            InitShell::Tcsh => Tcsh(opts).render(),
            InitShell::Xonsh => Xonsh(opts).render(),
            InitShell::Zsh => Zsh(opts).render(),
        }
        .context("could not render template")?;
        writeln!(io::stdout(), "{source}").pipe_exit("stdout")
    }
}

fn fzf_insert_binding(
    shell: &InitShell,
    keyspec: Option<&str>,
) -> Result<Option<FzfInsertBinding>> {
    match (keyspec, shell) {
        (None, _) => Ok(None),
        (Some(keyspec), InitShell::Bash) if keyspec.eq_ignore_ascii_case("^z") => {
            bail!("key specification '^z' is not supported for bash")
        }
        (
            Some(keyspec),
            InitShell::Bash | InitShell::Fish | InitShell::Nushell | InitShell::Zsh,
        ) => FzfInsertBinding::parse(keyspec).map(Some).map_err(anyhow::Error::msg),
        (Some(_), shell) => {
            let shell = match shell {
                InitShell::Elvish => "elvish",
                InitShell::Posix => "posix",
                InitShell::Powershell => "powershell",
                InitShell::Tcsh => "tcsh",
                InitShell::Xonsh => "xonsh",
                InitShell::Bash | InitShell::Fish | InitShell::Nushell | InitShell::Zsh => {
                    unreachable!()
                }
            };
            bail!("--bind-fzf-insert is not supported for {shell}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_fzf_insert_binding_for_unsupported_shells() {
        for (shell, name) in [
            (InitShell::Elvish, "elvish"),
            (InitShell::Posix, "posix"),
            (InitShell::Powershell, "powershell"),
            (InitShell::Tcsh, "tcsh"),
            (InitShell::Xonsh, "xonsh"),
        ] {
            let error = fzf_insert_binding(&shell, Some("^g")).unwrap_err();
            assert_eq!(error.to_string(), format!("--bind-fzf-insert is not supported for {name}"));
        }
    }

    #[test]
    fn rejects_ctrl_z_for_bash() {
        let error = fzf_insert_binding(&InitShell::Bash, Some("^Z")).unwrap_err();
        assert_eq!(error.to_string(), "key specification '^z' is not supported for bash");
    }
}
