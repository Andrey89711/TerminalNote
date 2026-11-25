//! Explicitly consented Windows executable-only updates. No downloaded scripts execute.
use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};

const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;
const MAX_UNPACKED: u64 = 160 * 1024 * 1024;
const FILES: &[&str] = &[
    "note.exe",
    "README.md",
    "LICENSE",
    "CHANGELOG.md",
    "setup.cmd",
    "setup.ps1",
    "uninstall.cmd",
    "uninstall.ps1",
    "scripts/setup-common.ps1",
    "scripts/uninstall-common.ps1",
    "docs/RELEASING.md",
];

#[derive(Debug)]
pub struct Prepared {
    stage: tempfile::TempDir,
    version: String,
    payload_hash: String,
}
impl Prepared {
    #[cfg(test)]
    pub fn fixture() -> Self {
        Self {
            stage: tempfile::tempdir().unwrap(),
            version: "99.0.0".into(),
            payload_hash: digest(b""),
        }
    }
}
#[derive(Debug)]
pub enum Event {
    Progress(&'static str),
    Done(std::result::Result<Prepared, String>),
}

fn stable(version: &str) -> Result<()> {
    let v = semver::Version::parse(version)?;
    ensure!(
        v.pre.is_empty() && v.build.is_empty() && v.to_string() == version,
        "Invalid stable version"
    );
    ensure!(
        v > semver::Version::parse(env!("CARGO_PKG_VERSION"))?,
        "Update must be newer"
    );
    Ok(())
}
fn asset(version: &str) -> String {
    format!("terminalnote-v{version}-windows-x86_64.zip")
}
fn url_allowed(url: &reqwest::Url, version: &str, filename: &str, redirect: bool) -> bool {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
        || url.fragment().is_some()
    {
        return false;
    }
    match url.host_str() {
        Some("github.com") => {
            url.query().is_none()
                && url.path()
                    == format!("/Andrey89711/TerminalNote/releases/download/v{version}/{filename}")
        }
        Some("release-assets.githubusercontent.com") if redirect => {
            url.path().starts_with("/github-production-release-asset/")
        }
        _ => false,
    }
}
fn download(version: &str, filename: &str, limit: u64) -> Result<Vec<u8>> {
    let v = version.to_owned();
    let f = filename.to_owned();
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(12))
        .timeout(Duration::from_secs(90))
        .user_agent(concat!("TerminalNote/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() <= 3 && url_allowed(attempt.url(), &v, &f, true) {
                attempt.follow()
            } else {
                attempt.error("Untrusted update redirect")
            }
        }))
        .build()?;
    let url = reqwest::Url::parse(&format!(
        "https://github.com/Andrey89711/TerminalNote/releases/download/v{version}/{filename}"
    ))?;
    ensure!(
        url_allowed(&url, version, filename, false),
        "Invalid asset URL"
    );
    let mut attempt = 0;
    let response = loop {
        match client
            .get(url.clone())
            .send()
            .and_then(|r| r.error_for_status())
        {
            Err(error)
                if attempt < 2
                    && (error.is_connect()
                        || error.is_timeout()
                        || error
                            .status()
                            .is_some_and(|s| matches!(s.as_u16(), 502..=504))) =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(300 * attempt));
            }
            result => break result?,
        }
    };
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "Official update asset unavailable"
    );
    ensure!(
        response.content_length().is_none_or(|s| s <= limit),
        "Update exceeds size limit"
    );
    let mut data = Vec::new();
    response.take(limit + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "Update exceeds size limit");
    Ok(data)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn file_digest(path: &Path) -> Result<String> {
    safe_path(path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_ARCHIVE + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_ARCHIVE,
        "Executable exceeds update size limit"
    );
    Ok(digest(&bytes))
}
fn verify_checksum(archive: &[u8], sums: &[u8], filename: &str) -> Result<()> {
    ensure!(sums.len() <= 64 * 1024, "Checksum file too large");
    let text = std::str::from_utf8(sums)?;
    let mut found = None;
    for line in text.lines() {
        if let Some((hash, name)) = line.split_once("  ") {
            if name == filename {
                ensure!(found.is_none(), "Duplicate checksum");
                ensure!(
                    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                    "Invalid checksum"
                );
                found = Some(hash.to_ascii_lowercase());
            }
        }
    }
    ensure!(
        found.as_deref() == Some(digest(archive).as_str()),
        "SHA256 verification failed"
    );
    Ok(())
}
fn validate_archive(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(bytes.len() as u64 <= MAX_ARCHIVE, "Archive too large");
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    ensure!(zip.len() == FILES.len(), "Unexpected archive file count");
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    let mut executable = None;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let name = file.name().to_owned();
        ensure!(
            FILES.contains(&name.as_str()) && names.insert(name.clone()),
            "Unexpected archive path"
        );
        ensure!(
            !file.is_dir()
                && file
                    .unix_mode()
                    .is_none_or(|m| m & 0o170000 == 0 || m & 0o170000 == 0o100000),
            "Non-regular archive member"
        );
        total = total
            .checked_add(file.size())
            .context("Archive size overflow")?;
        ensure!(
            total <= MAX_UNPACKED && file.size() <= MAX_ARCHIVE,
            "Archive expansion limit exceeded"
        );
        ensure!(
            file.size()
                <= file
                    .compressed_size()
                    .saturating_mul(250)
                    .saturating_add(4096),
            "Archive compression ratio exceeded"
        );
        // Read every member to enforce actual size limits and CRC, not just the directory.
        let mut data = Vec::new();
        (&mut file).take(MAX_ARCHIVE + 1).read_to_end(&mut data)?;
        ensure!(data.len() as u64 == file.size(), "Invalid archive size");
        if name == "note.exe" {
            executable = Some(data);
        }
    }
    executable.context("Missing executable")
}

