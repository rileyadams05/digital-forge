"""Opt-in real PKG verification through the existing independent reader.

Checks every GP4 input against the published file inventory; extracts selected
files and compares SHA-256. Does not implement PKG parsing or generation.
Usage: script OrbisPkgTool.exe project.gp4 output.pkg new-extraction-directory
              [relative payload paths to hash ...]
"""
import hashlib
from pathlib import Path
import subprocess
import sys
import xml.etree.ElementTree as ET


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    tool, gp4, pkg, extraction = map(lambda p: Path(p).resolve(), sys.argv[1:5])
    if extraction.exists():
        raise RuntimeError("Use a new extraction directory to avoid stale-file verification")
    expected = {}
    for entry in ET.parse(gp4).iter("file"):
        target = entry.attrib["targ_path"].replace("\\", "/")
        if target.endswith("/"):
            continue
        source = Path(entry.attrib["orig_path"])
        if not source.is_absolute():
            source = gp4.parent / source
        with source.open("rb"):
            pass  # Assert readable, not merely existent.
        expected[target] = source
    listing = subprocess.run([str(tool), "list", str(pkg)], check=True,
                             capture_output=True, text=True).stdout
    inventory = {}
    for line in listing.splitlines():
        if line.startswith("F "):
            _, size, path = line.split(" ", 2)
            inventory[path.casefold()] = (path, int(size))
    mapped = {}
    for target, source in expected.items():
        candidates = ["Image0/" + target]
        if target.casefold().startswith("sce_sys/"):
            candidates.append("Sc0/" + target[len("sce_sys/"):])
        found = [inventory[c.casefold()] for c in candidates if c.casefold() in inventory]
        if not found:
            raise RuntimeError("Missing published file: " + target)
        actual = found[0]
        if actual[1] != source.stat().st_size:
            raise RuntimeError(f"Size mismatch: {target}: {source.stat().st_size} != {actual[1]}")
        mapped[target] = actual[0]
    print(f"PASS: all {len(expected)} GP4 inputs readable and present at the expected size")
    extraction.mkdir(parents=True)
    for index, target in enumerate(sys.argv[5:]):
        internal = mapped[target]
        # The reader's single-file mode flattens the filename within Image0.
        # Isolate each extraction to prevent equal basenames hiding a failure.
        sample_dir = extraction / str(index)
        subprocess.run([str(tool), "extract", f"{pkg}:{internal}", str(sample_dir)], check=True)
        files = [p for p in sample_dir.rglob("*") if p.is_file()]
        if len(files) != 1:
            raise RuntimeError(f"Expected exactly one extracted file for {target}: {files}")
        actual = files[0]
        digest = sha256(actual)
        if digest != sha256(expected[target]):
            raise RuntimeError("SHA-256 mismatch: " + target)
        print(f"SHA256 MATCH {target}: {digest}")
    print(f"PASS: {len(sys.argv[5:])} selected real payload files independently extracted and hashed")


if __name__ == "__main__":
    main()
