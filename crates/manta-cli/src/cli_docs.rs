//! MAN-133: `manta completions <SHELL>` and `manta man-pages`, both rendered
//! from the clap tree that prints `--help`, so they cannot drift from it.

use crate::build_info;
use anyhow::{bail, Context, Result};
use clap_complete::aot::{generate, Shell};
use clap_mangen::Man;
use std::io::Write;
use std::path::Path;

/// Writes `shell`'s completion script for `cmd` to `out`.
pub fn write_completions(shell: Shell, cmd: &mut clap::Command, out: &mut dyn Write) -> Result<()> {
    generate(shell, cmd, "manta", out);
    out.flush().context("writing the completion script")
}

/// `(file name, roff)` for `manta.1` and one page per visible subcommand at
/// any depth (`manta-run.1`, `manta-config-check.1`, ...).
pub fn man_pages(cmd: clap::Command) -> Result<Vec<(String, Vec<u8>)>> {
    let mut cmd = cmd.disable_help_subcommand(true);
    cmd.build();
    let mut pages = Vec::new();
    collect(&cmd, &mut pages)?;
    Ok(pages)
}

fn collect(cmd: &clap::Command, pages: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    // clap_mangen quotes only .TH arguments that contain a space, so its
    // default empty date vanishes and the source slides into the date
    // column; `""` is roff's empty argument. Subcommands carry no version,
    // so the source is set here for every page.
    let man = Man::new(cmd.clone())
        .date("\"\"")
        .source(format!("manta {}", build_info::VERSION))
        .manual("User Commands");
    let mut roff = Vec::new();
    man.render(&mut roff).context("rendering a man page")?;
    pages.push((man.get_filename(), roff));
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        collect(sub, pages)?;
    }
    Ok(())
}

/// Writes every page from [`man_pages`] into `dir`; returns how many.
pub fn write_man_pages(cmd: clap::Command, dir: &Path) -> Result<usize> {
    if dir.exists() && !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let pages = man_pages(cmd)?;
    for (name, roff) in &pages {
        let path = dir.join(name);
        std::fs::write(&path, roff).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(pages.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> clap::Command {
        clap::Command::new("manta")
            .version("9.9.9")
            .subcommand(clap::Command::new("run").about("Run it"))
            .subcommand(clap::Command::new("secret").hide(true))
            .subcommand(
                clap::Command::new("config")
                    .subcommand(clap::Command::new("check"))
                    .subcommand(clap::Command::new("init")),
            )
    }

    #[test]
    fn one_page_per_visible_command_and_no_help_pages() {
        let names: Vec<String> = man_pages(tree())
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(
            names,
            [
                "manta.1",
                "manta-run.1",
                "manta-config.1",
                "manta-config-check.1",
                "manta-config-init.1"
            ]
        );
    }

    #[test]
    fn every_title_line_names_manta_and_its_version_with_an_empty_date() {
        for (name, roff) in man_pages(tree()).unwrap() {
            let text = String::from_utf8(roff).unwrap();
            let th = text.lines().find(|l| l.starts_with(".TH ")).unwrap();
            let title = name.trim_end_matches(".1");
            assert_eq!(
                th,
                format!(
                    ".TH {title} 1 \"\" \"manta {}\" \"User Commands\"",
                    build_info::VERSION
                ),
                "{name}"
            );
        }
    }
}
