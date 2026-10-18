param([string]$Output)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$art = Join-Path $PSScriptRoot '../../docs/ICONS/Xbox 360'
$front = [Drawing.Image]::FromFile((Join-Path $art 'Front.png'))
$back = [Drawing.Image]::FromFile((Join-Path $art 'Back.png'))
function Draw-Fitted($graphics, $source, [int]$x, [int]$y, [int]$width, [int]$height) {
    $scale = [Math]::Min($width / $source.Width, $height / $source.Height)
    $w = [int]($source.Width * $scale)
    $h = [int]($source.Height * $scale)
    $graphics.DrawImage($source, [int]($x + ($width-$w)/2), [int]($y + ($height-$h)/2), $w, $h)
}
function Draw-CoverPanel($graphics, $source, [int]$x) {
    # Aurora maps each entire 450x600 half onto a case. Letterboxing here leaves
    # a visible white slit, so cover panels must fill their destination exactly.
    $attributes = [Drawing.Imaging.ImageAttributes]::new()
    try {
        $attributes.SetWrapMode([Drawing.Drawing2D.WrapMode]::TileFlipXY)
        $rectangle = [Drawing.Rectangle]::new($x, 0, 450, 600)
        $graphics.DrawImage($source, $rectangle, 0, 0, $source.Width, $source.Height, [Drawing.GraphicsUnit]::Pixel, $attributes)
    } finally { $attributes.Dispose() }
}
try {
    # Preserve complete artwork; fill each cover half without outside padding.
    foreach ($kind in @('icon', 'cover')) {
        $width = if ($kind -eq 'icon') { 64 } else { 900 }
        $height = if ($kind -eq 'icon') { 64 } else { 600 }
        $bitmap = [Drawing.Bitmap]::new($width, $height)
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.Clear([Drawing.Color]::White)
            $graphics.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            if ($kind -eq 'icon') { Draw-Fitted $graphics $front 0 0 64 64 }
            else {
                Draw-CoverPanel $graphics $back 0
                Draw-CoverPanel $graphics $front 450
            }
            $bitmap.Save((Join-Path $Output "$kind.png"), [Drawing.Imaging.ImageFormat]::Png)
        } finally { $graphics.Dispose(); $bitmap.Dispose() }
    }
} finally { $front.Dispose(); $back.Dispose() }
$title = @'
<?xml version="1.0" encoding="utf-8"?>
<XboxLiveSubmissionProject Version="2.0.7978.3" xmlns="http://www.xboxlive.com/xlast">
  <GameConfigProject name="Digital360" titleName="Digital 360" multiplayer="false" projectVersion="2.00.0000.0" titleId="0xD3600001" titleType="1" schemaVersion="1.05.0000.0">
    <ProductInformation><Platform name="Xbox 360" id="2"/></ProductInformation>
    <LocalizedStrings nextId="1" defaultLocale="en-US">
      <SupportedLocale locale="en-US"/>
      <LocalizedString id="32768" friendlyName="X_STRINGID_TITLENAME"><Translation locale="en-US">Digital 360</Translation></LocalizedString>
    </LocalizedStrings>
    <Images><Image id="32768" friendlyName="X_IMAGEID_GAME"><Path>icon.png</Path></Image></Images>
  </GameConfigProject>
</XboxLiveSubmissionProject>
'@
[IO.File]::WriteAllText((Join-Path $Output 'Store.xlast'), $title)
