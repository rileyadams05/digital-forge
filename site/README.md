# Digital Forge website

This is the source for the static Digital Forge support website. It uses plain HTML, CSS and
JavaScript with no package installation or static-site generator.

## Pages and guides

- `index.html` is the short search-first homepage.
- `about.html` contains the project overview.
- `formats.html` shows one compact list of the format/workflow folder names currently under `consoles/Nintendo`, `consoles/PlayStation` and `consoles/Xbox`.
- `guides/` contains focused guides. Add a new standalone HTML guide there, then add its title,
  relative URL, search keys and short result summary to `assets/search.js`.
- `compatibility.html` explains game status and how to report tested failures.

Successful test reports are stored as one JSON file per game under assets/game-results/. A report
enters this directory through the server-side GitHub App pull-request flow; merging the review PR
publishes it to the site. The legacy catalog stays available for previously approved results.

Keep page links relative so they continue to work at the custom domain root. Add a useful page
title and meta description to every new page. External issue content is escaped and API links are
accepted only when they point to the specific upstream host needed by that result.

## Search sources

- Games look up the entered ID in its reviewed per-game file and the legacy assets/game-results.json catalog, then search the public GitHub
  Issues API for matching failure reports. Only successful results in the reviewed JSON catalog
  are shown as `Tested working`; a matching failure recommends avoiding the game until a newer
  successful result is approved. A game with no approved result or failure report is `Unknown`.
  Results and any matching failure links appear in one status card. API failures and rate limits
  are reported separately from game status. Successful test results are submitted through the
  GitHub sign-in flow on the compatibility page and become public only after a reviewer merges the
  generated pull request. The server-side setup is documented in site-api/README.md.
- Emulators searches take a console name or short name followed by a game ID, then open that
  emulator's compatibility source directly. The mapped sources are DuckStation (PS1), PCSX2 (PS2),
  RPCS3 (PS3), shadPS4 (PS4), Vita3K (PS Vita), Azahar (3DS), xemu (Original Xbox), and Xenia
  Canary (Xbox 360). RPCS3 and the GitHub report trackers filter by ID; xemu opens the matching
  title-ID page. List pages without a stable ID query use a browser text fragment as a best-effort
  location hint. Other catalog consoles report that no dedicated compatibility source has been
  verified; PS5 is excluded as requested. Searches leave the Digital Forge results area empty and
  do not use a general-purpose search engine.
- The GitHub failure form opens for the visitor to review and submit; the website never uploads a
  diagnostic log or creates an issue automatically. Successful results are not filed as issues.

## Supported formats page

`assets/supported-formats.json` contains the exact top-level folder names verified under the
Nintendo, PlayStation and Xbox console directories. The page displays extensions with a leading dot
and lowercase spelling, while directory/workflow labels such as `itemzflow`, `JB folder` and
`NoNpDrm folder` remain names. ISO2GOD is displayed as `GOD`; do not claim `.god` is a filename
extension. Keep the console grouping, order and exact source names in the JSON snapshot when these
folders change. Do not add entries from a broad platform catalogue or application matrix.

The community prompt links to the Vortex Prime Discord. After joining, visitors should open the
Community tab and click Request Page.

## Validate and deploy

Run these checks from the repository root:

```sh
node --check site/assets/search.js
node site/scripts/search-smoke.mjs
python3 site/scripts/validate.py
python3 site/scripts/build.py /tmp/digital-forge-pages-preview
python3 site/scripts/validate.py /tmp/digital-forge-pages-preview
```

`.github/workflows/pages.yml` runs those checks for pull requests and deploys when files under
`site/`, the relevant format-matrix sources, or the workflow change on the default branch (`master`). Manual deployment is also available
from GitHub Actions using **Deploy Digital Forge website → Run workflow**, on `master`. The Pages
artifact contains only the HTML pages, generated format data, CSS/JavaScript assets, guides and
`.nojekyll`; developer scripts and this README are excluded. Pages is already published; future
website and format-matrix changes deploy automatically from `master`.

The Digital Forge repository is `rileyadams05/digital-forge`; its intended public URL is
`https://digital-forge.vortex-prime-emu.com/`. The Vortex Prime site remains a separate website
in its own repository. Domain and GitHub App setup for the optional success-report flow is
documented in `site-api/README.md`.
