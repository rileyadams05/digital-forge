# Digital Forge OrbisPkgTool adjustments

The locally managed OrbisPkgTool build includes two small changes to upstream
OrbisPkgTool 1.0.0 source:

- GP4 serialization retains `TITLE_ID`, `TITLE`, `VERSION`, and `APP_VER` so
  the package builder can preserve the source `param.sfo` identity.
- The generated package SFO uses `CATEGORY=ac` for `pkg_ps4_ac_data` and
  `pkg_ps4_ac_nodata` volumes, `gp` for patch volumes, and `gd` for game
  volumes.
- Base-game and update packages can be merged by OrbisPkgTool's native merge
  workflow. It extracts the supplied packages, overlays the update on the base
  game, creates a validated GP4 project, and builds the merged FPKG with the
  existing OrbisPkgTool package builder. This path does not require Sony's
  proprietary `orbis-pub-cmd.exe`.
- DLC remains a separate add-on FPKG for each selected DLC. Digital Forge does
  not claim that DLC, base game, and update can be combined into one universal
  package.

The tool is built from the MIT-licensed upstream project:

- Source: <https://github.com/pearlxcore/OrbisPkgTool>
- Source revision used: `5cd42a515dc29a453c7abcb93318493bdb5b5e1f`
- License: MIT; preserve the upstream `LICENSE` when redistributing the tool.

These changes do not replace OrbisPkgTool or add a second package builder.
They retain title identity and correct the package category for separately
built DLC FPKGs.
