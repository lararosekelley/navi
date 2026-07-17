//! Dynamic shell completion for the `navi` binary.

use std::env;
use std::io::{self, Write};

use anyhow::{bail, Context, Result};
use clap_complete::env::Shells;
use clap_complete::Shell;

/// Env var a shell's completer sets to request candidates (see `main`).
pub const COMPLETE_VAR: &str = "COMPLETE";

/// Print the completion registration script for `shell` to stdout.
pub fn print(shell: Shell) -> Result<()> {
    // Point registration at this exact binary so completion works even when
    // `navi` is not on PATH, and stays correct across upgrades (shells re-source
    // this on every start).
    let completer = env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| "navi".to_owned());
    write(shell, &completer, &mut io::stdout().lock())
}

/// Write the dynamic-completion registration for `shell`. `completer` is the
/// binary the shell invokes at completion time.
pub fn write(shell: Shell, completer: &str, writer: &mut dyn Write) -> Result<()> {
    let shells = Shells::builtins();
    let name = shell.to_string();
    let Some(env_completer) = shells.completer(&name) else {
        bail!("no dynamic completion support for {name}");
    };
    env_completer
        .write_registration(COMPLETE_VAR, "navi", "navi", completer, writer)
        .with_context(|| format!("failed to write {name} completion registration"))?;
    Ok(())
}
