[CmdletBinding()]
param(
    [ValidateSet('WIN-X64', 'WIN-X86', 'LINUX-X64', 'LINUX-X86', 'All')]
    [string]$Platform = 'All'
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$releaseRoot = Join-Path (Split-Path $repo -Parent) 'Releases'

$targets = @{
    'WIN-X64' = @{ CargoTarget = 'x86_64-pc-windows-msvc'; Binary = 'digital-forge.exe'; Source = 'iso2god.exe'; Os = 'Windows'; Architecture = 'x86_64' }
    'WIN-X86' = @{ CargoTarget = 'i686-pc-windows-msvc'; Binary = 'digital-forge.exe'; Source = 'iso2god.exe'; Os = 'Windows'; Architecture = 'i686' }
    'LINUX-X64' = @{ CargoTarget = 'x86_64-unknown-linux-musl'; Binary = 'digital-forge'; Source = 'iso2god'; Os = 'Linux'; Architecture = 'x86_64' }
    'LINUX-X86' = @{ CargoTarget = 'i686-unknown-linux-gnu'; Binary = 'digital-forge'; Source = 'iso2god'; Os = 'Linux'; Architecture = 'i686' }
}

function Copy-ManagedTree([string]$Source, [string]$Destination) {
    if (-not (Test-Path -LiteralPath $Source)) {
        throw "Required release dependency is missing: $Source"
    }
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    $children = Get-ChildItem -LiteralPath $Source -Force | Where-Object { $_.Name -ne '.git' }
    $children | Copy-Item -Destination $Destination -Recurse -Force
    $staleGit = Join-Path $Destination '.git'
    if (Test-Path -LiteralPath $staleGit) {
        Remove-Item -LiteralPath $staleGit -Recurse -Force
    }
}

function Copy-AnyPS5Runtime([string]$Destination) {
    $sourceRoot = Join-Path $repo 'consoles\PlayStation\AnyPS5'
    $manifest = Join-Path $sourceRoot 'SHA256SUMS.txt'
    $binary = Join-Path $sourceRoot 'pkg-to-anyps5.exe'
    $sourceCode = Join-Path $sourceRoot 'pkg-to-anyps5-source'
    foreach ($required in @($manifest, $binary, $sourceCode, (Join-Path $sourceCode 'LICENSE'))) {
        if (-not (Test-Path -LiteralPath $required)) {
            throw "Required AnyPS5 runtime or corresponding source is missing: $required"
        }
    }

    $expected = (Get-Content -LiteralPath $manifest | Where-Object { $_ -match '^([0-9A-Fa-f]{64})\s+pkg-to-anyps5\.exe$' } | Select-Object -First 1)
    if (-not $expected) {
        throw "AnyPS5 checksum manifest has no pkg-to-anyps5.exe entry: $manifest"
    }
    $expectedHash = ($expected -split '\s+')[0].ToUpperInvariant()
    $actualHash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToUpperInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "AnyPS5 executable checksum mismatch. Expected $expectedHash; found $actualHash."
    }

    $runtime = Join-Path $Destination 'runtime\ps5\AnyPS5'
    New-Item -ItemType Directory -Path $runtime -Force | Out-Null
    Copy-Item -LiteralPath $binary -Destination $runtime -Force
    Copy-Item -LiteralPath $manifest -Destination $runtime -Force
    Copy-Item -LiteralPath (Join-Path $sourceRoot 'README.md') -Destination $runtime -Force
    Copy-Item -LiteralPath $sourceCode -Destination (Join-Path $runtime 'pkg-to-anyps5-source') -Recurse -Force
}

