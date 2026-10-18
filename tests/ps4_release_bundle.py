"""Verify a deployed PE architecture and exact embedded PS4 helper resources.

Usage: python ps4_release_bundle.py deployed.exe x64|x86 [resource files ...]
This is a packaging check, not an installation or console-compatibility test.
"""
import hashlib
import mmap
from pathlib import Path
import struct
import sys


binary = Path(sys.argv[1]).resolve()
expected_machine = {"x64": 0x8664, "x86": 0x14C}[sys.argv[2]]
with binary.open("rb") as stream:
    digest = hashlib.file_digest(stream, "sha256").hexdigest()
    with mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_READ) as image:
        assert image[:2] == b"MZ", "Not a Windows executable"
        pe = struct.unpack_from("<I", image, 0x3C)[0]
        assert image[pe:pe + 4] == b"PE\0\0", "Invalid PE signature"
        machine = struct.unpack_from("<H", image, pe + 4)[0]
        assert machine == expected_machine, f"Wrong architecture: {machine:#x}"
        for name in sys.argv[3:]:
            resource = Path(name).resolve()
            data = resource.read_bytes()
            assert data, f"Empty dependency: {resource}"
            assert image.find(data) >= 0, f"Missing or stale embedded dependency: {resource}"
            print(f"PASS embedded {resource.name}: {hashlib.sha256(data).hexdigest()}")
print(f"PASS PE {sys.argv[2]}: {binary}")
print(f"SHA256 {digest}")
