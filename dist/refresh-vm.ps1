# Rebuild Kestrel and hand the fresh image to the VirtualBox VM.
#
# The VM boots dist\kestrel.vhd rather than build\kestrel.vhd because
# VirtualBox holds the file open while the VM exists, which fights the build.
# The VHD footer carries a fixed UUID, so VirtualBox will refuse to register
# two copies of it at once -- that is why only the dist copy is ever attached.
#
# Usage:   .\dist\refresh-vm.ps1          rebuild, then update the VM
#          .\dist\refresh-vm.ps1 -NoBuild copy the existing build output only

param([switch]$NoBuild)

$ErrorActionPreference = "Stop"

$vbox = "C:\Program Files\Oracle\VirtualBox\VBoxManage.exe"
$root = Split-Path -Parent $PSScriptRoot
$vm = "Kestrel"

if (-not (Test-Path $vbox)) { throw "VBoxManage not found at $vbox" }

# Refusing to touch a running VM is deliberate: overwriting the disk underneath
# a live guest corrupts it.
$running = & $vbox list runningvms
if ($running -match "`"$vm`"") { throw "$vm is running -- shut it down first" }

if (-not $NoBuild) {
    Push-Location $root
    try {
        cargo xtask image --release
        if ($LASTEXITCODE -ne 0) { throw "build failed" }
    } finally {
        Pop-Location
    }
}

# Detach first. VirtualBox keeps a handle on the attached file, and the copy
# would fail with a sharing violation part-way through, leaving a torn image.
& $vbox storageattach $vm --storagectl SATA --port 0 --device 0 --type hdd --medium none
& $vbox closemedium disk "$root\dist\kestrel.vhd" 2>$null

Copy-Item "$root\build\kestrel.vhd" "$root\dist\kestrel.vhd" -Force
Copy-Item "$root\build\kestrel.iso" "$root\dist\kestrel.iso" -Force

& $vbox storageattach $vm --storagectl SATA --port 0 --device 0 --type hdd --medium "$root\dist\kestrel.vhd"
if ($LASTEXITCODE -ne 0) { throw "could not reattach the disk" }

Write-Host "$vm now boots the current build. Serial log: dist\serial.log"