function Copy-OrbisPkgToolRuntime([string]$Destination) {
    $archiveHash = 'C19FB3905AECEA32BCA41CD13D5EBA1172B82056AF5DCD69FCFECBF866BC8D63'
    $downloadUrl = 'https://github.com/pearlxcore/OrbisPkgTool/releases/download/v1.0.0/OrbisPkgTool-v1.0.0-win-x64.zip'
    $staging = Join-Path $repo '.tmp\OrbisPkgTool-v1.0.0'
    $archive = Join-Path $repo '.tmp\OrbisPkgTool-v1.0.0-win-x64.zip'
    $source = Join-Path $staging 'OrbisPkgTool-v1.0.0-win-x64'
    $exe = Join-Path $source 'OrbisPkgTool.exe'

    if (-not (Test-Path -LiteralPath $archive)) {
        New-Item -ItemType Directory -Path (Split-Path $archive -Parent) -Force | Out-Null
        Invoke-WebRequest -Uri $downloadUrl -OutFile $archive
    }
    $actualHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToUpperInvariant()
    if ($actualHash -ne $archiveHash) {
        throw "OrbisPkgTool archive checksum mismatch. Expected $archiveHash; found $actualHash."
    }
    if (-not (Test-Path -LiteralPath $exe)) {
        if (Test-Path -LiteralPath $staging) {
            Remove-Item -LiteralPath $staging -Recurse -Force
        }
        New-Item -ItemType Directory -Path $staging -Force | Out-Null
        Expand-Archive -LiteralPath $archive -DestinationPath $staging -Force
    }
    if (-not (Test-Path -LiteralPath $exe)) {
        throw "Verified OrbisPkgTool archive did not contain the expected CLI: $exe"
    }

    $runtime = Join-Path $Destination 'runtime\ps4\OrbisPkgTool'
    New-Item -ItemType Directory -Path $runtime -Force | Out-Null
    Copy-Item -LiteralPath $exe -Destination $runtime -Force
    Copy-Item -LiteralPath (Join-Path $source 'LICENSE') -Destination $runtime -Force
    Copy-Item -LiteralPath (Join-Path $source 'README.md') -Destination $runtime -Force
    @(
        "OrbisPkgTool v1.0.0 archive SHA256: $archiveHash"
        "Source: $downloadUrl"
        'License: MIT (see LICENSE)'
        'Use: GP4 generation, FPKG creation and deep structural validation.'
        'Limit: tool reports PC structural validation; it does not establish installation, launch, or gameplay on PS4 hardware.'
    ) | Set-Content -LiteralPath (Join-Path $runtime 'THIRD-PARTY-NOTICE.txt') -Encoding utf8
}

function Copy-PS3IsoRuntime([string]$Destination) {
    $source = Join-Path (Split-Path $repo -Parent) 'consoles\PlayStation\PS3 ISO Utilities'
    $runtime = Join-Path $Destination 'runtime\ps3iso'
    $msvcSource = Join-Path $source 'windows-msvc-source'
    $files = @{
        'extractps3iso.exe' = 'F8E25A55C5CCEFF384B2B3B890F9FB7926FF5A1BA7B08AEE54A800F7250611A9'
        'makeps3iso.exe' = '1EF8D40D6F0386E7A08A9ABBFA794ED8EB0E2FFA0E7F694E1FCC83298818C579'
        'ps3iso-utils-277db7de-source.tar.gz' = 'CD86F6F77DFD4A2606ADC3C10EFEF6565168D2BABB34C3A559DFBC5F7128F521'
    }
    New-Item -ItemType Directory -Path $runtime -Force | Out-Null
    foreach ($name in $files.Keys) {
        $path = Join-Path $source $name
        if (-not (Test-Path -LiteralPath $path)) {
            throw "Verified PS3 ISO utility component is missing: $path"
        }
        $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToUpperInvariant()
        if ($actual -ne $files[$name]) {
            throw "PS3 ISO utility checksum mismatch for $name. Expected $($files[$name]); found $actual."
        }
        Copy-Item -LiteralPath $path -Destination $runtime -Force
    }
    foreach ($name in @('extractps3iso.c', 'makeps3iso.c', 'dirent.h')) {
        $path = Join-Path $msvcSource $name
        if (-not (Test-Path -LiteralPath $path)) {
            throw "Corresponding locally-built PS3 utility source is missing: $path"
        }
    }
    Copy-Item -LiteralPath $msvcSource -Destination (Join-Path $runtime 'windows-msvc-source') -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $source 'BUILD-NOTES.txt') -Destination $runtime -Force
    @(
        'PS3 ISO utilities by Bucanero / Estwald (Hermes), upstream tag 277db7de.'
        'Source archive is included here; it contains the corresponding GPL-3.0 source and license.'
        'Upstream: https://github.com/bucanero/ps3iso-utils/releases/tag/277db7de'
        'extractps3iso.exe SHA256: F8E25A55C5CCEFF384B2B3B890F9FB7926FF5A1BA7B08AEE54A800F7250611A9 (locally rebuilt from GPL source with MSVC after the upstream MinGW build failed with Windows loader error 623)'
        'makeps3iso.exe SHA256: 1EF8D40D6F0386E7A08A9ABBFA794ED8EB0E2FFA0E7F694E1FCC83298818C579 (locally rebuilt from GPL source with MSVC after the upstream MinGW build failed with Windows loader error 623)'
        'Source archive SHA256: CD86F6F77DFD4A2606ADC3C10EFEF6565168D2BABB34C3A559DFBC5F7128F521'
    ) | Set-Content -LiteralPath (Join-Path $runtime 'THIRD-PARTY-NOTICE.txt') -Encoding utf8
}

