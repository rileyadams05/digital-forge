"""Regression for publisher-generated localized DDS inputs (no copyrighted fixture).

Usage: python tests/ps4_gp4_system_resources.py path/to/OrbisPkgTool.exe
This exercises the actual GP4 generator, not a replacement packaging implementation.
"""
import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET


def main():
    tool = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="df-gp4-regression-") as tmp:
        root = Path(tmp)
        source = root / "payload"
        generated = {
            f"sce_sys/{kind}{suffix}.dds"
            for kind in ("icon0", "pic0", "pic1")
            for suffix in ("", "_00", "_05", "_07", "_21", "_30")
        }
        preserved = {
            name.replace(".dds", ".png") for name in generated
        } | {"textures/icon0_05.dds", "sce_sys/icon0_custom.dds", "eboot.bin"}
        patch_only = {"sce_sys/origin-deltainfo.dat"}
        all_files = generated | preserved | patch_only
        for name in all_files:
            path = source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(name.encode())
        before = {name: hashlib.sha256((source / name).read_bytes()).hexdigest()
                  for name in all_files}
        for strict, patch in ((False, False), (True, False), (True, True)):
            project = root / f"strict-{strict}-patch-{patch}.gp4"
            args = [tool, "gp4gen", str(source), "--out", str(project)]
            if strict:
                args.append("--strict-gengp4")
            if patch:
                args.append("--patch")
            subprocess.run(args, check=True, capture_output=True, text=True)
            actual = {f.attrib["targ_path"] for f in ET.parse(project).iter("file")}
            expected = (preserved | (patch_only if patch else set())) if strict else all_files
            assert actual == expected, (strict, actual ^ expected)
        after = {name: hashlib.sha256((source / name).read_bytes()).hexdigest()
                 for name in all_files}
        assert before == after, "GP4 generation modified source artwork"
    print("PASS: generated DDS excluded; patch-origin metadata excluded only from base publication; source hashes preserved")


if __name__ == "__main__":
    main()
