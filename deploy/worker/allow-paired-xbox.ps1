param(
    [Parameter(Mandatory=$true)][string]$HostAddress,
    [Parameter(Mandatory=$true)][string]$XboxAddress
)
$ErrorActionPreference = 'Stop'
foreach ($address in @($HostAddress, $XboxAddress)) {
    $ip = [Net.IPAddress]::Parse($address)
    if ($ip.AddressFamily -ne [Net.Sockets.AddressFamily]::InterNetwork) { throw 'IPv4 address required.' }
    $bytes = $ip.GetAddressBytes()
    if (-not (($bytes[0] -eq 10) -or ($bytes[0] -eq 192 -and $bytes[1] -eq 168) -or ($bytes[0] -eq 172 -and $bytes[1] -ge 16 -and $bytes[1] -le 31))) { throw 'Private LAN addresses only.' }
}
$ruleName = 'ISO2GOD-Xbox-Worker'
$existing = Get-NetFirewallRule -Name $ruleName -ErrorAction SilentlyContinue
if ($existing) {
    $existing | Get-NetFirewallAddressFilter | Set-NetFirewallAddressFilter -LocalAddress $HostAddress -RemoteAddress $XboxAddress
    $existing | Get-NetFirewallPortFilter | Set-NetFirewallPortFilter -Protocol TCP -LocalPort 36061
} else {
    New-NetFirewallRule -Name $ruleName -DisplayName 'ISO2GOD worker - paired Xbox only' -Direction Inbound -Action Allow -Protocol TCP -LocalAddress $HostAddress -LocalPort 36061 -RemoteAddress $XboxAddress -Profile Any | Out-Null
}