function Write-ReleaseManifest($Name, $Config, $Destination) {
    @(
        "Digital Forge release: $Name"
        "Platform: $($Config.Os)"
        "Architecture: $($Config.Architecture)"
        'Legacy FTP/rclone console transfer has been removed. The PS4 update downloader continues to use HTTPS, and the PS5 dump receiver remains a separate feature.'
        "PS3 ISO / JB folder: extractps3iso and makeps3iso from Bucanero/Estwald tag 277db7de were rebuilt locally with MSVC because the upstream MinGW binaries failed to start on this Windows host (loader error 623). Synthetic JB→ISO→JB and split-ISO extraction round-trip tests passed. Upstream GPL source archive, modified MSVC-compatible C source, Win32 directory shim, build notes and GPL-3.0 license are included in runtime/ps3iso. Real commercial game ISO testing was not performed; encrypted ISOs require separate handling."
        "PS3 PKG creation and extraction: unavailable. Digital Forge does not create installable game PKGs from disc dumps or bypass PS3 package licensing/signing. No supported safe PKG extractor is bundled. Existing package files are not converted; the retired PS3DK implementation remains removed."
        "PS4 base and update GP4 generation / FPKG creation: OrbisPkgTool v1.0.0 is bundled after verifying its official release archive SHA256 (C19FB3905AECEA32BCA41CD13D5EBA1172B82056AF5DCD69FCFECBF866BC8D63). Synthetic packaging fixtures with non-launchable dummy eboot files passed the Digital Forge backend, GP4 generation, FPKG creation, 8-stage validation, and metadata readback for both base and update categories. Windows error 623 is avoided by using this managed builder instead of the patched publisher; no real-game or PS4 hardware result is claimed."
        "PS4 automatic update catalogue: Digital Forge queries Sony's public per-title XML, retains and numerically sorts advertised package/version records, and filters using required firmware decoded from system_ver. The user enters the PS4 system software version manually; the PC does not infer console firmware. Selected versions use Sony package manifests and the TLS-validated, digest-checked package-piece downloader. Itemzflow's PATCH_API_URL is private in its public source and is not guessed or queried. The active live tests retrieve Sony catalogue data and download/cache a real update, then verify TLS, package metadata, PKG integrity, and every piece hash; tests fail on live network or validation errors. This does not claim real-game merge or console hardware testing."
        'Automated tests: the normal Cargo suite has no ignored tests. PS3-specific and AnyPS5-specific tests are preserved behind the opt-in deferred-console-integration-tests feature and are excluded from normal test runs while those systems are on hold.'
        "PS5 ItemzFlow: plain TAR input is detected for PS5, extracted with path traversal/link/special-file rejection, scanned for one game root, validated, and copied to the Itemzflow output. Synthetic TAR extraction, traversal rejection, metadata checks, and export tests passed. This requires an already prepared native eboot.bin, sce_sys/param.json, matching param.sfo, and sce_module. Digital Forge does not fake-sign or transform executables; raw dumps missing these prepared files are rejected. No physical PS5 launch test was performed."
        "PS5 AnyPS5: debug-FIH-PKG/folder preparation is bundled and synthetic-fixture tested; output still requires the separate AnyPS5 relinker and is not presented as playable."
        "Build source: $($Config.CargoTarget) / cargo build --release --locked"
    ) | Set-Content -LiteralPath (Join-Path $Destination 'RELEASE-MANIFEST.txt') -Encoding utf8
}

