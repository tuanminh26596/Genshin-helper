<#
.SYNOPSIS
    End-to-end Windows installer smoke test for CI runners (silent install, layout/task/registry checks,
    first-launch /status + /shutdown, silent upgrade, and /PURGEDATA uninstall cleanup).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,

    [string]$ExpectedVersion = "",

    [string]$InstallDir = $(if ($env:ProgramFiles) { Join-Path $env:ProgramFiles "Genshin Helper" } else { "C:\Program Files\Genshin Helper" }),

    [switch]$ConfirmCiRunner,

    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

$ElevatedTaskName = "GenshinHelper_Elevated"
$PinnedExtOrigin = "chrome-extension://nfkijdoblmphmdljfogooajhbldkeojh/"
$NativeHostRegistryKeys = @(
    "HKLM:\Software\Google\Chrome\NativeMessagingHosts\com.genshinhelper.host",
    "HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.genshinhelper.host",
    "HKLM:\Software\Microsoft\Edge\NativeMessagingHosts\com.genshinhelper.host",
    "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.genshinhelper.host",
    "HKLM:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.genshinhelper.host",
    "HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.genshinhelper.host"
)

if (-not $ExpectedVersion) {
    $leaf = Split-Path -Leaf $InstallerPath
    if ($leaf -match 'GenshinHelper-Setup-v([0-9]+\.[0-9]+\.[0-9]+)') {
        $ExpectedVersion = $Matches[1]
    } else {
        $manifestPath = Join-Path (Split-Path -Parent $PSScriptRoot) "extension\manifest.json"
        if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
            $manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding utf8 | ConvertFrom-Json
            if ($manifest.version) {
                $ExpectedVersion = [string]$manifest.version
            }
        }
    }
}

if ($DryRun) {
    Write-Host "[DryRun] InstallerPath   : $InstallerPath"
    Write-Host "[DryRun] ExpectedVersion : $ExpectedVersion"
    Write-Host "[DryRun] InstallDir      : $InstallDir"
    Write-Host "[DryRun] Planned stages:"
    Write-Host "  1. Silent install (/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /AUTOUPDATE=exit)"
    Write-Host "  2. Verify layout, Native Messaging JSON + 6 HKLM/HKCU registry keys, and $ElevatedTaskName task XML"
    Write-Host "  3. First-launch sandboxed smoke (GH_DESKTOP_SANDBOX=1 --no-tray --standby --no-elevate --no-mutex --port 18765 --lan-port 18766 -> /status -> POST /shutdown)"
    Write-Host "  4. In-place silent upgrade (/CLOSEAPPLICATIONS /AUTOUPDATE=exit) + layout/task/registry re-check"
    Write-Host "  5. Silent uninstall (/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /PURGEDATA) + cleanup verification"
    exit 0
}

if (-not ($ConfirmCiRunner -or ($env:GITHUB_ACTIONS -eq "true"))) {
    Write-Error "Refusing to install/purge on a non-CI developer machine without -ConfirmCiRunner."
    exit 2
}

if (-not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) {
    throw "Installer executable not found: $InstallerPath"
}

