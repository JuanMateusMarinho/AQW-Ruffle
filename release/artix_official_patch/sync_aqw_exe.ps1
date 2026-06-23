param(
    [string]$AqwExePath,
    [string]$OutputDir,
    [string]$PortableLauncherPath,
    [string]$ArtixLauncherPath = "C:\Program Files\Artix Game Launcher",
    [switch]$AllGames,
    [switch]$Install
)

$ErrorActionPreference = "Stop"

$scriptRoot = Split-Path -Parent $PSCommandPath
$repoRoot = (Resolve-Path (Join-Path $scriptRoot "..\..")).Path
$exeNames = if ($AllGames) {
    @(
        "AQW.exe",
        "EpicDuel.exe",
        "DragonFable.exe",
        "AdventureQuest.exe",
        "MechQuest.exe",
        "OverSoul.exe"
    )
} else {
    @("AQW.exe")
}

if (-not $AqwExePath) {
    $AqwExePath = Join-Path $repoRoot "release\AQW.exe"
}
if (-not $OutputDir) {
    $OutputDir = $scriptRoot
}
if (-not $PortableLauncherPath) {
    $PortableLauncherPath = Join-Path $OutputDir "Artix Game Launcher"
}

if (-not (Test-Path -LiteralPath $AqwExePath)) {
    throw "Could not find AQW.exe at $AqwExePath"
}

if ($Install) {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    $isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $isAdmin) {
        $args = @(
            "-NoProfile",
            "-ExecutionPolicy", "Bypass",
            "-File", "`"$PSCommandPath`"",
            "-AqwExePath", "`"$AqwExePath`"",
            "-OutputDir", "`"$OutputDir`"",
            "-PortableLauncherPath", "`"$PortableLauncherPath`"",
            "-ArtixLauncherPath", "`"$ArtixLauncherPath`""
        )
        if ($AllGames) {
            $args += "-AllGames"
        }
        $args += "-Install"
        Start-Process -FilePath "powershell.exe" -ArgumentList $args -Verb RunAs -Wait
        return
    }
}

$portableBin = Join-Path $PortableLauncherPath "resources\bin"
$sourceByExe = @{}
foreach ($exeName in $exeNames) {
    $sourceByExe[$exeName] = Join-Path $repoRoot "release\$exeName"
}
$sourceByExe["AQW.exe"] = $AqwExePath

foreach ($exeName in $exeNames) {
    $sourcePath = $sourceByExe[$exeName]
    if (-not (Test-Path -LiteralPath $sourcePath)) {
        throw "Could not find $exeName at $sourcePath"
    }

    $destinations = New-Object System.Collections.Generic.List[string]
    $destinations.Add((Join-Path $OutputDir "bin\$exeName"))

    if (Test-Path -LiteralPath $PortableLauncherPath) {
        $destinations.Add((Join-Path $portableBin $exeName))
    }

    if ($Install) {
        $destinations.Add((Join-Path $ArtixLauncherPath "resources\bin\$exeName"))
    }

    $sourceHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $sourcePath).Hash
    foreach ($destination in $destinations) {
        $destinationDir = Split-Path -Parent $destination
        New-Item -ItemType Directory -Path $destinationDir -Force | Out-Null
        Copy-Item -LiteralPath $sourcePath -Destination $destination -Force

        $destinationHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $destination).Hash
        if ($destinationHash -ne $sourceHash) {
            throw "Hash mismatch after copying $exeName to $destination"
        }

        Write-Host "Synced $exeName -> $destination"
    }
}