/// Reject symlinks/junctions in every existing path component, before and during apply.
fn safe_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "Update paths must be absolute");
    for ancestor in path.ancestors() {
        ensure!(
            !ancestor
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "Unsafe path"
        );
        {
            let metadata = fs::symlink_metadata(ancestor).context("Cannot inspect update path")?;
            ensure!(!metadata.file_type().is_symlink(), "Update path is a link");
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    metadata.file_attributes() & 0x400 == 0,
                    "Update path is a reparse point"
                );
            }
        }
    }
    Ok(())
}
#[cfg(windows)]
fn owned(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            EqualSid, GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    safe_path(path)?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut token = std::ptr::null_mut();
        ensure!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0,
            "Cannot identify update user"
        );
        let mut size = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        );
        CloseHandle(token);
        ensure!(ok != 0, "Cannot identify update user");
        let user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
        let mut owner = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let result = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        );
        let matches = result == 0 && !owner.is_null() && EqualSid(owner, user.User.Sid) != 0;
        if !descriptor.is_null() {
            LocalFree(descriptor.cast());
        }
        ensure!(matches, "Update path must be owned by the current user");
    }
    Ok(())
}
#[cfg(not(windows))]
fn owned(path: &Path) -> Result<()> {
    safe_path(path)
}

fn installation_owner(target: &Path) -> Result<()> {
    owned(target)?;
    owned(target.parent().context("Missing program directory")?)?;
    let marker = target.with_file_name(".terminalnote-install.json");
    if marker.try_exists()? {
        safe_path(&marker)?;
        owned(&marker)?;
        validate_installation_marker(&marker)?;
    }
    Ok(())
}

