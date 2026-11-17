"""Standard-library-only release validation; never executes packaged binaries."""

import argparse
import gzip
import hashlib
import io
from pathlib import Path
import re
import stat
import struct
import tarfile
import tomllib
import zipfile


ROOT = Path(__file__).resolve().parent.parent
DOCUMENTS = ("README.md", "LICENSE", "CHANGELOG.md")
WINDOWS_INSTALLER = ("setup.cmd", "setup.ps1", "uninstall.cmd", "uninstall.ps1",
                     "scripts/setup-common.ps1", "scripts/uninstall-common.ps1",
                     "docs/RELEASING.md")
PLATFORMS = {"windows-x86_64": ("note.exe", ".zip"),
             "linux-x86_64": ("note", ".tar.gz")}


def version(root=ROOT):
    return tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]


def validate_tag(tag, root=ROOT):
    if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("Release tag must be exactly vX.Y.Z (no prerelease or leading zeros)")
    if tag != f"v{version(root)}":
        raise ValueError("Release tag must match the package version in Cargo.toml")


def windows_imports(data):
    """Parse PE32+ normal and delay imports, rejecting malformed/foreign images."""
    if data[:2] != b"MZ":
        raise ValueError("Not a PE executable")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("Invalid PE signature")
    machine, sections = struct.unpack_from("<HH", data, pe + 4)
    optional_size = struct.unpack_from("<H", data, pe + 20)[0]
    opt = pe + 24
    if machine != 0x8664 or struct.unpack_from("<H", data, opt)[0] != 0x20B:
        raise ValueError("Expected Windows x64 PE32+")
    if optional_size < 240 or struct.unpack_from("<I", data, opt + 108)[0] < 14:
        raise ValueError("Missing PE import directories")
    image_base = struct.unpack_from("<Q", data, opt + 24)[0]
    table = opt + optional_size

    def offset(rva, size=1):
        for index in range(sections):
            section = table + index * 40
            _, virtual_address, raw_size, raw = struct.unpack_from("<IIII", data, section + 8)
            delta = rva - virtual_address
            if 0 <= delta and delta + size <= raw_size:
                if raw + delta + size <= len(data):
                    return raw + delta
        raise ValueError("PE RVA outside file-backed section")

    imports = []
    for directory, stride in ((1, 20), (13, 32)):
        rva, size = struct.unpack_from("<II", data, opt + 112 + directory * 8)
        if not rva and not size:
            continue
        if not rva or size < stride:
            raise ValueError("Invalid PE import directory")
        terminated = False
        for delta in range(0, size - stride + 1, stride):
            entry = offset(rva + delta, stride)
            fields = struct.unpack_from("<" + "I" * (stride // 4), data, entry)
            if not any(fields):
                terminated = True
                break
            name_rva = fields[3] if directory == 1 else fields[1]
            if directory == 13 and not fields[0] & 1:
                name_rva -= image_base
            name = bytearray()
            for index in range(256):
                char = data[offset(name_rva + index)]
                if not char:
                    break
                name.append(char)
            else:
                raise ValueError("Unterminated DLL name")
            imports.append(name.decode("ascii").lower())
        if not terminated:
            raise ValueError("Unterminated PE import directory")
    if not imports:
        raise ValueError("Executable has no imports; cannot verify runtime dependencies")
    return imports


def check_windows(binary):
    imports = windows_imports(Path(binary).read_bytes())
    forbidden = [name for name in imports if re.match(
        r"(?:vcruntime|msvcp|msvcr|concrt|vcomp|vcamp|ucrtbase|api-ms-win-crt-)", name)]
    if forbidden:
        raise ValueError(f"Dynamic C/C++ runtime dependencies: {', '.join(forbidden)}")
    print("Verified Windows x64 imports: " + ", ".join(sorted(imports)))


def archive_name(platform, root=ROOT):
    return f"terminalnote-v{version(root)}-{platform}{PLATFORMS[platform][1]}"


def archive_members(platform):
    return [PLATFORMS[platform][0], *DOCUMENTS,
            *(WINDOWS_INSTALLER if platform == "windows-x86_64" else ())]


def verify_archive(path, platform):
    expected = archive_members(platform)
    if platform == "windows-x86_64":
        with zipfile.ZipFile(path) as archive:
            if archive.namelist() != expected or archive.testzip() is not None:
                raise ValueError("ZIP must contain only the ordered release allowlist")
            for member in archive.infolist():
                if member.is_dir() or not stat.S_ISREG(member.external_attr >> 16):
                    raise ValueError("ZIP contains a non-file")
    else:
        with tarfile.open(path, "r:gz") as archive:
            members = archive.getmembers()
            if [member.name for member in members] != expected:
                raise ValueError("TAR must contain only the ordered release allowlist")
            for member in members:
                if not member.isfile() or member.mode != (0o755 if member.name == "note" else 0o644):
                    raise ValueError("TAR contains a non-file or incorrect permissions")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def package(platform, binary, output, root=ROOT):
    binary, output = Path(binary), Path(output)
    if platform == "windows-x86_64":
        check_windows(binary)
    files = [(PLATFORMS[platform][0], binary.read_bytes())]
    files += [(name, (root / name).read_bytes()) for name in archive_members(platform)[1:]]
    output.mkdir(parents=True, exist_ok=True)
    path = output / archive_name(platform, root)
    # Fixed ordering, timestamps, owner metadata and permissions. This makes
    # archives reproducible for identical inputs, not Rust binaries reproducible.
    if platform == "windows-x86_64":
        with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for name, content in files:
                info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = (0o100644 << 16)
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, content)
    else:
        with path.open("wb") as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for name, content in files:
                    info = tarfile.TarInfo(name)
                    info.size = len(content)
                    info.mode = 0o755 if name == "note" else 0o644
                    archive.addfile(info, io.BytesIO(content))
    verify_archive(path, platform)
    (output / f"{path.name}.sha256").write_text(f"{digest(path)}  {path.name}\n", encoding="ascii")
    print(f"Created and verified {path}")
    return path


def checksums(directory, root=ROOT):
    directory = Path(directory)
    names = sorted(archive_name(platform, root) for platform in PLATFORMS)
    expected = set(names + [name + ".sha256" for name in names])
    if {path.name for path in directory.iterdir()} != expected:
        raise ValueError("Unexpected or missing release artifacts")
    lines = []
    for name in names:
        path = directory / name
        line = f"{digest(path)}  {name}\n"
        if (directory / (name + ".sha256")).read_text(encoding="ascii") != line:
            raise ValueError(f"Checksum mismatch: {name}")
        platform = next(p for p in PLATFORMS if archive_name(p, root) == name)
        verify_archive(path, platform)
        lines.append(line)
    (directory / "SHA256SUMS.txt").write_text("".join(lines), encoding="ascii")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("validate-tag").add_argument("tag")
    commands.add_parser("check-windows").add_argument("binary", type=Path)
    pack = commands.add_parser("package")
    pack.add_argument("--platform", choices=PLATFORMS, required=True)
    pack.add_argument("--binary", type=Path, required=True)
    pack.add_argument("--output", type=Path, required=True)
    commands.add_parser("checksums").add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "validate-tag":
        validate_tag(args.tag)
    elif args.command == "check-windows":
        check_windows(args.binary)
    elif args.command == "package":
        package(args.platform, args.binary, args.output)
    else:
        checksums(args.directory)


if __name__ == "__main__":
    main()
