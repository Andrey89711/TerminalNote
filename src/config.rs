//! Small, strict configuration stored separately from the notes directory.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
}

/// Return the platform configuration file location (including XDG on Unix).
pub fn config_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("Cannot determine the user configuration directory")?
        .join("terminalnote")
        .join("config.toml"))
}

/// Read the configured absolute root, or use ~/Notes without creating it.
pub fn default_root() -> Result<PathBuf> {
    let config = read_config(&config_path()?)?;
    match config.root {
        Some(root) => Ok(PathBuf::from(root)),
        None => Ok(dirs::home_dir()
            .context("Cannot determine the user home directory")?
            .join("Notes")),
    }
}

/// Validate and atomically save an existing absolute directory as the default.
/// Returns the configuration file path, not the notes directory.
pub fn set_default_root(root: &Path) -> Result<PathBuf> {
    let path = config_path()?;
    set_default_root_at(root, &path)?;
    Ok(path)
}

fn read_config(path: &Path) -> Result<Config> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Cannot inspect config {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        bail!("Refusing symlink config file {}", path.display());
    }
    if !metadata.is_file() {
        bail!("Config is not a regular file: {}", path.display());
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        bail!("Config exceeds the 64 KiB limit: {}", path.display());
    }
    let file =
        File::open(path).with_context(|| format!("Cannot open config {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Cannot read config {}", path.display()))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!("Config exceeds the 64 KiB limit: {}", path.display());
    }
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("Config is not UTF-8: {}", path.display()))?;
    let config: Config =
        toml::from_str(text).with_context(|| format!("Invalid TOML config {}", path.display()))?;
    if let Some(root) = &config.root {
        if !Path::new(root).is_absolute() {
            bail!("Config root must be an absolute path: {}", path.display());
        }
    }
    Ok(config)
}

fn set_default_root_at(root: &Path, path: &Path) -> Result<()> {
    if !root.is_absolute() {
        bail!("Default notes root must be an absolute path");
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("Cannot resolve notes directory {}", root.display()))?;
    if !root.is_dir() {
        bail!("Notes root is not a directory: {}", root.display());
    }
    let root = root
        .to_str()
        .context("Default notes root must be a valid Unicode path")?
        .to_owned();
    // Validate any existing file before making changes. Unknown fields are errors,
    // rather than silently discarded settings from another configuration version.
    let mut config = read_config(path)?;
    config.root = Some(root);
    let text = toml::to_string_pretty(&config).context("Cannot serialize configuration")?;
    let parent = path
        .parent()
        .context("Config path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Cannot create config directory {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("Cannot create temporary config in {}", parent.display()))?;
    temporary
        .write_all(text.as_bytes())
        .context("Cannot write temporary config")?;
    temporary
        .as_file()
        .sync_all()
        .context("Cannot flush temporary config")?;
    // Check again immediately before replacement; never deliberately replace a link.
    read_config(path)?;
    temporary
        .persist(path)
        .with_context(|| format!("Cannot atomically save config {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_empty_config_have_no_override() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("config.toml");
        assert!(read_config(&path)?.root.is_none());
        fs::write(&path, "# No default configured\n")?;
        assert!(read_config(&path)?.root.is_none());
        Ok(())
    }

    #[test]
    fn writes_and_replaces_canonical_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir(&first)?;
        fs::create_dir(&second)?;
        let path = temp.path().join("config").join("config.toml");
        for root in [&first, &second] {
            set_default_root_at(root, &path)?;
            assert_eq!(
                PathBuf::from(read_config(&path)?.root.unwrap()),
                root.canonicalize()?
            );
        }
        assert_eq!(fs::read_dir(path.parent().unwrap())?.count(), 1);
        Ok(())
    }

    #[test]
    fn refuses_invalid_config_without_modifying_it() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("config.toml");
        for contents in [
            "root = [",
            "other = true",
            "root = 123",
            "root = 'relative'",
        ] {
            fs::write(&path, contents)?;
            assert!(set_default_root_at(temp.path(), &path).is_err());
            assert_eq!(fs::read_to_string(&path)?, contents);
        }
        Ok(())
    }

    #[test]
    fn refuses_large_and_non_utf8_config() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("config.toml");
        for bytes in [vec![b' '; MAX_CONFIG_BYTES as usize + 1], vec![0xff]] {
            fs::write(&path, &bytes)?;
            assert!(read_config(&path).is_err());
            assert!(set_default_root_at(temp.path(), &path).is_err());
            assert_eq!(fs::read(&path)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn rejects_relative_missing_and_file_roots() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("config.toml");
        let file = temp.path().join("file.txt");
        fs::write(&file, "notes")?;
        for root in [Path::new("relative"), &temp.path().join("missing"), &file] {
            assert!(set_default_root_at(root, &path).is_err());
            assert!(!path.exists());
        }
        Ok(())
    }

    #[test]
    fn rejects_directory_as_config() -> Result<()> {
        let temp = tempfile::tempdir()?;
        assert!(read_config(temp.path()).is_err());
        assert!(set_default_root_at(temp.path(), temp.path()).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_config_without_changing_target() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("target.toml");
        let path = temp.path().join("config.toml");
        fs::write(&target, "# keep\n")?;
        std::os::unix::fs::symlink(&target, &path)?;
        assert!(read_config(&path).is_err());
        assert!(set_default_root_at(temp.path(), &path).is_err());
        assert_eq!(fs::read_to_string(&target)?, "# keep\n");
        Ok(())
    }
}
