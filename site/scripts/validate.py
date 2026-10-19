#!/usr/bin/env python3
"""Dependency-free validation for the static Digital Forge website."""

from html.parser import HTMLParser
import json
from pathlib import Path
import sys
from urllib.parse import unquote, urlsplit

ROOT = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
REQUIRED_PAGES = (
    "index.html",
    "about.html",
    "compatibility.html",
    "formats.html",
    "guides/itemzflow.html",
    "guides/pkg.html",
    "guides/fpkg.html",
    "guides/iso2god.html",
)


class PageParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.links = []
        self.ids = set()
        self.title = False
        self.has_description = False
        self.in_title = False

    def handle_starttag(self, tag, attrs):
        values = dict(attrs)
        if "id" in values:
            self.ids.add(values["id"])
        if tag in {"a", "link", "script"}:
            attribute = "href" if tag in {"a", "link"} else "src"
            if values.get(attribute):
                self.links.append(values[attribute])
        if tag == "meta" and values.get("name", "").lower() == "description" and values.get("content", "").strip():
            self.has_description = True
        if tag == "title":
            self.in_title = True

    def handle_endtag(self, tag):
        if tag == "title":
            self.in_title = False

    def handle_data(self, data):
        if self.in_title and data.strip():
            self.title = True


def main():
    errors = []
    pages = {path.relative_to(ROOT).as_posix(): path for path in ROOT.rglob("*.html")}
    for relative in REQUIRED_PAGES:
        if relative not in pages:
            errors.append(f"Missing required page: {relative}")

    parsed = {}
    for relative, path in pages.items():
        parser = PageParser()
        try:
            parser.feed(path.read_text(encoding="utf-8"))
            parser.close()
        except Exception as exc:  # HTMLParser emits source context for malformed declarations.
            errors.append(f"Could not parse {relative}: {exc}")
            continue
        parsed[relative] = parser
        if not parser.title:
            errors.append(f"Missing page title: {relative}")
        if not parser.has_description:
            errors.append(f"Missing meta description: {relative}")

    for relative, parser in parsed.items():
        source = pages[relative]
        for link in parser.links:
            parts = urlsplit(link)
            if parts.scheme or parts.netloc or link.startswith(("mailto:", "tel:", "data:")):
                continue
            target_name = unquote(parts.path)
            target = (source.parent / target_name).resolve() if target_name else source
            try:
                target.relative_to(ROOT.resolve())
            except ValueError:
                errors.append(f"Link escapes website root: {relative} -> {link}")
                continue
            if not target.is_file():
                errors.append(f"Broken local link: {relative} -> {link}")
            elif parts.fragment:
                target_relative = target.relative_to(ROOT).as_posix()
                target_parser = parsed.get(target_relative)
                if target_parser and unquote(parts.fragment) not in target_parser.ids:
                    errors.append(f"Missing anchor: {relative} -> {link}")

    home = (ROOT / "index.html").read_text(encoding="utf-8")
    required_home_terms = ("Digital Forge", 'id="search-form"', 'id="search-mode"', 'id="site-search"', 'value="guides"', 'value="games"', 'value="emulators"', "What is Digital Forge?", "Submit a successful test", 'href="compatibility.html#success-report"')
    for term in required_home_terms:
        if term not in home:
            errors.append(f"Homepage is missing required content: {term}")
    if "type=\"submit\"" in home or "Search <span" in home:
        errors.append("Homepage search should submit with Enter and must not show a Search button")
    if 'id="home-button"' not in home or 'class="home-control"' not in home:
        errors.append("Homepage is missing its Home control")

    search = (ROOT / "assets/search.js").read_text(encoding="utf-8")
    for term in ("itemflows", "api.github.com/search/issues", "www.rpcs3.net/compatibility?g=", "shadps4-game-compatibility/issues?q=", 'searchMode === "guides"', 'mode?.value === "emulators"', "REPORT_FORM", "noopener,noreferrer"):
        if term not in search:
            errors.append(f"Search integration is missing required behavior marker: {term}")
    if any(marker in search for marker in ("game-identifiers.json.gz", "titlescreen.org/api/v1/search", "loadGameIndex", "lookupGameIdentity")):
        errors.append("Games search should use GitHub Issues only, without a separate title identifier index")
    success_reports = (ROOT / "assets" / "success-reports.js").read_text(encoding="utf-8") if (ROOT / "assets" / "success-reports.js").exists() else ""
    if "api/success-reports" not in success_reports or "github_session" not in success_reports:
        errors.append("Successful test reporting is missing its GitHub sign-in or submission flow")
    if 'id="success-report-form"' not in (ROOT / "compatibility.html").read_text(encoding="utf-8"):
        errors.append("Compatibility page is missing its successful test submission form")

    formats_page = (ROOT / "formats.html").read_text(encoding="utf-8")
    if ('src="assets/formats.js?v=12"' not in formats_page
            or 'href="assets/site.css?v=20261019-supported-matrix-1"' not in formats_page
            or 'id="format-list"' not in formats_page
            or 'id="format-note"' not in formats_page):
        errors.append("Supported formats page is missing its console testing table")
    if 'class="home-control" href="index.html">Home</a>' not in formats_page:
        errors.append("Supported formats page should use the shared Home control")
    if 'class="home-control" href="index.html">Home</a>' not in (ROOT / "about.html").read_text(encoding="utf-8"):
        errors.append("About page should use the shared Home control")
    formats_script = (ROOT / "assets/formats.js").read_text(encoding="utf-8")
    if ("supported-formats.json?v=8" not in formats_script or "textContent" not in formats_script
            or "format-table" not in formats_script):
        errors.append("Supported formats renderer is missing its table data source or safe text rendering")

    generated_formats = ROOT / "assets" / "supported-formats.json"
    if generated_formats.exists():
        try:
            format_data = json.loads(generated_formats.read_text(encoding="utf-8"))
            consoles = format_data.get("consoles")
            fields = ("name", "formats", "convertType", "testingStatus")
            if not isinstance(consoles, list) or not consoles:
                errors.append("Supported formats data has no console entries")
            elif any(
                not isinstance(console, dict)
                or any(not isinstance(console.get(field), str) or not console[field].strip() for field in fields)
                for console in consoles
            ):
                errors.append("Supported formats data has an invalid console entry")
            else:
                names = [console["name"] for console in consoles]
                expected_prefix = ["PlayStation 1", "PlayStation 2", "PlayStation Portable (PSP)", "PlayStation 3", "PlayStation Vita", "PlayStation 4", "PlayStation 5", "Original Xbox", "Xbox 360"]
                if names[:len(expected_prefix)] != expected_prefix:
                    errors.append("Supported formats consoles must follow the requested PlayStation and Xbox release order")
                records = {console["name"]: console for console in consoles}
                if not records.get("PlayStation 5", {}).get("testingStatus", "").startswith("Testing planned with AnyPS5 and a PS5 emulator"):
                    errors.append("PS5 testing must be shown as planned through AnyPS5 and a PS5 emulator")
                for name in ("PlayStation 3", "PlayStation 4", "Xbox 360"):
                    if records.get(name, {}).get("testingStatus") != "Tested on hardware":
                        errors.append(f"{name} hardware testing status must remain unchanged")
                if any("Tested with PC emulator" in item.get("testingStatus", "") for item in consoles):
                    errors.append("No console can be marked emulator-tested before an actual Digital Forge output is tested")
                if "not been tested" not in format_data.get("note", "").lower():
                    errors.append("Supported formats must distinguish planned emulator tests from verified results")
        except (OSError, json.JSONDecodeError) as exc:
            errors.append(f"Generated supported formats data is invalid: {exc}")

    compatibility = (ROOT / "compatibility.html").read_text(encoding="utf-8")
    if 'id="game-result-form"' in compatibility or 'game-report.js' in compatibility:
        errors.append("Successful game results should not use a separate website upload form")
    if "GitHub Issues are reserved for games that were tested and failed" not in compatibility:
        errors.append("Compatibility page should clearly reserve Issues for tested game failures")

    issue_template_path = ROOT.parent / ".github" / "ISSUE_TEMPLATE" / "compatibility-report.yml"
    if issue_template_path.exists():
        issue_template = issue_template_path.read_text(encoding="utf-8")
        if 'title: "[Game failure] "' not in issue_template or "Tested and working" in issue_template:
            errors.append("GitHub compatibility Issues must be for game failures only")

    results_path = ROOT / "assets" / "game-results.json"
    try:
        catalog = json.loads(results_path.read_text(encoding="utf-8"))
        if catalog.get("schema_version") != 1 or not isinstance(catalog.get("games"), list):
            errors.append("Game test result catalog has an invalid schema")
        for record in catalog.get("games", []):
            if (not isinstance(record, dict) or not isinstance(record.get("game_id"), str)
                    or record.get("status") != "working" or not record.get("tested_at")):
                errors.append("Game test result catalog contains an invalid or unreviewed record")
                break
    except (OSError, json.JSONDecodeError) as exc:
        errors.append(f"Game test result catalog is invalid: {exc}")
    individual_results = ROOT / "assets" / "game-results"
    if individual_results.exists():
        for path in individual_results.glob("*.json"):
            try:
                record = json.loads(path.read_text(encoding="utf-8"))
                game = record.get("game")
                if (record.get("schema_version") != 1 or not isinstance(game, dict)
                        or path.stem != str(game.get("game_id", "")).upper()
                        or game.get("status") != "working" or not game.get("tested_at")):
                    errors.append(f"Individual game result is invalid or unapproved: {path.relative_to(ROOT)}")
            except (OSError, json.JSONDecodeError) as exc:
                errors.append(f"Individual game result is invalid: {path.relative_to(ROOT)} ({exc})")

    if not (ROOT / ".nojekyll").exists():
        errors.append("Missing .nojekyll marker")

    if errors:
        print("Website validation failed:")
        for error in errors:
            print(f"- {error}")
        raise SystemExit(1)
    print(f"Website validation passed: {len(pages)} HTML pages, required routes and local links are valid.")


if __name__ == "__main__":
    main()
