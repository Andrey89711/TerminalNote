# CI/CD and releases

## Continuous integration

Windows and Linux CI use stable Rust to check formatting, compile all targets,
run Clippy with warnings denied and run tests. Tagged release jobs separately
build the distributable binaries after these checks succeed.
Tests use temporary notes and configuration, not personal files.

## Draft release packaging

Stable `vX.Y.Z` tags must match `Cargo.toml`; prerelease suffixes are rejected.
The workflow reruns CI before packaging for Windows x64 and Linux x64. Windows builds use
the static C runtime so users do not need a separate VC++ runtime installation.
Archives contain the executable, documentation and license, not user notes or
configuration. Linux includes `note`, `README.md`, `LICENSE` and `CHANGELOG.md`.
The Windows ZIP additionally includes `setup.cmd`, `setup.ps1`, `uninstall.cmd`,
`uninstall.ps1`, `scripts/setup-common.ps1`, `scripts/uninstall-common.ps1` and
this release guide. Installer/uninstaller tests run only against disposable
directories with real PATH, registry and user settings integration disabled.
Packages are named `terminalnote-vX.Y.Z-windows-x86_64.zip` and
`terminalnote-vX.Y.Z-linux-x86_64.tar.gz`; combined SHA-256 checksums are written to
`SHA256SUMS.txt`. Linux binaries require a compatible system C library.

The release pipeline creates a **draft**, not an automatically published release.
Before approving publication, verify the intended version and commit, review CI,
check archive contents and checksums, and test the binary on each target platform
with disposable notes. Check browsing, editing/saving, links, the command palette
and persistent English/Russian language selection.

Only a maintainer should create and push a version tag after review. Updating
documentation or workflows does not itself commit, push, tag or publish anything.
Do not reuse or move a published tag. Public publication is a separate, explicit
maintainer action after artifact review.

Checksums detect mismatches; they are not code signatures and cannot protect
against a compromised publisher. Keep independent backups of notes.

## Local build

```sh
cargo build --locked --release
```

The executable is `target/release/note` on Linux or
`target/release/note.exe` on Windows. An ordinary local build does not by itself
enable static CRT linking; use the release pipeline's target configuration for
distributable Windows builds.
