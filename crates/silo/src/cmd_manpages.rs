//! `silo manpages`: writes man pages generated from the live clap command
//! definitions, for packaging.

use std::path::Path;

use anyhow::Context;
use clap::CommandFactory;

pub fn execute(output_dir: &Path) -> anyhow::Result<u8> {
    let written = write_man_pages(output_dir)?;
    eprintln!(
        "wrote {} man page(s) to {}",
        written.len(),
        output_dir.display()
    );
    Ok(0)
}

/// Writes `silo.1` plus one `silo-<subcommand>.1` per visible subcommand
/// into `dir`, creating it if needed. Returns the written file names.
pub fn write_man_pages(dir: &Path) -> anyhow::Result<Vec<String>> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating output directory {}", dir.display()))?;
    let mut command = crate::cli::Cli::command();
    command.build();

    let mut written = Vec::new();
    let mut render = |name: &str, command: clap::Command| -> anyhow::Result<()> {
        let mut buffer = Vec::new();
        clap_mangen::Man::new(command).render(&mut buffer)?;
        let file = format!("{name}.1");
        let path = dir.join(&file);
        std::fs::write(&path, buffer).with_context(|| format!("writing {}", path.display()))?;
        written.push(file);
        Ok(())
    };

    render("silo", command.clone())?;
    for subcommand in command.get_subcommands() {
        if subcommand.is_hide_set() || subcommand.get_name() == "help" {
            continue;
        }
        let name = format!("silo-{}", subcommand.get_name());
        render(&name, subcommand.clone().name(name.clone()))?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_page_per_visible_command_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let written = write_man_pages(dir.path()).unwrap();
        let expected = [
            "silo.1",
            "silo-run.1",
            "silo-workspace.1",
            "silo-shell.1",
            "silo-replay-test.1",
            "silo-harnesses.1",
        ];
        for file in expected {
            assert!(dir.path().join(file).is_file(), "missing {file}");
        }
        assert_eq!(written.len(), expected.len(), "{written:?}");

        let top = std::fs::read(dir.path().join("silo.1")).unwrap();
        let top = String::from_utf8_lossy(&top);
        assert!(top.contains("silo"), "{top}");
    }
}