function Assert-InstalledState {
    param(
        [Parameter(Mandatory = $true)][string]$Dir
    )

    $requiredFiles = @(
        "GenshinHelper.exe",
        "GenshinHelperHost.exe",
        "com.genshinhelper.host.json",
        "extension\manifest.json",
        "THIRD_PARTY_LICENSES.md",
        "unins000.exe"
    )
    foreach ($rel in $requiredFiles) {
        $full = Join-Path $Dir $rel
        if (-not (Test-Path -LiteralPath $full -PathType Leaf)) {
            throw "Expected installed file missing: $full"
        }
    }

    $nhPath = Join-Path $Dir "com.genshinhelper.host.json"
    $nhJson = Get-Content -LiteralPath $nhPath -Raw -Encoding utf8 | ConvertFrom-Json
    $expectedHostExe = Join-Path $Dir "GenshinHelperHost.exe"
    if ([string]$nhJson.path -ne $expectedHostExe) {
        throw "Native messaging host path mismatch: '$($nhJson.path)' != '$expectedHostExe'"
    }
    if (-not ($nhJson.allowed_origins -contains $PinnedExtOrigin)) {
        throw "Native messaging manifest missing pinned extension origin: $PinnedExtOrigin"
    }

    $taskXmlOut = & schtasks.exe /query /tn $ElevatedTaskName /xml 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Expected scheduled task '$ElevatedTaskName' missing after install: $taskXmlOut"
    }
    $taskXml = ($taskXmlOut | Out-String)
    if ($taskXml -notmatch '<RunLevel>HighestAvailable</RunLevel>') {
        throw "Scheduled task '$ElevatedTaskName' missing HighestAvailable run level."
    }
    if ($taskXml -notmatch 'GenshinHelper\.exe') {
        throw "Scheduled task '$ElevatedTaskName' command does not reference GenshinHelper.exe."
    }

    foreach ($regPath in $NativeHostRegistryKeys) {
        if (-not (Test-Path -LiteralPath $regPath)) {
            throw "Missing Native Messaging registry key: $regPath"
        }
        $val = (Get-ItemProperty -LiteralPath $regPath -ErrorAction Stop)."(default)"
        if ([string]$val -ne $nhPath) {
            throw "Registry value mismatch at $regPath : '$val' != '$nhPath'"
        }
    }
}

$prevSandbox = $env:GH_DESKTOP_SANDBOX
$smokeProc = $null
$installedByTest = $false