fn validate_installation_marker(marker: &Path) -> Result<()> {
    let mut bytes = Vec::new();
    fs::File::open(marker)?.take(4097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "Invalid installation ownership marker");
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    ensure!(
        value["product"] == "TerminalNote" && value["schema"] == 1,
        "Invalid installation ownership marker"
    );
    let version = semver::Version::parse(
        value["version"]
            .as_str()
            .context("Invalid installation version")?,
    )?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "Invalid installation version"
    );
    ensure!(
        version <= semver::Version::parse(env!("CARGO_PKG_VERSION"))?,
        "Installation ownership version mismatch"
    );
    Ok(())
}
fn payload_version(path: &Path, version: &str) -> Result<()> {
    // Only called after archive/hash verification, from the private local stage.
    let mut child = std::process::Command::new(path)
        .current_dir(path.parent().context("Missing payload directory")?)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "Staged version probe failed");
            let mut text = String::new();
            child
                .stdout
                .take()
                .context("Missing version output")?
                .take(256)
                .read_to_string(&mut text)?;
            ensure!(
                text.trim() == format!("note {version}"),
                "Staged executable version mismatch"
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Staged version probe timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn prepare(version: &str, sender: &Sender<Event>) -> Result<Prepared> {
    ensure!(
        cfg!(all(windows, target_arch = "x86_64")),
        "In-app installation is supported only on Windows x86_64"
    );
    stable(version)?;
    let _ = sender.send(Event::Progress(
        "Downloading checksums / Скачивание контрольных сумм",
    ));
    let sums = download(version, "SHA256SUMS.txt", 64 * 1024)
        .map_err(|error| download_error("SHA256SUMS.txt", &error))?;
    let _ = sender.send(Event::Progress("Downloading archive / Скачивание архива"));
    let bytes = download(version, &asset(version), MAX_ARCHIVE)
        .map_err(|error| download_error("ZIP", &error))?;
    let _ = sender.send(Event::Progress("Verifying archive / Проверка архива"));
    verify_checksum(&bytes, &sums, &asset(version))
        .map_err(|_| anyhow::anyhow!("SHA-256 mismatch / Ошибка SHA-256"))?;
    let exe = validate_archive(&bytes)
        .map_err(|_| anyhow::anyhow!("Invalid update ZIP / Недопустимый ZIP обновления"))?;
    stage_payload(version, &exe).map_err(|_| {
        anyhow::anyhow!("Cannot stage update in LocalAppData / Не удалось подготовить обновление в LocalAppData")
    })
}

fn download_error(asset: &str, error: &anyhow::Error) -> anyhow::Error {
    // Never expose a signed CDN URL, proxy credentials, or a local path.
    let cause = error.downcast_ref::<reqwest::Error>();
    let detail = match cause {
        Some(error) if error.is_timeout() => "timeout / тайм-аут",
        Some(error) if error.is_redirect() => "redirect rejected / перенаправление отклонено",
        Some(error) if error.is_connect() => "connection or TLS / соединение или TLS",
        Some(error) if error.is_status() => {
            return anyhow::anyhow!("{asset}: HTTP {}", error.status().unwrap());
        }
        _ => "transfer failed / ошибка передачи",
    };
    anyhow::anyhow!("{asset}: {detail}")
}

fn stage_payload(version: &str, exe: &[u8]) -> Result<Prepared> {
    let root = dirs::data_local_dir().context("Local application data is unavailable")?;
    owned(&root)?;
    let stage = tempfile::Builder::new()
        .prefix("terminalnote-update-")
        .tempdir_in(root)?;
    owned(stage.path())?;
    let payload = stage.path().join("payload.exe");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&payload)?
        .write_all(exe)?;
    // Version probing executes the verified unsigned package only after final consent.
    Ok(Prepared {
        stage,
        version: version.into(),
        payload_hash: digest(exe),
    })
}
pub fn start_download(version: String) -> Result<Receiver<Event>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("terminalnote-update-download".into())
        .spawn(move || {
            let result = prepare(&version, &tx).map_err(|error| error.to_string());
            let _ = tx.send(Event::Done(result));
        })?;
    Ok(rx)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Plan {
    target: PathBuf,
    original_hash: String,
    payload_hash: String,
    version: String,
    pid: u32,
    args: Vec<std::ffi::OsString>,
    cwd: PathBuf,
}
pub fn launch(prepared: &mut Prepared) -> Result<()> {
    ensure!(cfg!(windows), "Installation is supported only on Windows");
    // A failed/timed-out attempt is not reusable: in particular, its ready file
    // must never acknowledge a different helper process. Require a fresh download.
    for name in ["ready", "helper.exe", "plan.json", "result.txt"] {
        match fs::symlink_metadata(prepared.stage.path().join(name)) {
            Ok(_) => bail!("Stale update handoff artifact; download the update again"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let target = std::env::current_exe()?;
    safe_path(&target)?;
    installation_owner(&target)?;
    ensure!(
        target.file_name().is_some_and(|n| n == "note.exe"),
        "Only the running note.exe can be updated"
    );
    safe_path(prepared.stage.path())?;
    owned(prepared.stage.path())?;
    ensure!(
        !prepared
            .stage
            .path()
            .starts_with(target.parent().context("Missing program directory")?),
        "Stage must be outside program directory"
    );
    let payload = prepared.stage.path().join("payload.exe");
    owned(&payload)?;
    ensure!(
        file_digest(&payload)? == prepared.payload_hash,
        "Verified payload changed"
    );
    payload_version(&payload, &prepared.version)?;
    ensure!(
        file_digest(&payload)? == prepared.payload_hash,
        "Verified payload changed"
    );
    let plan = Plan {
        original_hash: file_digest(&target)?,
        target: target.clone(),
        payload_hash: prepared.payload_hash.clone(),
        version: prepared.version.clone(),
        pid: std::process::id(),
        args: std::env::args_os().skip(1).collect(),
        cwd: std::env::current_dir()?,
    };
    let helper = prepared.stage.path().join("helper.exe");
    fs::copy(&target, &helper)?;
    fs::write(
        prepared.stage.path().join("plan.json"),
        serde_json::to_vec(&plan)?,
    )?;
    let mut child = std::process::Command::new(&helper)
        .arg("--apply-terminalnote-update")
        .current_dir(prepared.stage.path())
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait()?.is_some() {
            bail!("Update helper could not prepare the handoff");
        }
        if prepared.stage.path().join("ready").try_exists()? {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Update helper handoff timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    prepared.stage.disable_cleanup(true);
    Ok(())
}

#[cfg(any(windows, test))]
fn transient_sharing_error(error: &anyhow::Error) -> bool {
    #[cfg(windows)]
    {
        error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION only. Do not
            // disguise missing paths, access-control failures or validation errors.
            matches!(error.raw_os_error(), Some(32 | 33))
        })
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

#[cfg(any(windows, test))]
fn original_hash_with_retry(
    target: &Path,
    expected: &str,
    deadline: std::time::Instant,
) -> Result<()> {
    loop {
        // file_digest also validates every path component. Validation failures
        // have no sharing-error source and therefore never enter the retry arm.
        match file_digest(target) {
            Ok(actual) => {
                ensure!(actual == expected, "Original executable changed");
                return Ok(());
            }
            Err(error)
                if transient_sharing_error(&error) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(any(windows, test))]
fn replace(
    target: &Path,
    payload: &Path,
    expected_original: &str,
    expected_payload: &str,
) -> Result<()> {
    replace_with(
        target,
        payload,
        expected_original,
        expected_payload,
        |from, to| fs::rename(from, to),
    )
}
#[cfg(any(windows, test))]
fn replace_with(
    target: &Path,
    payload: &Path,
    expected_original: &str,
    expected_payload: &str,
    install: fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    safe_path(payload)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    original_hash_with_retry(target, expected_original, deadline)?;
    // Complete the write before touching the original. Keep rollback beside target.
    let parent = target.parent().context("Missing executable directory")?;
    let mut next = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(
        &mut fs::File::open(payload)?.take(MAX_ARCHIVE + 1),
        &mut next,
    )?;
    next.as_file().sync_all()?;
    ensure!(
        file_digest(next.path())? == expected_payload,
        "Replacement payload changed"
    );
    let backup = tempfile::NamedTempFile::new_in(parent)?;
    let backup_path = backup.into_temp_path();
    fs::remove_file(&backup_path)?;
    loop {
        safe_path(payload)?;
        original_hash_with_retry(target, expected_original, deadline)?;
        match fs::rename(target, &backup_path) {
            Ok(()) => break,
            Err(e) if std::time::Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.into()),
        }
    }
    if let Err(error) = install(next.path(), target) {
        // Never delete the backup if rollback itself fails.
        if fs::rename(&backup_path, target).is_err() {
            let saved = backup_path.keep()?;
            bail!("Update failed; original retained at {}", saved.display());
        }
        return Err(error.into());
    }
    Ok(())
}

pub fn helper_entry() -> Result<bool> {
    if std::env::args_os().nth(1).as_deref()
        != Some(std::ffi::OsStr::new("--apply-terminalnote-update"))
    {
        return Ok(false);
    }
    #[cfg(not(windows))]
    bail!("In-app installation is supported only on Windows");
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            },
        };
        let me = std::env::current_exe()?;
        safe_path(&me)?;
        ensure!(
            me.file_name().is_some_and(|n| n == "helper.exe"),
            "Invalid helper"
        );
        let stage = me.parent().context("Missing stage")?;
        ensure!(
            stage
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("terminalnote-update-")),
            "Invalid stage"
        );
        safe_path(stage)?;
        owned(stage)?;
        owned(&me)?;
        ensure!(
            stage.parent() == dirs::data_local_dir().as_deref(),
            "Helper must run from local application data"
        );
        let plan_path = stage.join("plan.json");
        owned(&plan_path)?;
        ensure!(fs::metadata(&plan_path)?.len() < 64 * 1024, "Invalid plan");
        let plan: Plan = serde_json::from_slice(&fs::read(plan_path)?)?;
        stable(&plan.version)?;
        safe_path(&plan.target)?;
        installation_owner(&plan.target)?;
        ensure!(
            plan.target.file_name().is_some_and(|n| n == "note.exe")
                && !plan.target.starts_with(stage),
            "Invalid target"
        );
        ensure!(file_digest(&me)? == plan.original_hash, "Untrusted helper");
        let payload = stage.join("payload.exe");
        owned(&payload)?;
        ensure!(
            file_digest(&payload)? == plan.payload_hash,
            "Payload changed"
        );
        // Bind the target to the still-running process before waiting for it to exit.
        unsafe {
            let process = OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                plan.pid,
            );
            ensure!(!process.is_null(), "Cannot identify parent process");
            let mut buffer = vec![0u16; 32768];
            let mut len = buffer.len() as u32;
            let queried = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len);
            let image = PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]));
            if queried == 0 || image != plan.target {
                CloseHandle(process);
                bail!("Parent target mismatch");
            }
            if let Err(error) = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(stage.join("ready"))
            {
                CloseHandle(process);
                return Err(error.into());
            }
            let waited = WaitForSingleObject(process, 120_000);
            CloseHandle(process);
            ensure!(waited == 0, "Parent did not exit");
        }
        payload_version(&payload, &plan.version)?;
        let result = replace(
            &plan.target,
            &payload,
            &plan.original_hash,
            &plan.payload_hash,
        );
        // Result stays in the stage for inspection; never edit user settings.
        fs::write(
            stage.join("result.txt"),
            if result.is_ok() {
                "Update installed"
            } else {
                "Update failed; original retained"
            },
        )?;
        result?;
        std::process::Command::new(&plan.target)
            .args(plan.args)
            .current_dir(plan.cwd)
            .spawn()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "manual network smoke test; uses official assets and never installs"]
    #[cfg(all(windows, target_arch = "x86_64"))]
    fn published_windows_update_can_be_staged() {
        let version = std::env::var("TERMINALNOTE_SMOKE_VERSION").unwrap();
        assert_eq!(
            semver::Version::parse(&version).unwrap().to_string(),
            version
        );
        let filename = super::asset(&version);
        let sums = super::download(&version, "SHA256SUMS.txt", 64 * 1024)
            .map_err(|e| super::download_error("SHA256SUMS.txt", &e))
            .unwrap();
        let zip = super::download(&version, &filename, super::MAX_ARCHIVE)
            .map_err(|e| super::download_error("ZIP", &e))
            .unwrap();
        super::verify_checksum(&zip, &sums, &filename).unwrap();
        super::stage_payload(&version, &super::validate_archive(&zip).unwrap()).unwrap();
    }
    use super::*;
    #[cfg(windows)]
    #[test]
    fn native_launch_rejects_stale_ready_without_handoff() {
        const CHILD: &str = "TERMINALNOTE_STALE_READY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let stage = tempfile::Builder::new()
                .prefix("terminalnote-update-")
                .tempdir_in(dirs::data_local_dir().unwrap())
                .unwrap();
            let payload = stage.path().join("payload.exe");
            fs::copy(std::env::current_exe().unwrap(), &payload).unwrap();
            let mut prepared = Prepared {
                payload_hash: file_digest(&payload).unwrap(),
                stage,
                version: "99.0.0".into(),
            };
            fs::write(prepared.stage.path().join("ready"), b"stale attempt").unwrap();
            let error = launch(&mut prepared).unwrap_err();
            assert!(error.to_string().contains("Stale update handoff artifact"));
            assert!(!prepared.stage.path().join("helper.exe").exists());
            assert!(!prepared.stage.path().join("plan.json").exists());
            assert_eq!(
                fs::read(prepared.stage.path().join("ready")).unwrap(),
                b"stale attempt"
            );
            return;
        }
        // Run the real launch function inside a native executable named note.exe,
        // not a mock worker. A stale acknowledgement must fail before any probe
        // or helper spawn, and the caller remains alive to handle the error.
        let program = tempfile::tempdir().unwrap();
        let executable = program.path().join("note.exe");
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let original = file_digest(&executable).unwrap();
        let output = std::process::Command::new(&executable)
            .args([
                "--exact",
                "update_install::tests::native_launch_rejects_stale_ready_without_handoff",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(file_digest(&executable).unwrap(), original);
    }

    #[cfg(windows)]
    #[test]
    fn replacement_retries_exclusive_read_lock_but_not_changed_hash() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.exe");
        let payload = dir.path().join("payload.exe");
        fs::write(&target, b"original").unwrap();
        fs::write(&payload, b"replacement").unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(450));
            drop(lock);
        });
        replace(
            &target,
            &payload,
            &digest(b"original"),
            &digest(b"replacement"),
        )
        .unwrap();
        release.join().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"replacement");

        fs::write(&target, b"changed").unwrap();
        let started = std::time::Instant::now();
        let error = replace(
            &target,
            &payload,
            &digest(b"original"),
            &digest(b"replacement"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Original executable changed"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(fs::read(&target).unwrap(), b"changed");
    }

    #[test]
    fn urls_reject_credentials_hosts_paths_and_http() {
        let good =
            "https://github.com/Andrey89711/TerminalNote/releases/download/v9.0.0/SHA256SUMS.txt";
        assert!(url_allowed(
            &good.parse().unwrap(),
            "9.0.0",
            "SHA256SUMS.txt",
            false
        ));
        for bad in [
            good.replace("https:", "http:"),
            good.replace("github.com", "evil.example"),
            good.replace("github.com", "user@github.com"),
            good.replace("TerminalNote", "Other"),
            format!("{good}?x=1"),
            format!("{good}#x"),
            "https://release-assets.githubusercontent.com/evil".into(),
        ] {
            assert!(!url_allowed(
                &bad.parse().unwrap(),
                "9.0.0",
                "SHA256SUMS.txt",
                true
            ));
        }
        assert!(url_allowed(&"https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sig=x".parse().unwrap(), "9.0.0", "SHA256SUMS.txt", true));
    }
    fn archive(names: &[&str]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in names {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"synthetic test payload").unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    #[test]
    fn exact_archive_and_checksums() {
        let bytes = archive(FILES);
        assert_eq!(validate_archive(&bytes).unwrap(), b"synthetic test payload");
        let sums = format!("{}  {}\n", digest(&bytes), asset("9.0.0"));
        verify_checksum(&bytes, sums.as_bytes(), &asset("9.0.0")).unwrap();
        assert!(verify_checksum(b"changed", sums.as_bytes(), &asset("9.0.0")).is_err());
        assert!(
            verify_checksum(&bytes, format!("{sums}{sums}").as_bytes(), &asset("9.0.0")).is_err()
        );
        for bad in [
            "../note.exe",
            "/note.exe",
            "scripts/../../note.exe",
            "NOTE.EXE",
            "unknown",
        ] {
            let mut names = FILES.to_vec();
            names[0] = bad;
            assert!(validate_archive(&archive(&names)).is_err());
        }
        assert!(validate_archive(&archive(&FILES[1..])).is_err());
        assert!(validate_archive(b"not zip").is_err());
    }
    #[test]
    fn archive_rejects_symlinks_bombs_and_duplicate_central_paths() {
        let original = archive(FILES);
        let positions: Vec<usize> = original
            .windows(4)
            .enumerate()
            .filter_map(|(i, b)| (b == b"PK\x01\x02").then_some(i))
            .collect();
        for mode in [0o120777u32, 0o040755u32] {
            let mut bytes = original.clone();
            let p = positions[0];
            bytes[p + 5] = 3; // Unix creator OS
            bytes[p + 38..p + 42].copy_from_slice(&(mode << 16).to_le_bytes());
            assert!(validate_archive(&bytes).is_err());
        }
        let mut bytes = original.clone();
        let p = positions[0];
        bytes[p + 24..p + 28].copy_from_slice(&((MAX_UNPACKED + 1) as u32).to_le_bytes());
        assert!(validate_archive(&bytes).is_err());
        let mut bytes = original.clone();
        let p = positions[2]; // LICENSE and the unsafe replacement are both seven bytes.
        bytes[p + 46..p + 53].copy_from_slice(b"../evil");
        assert!(validate_archive(&bytes).is_err());
        let mut bytes = original.clone();
        let p = positions[4]; // setup.cmd and README.md have the same name length.
        bytes[p + 46..p + 55].copy_from_slice(b"README.md");
        assert!(validate_archive(&bytes).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn rejects_junctions_in_target_and_stage_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("destination");
        let junction = root.path().join("junction");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("note.exe"), b"original").unwrap();
        let status = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&destination)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated junction fixture creation failed"
        );
        assert!(safe_path(&junction).is_err());
        assert!(safe_path(&junction.join("note.exe")).is_err());
        fs::remove_dir(&junction).unwrap();
        assert_eq!(fs::read(destination.join("note.exe")).unwrap(), b"original");
    }
    #[test]
    fn validates_installation_ownership_marker_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".terminalnote-install.json");
        fs::write(
            &marker,
            br#"{"product":"Other","schema":1,"version":"0.1.0"}"#,
        )
        .unwrap();
        assert!(validate_installation_marker(&marker).is_err());
        let good = br#"{"product":"TerminalNote","schema":1,"version":"0.1.0","unknown":"keep"}"#;
        fs::write(&marker, good).unwrap();
        validate_installation_marker(&marker).unwrap();
        assert_eq!(fs::read(marker).unwrap(), good);
    }
    #[test]
    fn replacement_failure_preserves_original_and_unknown_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.exe");
        let payload = dir.path().join("payload.exe");
        fs::write(&target, b"original").unwrap();
        fs::write(&payload, b"new").unwrap();
        fs::write(dir.path().join("unknown"), b"keep").unwrap();
        assert!(replace(&target, &payload, "wrong hash", &digest(b"new")).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(replace_with(
            &target,
            &payload,
            &digest(b"original"),
            &digest(b"new"),
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected copy failure",
                ))
            }
        )
        .is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        replace(&target, &payload, &digest(b"original"), &digest(b"new")).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(dir.path().join("unknown")).unwrap(), b"keep");
    }
}
