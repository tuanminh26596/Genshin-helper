param(
    [string]$Scenario = '',
    [Nullable[int]]$Limit = $null,
    [string]$BrowserPath = '',
    [int]$Port = 8765,
    # Minimize the bench browser after the map page has loaded, so the tab is hidden and
    # throttled like a real HoYoLAB tab sitting behind the fullscreen game.
    [switch]$Minimize
)

$ErrorActionPreference = 'Stop'

function Write-ErrLine([string]$Message) {
    [Console]::Error.WriteLine("[web_bench.ps1] ERROR: $Message")
}

if ([string]::IsNullOrWhiteSpace($Scenario)) {
    Write-ErrLine "Missing required parameter -Scenario <path>."
    exit 2
}

$PassThruArgs = @($args)

if (-not (Test-Path -LiteralPath $Scenario -PathType Leaf)) {
    Write-ErrLine "Scenario file not found: $Scenario"
    exit 2
}

$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$ExtDir = Join-Path $RepoRoot 'extension'
$ManifestPath = Join-Path $RepoRoot 'rust\Cargo.toml'

if (-not (Test-Path -LiteralPath $ExtDir -PathType Container)) {
    Write-ErrLine "Extension directory not found: $ExtDir"
    exit 2
}
if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
    Write-ErrLine "Cargo manifest not found: $ManifestPath"
    exit 2
}

# Refuse immediately with exit 2 if anything already listens on 127.0.0.1:$Port.
# Never send /shutdown or take over a port that may belong to a running GenshinHelper.
$probeListener = $null
$probeStarted = $false
try {
    $probeListener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, $Port)
    $probeListener.Start()
    $probeStarted = $true
} catch {
    Write-ErrLine "Port 127.0.0.1:$Port is already in use. Please close GenshinHelper (or any process listening on port $Port) and try again."
    exit 2
} finally {
    if ($probeStarted -and $null -ne $probeListener) {
        $probeListener.Stop()
    }
}

# Locate unbranded Chromium (supports --load-extension) if -BrowserPath was not specified.
if ([string]::IsNullOrWhiteSpace($BrowserPath)) {
    $pwRoot = Join-Path $env:LOCALAPPDATA 'ms-playwright'
    $candidates = @()
    if (Test-Path -LiteralPath $pwRoot -PathType Container) {
        $candidates = @(
            Get-ChildItem -LiteralPath $pwRoot -Directory -Filter 'chromium-*' -ErrorAction SilentlyContinue |
                ForEach-Object {
                    $exe = Join-Path $_.FullName 'chrome-win64\chrome.exe'
                    if (Test-Path -LiteralPath $exe -PathType Leaf) {
                        $rev = 0
                        if ($_.Name -match '^chromium-(\d+)$') {
                            $rev = [int]$Matches[1]
                        }
                        [PSCustomObject]@{
                            Path  = $exe
                            Rev   = $rev
                            Mtime = $_.LastWriteTimeUtc
                        }
                    }
                } |
                Sort-Object -Property @{ Expression = 'Rev'; Descending = $true }, @{ Expression = 'Mtime'; Descending = $true }
        )
    }
    if ($candidates.Count -eq 0) {
        Write-ErrLine "No Playwright Chromium found under '$pwRoot\chromium-*\chrome-win64\chrome.exe'. Pass -BrowserPath explicitly."
        exit 2
    }
    $BrowserPath = $candidates[0].Path
} elseif (-not (Test-Path -LiteralPath $BrowserPath -PathType Leaf)) {
    Write-ErrLine "Specified -BrowserPath does not exist: $BrowserPath"
    exit 2
}

# Pre-build gh-sim before launching Chromium so port binding starts promptly.
& cargo build -q --profile cv-bench --manifest-path $ManifestPath -p gh-sim
if ($LASTEXITCODE -ne 0) {
    Write-ErrLine "Failed to build gh-sim (cargo exit code $LASTEXITCODE)."
    exit 2
}

