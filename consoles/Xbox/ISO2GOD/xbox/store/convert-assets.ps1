param([Parameter(Mandatory=$true)][string]$ToolDirectory)
$ErrorActionPreference='Stop'
if ([IntPtr]::Size -ne 4) { throw 'Run this converter with Windows SysWOW64 WindowsPowerShell (32-bit).' }
$ToolDirectory=[IO.Path]::GetFullPath($ToolDirectory)
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition 'using System.Runtime.InteropServices; public static class NativeAssetPath { [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] public static extern bool SetDllDirectory(string path); }'
[void][NativeAssetPath]::SetDllDirectory($ToolDirectory)
Add-Type -Path @("$ToolDirectory/AuroraAssetDll.cs","$ToolDirectory/AuroraAsset.cs") -ReferencedAssemblies @('System.Drawing','System.Core')
$output=Join-Path $PSScriptRoot 'build'
New-Item -ItemType Directory -Force $output | Out-Null
& "$PSScriptRoot/artwork.ps1" -Output $output
$destination=Join-Path $PSScriptRoot 'assets'
New-Item -ItemType Directory -Force $destination | Out-Null
foreach($kind in @('cover','icon')) {
    $img=[Drawing.Image]::FromFile("$output/$kind.png")
    try {
        $asset=New-Object AuroraAssetEditor.Classes.AuroraAsset+AssetFile
        $ok=if($kind -eq 'cover'){$asset.SetBoxart($img,$false)}else{$asset.SetIcon($img,$false)}
        if(!$ok){throw 'Asset conversion failed'}
        $verify=New-Object AuroraAssetEditor.Classes.AuroraAsset+AssetFile -ArgumentList (,$asset.FileData)
        $decoded=if($kind -eq 'cover'){$verify.GetBoxart()}else{$verify.GetIcon()}
        if(!$decoded -or $decoded.Width -ne $img.Width -or $decoded.Height -ne $img.Height){throw 'Asset decode verification failed'}
        [IO.File]::WriteAllBytes("$destination/$kind.asset",$asset.FileData)
    } finally { $img.Dispose() }
}
$sources=[ordered]@{}
foreach($name in @('Front.png','Back.png')) {
    $sources[$name]=(Get-FileHash -LiteralPath "$PSScriptRoot/../../docs/ICONS/Xbox 360/$name").Hash
}
$sources | ConvertTo-Json | Set-Content -LiteralPath "$destination/sources.json" -Encoding ASCII
Write-Output 'Converted and decoded both native Aurora assets; recorded original image hashes.'
