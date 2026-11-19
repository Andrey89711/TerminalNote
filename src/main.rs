mod app;
mod commands;
mod config;
mod explorer;
mod i18n;
mod links;
mod markdown;
mod storage;
mod theme;
mod update_install;
mod updates;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::{io::IsTerminal, path::PathBuf};
use storage::NoteStore;

#[derive(Debug, Parser)]
#[command(
    name = "note",
    version,
    about = "Local Markdown and text notes in your terminal"
)]
struct Cli {
    /// Notes directory; omit to use your configured directory or ~/Notes
    path: Option<PathBuf>,
    /// Explicitly create the notes directory, then open the interface
    #[arg(long, conflicts_with_all = ["list", "check", "set_default"])]
    init: bool,
    /// Save an existing directory as the default, without opening the interface
    #[arg(long, value_name = "PATH", conflicts_with_all = ["path", "list", "check"])]
    set_default: Option<PathBuf>,
    /// Print relative note paths without starting the interface
    #[arg(long, conflicts_with = "check")]
    list: bool,
    /// Check UTF-8 notes and internal links without starting the interface
    #[arg(long)]
    check: bool,
    /// Explicit configuration file, used for isolated test and demo sessions
    #[arg(long, hide = true, value_name = "PATH")]
    config_file: Option<PathBuf>,
}

fn expand_path(path: PathBuf) -> Result<PathBuf> {
    let path = if let Ok(suffix) = path.strip_prefix("~") {
        dirs::home_dir()
            .context("Home directory is unavailable")?
            .join(suffix)
    } else {
        path
    };
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn run() -> Result<()> {
    if update_install::helper_entry()? {
        return Ok(());
    }
    let cli = Cli::parse();
    if cli
        .config_file
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        bail!("--config-file must be an absolute file path");
    }
    if let Some(path) = cli.set_default {
        let path = expand_path(path)?;
        let config = if let Some(config) = &cli.config_file {
            config::set_default_root_at(&path, config)?;
            config.clone()
        } else {
            config::set_default_root(&path)?
        };
        println!(
            "Default directory saved in {}",
            safe(&config.display().to_string())
        );
        return Ok(());
    }
    if !cli.list
        && !cli.check
        && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
    {
        bail!("The interactive interface needs a terminal. Use --list or --check for scripts.");
    }
    let root = expand_path(match cli.path {
        Some(path) => path,
        None => match &cli.config_file {
            Some(path) => config::default_root_at(path)?,
            None => config::default_root()?,
        },
    })?;
    if cli.init {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("Cannot create {}", safe(&root.display().to_string())))?;
    }
    let store = NoteStore::open(&root).with_context(|| {
        format!(
            "Cannot open {}. Use `note --init [PATH]` to explicitly create a directory.",
            safe(&root.display().to_string())
        )
    })?;
    if cli.list || cli.check {
        let listing = store.list()?;
        let mut errors = Vec::new();
        for warning in &listing.warnings {
            eprintln!("Warning: {}", safe(warning));
        }
        for entry in &listing.notes {
            if cli.list {
                println!("{}", safe(&entry.path.to_string_lossy()));
                continue;
            }
            match store.load(&entry.path) {
                Ok(doc) => {
                    for link in links::extract_links(&doc.text) {
                        if let Err(error) =
                            links::resolve_link(&doc.path, &link.target, &listing.notes)
                        {
                            errors.push(format!(
                                "{}: {}: {error}",
                                entry.path.display(),
                                link.target
                            ));
                        }
                    }
                }
                Err(error) => errors.push(format!("{}: {error}", entry.path.display())),
            }
        }
        if cli.check {
            if !listing.warnings.is_empty() {
                errors.extend(listing.warnings);
            }
            if !errors.is_empty() {
                bail!("Check failed:\n{}", safe(&errors.join("\n")));
            }
            println!(
                "Checked {} notes: UTF-8 and internal links OK.",
                listing.notes.len()
            );
        }
        return Ok(());
    }
    app::run(store, cli.config_file)
}

fn safe(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .filter(|c| !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .collect()
}

fn main() {
    if let Err(error) = run() {
        eprintln!("note: {}", safe(&format!("{error:#}")));
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_contract_and_conflicts() {
        assert!(Cli::try_parse_from(["note"]).is_ok());
        assert!(Cli::try_parse_from(["note", "."]).is_ok());
        assert!(
            Cli::try_parse_from(["note", "--init", "notes"])
                .unwrap()
                .init
        );
        assert!(Cli::try_parse_from(["note", "--list", "--check"]).is_err());
        assert!(Cli::try_parse_from(["note", "--set-default", ".", "extra"]).is_err());
    }
    #[test]
    fn relative_paths_and_terminal_output() {
        assert!(expand_path(PathBuf::from(".")).unwrap().is_absolute());
        assert_eq!(safe("a\x1b[31m\u{202e}b"), "a[31mb");
    }
}
