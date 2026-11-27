param(
    [string]$Scenario = '',
    [Nullable[int]]$Limit = $null,
    [string]$BrowserPath = '',
    [int]$Port = 8765
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
    $chromeArgLine = @(
        "`"--user-data-dir=$UserDataDir`""
        "`"--load-extension=$ExtDir`""
        "`"--disable-extensions-except=$ExtDir`""
        '--no-first-run'
        '--no-default-browser-check'
        "`"$mapUrl`""
    ) -join ' '

    $null = Start-Process -FilePath $BrowserPath -ArgumentList $chromeArgLine -PassThru

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