function Stop-RunningReleaseBinary([string]$BinaryPath) {
    $running = Get-Process -ErrorAction SilentlyContinue | Where-Object {
        try {
            $_.Path -eq $BinaryPath
        } catch {
            $false
        }
    }
    foreach ($process in $running) {
        Write-Host "Closing running release process $($process.Id) before deployment..."
        Stop-Process -Id $process.Id -Force -ErrorAction Stop
    }
}

if (-not (Test-Path -LiteralPath $releaseRoot)) {
    throw "The official release root does not exist: $releaseRoot"
}

$names = if ($Platform -eq 'All') { @('WIN-X64', 'WIN-X86', 'LINUX-X64', 'LINUX-X86') } else { @($Platform) }
foreach ($name in $names) {
    $config = $targets[$name]
    $destination = Join-Path $releaseRoot $name
    if (-not (Test-Path -LiteralPath $destination)) {
        throw "Official release directory is missing: $destination"
    }

    Write-Host "Building $name ($($config.CargoTarget))..."
    & cargo build --release --locked --target $config.CargoTarget --bin iso2god
    if ($LASTEXITCODE -ne 0) {
        @(
            "Digital Forge release build blocked for $name."
            "Target: $($config.CargoTarget)"
            "The previous executable was deliberately removed and must not be used as a current release."
            "Install the target's compatible compiler/linker toolchain, then rerun scripts/build-release.ps1."
        ) | Set-Content -LiteralPath (Join-Path $destination 'RELEASE-BUILD-BLOCKED.txt') -Encoding utf8
        throw "Cargo release build failed for $name. Existing release files were not replaced."
    }

    $built = Join-Path $repo (Join-Path "target\$($config.CargoTarget)\release" $config.Source)
    if (-not (Test-Path -LiteralPath $built)) {
        throw "Cargo reported success but the expected executable is missing: $built"
    }

    $blocked = Join-Path $destination 'RELEASE-BUILD-BLOCKED.txt'
    if (Test-Path -LiteralPath $blocked) {
        Remove-Item -LiteralPath $blocked -Force
    }

    # Replace only the managed application binary after a successful build.
    $deployedBinary = Join-Path $destination $config.Binary
    Stop-RunningReleaseBinary $deployedBinary
    Copy-Item -LiteralPath $built -Destination $deployedBinary -Force

    $runtime = Join-Path $destination 'runtime\ps3'
    if ($config.Os -eq 'Windows' -and $config.Architecture -eq 'x86_64') {
        foreach ($retired in @('ps3-pkg-module', 'ps3dk')) {
            $retiredPath = Join-Path $runtime $retired
            if (Test-Path -LiteralPath $retiredPath) {
                Remove-Item -LiteralPath $retiredPath -Recurse -Force
            }
        }
        Copy-ManagedTree (Join-Path $repo 'vendor\ps3-update-dl') (Join-Path $runtime 'ps3-update-dl')
        Copy-AnyPS5Runtime $destination
        Copy-OrbisPkgToolRuntime $destination
        Copy-PS3IsoRuntime $destination
        Write-ReleaseManifest $name $config $destination
    } elseif ($config.Os -eq 'Windows') {
        foreach ($managed in @('ps3-update-dl', 'ps3-pkg-module', 'ps3dk')) {
            $managedPath = Join-Path $runtime $managed
            if (Test-Path -LiteralPath $managedPath) {
                Remove-Item -LiteralPath $managedPath -Recurse -Force
            }
        }
        Write-ReleaseManifest $name $config $destination
    } else {
        # Keep the established Python sources/extractor available, but do not
        # copy the Windows-only embedded Python runtime into Linux releases.
        Write-ReleaseManifest $name $config $destination
    }
    Write-Host "Deployed $name to $destination"
}
