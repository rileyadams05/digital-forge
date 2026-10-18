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

function Write-ReleaseManifest($Name, $Config, $Destination) {
    $ps3Status = 'PS3 PKG creation is unavailable: CFW2OFW prepares compatible disc games for the OFW data-transfer workflow and make_npdata only encrypts/decrypts EDAT/SDAT data; neither is a general JB-folder-to-installable-PKG builder. PS3DK remains retired. ISO/folder conversions require compatible external CLI tools.'
    @(
        "Digital Forge release: $Name"
        "Platform: $($Config.Os)"
        "Architecture: $($Config.Architecture)"
        "PS3 PKG creation: unavailable until a replacement package builder is verified"
        "PS3 ISO and folder conversions: require compatible configured CLI tools"
        "PS3 dependency status: $ps3Status"
        "PS4 FPKG creation: code is configured for the CyB1K 3.87 publisher. Its SHA256 matches pinned upstream revision c08d922e8998cb413f44fdb005c7d6d7eb1b1f13, but the matching ext/sc.exe dependency fails to launch on the build host with Windows error 623, so package creation was not verified here. Publisher tools are external and not bundled."
        "PS5 ItemzFlow: listens on the selected local interface/port for a PS5 TAR stream, validates an already prepared game folder, and copies that prepared folder into the selected output workspace. TAR extraction, native executable preparation/fake-signing, and creation of a prepared ItemzFlow folder from a raw dump are not included."
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
