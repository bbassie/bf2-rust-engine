<#
.SYNOPSIS
Builds the Windows release package: dist\bf2-rust-engine-<version>-windows.zip with client.exe,
server.exe, bf2-import.exe, a player README and third-party notices. No BF2 content.

.DESCRIPTION
  powershell -ExecutionPolicy Bypass -File scripts\package.ps1                  # build + package
  powershell -ExecutionPolicy Bypass -File scripts\package.ps1 -SkipBuild       # package existing binaries
  powershell -ExecutionPolicy Bypass -File scripts\package.ps1 -CargoProfile dist    # thin LTO (much slower build)

The version comes from [workspace.package] in Cargo.toml. Each binary is built on its own
(`cargo build -p ...`): one invocation for all three would unify the server's Bevy features
with the client's. Release builds use the embedded copies of the shaders (`embedded_shader!`
reads the .wgsl sources only in dev builds), so the package runs from any folder.

The script fails if anything but the expected files ends up in the package, and if any file
in it looks like converted BF2 content (see docs/ARCHITECTURE.md, "Packaging").
#>
param(
    [ValidateSet('release', 'dist')]
    [string]$CargoProfile = 'release',
    [string]$TargetDir = '',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# --- Version ---
$cargoToml = Get-Content -Raw (Join-Path $root 'Cargo.toml')
if ($cargoToml -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    throw 'no version in [workspace.package] of Cargo.toml'
}
$version = $Matches[1]
$name = "bf2-rust-engine-$version-windows"
Write-Host "Packaging $name ($CargoProfile build)"

if ($TargetDir -eq '') {
    $TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
}
$env:CARGO_TARGET_DIR = $TargetDir
$profileArgs = @(if ($CargoProfile -eq 'release') { '--release' } else { '--profile'; $CargoProfile })
$binDir = Join-Path $TargetDir $CargoProfile

# --- Build ---
if (-not $SkipBuild) {
    foreach ($package in @('bf2_import', 'game_server', 'game_client')) {
        Write-Host "cargo build $($profileArgs -join ' ') -p $package"
        # Cargo reports progress on standard error: not an error.
        $ErrorActionPreference = 'Continue'
        & rustup run stable cargo build @profileArgs -p $package
        $ErrorActionPreference = 'Stop'
        if ($LASTEXITCODE -ne 0) { throw "building $package failed" }
    }
}

# --- Stage ---
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null

foreach ($exe in @('client.exe', 'server.exe', 'bf2-import.exe')) {
    $source = Join-Path $binDir $exe
    if (-not (Test-Path $source)) { throw "missing $source (build it, or drop -SkipBuild)" }
    Copy-Item $source $stage
}

# The repo's own assets/ folder: copied if it ever has anything (today everything the game
# needs is embedded in the executable or imported from BF2).
$assets = Join-Path $root 'assets'
if ((Test-Path $assets) -and (Get-ChildItem -Recurse -File $assets | Select-Object -First 1)) {
    Copy-Item -Recurse $assets (Join-Path $stage 'assets')
}

$readme = (Get-Content -Raw (Join-Path $PSScriptRoot 'package\README.txt')).Replace('{VERSION}', $version)
[IO.File]::WriteAllText((Join-Path $stage 'README.txt'), ($readme -replace "`r?`n", "`r`n"))

# --- Licenses: this project's, and every third-party crate in the three binaries ---
$license = @"
bf2-rust-engine $version
Licensed under either of the MIT license or the Apache License, Version 2.0, at your option
(SPDX: MIT OR Apache-2.0).

Battlefield 2 is a trademark of Electronic Arts. This program contains no EA code or assets.
"@
[IO.File]::WriteAllText((Join-Path $stage 'LICENSE.txt'), ($license -replace "`r?`n", "`r`n"))

Write-Host 'Listing third-party crates (cargo metadata)'
$ErrorActionPreference = 'Continue'
$notices = & rustup run stable cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc |
    & python (Join-Path $PSScriptRoot 'package\third_party.py') $version bf2_import game_server game_client
$ErrorActionPreference = 'Stop'
if ($LASTEXITCODE -ne 0 -or -not $notices) { throw 'listing third-party crates failed (needs cargo and python)' }
[IO.File]::WriteAllText((Join-Path $stage 'THIRD-PARTY-NOTICES.txt'), (($notices -join "`r`n") + "`r`n"))
Write-Host "$(@($notices).Count - 6) third-party crates listed"

# --- Check: only our files, nothing converted from BF2 ---
$allowed = @('client.exe', 'server.exe', 'bf2-import.exe', 'README.txt', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt')
# What bf2-import writes (and BF2's own formats): never in a package.
$forbiddenExt = @('.glb', '.gltf', '.dds', '.tga', '.ogg', '.wav', '.bin', '.zip', '.con', '.raw', '.ron',
    '.bundledmesh', '.staticmesh', '.skinnedmesh', '.collisionmesh', '.baf', '.ske', '.tai', '.fx', '.json')
$problems = New-Object System.Collections.Generic.List[string]
foreach ($file in Get-ChildItem -Recurse -Force $stage) {
    $relative = $file.FullName.Substring($stage.Length + 1)
    if ($file.PSIsContainer) {
        if ($file.Name -in @('imported', 'mods', 'cache', 'levels', 'objects')) { $problems.Add("folder $relative") }
        continue
    }
    $topLevel = -not $relative.Contains('\')
    if (-not ($topLevel -and $allowed -contains $file.Name) -and -not $relative.StartsWith('assets\')) {
        $problems.Add("unexpected file $relative")
    }
    if ($forbiddenExt -contains $file.Extension.ToLower()) { $problems.Add("BF2-derived file type $relative") }
    if ($file.Extension -eq '.txt' -and (Select-String -Quiet -SimpleMatch -Path $file.FullName -Pattern 'Converted from a local Battlefield 2 installation')) {
        $problems.Add("bf2-import output marker in $relative")
    }
}
if ($problems.Count -gt 0) {
    $problems | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    throw "the package has files that must not be distributed (above)"
}

# --- Zip ---
$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
$size = (Get-Item $zip).Length / 1MB
Write-Host ("{0} ({1:N1} MB)" -f $zip, $size)
Get-ChildItem $stage | ForEach-Object { Write-Host ("  {0,-26} {1,8:N1} MB" -f $_.Name, ($_.Length / 1MB)) }
