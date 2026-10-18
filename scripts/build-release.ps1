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

function Write-ReleaseManifest($Name, $Config, $Destination) {
    $ps3Status = 'PS3 PKG creation is unavailable: CFW2OFW prepares compatible disc games for the OFW data-transfer workflow and make_npdata only encrypts/decrypts EDAT/SDAT data; neither is a general JB-folder-to-installable-PKG builder. PS3DK remains retired. ISO/folder conversions require compatible external CLI tools.'
    @(
        "Digital Forge release: $Name"
        "Platform: $($Config.Os)"
        "Architecture: $($Config.Architecture)"
        "PS3 PKG creation: unavailable until a replacement package builder is verified"
        "PS3 ISO and folder conversions: require compatible configured CLI tools"
        "PS3 dependency status: $ps3Status"
        "PS4 base and update GP4 generation / FPKG creation: OrbisPkgTool v1.0.0 is bundled after verifying its official release archive SHA256 (C19FB3905AECEA32BCA41CD13D5EBA1172B82056AF5DCD69FCFECBF866BC8D63). Synthetic packaging fixtures with non-launchable dummy eboot files passed the Digital Forge backend, GP4 generation, FPKG creation, 8-stage validation, and metadata readback for both base and update categories. Windows error 623 is avoided by using this managed builder instead of the patched publisher; no real-game or PS4 hardware result is claimed."
        "PS5 ItemzFlow: receives a TAR stream, and the generic archive workflow can extract TAR files when 7-Zip is configured. The ItemzFlow backend validates/copies an already prepared native game folder. PS5-specific dump normalization and makefself/executable preparation are not implemented, so a raw TAR is not yet transformed into a ready-to-launch ItemzFlow game folder."
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
        Copy-ManagedTree (Join-Path $repo 'vendor\ps3-pkg-module') (Join-Path $runtime 'ps3-pkg-module')
        Copy-ManagedTree (Join-Path $repo 'vendor\ps3-update-dl') (Join-Path $runtime 'ps3-update-dl')
        Copy-AnyPS5Runtime $destination
        Copy-OrbisPkgToolRuntime $destination
        Write-ReleaseManifest $name $config $destination
    } elseif ($config.Os -eq 'Windows') {
        foreach ($managed in @('ps3-update-dl', 'ps3dk')) {
            $managedPath = Join-Path $runtime $managed
            if (Test-Path -LiteralPath $managedPath) {
                Remove-Item -LiteralPath $managedPath -Recurse -Force
            }
        }
        Copy-ManagedTree (Join-Path $repo 'vendor\ps3-pkg-module') (Join-Path $runtime 'ps3-pkg-module')
        Write-ReleaseManifest $name $config $destination
    } else {
        # Keep the established Python sources/extractor available, but do not
        # copy the Windows-only embedded Python runtime into Linux releases.
        Copy-ManagedTree (Join-Path $repo 'vendor\ps3-pkg-module') (Join-Path $runtime 'ps3-pkg-module')
        Write-ReleaseManifest $name $config $destination
    }
    Write-Host "Deployed $name to $destination"
}
