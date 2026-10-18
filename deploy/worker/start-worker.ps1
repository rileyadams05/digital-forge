# Task Scheduler entry point. No console, browser, or VM window is opened.
$ErrorActionPreference = 'Stop'
$vbox = Join-Path $env:ProgramFiles 'Oracle\VirtualBox\VBoxManage.exe'
$workerName = 'ISO2GOD-Worker'
$info = & $vbox showvminfo $workerName --machinereadable 2>&1
if ($LASTEXITCODE -ne 0) { throw 'The ISO2GOD worker VM is not registered.' }
if ($info -contains 'VMState="running"') { exit 0 }
if (($info -contains 'VMState="poweroff"') -or ($info -contains 'VMState="saved"') -or ($info -contains 'VMState="aborted"')) {
    & $vbox startvm $workerName --type headless | Out-Null
    exit $LASTEXITCODE
}
throw 'The worker is busy changing power state; no action taken.'
