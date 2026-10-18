param([string]$Xedk = 'C:\Users\Riley\Downloads\XBOX360 SDK 21256.3')
$ErrorActionPreference = 'Stop'
$tools = Join-Path $Xedk 'bin\win32'
$common = Join-Path $Xedk 'Source\Samples\Common'
$output = Join-Path $PSScriptRoot 'build'
New-Item -ItemType Directory -Force -Path $output | Out-Null
$assetSources = Get-Content "$PSScriptRoot/assets/sources.json" -Raw | ConvertFrom-Json
foreach ($source in $assetSources.PSObject.Properties) {
    if ((Get-FileHash -LiteralPath (Join-Path $PSScriptRoot "../../docs/ICONS/Xbox 360/$($source.Name)")).Hash -ne $source.Value) {
        throw 'Artwork changed. Regenerate the Aurora assets with convert-assets.ps1 before building.'
    }
}
& (Join-Path $PSScriptRoot 'artwork.ps1') -Output $output
& (Join-Path $tools 'spac.exe') -nologo -noh -o "$output\title.spa" "$output\Store.xlast"
if ($LASTEXITCODE -ne 0) { throw 'Title icon metadata build failed' }
Push-Location (Join-Path $Xedk 'Source\Samples\Media\Fonts')
try { & (Join-Path $tools 'bundler.exe') Courier_New_11.rdf -o "$output\font.xpr"; if ($LASTEXITCODE -ne 0) { throw 'Font build failed' } } finally { Pop-Location }
$sources = @((Join-Path $PSScriptRoot 'Store.cpp'))
foreach ($name in @('AtgFont','AtgResource','AtgUtil','AtgDevice')) { $sources += Join-Path $common "$name.cpp" }
$objects = @()
foreach ($source in $sources) {
    $object = Join-Path $output (([IO.Path]::GetFileNameWithoutExtension($source)) + '.obj')
    & (Join-Path $tools 'cl.exe') /nologo /c /O2 /Gy /MT /EHsc /D_XBOX /DXBOX /DWIN32_LEAN_AND_MEAN "/I$Xedk\include\xbox" "/I$common" "/Fo$object" $source
    if ($LASTEXITCODE -ne 0) { throw "Xbox compilation failed: $source" }
    $objects += $object
}
& (Join-Path $tools 'link.exe') /nologo /XEX:NO /subsystem:xbox /entry:mainCRTStartup /opt:ref /opt:icf '/include:XboxKrnlBuildNumber' "/libpath:$Xedk\lib\xbox" "/out:$output\Store.exe" $objects libcmt.lib xapilib.lib xnet.lib xboxkrnl.lib d3d9.lib d3dx9.lib xgraphics.lib
if ($LASTEXITCODE -ne 0) { throw 'Xbox linking failed' }
$art = Join-Path $PSScriptRoot '../../docs/ICONS/Xbox 360'
[xml]$config = Get-Content (Join-Path $PSScriptRoot 'xex.xml')
$resources = [ordered]@{ D3600001="$output\title.spa"; FRONT="$art\Front.png"; BACK="$art\Back.png"; COVER="$output\cover.png"; ICON="$output\icon.png"; FONT="$output\font.xpr"; BOXART="$PSScriptRoot\assets\cover.asset"; TILEICON="$PSScriptRoot\assets\icon.asset" }
foreach ($entry in $resources.GetEnumerator()) {
    $section = $config.CreateElement('section')
    $section.SetAttribute('name',$entry.Key)
    $section.SetAttribute('file',[IO.Path]::GetFullPath($entry.Value))
    $section.SetAttribute('memory','RO')
    [void]$config.xex.AppendChild($section)
}
$config.Save("$output\xex-resources.xml")
& (Join-Path $tools 'imagexex.exe') "/in:$output\Store.exe" "/out:$output\default.xex" "/config:$output\xex-resources.xml"
if ($LASTEXITCODE -ne 0) { throw 'Xbox image build failed' }
foreach ($entry in $resources.GetEnumerator()) {
    $extracted = Join-Path $output ("verify-" + $entry.Key + '.bin')
    & (Join-Path $tools 'imagexex.exe') /dump "$output\default.xex" "/sectionname:$($entry.Key)" "/sectionfile:$extracted" | Out-Null
    if ($LASTEXITCODE -ne 0 -or (Get-FileHash -LiteralPath $extracted).Hash -ne (Get-FileHash -LiteralPath $entry.Value).Hash) {
        throw "Embedded resource verification failed: $($entry.Key)"
    }
}
Write-Host 'Verified every embedded resource against its source.'
Copy-Item -LiteralPath "$output\default.xex" -Destination (Join-Path $PSScriptRoot '../../vendor/xbox/digital360.xex')
Write-Host "Built $output\default.xex"
