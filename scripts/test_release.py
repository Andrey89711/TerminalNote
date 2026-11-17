"""Release tooling tests use synthetic binaries, never application/user data."""

import contextlib
import io
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest
import zipfile

import release


def pe_fixture(dll="kernel32.dll", delay=False):
    data = bytearray(2048)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x80)
    data[0x80:0x84] = b"PE\0\0"
    struct.pack_into("<HH", data, 0x84, 0x8664, 1)
    struct.pack_into("<H", data, 0x94, 240)
    opt = 0x98
    struct.pack_into("<H", data, opt, 0x20B)
    struct.pack_into("<Q", data, opt + 24, 0x140000000)
    struct.pack_into("<I", data, opt + 108, 16)
    stride = 32 if delay else 20
    directory = 13 if delay else 1
    struct.pack_into("<II", data, opt + 112 + directory * 8, 0x1000, stride * 2)
    struct.pack_into("<IIII", data, opt + 240 + 8, 1536, 0x1000, 1536, 512)
    if delay:
        struct.pack_into("<II", data, 512, 1, 0x1100)
    else:
        struct.pack_into("<I", data, 512 + 12, 0x1100)
    name = dll.encode("ascii") + b"\0"
    data[768:768 + len(name)] = name
    return bytes(data)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n', encoding="utf-8")
        for name in release.DOCUMENTS:
            (self.root / name).write_text(name + "\n", encoding="utf-8")
        self.binary = self.root / "fixture"
        self.binary.write_bytes(pe_fixture())
        # Unexpected workspace data must never enter an archive.
        (self.root / "private-note.txt").write_text("not release content", encoding="utf-8")

    def test_tag_validation(self):
        release.validate_tag("v0.1.0", self.root)
        for tag in ("v0.2.0", "0.1.0", "v0.1.0-rc1", "v00.1.0", "v0.1.0/extra"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.validate_tag(tag, self.root)

    def test_pe_normal_and_delay_imports(self):
        for delay in (False, True):
            self.assertEqual(release.windows_imports(pe_fixture(delay=delay)), ["kernel32.dll"])
            for dll in ("VCRUNTIME140.dll", "MSVCP140.dll", "msvcr120.dll", "concrt140.dll",
                        "ucrtbase.dll", "api-ms-win-crt-runtime-l1-1-0.dll"):
                self.binary.write_bytes(pe_fixture(dll, delay))
                with self.subTest(dll=dll, delay=delay), self.assertRaises(ValueError):
                    release.check_windows(self.binary)

    def test_invalid_binary_fails_closed(self):
        for data in (b"not PE", pe_fixture()[:100]):
            with self.assertRaises((ValueError, struct.error)):
                release.windows_imports(data)
        data = bytearray(pe_fixture())
        struct.pack_into("<H", data, 0x84, 0x14C)
        with self.assertRaises(ValueError):
            release.windows_imports(data)

    def package(self, platform, directory):
        with contextlib.redirect_stdout(io.StringIO()):
            return release.package(platform, self.binary, directory, self.root)

    def test_allowlist_permissions_and_reproducibility(self):
        for platform in release.PLATFORMS:
            first = self.package(platform, self.root / "first")
            second = self.package(platform, self.root / "second")
            self.assertEqual(first.read_bytes(), second.read_bytes())
            expected = [release.PLATFORMS[platform][0], *release.DOCUMENTS]
            if platform == "windows-x86_64":
                with zipfile.ZipFile(first) as archive:
                    self.assertEqual(archive.namelist(), expected)
                    self.assertEqual(archive.read("note.exe"), self.binary.read_bytes())
            else:
                with tarfile.open(first) as archive:
                    self.assertEqual(archive.getnames(), expected)
                    self.assertEqual(archive.getmember("note").mode, 0o755)
                    self.assertTrue(all(m.uid == m.gid == m.mtime == 0 for m in archive.getmembers()))

    def test_checksums_require_both_platforms_and_reject_tampering(self):
        output = self.root / "dist"
        self.package("windows-x86_64", output)
        with self.assertRaises(ValueError):
            release.checksums(output, self.root)
        archive = self.package("linux-x86_64", output)
        release.checksums(output, self.root)
        lines = (output / "SHA256SUMS.txt").read_text(encoding="ascii").splitlines()
        self.assertEqual(len(lines), 2)
        (output / "SHA256SUMS.txt").unlink()
        archive.write_bytes(archive.read_bytes() + b"tampered")
        with self.assertRaises(ValueError):
            release.checksums(output, self.root)

    def test_archive_verification_rejects_extra_files(self):
        archive = self.package("windows-x86_64", self.root / "dist")
        with zipfile.ZipFile(archive, "a") as output:
            output.writestr("private-note.txt", "must not ship")
        with self.assertRaises(ValueError):
            release.verify_archive(archive, "windows-x86_64")


if __name__ == "__main__":
    unittest.main()
