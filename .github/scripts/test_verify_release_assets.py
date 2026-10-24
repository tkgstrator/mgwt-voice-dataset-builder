import io
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

from verify_release_assets import ARCHIVES, verify_archives


def binary_bytes(kind: str) -> bytes:
    data = bytearray(128)
    if kind == "elf-amd64":
        data[:6] = b"\x7fELF\x02\x01"
        data[18:20] = (62).to_bytes(2, "little")
    elif kind == "elf-arm64":
        data[:6] = b"\x7fELF\x02\x01"
        data[18:20] = (183).to_bytes(2, "little")
    elif kind == "macho-amd64":
        data[:4] = b"\xcf\xfa\xed\xfe"
        data[4:8] = (0x01000007).to_bytes(4, "little")
    elif kind == "macho-arm64":
        data[:4] = b"\xcf\xfa\xed\xfe"
        data[4:8] = (0x0100000C).to_bytes(4, "little")
    elif kind == "pe-amd64":
        data[:2] = b"MZ"
        data[0x3C:0x40] = (0x40).to_bytes(4, "little")
        data[0x40:0x44] = b"PE\x00\x00"
        data[0x44:0x46] = (0x8664).to_bytes(2, "little")
        data[0x58:0x5A] = (0x20B).to_bytes(2, "little")
    else:
        raise AssertionError(kind)
    return bytes(data)


def create_archives(root: Path) -> None:
    for archive_name, (platform, kind) in ARCHIVES.items():
        binary_name = "mgwt-voice-dataset-builder.exe" if kind == "pe-amd64" else "mgwt-voice-dataset-builder"
        content = {
            f"{platform}/{binary_name}": binary_bytes(kind),
            f"{platform}/README.md": b"README",
            f"{platform}/LICENSE": b"MIT",
        }
        archive = root / archive_name
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
                package.writestr(f"{platform}/", b"")
                for name, data in content.items():
                    package.writestr(name, data)
        else:
            with tarfile.open(archive, "w:gz") as package:
                directory = tarfile.TarInfo(f"{platform}/")
                directory.type = tarfile.DIRTYPE
                package.addfile(directory)
                for name, data in content.items():
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    member.mode = 0o755 if name.endswith(binary_name) else 0o644
                    package.addfile(member, io.BytesIO(data))


class VerifyReleaseAssetsTests(unittest.TestCase):
    def test_windows_amd64_archive_is_required(self) -> None:
        self.assertIn("mgwt-voice-dataset-builder-windows-amd64.zip", ARCHIVES)

    def test_valid_archives_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            create_archives(root)
            verify_archives(root)

    def test_extra_archive_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            create_archives(root)
            (root / "unexpected.txt").write_text("oops")
            with self.assertRaisesRegex(ValueError, "unexpected files"):
                verify_archives(root)

    def test_wrong_architecture_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            create_archives(root)
            platform, _ = ARCHIVES["mgwt-voice-dataset-builder-linux-amd64.tar.gz"]
            archive = root / "mgwt-voice-dataset-builder-linux-amd64.tar.gz"
            archive.unlink()
            with tarfile.open(archive, "w:gz") as package:
                directory_info = tarfile.TarInfo(f"{platform}/")
                directory_info.type = tarfile.DIRTYPE
                package.addfile(directory_info)
                for name, data in (
                    ("mgwt-voice-dataset-builder", binary_bytes("elf-arm64")),
                    ("README.md", b"README"),
                    ("LICENSE", b"MIT"),
                ):
                    member = tarfile.TarInfo(f"{platform}/{name}")
                    member.size = len(data)
                    member.mode = 0o755 if name == "mgwt-voice-dataset-builder" else 0o644
                    package.addfile(member, io.BytesIO(data))
            with self.assertRaisesRegex(ValueError, "architecture"):
                verify_archives(root)

    def test_windows_wrong_machine_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            create_archives(root)
            platform = "mgwt-voice-dataset-builder-windows-amd64"
            archive = root / f"{platform}.zip"
            wrong = bytearray(binary_bytes("pe-amd64"))
            wrong[0x44:0x46] = (0xAA64).to_bytes(2, "little")
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
                package.writestr(f"{platform}/", b"")
                package.writestr(f"{platform}/mgwt-voice-dataset-builder.exe", wrong)
                package.writestr(f"{platform}/README.md", b"README")
                package.writestr(f"{platform}/LICENSE", b"MIT")
            with self.assertRaisesRegex(ValueError, "architecture"):
                verify_archives(root)

    def test_windows_extra_zip_member_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            create_archives(root)
            platform = "mgwt-voice-dataset-builder-windows-amd64"
            archive = root / f"{platform}.zip"
            with zipfile.ZipFile(archive, "a") as package:
                package.writestr(f"{platform}/secret.txt", b"unexpected")
            with self.assertRaisesRegex(ValueError, "unexpected members"):
                verify_archives(root)


if __name__ == "__main__":
    unittest.main()
