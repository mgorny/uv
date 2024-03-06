use std::fmt::Write;

use anyhow::Result;
use axoupdater::AxoUpdater;
use owo_colors::OwoColorize;

use crate::commands::ExitStatus;
use crate::printer::Printer;

/// Attempt to update the `uv` binary.
pub(crate) async fn self_update(printer: Printer) -> Result<ExitStatus> {
    let mut updater = AxoUpdater::new_for("uv");

    // Load the "install receipt" for the current binary. If the receipt is found, then
    // `uv` was likely installed via a package manager.
    let Ok(receipt) = updater.load_receipt() else {
        writeln!(
            printer.stderr(),
            "{}",
            format_args!(
                "{}{} Self-update is only available for `uv` binaries installed via the standalone installation scripts.\n\nIf you installed `uv` with `pip`, `brew`, or another package manager, update `uv` with `pip install --upgrade`, `brew upgrade`, or similar.",
                "warning".yellow().bold(),
                ":".bold()
            )
        )?;
        return Ok(ExitStatus::Error);
    };

    if receipt.run().await? {
        writeln!(
            printer.stderr(),
            "{}",
            format_args!(
                "{}{} Upgraded uv to {}! {}",
                "success".green().bold(),
                ":".bold(),
                format!("v{}", env!("CARGO_PKG_VERSION")).bold().white(),
                format!(
                    "https://github.com/astral-sh/uv/releases/tag/{}",
                    env!("CARGO_PKG_VERSION")
                )
                .cyan()
            )
        )?;
    } else {
        writeln!(
            printer.stderr(),
            "{}",
            format_args!(
                "{}{} You're on the latest version of uv ({}).",
                "success".green().bold(),
                ":".bold(),
                format!("v{}", env!("CARGO_PKG_VERSION")).bold().white()
            )
        )?;
    }

    Ok(ExitStatus::Success)
}