try {
    Write-Host "[1/5] Running silent install..." -ForegroundColor Cyan
    $instProc = Start-Process -FilePath $InstallerPath -ArgumentList @(
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/AUTOUPDATE=exit"
    ) -Wait -PassThru
    if ($instProc.ExitCode -ne 0) {
        throw "Silent install failed with exit code $($instProc.ExitCode)"
    }
    $installedByTest = $true

    Write-Host "[2/5] Verifying installed layout, Scheduled Task, and Native Messaging registry..." -ForegroundColor Cyan
    Assert-InstalledState -Dir $InstallDir

    Write-Host "[3/5] Running first-launch sandboxed /status and /shutdown smoke test..." -ForegroundColor Cyan
    $env:GH_DESKTOP_SANDBOX = "1"
    $appExe = Join-Path $InstallDir "GenshinHelper.exe"
    $smokeProc = Start-Process -FilePath $appExe -ArgumentList @(
        "--no-tray",
        "--standby",
        "--no-elevate",
        "--no-mutex",
        "--port", "18765",
        "--lan-port", "18766"
    ) -PassThru -WindowStyle Hidden

    $status = $null
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        if ($smokeProc.HasExited) {
            throw "Installed GenshinHelper.exe exited prematurely during startup with code $($smokeProc.ExitCode)"
        }
        try {
            $status = Invoke-RestMethod -Method Get -Uri "http://127.0.0.1:18765/status" -TimeoutSec 2
            if ($null -ne $status) {
                break
            }
        } catch {
            Start-Sleep -Milliseconds 250
        }
    }
    if ($null -eq $status) {
        throw "Timed out waiting for http://127.0.0.1:18765/status"
    }
    $actualVer = ([string]$status.version).Trim().TrimStart("v", "V")
    if (-not $actualVer) {
        throw "Status response missing non-empty version field"
    }
    if ($ExpectedVersion) {
        $wantVer = $ExpectedVersion.Trim().TrimStart("v", "V")
        if ($actualVer -ne $wantVer) {
            throw "Installed app version mismatch: got '$actualVer', expected '$wantVer'"
        }
    }

    $null = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:18765/shutdown" -ContentType "application/json" -Body "{}" -TimeoutSec 5
    if (-not $smokeProc.WaitForExit(10000)) {
        throw "Installed GenshinHelper.exe did not exit within 10s after POST /shutdown"
    }
    if ($smokeProc.ExitCode -ne 0) {
        throw "Installed GenshinHelper.exe exited with non-zero code $($smokeProc.ExitCode) after POST /shutdown"
    }
    $smokeProc = $null

    Write-Host "[4/5] Running in-place silent upgrade smoke test..." -ForegroundColor Cyan
    $upgProc = Start-Process -FilePath $InstallerPath -ArgumentList @(
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/CLOSEAPPLICATIONS",
        "/AUTOUPDATE=exit"
    ) -Wait -PassThru
    if ($upgProc.ExitCode -ne 0) {
        throw "In-place silent upgrade failed with exit code $($upgProc.ExitCode)"
    }
    Assert-InstalledState -Dir $InstallDir

    Write-Host "[5/5] Running silent uninstall (/PURGEDATA) and verifying cleanup..." -ForegroundColor Cyan
    $uninstExe = Join-Path $InstallDir "unins000.exe"
    $uninstProc = Start-Process -FilePath $uninstExe -ArgumentList @(
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/PURGEDATA"
    ) -Wait -PassThru
    if ($uninstProc.ExitCode -ne 0) {
        throw "Silent uninstall failed with exit code $($uninstProc.ExitCode)"
    }

    # Inno Setup unins000.exe delegates cleanup to a temp copy and may return slightly before final deletion.
    $cleanupDeadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $cleanupDeadline) {
        $appGone = -not (Test-Path -LiteralPath (Join-Path $InstallDir "GenshinHelper.exe"))
        $uninstGone = -not (Test-Path -LiteralPath $uninstExe)
        $null = & schtasks.exe /query /tn $ElevatedTaskName 2>&1
        $taskGone = ($LASTEXITCODE -ne 0)
        if ($appGone -and $uninstGone -and $taskGone) {
            break
        }
        Start-Sleep -Milliseconds 500
    }

    $null = & schtasks.exe /query /tn $ElevatedTaskName 2>&1
    if ($LASTEXITCODE -eq 0) {
        throw "Scheduled task '$ElevatedTaskName' still exists after uninstall."
    }

    foreach ($regPath in $NativeHostRegistryKeys) {
        if (Test-Path -LiteralPath $regPath) {
            throw "Native Messaging registry key still exists after uninstall: $regPath"
        }
    }

    foreach ($leftover in @("GenshinHelper.exe", "GenshinHelperHost.exe")) {
        $leftoverPath = Join-Path $InstallDir $leftover
        if (Test-Path -LiteralPath $leftoverPath) {
            throw "Installed binary still exists after uninstall: $leftoverPath"
        }
    }

    $installedByTest = $false
    Write-Host "INSTALLER SMOKE TEST PASSED (install, layout, first-launch, upgrade, uninstall)." -ForegroundColor Green
} finally {
    if ($null -eq $prevSandbox) {
        Remove-Item Env:\GH_DESKTOP_SANDBOX -ErrorAction SilentlyContinue
    } else {
        $env:GH_DESKTOP_SANDBOX = $prevSandbox
    }

    if ($null -ne $smokeProc -and (-not $smokeProc.HasExited)) {
        try {
            Stop-Process -Id $smokeProc.Id -Force -ErrorAction SilentlyContinue
        } catch {}
    }

    if ($installedByTest) {
        $uninstExe = Join-Path $InstallDir "unins000.exe"
        if (Test-Path -LiteralPath $uninstExe -PathType Leaf) {
            try {
                $null = Start-Process -FilePath $uninstExe -ArgumentList @(
                    "/VERYSILENT",
                    "/SUPPRESSMSGBOXES",
                    "/NORESTART",
                    "/PURGEDATA"
                ) -Wait -PassThru -ErrorAction SilentlyContinue
            } catch {}
        }
    }
}
