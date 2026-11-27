<#
.SYNOPSIS
    Dedicated, persistent Chromium profile that stays signed in to HoYoLAB, for live web tests.

.DESCRIPTION
    A guest HoYoLAB session cannot mark pins, so tests of the real mark / already-marked flow need a
    signed-in profile. This script owns that profile so every agent and person uses the same one:

      -Login    Opens ONE visible window on the HoYoLAB Interactive Map so the maintainer can sign in
                (the only visible use of this profile). Close the window when you are done.
                `-Login -Site appsample` opens the AppSample map instead (needed for AppSample marking).
      -Status   Says whether the profile exists, whether a browser is using it right now, and which
                Chromium it uses. Never reads or prints cookies.
      -Verify   Headless check that the profile is still signed in (scripts/hoyolab_profile_check.mjs):
                exit 0 = signed in, 3 = signed out (run -Login again), 1 = could not check.
                `-Verify -Site appsample` checks the AppSample sign-in instead.
      -Path     Prints the profile directory (default for tests: $env:GH_HOYOLAB_PROFILE).

    Rules for tests that use it (also in AGENTS.md / CLAUDE.md):
      * Headless only (--headless=new) after the one-time login; one browser per profile at a time.
      * Never print, copy, upload or commit anything from the profile directory (session cookies).
      * A test that marks or unmarks a pin must restore the original state in the same run.

.EXAMPLE
    pwsh -NoProfile -File scripts/hoyolab_test_profile.ps1 -Login
    pwsh -NoProfile -File scripts/hoyolab_test_profile.ps1 -Status
#>
[CmdletBinding(DefaultParameterSetName = 'Status')]
param(
    [Parameter(ParameterSetName = 'Login')][switch]$Login,
    [Parameter(ParameterSetName = 'Status')][switch]$Status,
    [Parameter(ParameterSetName = 'Path')][switch]$Path,
    [Parameter(ParameterSetName = 'Verify')][switch]$Verify,
    [string]$ProfileDir,
    [string]$Lang = 'en-us',
    # Site to open with -Login. AppSample marking needs its own sign-in (provider_runtime.js
    # markPin returns authRequired without store.getState().user); Kongying needs none.
    [ValidateSet('hoyolab', 'appsample')]
    [string]$Site = 'hoyolab'
)

$ErrorActionPreference = 'Stop'

function Get-ProfileDir {
    if ($ProfileDir) { return [IO.Path]::GetFullPath($ProfileDir) }
    if ($env:GH_HOYOLAB_PROFILE) { return [IO.Path]::GetFullPath($env:GH_HOYOLAB_PROFILE) }
    return Join-Path $env:LOCALAPPDATA 'GenshinHelperDev\hoyolab-test-profile'
}

# Same lookup order as tests/extension_browser_smoke.mjs: GH_CHROME, else newest Playwright Chromium.
function Find-Chromium {
    if ($env:GH_CHROME -and (Test-Path -LiteralPath $env:GH_CHROME)) { return $env:GH_CHROME }
    $base = Join-Path $env:LOCALAPPDATA 'ms-playwright'
    if (-not (Test-Path -LiteralPath $base)) { return $null }
    $dirs = Get-ChildItem -LiteralPath $base -Directory |
        Where-Object { $_.Name -match '^chromium-(\d+)$' } |
        Sort-Object { [int]($_.Name -replace '^chromium-', '') } -Descending
    foreach ($d in $dirs) {
        foreach ($sub in 'chrome-win64', 'chrome-win') {
            $exe = Join-Path $d.FullName "$sub\chrome.exe"
            if (Test-Path -LiteralPath $exe) { return $exe }
        }
    }
    return $null
}

function Get-ProfileBrowsers([string]$dir) {
    $needle = $dir.TrimEnd('\')
    @(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine.Contains($needle) -and $_.CommandLine -notmatch '--type=' })
}

$dir = Get-ProfileDir

if ($Path) {
    Write-Output $dir
    exit 0
}

$chrome = Find-Chromium

if ($Login) {
    if (-not $chrome) { throw 'No Chromium found: set GH_CHROME or install Playwright Chromium.' }
    $running = Get-ProfileBrowsers $dir
    if ($running.Count -gt 0) {
        Write-Output "A browser is already using this profile (PID $($running[0].ProcessId)). Use that window, or close it first."
        exit 1
    }
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $url = switch ($Site) {
        'appsample' { 'https://genshin-impact-map.appsample.com/' }
        default { "https://act.hoyolab.com/ys/app/interactive-map/index.html?lang=$Lang" }
    }
    # No extension and no remote debugging here: this window is only for signing in.
    # Start-Process joins arguments with spaces without quoting, so quote the profile path.
    $chromeArgs = @("--user-data-dir=`"$dir`"", '--no-first-run', '--no-default-browser-check', $url)
    $p = Start-Process -FilePath $chrome -ArgumentList $chromeArgs -PassThru
    $siteName = if ($Site -eq 'appsample') { 'AppSample' } else { 'HoYoLAB' }
    Write-Output "Opened the $siteName sign-in window (PID $($p.Id)) on profile:"
    Write-Output "  $dir"
    Write-Output "Sign in to $siteName there, then close the window."
    exit 0
}

if ($Verify) {
    if (-not $chrome) { Write-Output 'check: no Chromium found'; exit 1 }
    if (-not (Test-Path -LiteralPath (Join-Path $dir 'Default'))) {
        Write-Output "check: profile does not exist yet; run -Login first ($dir)"
        exit 3
    }
    $running = Get-ProfileBrowsers $dir
    if ($running.Count -gt 0) {
        Write-Output "check: profile is in use (PID $($running.ProcessId -join ', ')); wait for it or close it"
        exit 1
    }
    & node (Join-Path $PSScriptRoot 'hoyolab_profile_check.mjs') $chrome $dir $Site
    exit $LASTEXITCODE
}

# -Status (default)
$exists = Test-Path -LiteralPath (Join-Path $dir 'Default')
$running = Get-ProfileBrowsers $dir
Write-Output "profile:  $dir"
Write-Output "exists:   $exists"
Write-Output "in use:   $(if ($running.Count) { 'yes (PID ' + ($running.ProcessId -join ', ') + ')' } else { 'no' })"
Write-Output "chromium: $(if ($chrome) { $chrome } else { 'not found' })"
exit 0
