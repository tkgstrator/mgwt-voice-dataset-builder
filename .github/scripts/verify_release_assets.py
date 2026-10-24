"""Check the complete release archive set before publishing it."""

import stat
import sys
import tarfile
import zipfile
from pathlib import Path


PREFIX = "mgwt-voice-dataset-builder"
ARCHIVES = {
    f"{PREFIX}-linux-amd64.tar.gz": (f"{PREFIX}-linux-amd64", "elf-amd64"),
    f"{PREFIX}-linux-arm64.tar.gz": (f"{PREFIX}-linux-arm64", "elf-arm64"),
    f"{PREFIX}-macos-amd64.tar.gz": (f"{PREFIX}-macos-amd64", "macho-amd64"),
    f"{PREFIX}-macos-arm64.tar.gz": (f"{PREFIX}-macos-arm64", "macho-arm64"),
    f"{PREFIX}-windows-amd64.zip": (f"{PREFIX}-windows-amd64", "pe-amd64"),
}


def _check_architecture(binary: bytes, kind: str, archive: Path) -> None:
    if kind.startswith("elf"):
        machine = 62 if kind == "elf-amd64" else 183
        valid = (
            len(binary) >= 20
            and binary[:6] == b"\x7fELF\x02\x01"
            and int.from_bytes(binary[18:20], "little") == machine
        )
    elif kind.startswith("macho"):
        cpu = 0x01000007 if kind == "macho-amd64" else 0x0100000C
        valid = (
            len(binary) >= 8
            and binary[:4] == b"\xcf\xfa\xed\xfe"
            and int.from_bytes(binary[4:8], "little") == cpu
        )
    else:
        offset = int.from_bytes(binary[0x3C:0x40], "little") if len(binary) >= 0x40 else 0
        valid = (
            len(binary) >= 0x40
            and binary[:2] == b"MZ"
            and offset + 26 <= len(binary)
            and binary[offset : offset + 4] == b"PE\0\0"
            and int.from_bytes(binary[offset + 4 : offset + 6], "little") == 0x8664
            and int.from_bytes(binary[offset + 24 : offset + 26], "little") == 0x20B
        )
    if not valid:
        raise ValueError(f"wrong executable architecture in {archive.name}: {kind}")


def _check_tar(archive: Path, platform: str, binary_name: str) -> bytes:
    expected = {
        f"{platform}/",
        f"{platform}/{binary_name}",
        f"{platform}/README.md",
        f"{platform}/LICENSE",
    }
    with tarfile.open(archive, "r:gz") as package:
        members = package.getmembers()
        names = [member.name.rstrip("/") + ("/" if member.isdir() else "") for member in members]
        if len(names) != len(expected) or set(names) != expected:
            raise ValueError(f"unexpected members in {archive.name}: {names!r}")
        for member, name in zip(members, names):
            if name == f"{platform}/":
                if not member.isdir():
                    raise ValueError(f"not a directory in {archive.name}: {name}")
            elif not member.isfile():
                raise ValueError(f"not a regular file in {archive.name}: {name}")
            elif name.endswith(binary_name) and member.mode & 0o111 == 0:
                raise ValueError(f"executable bit missing in {archive.name}: {name}")
        stream = package.extractfile(f"{platform}/{binary_name}")
        if stream is None:
            raise ValueError(f"missing binary in {archive.name}")
        return stream.read()


def _check_zip(archive: Path, platform: str, binary_name: str) -> bytes:
    expected = {
        f"{platform}/",
        f"{platform}/{binary_name}",
        f"{platform}/README.md",
        f"{platform}/LICENSE",
    }
    with zipfile.ZipFile(archive) as package:
        members = package.infolist()
        names = [member.filename for member in members]
        if len(names) != len(expected) or set(names) != expected:
            raise ValueError(f"unexpected members in {archive.name}: {names!r}")
        for member in members:
            file_type = stat.S_IFMT((member.external_attr >> 16) & 0xFFFF)
            if member.flag_bits & 1:
                raise ValueError(f"encrypted member in {archive.name}: {member.filename}")
            if member.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
                raise ValueError(f"unsupported compression in {archive.name}: {member.filename}")
            if member.filename == f"{platform}/":
                if not member.is_dir() or file_type not in (0, stat.S_IFDIR):
                    raise ValueError(f"not a directory in {archive.name}: {member.filename}")
            elif member.is_dir() or file_type not in (0, stat.S_IFREG):
                raise ValueError(f"not a regular file in {archive.name}: {member.filename}")
        if package.testzip() is not None:
            raise ValueError(f"ZIP CRC mismatch in {archive.name}")
        return package.read(f"{platform}/{binary_name}")


def verify_archives(root: Path) -> None:
    actual = {path.name for path in root.iterdir()}
    if actual != set(ARCHIVES):
        raise ValueError(f"unexpected files in release directory: {sorted(actual)!r}")
    for archive_name, (platform, kind) in ARCHIVES.items():
        archive = root / archive_name
        if archive.is_symlink() or not archive.is_file():
            raise ValueError(f"archive is not a regular file: {archive}")
        binary = (
            _check_zip(archive, platform, f"{PREFIX}.exe")
            if archive.suffix == ".zip"
            else _check_tar(archive, platform, PREFIX)
        )
        _check_architecture(binary, kind, archive)
        print(f"verified {archive_name}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: verify_release_assets.py DIST_DIR")
    verify_archives(Path(sys.argv[1]))
