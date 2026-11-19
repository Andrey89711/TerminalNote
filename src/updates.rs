//! Caller-triggered release metadata checks, separate from consented download/installation.
use crate::i18n::Language;
use anyhow::{anyhow, bail, Result};
use semver::Version;
use serde::Deserialize;
use std::{
    io::Read,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

const ENDPOINT: &str = "https://api.github.com/repos/Andrey89711/TerminalNote/releases/latest";
const MAX_RESPONSE: u64 = 64 * 1024;
const MAX_TAG_LENGTH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    UpToDate,
    Available(String),
    NoRelease,
}

impl UpdateStatus {
    pub fn description(&self, language: Language) -> String {
        let current = env!("CARGO_PKG_VERSION");
        match self {
            Self::UpToDate => format!(
                "{} {current}.",
                language.text(
                    "No updates. Installed version:",
                    "Новых версий нет. Установлена версия:"
                )
            ),
            Self::Available(version) => format!(
                "{} {version}. {} {current}.\n\n{}\nhttps://github.com/Andrey89711/TerminalNote/releases/tag/v{version}\n\n{}",
                language.text("Version available:", "Доступна версия:"),
                language.text("Installed:", "Установлена:"),
                language.text("Release page:", "Страница релиза:"),
                language.text(
                    "Download and installation each require separate consent. Packages are unsigned.",
                    "Скачивание и установка требуют отдельных согласий. Пакеты не подписаны."
                )
            ),
            Self::NoRelease => language
                .text(
                    "No published stable release found yet.",
                    "Опубликованный стабильный релиз пока не найден."
                )
                .into(),
        }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

fn parse_release(bytes: &[u8], current: &str) -> Result<UpdateStatus> {
    if bytes.len() as u64 > MAX_RESPONSE {
        bail!("GitHub response exceeds the size limit");
    }
    let release: Release =
        serde_json::from_slice(bytes).map_err(|_| anyhow!("Invalid GitHub response"))?;
    if release.draft || release.prerelease {
        bail!("Expected a published stable release");
    }
    if release.tag_name.len() > MAX_TAG_LENGTH {
        bail!("Invalid release tag");
    }
    let tag = release
        .tag_name
        .strip_prefix('v')
        .ok_or_else(|| anyhow!("Invalid release tag"))?;
    let version = Version::parse(tag).map_err(|_| anyhow!("Invalid release version"))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("Expected a stable vX.Y.Z tag");
    }
    let current = Version::parse(current).map_err(|_| anyhow!("Invalid application version"))?;
    Ok(if version > current {
        UpdateStatus::Available(version.to_string())
    } else {
        UpdateStatus::UpToDate
    })
}

fn check() -> Result<UpdateStatus> {
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("TerminalNote/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| anyhow!("Cannot prepare HTTPS update check"))?;
    let response = client
        .get(ENDPOINT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .map_err(|_| anyhow!("Cannot reach GitHub. Check your network and retry."))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(UpdateStatus::NoRelease);
    }
    if response.status() != reqwest::StatusCode::OK {
        bail!("GitHub rejected the check. Try again later.");
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE)
    {
        bail!("GitHub response exceeds the size limit");
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("Cannot read GitHub response"))?;
    parse_release(&bytes, env!("CARGO_PKG_VERSION"))
}

pub type CheckResult = std::result::Result<UpdateStatus, String>;

/// Call only after explicit user consent. Nothing schedules or starts this automatically.
pub fn start_check() -> Result<Receiver<CheckResult>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("terminalnote-update-check".into())
        .spawn(move || {
            // Never expose HTTP payloads, request objects, or underlying network errors.
            let _ = sender.send(check().map_err(|error| error.to_string()));
        })
        .map_err(|_| anyhow!("Cannot start update check"))?;
    Ok(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(tag: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "html_url": "https://untrusted.example/release"
        }))
        .unwrap()
    }

    #[test]
    fn compares_versions_without_downgrades_or_untrusted_links() {
        for (remote, expected) in [
            ("v0.1.0", UpdateStatus::UpToDate),
            ("v0.0.9", UpdateStatus::UpToDate),
            ("v0.10.0", UpdateStatus::Available("0.10.0".into())),
        ] {
            let status = parse_release(&metadata(remote), "0.1.0").unwrap();
            assert_eq!(status, expected);
            for language in [Language::English, Language::Russian] {
                let description = status.description(language);
                assert!(!description.contains("untrusted.example"));
                if matches!(status, UpdateStatus::Available(_)) {
                    assert!(description.contains(
                        "https://github.com/Andrey89711/TerminalNote/releases/tag/v0.10.0"
                    ));
                }
            }
        }
    }

    #[test]
    fn rejects_unstable_malformed_and_overlong_tags() {
        for tag in [
            "1.0.0",
            "V1.0.0",
            "v1.0",
            "v01.0.0",
            "v1.0.0-beta",
            "v1.0.0+build",
            "v1.0.0/evil",
            "https://evil.example",
            "v1.0.0\n",
        ] {
            assert!(parse_release(&metadata(tag), "0.1.0").is_err(), "{tag}");
        }
        assert!(parse_release(&metadata(&"v".repeat(MAX_TAG_LENGTH + 1)), "0.1.0").is_err());
        assert_eq!(
            parse_release(&metadata("v1.0.0"), "invalid")
                .unwrap_err()
                .to_string(),
            "Invalid application version"
        );
    }

    #[test]
    fn rejects_unstable_missing_or_invalid_metadata_and_oversize_bodies() {
        for value in [
            serde_json::json!({"tag_name":"v1.0.0","draft":true,"prerelease":false}),
            serde_json::json!({"tag_name":"v1.0.0","draft":false,"prerelease":true}),
            serde_json::json!({"tag_name":"v1.0.0"}),
            serde_json::json!({"tag_name":42,"draft":false,"prerelease":false}),
        ] {
            assert!(parse_release(&serde_json::to_vec(&value).unwrap(), "0.1.0").is_err());
        }
        assert_eq!(
            parse_release(b"not JSON: private payload", "0.1.0")
                .unwrap_err()
                .to_string(),
            "Invalid GitHub response"
        );
        assert!(parse_release(&vec![b' '; MAX_RESPONSE as usize + 1], "0.1.0").is_err());
        // Exactly the limit is accepted when trailing JSON whitespace fills the body.
        let mut bytes = metadata("v0.1.0");
        bytes.resize(MAX_RESPONSE as usize, b' ');
        assert_eq!(
            parse_release(&bytes, "0.1.0").unwrap(),
            UpdateStatus::UpToDate
        );
    }
}
