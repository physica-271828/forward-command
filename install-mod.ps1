# Forward Command - install the HOI4 mod component.
# Copies hoi4-mod\forward-command into the user's Paradox mod folder and writes
# the .mod descriptor file that the launcher picks up.
param(
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$modName = "forward-command"
$src = Join-Path $PSScriptRoot "hoi4-mod\$modName"
$paradoxDir = Join-Path $env:USERPROFILE "Documents\Paradox Interactive\Hearts of Iron IV"
$modDir = Join-Path $paradoxDir "mod"

if (-not (Test-Path $src)) { throw "mod source not found: $src" }
if (-not (Test-Path $paradoxDir)) { throw "HOI4 user directory not found: $paradoxDir" }
New-Item -ItemType Directory -Force -Path $modDir | Out-Null

$dest = Join-Path $modDir $modName
$descriptorFile = Join-Path $modDir "$modName.mod"

# Also remove any install under the former mod name so the launcher never
# lists both copies (the folder/descriptor used to be "iron-and-steel").
$legacyDest = Join-Path $modDir "iron-and-steel"
$legacyDescriptor = Join-Path $modDir "iron-and-steel.mod"
if (Test-Path $legacyDest) { Remove-Item -Recurse -Force $legacyDest; Write-Host "removed legacy $legacyDest" }
if (Test-Path $legacyDescriptor) { Remove-Item -Force $legacyDescriptor; Write-Host "removed legacy $legacyDescriptor" }

if ($Uninstall) {
    if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
    if (Test-Path $descriptorFile) { Remove-Item -Force $descriptorFile }
    Write-Host "Forward Command mod uninstalled."
    exit 0
}

# Copy mod folder
if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
# Clean target first: Copy-Item -Recurse MERGES into an existing destination,
# so files deleted from the source (e.g. d_tac_apply_damage.txt) would linger
# in the installed mod forever.
if (Test-Path $dest) {
    Remove-Item -Recurse -Force $dest
    Write-Host "removed stale $dest"
}
Copy-Item -Recurse $src $dest
Write-Host "copied $src -> $dest"

# Preserve the Workshop linkage: the launcher appends remote_file_id to the
# descriptor after the first upload, and dropping it would make the next
# upload create a NEW Workshop item instead of updating the existing one.
$remoteFileId = $null
if (Test-Path $descriptorFile) {
    $existingDescriptor = Get-Content $descriptorFile -Raw
    if ($existingDescriptor -match 'remote_file_id="(\d+)"') {
        $remoteFileId = $Matches[1]
    }
}

# Version follows the repo descriptor (release bumps change it there). A
# hardcoded copy here desyncs the launcher .mod from the mod folder, and
# Workshop uploads then report "no changes" after every release.
$srcDescriptor = Get-Content (Join-Path $src "descriptor.mod") -Raw
if ($srcDescriptor -notmatch '(?m)^version="([^"]+)"') {
    throw "no version= line in $(Join-Path $src 'descriptor.mod')"
}
$modVersion = $Matches[1]

# Launcher .mod descriptor (points at the copied folder)
# ASCII-only: this script is UTF-8 without BOM, and PowerShell 5.1 reads
# .ps1 files as ANSI (GBK) - a non-ASCII char here would corrupt on write.
$descriptor = @"
version="$modVersion"
tags={
	"Gameplay"
	"Military"
}
name="Forward Command"
supported_version="1.19.*"
path="$($dest -replace '\\','/')"
"@
if ($remoteFileId) {
    $descriptor += "`nremote_file_id=`"$remoteFileId`"`n"
}
# Write UTF-8 WITHOUT BOM (PS 5.1 Set-Content -Encoding UTF8 adds a BOM,
# which the Paradox launcher's .mod parser can reject). Note: PS 5.1
# mis-parses a `New-Object` expression inside a method-call argument list,
# so the encoder is built on its own line first.
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($descriptorFile, $descriptor, $utf8NoBom)
Write-Host "wrote $descriptorFile"

# Check save format setting (DESIGN.md §14: save_as_binary=no required)
$settingsFile = Join-Path $paradoxDir "settings.txt"
if (Test-Path $settingsFile) {
    $settings = Get-Content $settingsFile -Raw
    if ($settings -match 'save_as_binary\s*=\s*yes') {
        Write-Warning "settings.txt has save_as_binary=yes - tactical save parsing needs save_as_binary=no"
    } else {
        Write-Host "save format OK (text saves)"
    }
}
Write-Host "Done. Enable 'Forward Command' in the HOI4 launcher."
Write-Host "NOTE: if you also subscribed on the Steam Workshop, enable only ONE copy"
Write-Host "      (two same-name mods enabled together break the game)."
