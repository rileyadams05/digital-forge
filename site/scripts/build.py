#!/usr/bin/env python3
"""Prepare a clean, static-only GitHub Pages artifact."""

import shutil
import sys
import os
import json
from pathlib import Path

SOURCE = Path(__file__).resolve().parents[1]


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: python site/scripts/build.py OUTPUT_DIRECTORY")
    output = Path(sys.argv[1]).resolve()
    if output.exists():
        raise SystemExit(f"Refusing to overwrite existing output directory: {output}")
    output.mkdir(parents=True)
    for item in (".nojekyll", "CNAME", "index.html", "about.html", "compatibility.html", "formats.html"):
        shutil.copy2(SOURCE / item, output / item)
    for directory in ("assets", "guides"):
        shutil.copytree(SOURCE / directory, output / directory)
    api_url = os.environ.get("DIGITAL_FORGE_SUCCESS_API", "").strip().rstrip("/")
    if api_url:
        if not api_url.startswith("https://"):
            raise SystemExit("DIGITAL_FORGE_SUCCESS_API must be an HTTPS URL")
        config = output / "assets" / "success-config.js"
        config.write_text("window.DF_SUCCESS_API = " + json.dumps(api_url) + ";\n", encoding="utf-8")
    print(f"Prepared static Pages artifact in {output}")


if __name__ == "__main__":
    main()