$UserDataDir = Join-Path ([System.IO.Path]::GetTempPath()) ("gh_web_bench_" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $UserDataDir -Force | Out-Null

$exitCode = 2
try {
    $mapUrl = 'https://act.hoyolab.com/ys/app/interactive-map/index.html#/map/2'
    # Never pop a window over the developer's screen: headless by default; -Minimize uses an
    # off-screen window that is then minimized (a hidden tab needs a real window to be throttled).
    $windowArgs = if ($Minimize) {
        @('--window-position=-32000,-32000', '--window-size=1280,800')
    } else {
        @('--headless=new', '--window-size=1280,800')
    }
    $chromeArgLine = @(
        "`"--user-data-dir=$UserDataDir`""
        "`"--load-extension=$ExtDir`""
        "`"--disable-extensions-except=$ExtDir`""
        '--no-first-run'
        '--no-default-browser-check'
    ) + $windowArgs + @("`"$mapUrl`"") -join ' '

    $null = Start-Process -FilePath $BrowserPath -ArgumentList $chromeArgLine -PassThru

    if ($Minimize) {
        if (-not ('GhWebBench.Win' -as [type])) {
            Add-Type -Namespace GhWebBench -Name Win -MemberDefinition @'
public delegate bool EnumProc(System.IntPtr h, System.IntPtr l);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, System.IntPtr l);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint pid);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr h, int cmd);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(System.IntPtr h, System.Text.StringBuilder s, int n);
'@
        }
        $minimized = $false
        $deadline = (Get-Date).AddSeconds(45)
        while (-not $minimized -and (Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 500
            $benchPids = @(
                Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
                    Where-Object {
                        $_.CommandLine -and
                        $_.CommandLine.IndexOf($UserDataDir, [System.StringComparison]::OrdinalIgnoreCase) -ge 0
                    } |
                    ForEach-Object { [uint32]$_.ProcessId }
            )
            $hits = New-Object System.Collections.ArrayList
            $cb = [GhWebBench.Win+EnumProc]{
                param($h, $l)
                $wp = [uint32]0
                [void][GhWebBench.Win]::GetWindowThreadProcessId($h, [ref]$wp)
                if ($benchPids -contains $wp -and [GhWebBench.Win]::IsWindowVisible($h)) {
                    $sb = New-Object System.Text.StringBuilder 256
                    [void][GhWebBench.Win]::GetWindowText($h, $sb, 256)
                    if ($sb.ToString() -match 'HoYoLAB') { [void]$hits.Add($h) }
                }
                return $true
            }
            [void][GhWebBench.Win]::EnumWindows($cb, [System.IntPtr]::Zero)
            if ($hits.Count -gt 0) {
                # Give the extension a few seconds to inject before hiding the tab.
                Start-Sleep -Seconds 5
                foreach ($h in $hits) { [void][GhWebBench.Win]::ShowWindow($h, 6) }  # SW_MINIMIZE
                $minimized = $true
            }
        }
        if (-not $minimized) {
            Write-ErrLine "Could not find the bench browser window to minimize."
            exit 2
        }
        [Console]::Error.WriteLine("[web_bench.ps1] Browser minimized (tab hidden).")
    }

    $benchArgs = @(
        'run', '-q',
        '--profile', 'cv-bench',
        '--manifest-path', $ManifestPath,
        '-p', 'gh-sim',
        '--',
        'web-bench',
        '--scenario', $Scenario,
        '--port', "$Port"
    )
    if ($null -ne $Limit) {
        $benchArgs += @('--limit', "$Limit")
    }
    if ($null -ne $PassThruArgs -and $PassThruArgs.Count -gt 0) {
        $benchArgs += $PassThruArgs
    }

    & cargo @benchArgs
    $exitCode = $LASTEXITCODE
} finally {
    # Stop ONLY processes whose command line contains our dedicated temp user-data-dir.
    for ($attempt = 0; $attempt -lt 3; $attempt++) {
        $matching = @(
            Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
                Where-Object {
                    $_.CommandLine -and
                    $_.CommandLine.IndexOf($UserDataDir, [System.StringComparison]::OrdinalIgnoreCase) -ge 0
                }
        )
        if ($matching.Count -eq 0) {
            break
        }
        foreach ($proc in $matching) {
            Stop-Process -Id $proc.ProcessId -Force -ErrorAction SilentlyContinue
        }
        Start-Sleep -Milliseconds 250
    }

    if (Test-Path -LiteralPath $UserDataDir) {
        for ($attempt = 0; $attempt -lt 6; $attempt++) {
            Remove-Item -LiteralPath $UserDataDir -Recurse -Force -ErrorAction SilentlyContinue
            if (-not (Test-Path -LiteralPath $UserDataDir)) {
                break
            }
            Start-Sleep -Milliseconds 250
        }
    }
}

exit $exitCode
