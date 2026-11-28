<#
.SYNOPSIS
Unified change-aware verification command for Genshin-Helper.

.DESCRIPTION
Selects the checks required for the current change set (diff against -Base plus
staged, unstaged, and untracked working-tree files), executes them from cheapest
to most expensive in a fixed order, stops at the first failure, and prints a
compact summary ending with `RESULT: PASS` or `RESULT: FAIL (<check>)`.

Works identically in the main checkout and in any git worktree: when gitignored
files (.cargo/config.toml, .venv\Scripts\python.exe, data\, scratch\qa_sessions\,
scratch\web_scenario_all.json) are absent from the current worktree, they are
resolved automatically from the main checkout reported by `git worktree list`.

Fixed check order:
  guards, audit, ext-i18n, multimap-unit, clippy, nextest, frames, provider-fixtures, ext-smoke, blackbox, qa, web-bench, provider-live

Additional repeatable parameter:
  -QaOnly <session>
      Filter `gh-sim qa` to one or more session names. May be passed multiple
      times (`-QaOnly sess_A -QaOnly sess_B`) or comma-separated
      (`-QaOnly sess_A,sess_B`). Passed through to `gh-sim qa` as repeatable
      `--only <session>`.

.PARAMETER Base
Base git ref used to compute the merge-base for changed-file detection and guard
comparisons. Default: `main`. On `main` itself this checks uncommitted changes.

.PARAMETER Tier
Selection tier: `auto` (default, path-based selection) or `full` (every check
except `web-bench` and `provider-live`; add `-WebBench` / `-ProviderLive` to include them).

.PARAMETER Checks
Explicit comma-separated list (or array) of check names to run in the fixed
order, ignoring path-based selection.

.PARAMETER AllowBaselineChange
Permit changes to `tests/qa_baseline.json` or `tests/verified_gt.json` vs -Base.
Use only with explicit user approval; usage is always logged.

.PARAMETER WebBench
Include `web-bench` (requires port 8765 to be free; close GenshinHelper first).

.PARAMETER ProviderLive
Include `provider-live` (runs `tests/provider_live_smoke.mjs` against live AppSample, Kongying v3, and HoYoLAB in headless Chromium; requires network).

.PARAMETER DryRun
Print the selection plan only (`<name>: selected (<reason>)` or
`<name>: not selected`) without executing any checks.

.PARAMETER SelfTest
Run built-in unit tests for path selection, guards, config parsing, and runner
short-circuiting without invoking external build/test tools. Prints
`SELFTEST PASS` or `SELFTEST FAIL`.

.EXAMPLE
pwsh -NoProfile -File scripts/check.ps1 -DryRun -Base main
Show which checks would run for the current changes relative to main.

.EXAMPLE
pwsh -NoProfile -File scripts/check.ps1 -Base main
Run the auto-selected checks for the current changes.

.EXAMPLE
pwsh -NoProfile -File scripts/check.ps1 -Checks guards,clippy,nextest -Base main
Run guards, clippy, and nextest explicitly in fixed order.

.EXAMPLE
pwsh -NoProfile -File scripts/check.ps1 -Checks qa -QaOnly sess_20261127_011805 -Base main
Run only the specified QA session (repeatable: -QaOnly sess_A -QaOnly sess_B).

.EXAMPLE
pwsh -NoProfile -File scripts/check.ps1 -Tier full -Base main
Run every offline/automated check except web-bench and provider-live.

.EXAMPLE
pwsh -NoProfile -File scripts/check.ps1 -SelfTest
Run the script's self-test suite.
#>

param(
    [string]$Base = 'main',
    [ValidateSet('auto', 'full')]
    [string]$Tier = 'auto',
    [string[]]$Checks = @(),
    [switch]$AllowBaselineChange,
    [switch]$WebBench,
    [switch]$ProviderLive,
    [switch]$DryRun,
    [switch]$SelfTest,
    [switch]$PruneWorktreeTargets
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$script:CheckOrder = @(
    'guards'
    'audit'
    'ext-i18n'
    'multimap-unit'
    'clippy'
    'nextest'
    'frames'
    'provider-fixtures'
    'ext-smoke'
    'blackbox'
    'qa'
    'web-bench'
    'provider-live'
)

$script:ProtectedBaselineFiles = @(
    'tests/qa_baseline.json'
    'tests/verified_gt.json'
)

# Constructed via concatenation so static source scanners do not mistake the
# guard pattern itself for an added Rust ignored-test attribute.
$script:RustIgnoreAttr = '#[' + 'ignore'

function Parse-ExtraArgs {
    param([object[]]$RawArgs = @())

    $qaOnlyList = New-Object System.Collections.Generic.List[string]
    $i = 0
    while ($i -lt $RawArgs.Count) {
        $token = [string]$RawArgs[$i]
        if ($token -match '^(?i)-QaOnly:(.+)$') {
            foreach ($part in ($Matches[1] -split ',')) {
                $trimmed = $part.Trim()
                if ($trimmed.Length -gt 0) {
                    $qaOnlyList.Add($trimmed)
                }
            }
            $i++
            continue
        }
        if ($token -ieq '-QaOnly') {
            if (($i + 1) -ge $RawArgs.Count) {
                throw 'Missing value for parameter -QaOnly.'
            }
            $val = $RawArgs[$i + 1]
            if ($val -is [System.Array]) {
                foreach ($item in $val) {
                    foreach ($part in ([string]$item -split ',')) {
                        $trimmed = $part.Trim()
                        if ($trimmed.Length -gt 0) {
                            $qaOnlyList.Add($trimmed)
                        }
                    }
                }
            } else {
                foreach ($part in ([string]$val -split ',')) {
                    $trimmed = $part.Trim()
                    if ($trimmed.Length -gt 0) {
                        $qaOnlyList.Add($trimmed)
                    }
                }
            }
            $i += 2
            continue
        }
        throw "Unknown argument: $token"
    }

    return [PSCustomObject]@{
        QaOnly = @($qaOnlyList.ToArray())
    }
}

function Normalize-CheckNames {
    param([string[]]$RawChecks = @())

    $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    $result = New-Object System.Collections.Generic.List[string]
    foreach ($entry in $RawChecks) {
        if ($null -eq $entry) { continue }
        foreach ($part in ($entry -split ',')) {
            $name = $part.Trim().ToLowerInvariant()
            if ($name.Length -eq 0) { continue }
            if ($script:CheckOrder -notcontains $name) {
                throw "Unknown check '$name'. Valid checks: $($script:CheckOrder -join ', ')"
            }
            if ($seen.Add($name)) {
                $result.Add($name)
            }
        }
    }
    return $result.ToArray()
}

function Normalize-RepoPath {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return ''
    }
    $norm = $Path.Trim() -replace '\\', '/'
    $norm = $norm -replace '^(\./)+', ''
    return $norm
}

function Format-MatchReason {
    param([string[]]$MatchedPaths)

    if ($MatchedPaths.Count -eq 0) {
        return ''
    }
    if ($MatchedPaths.Count -le 2) {
        return ($MatchedPaths -join ', ')
    }
    $head = $MatchedPaths[0..1] -join ', '
    $extra = $MatchedPaths.Count - 2
    return "$head (+$extra more)"
}

function Select-CheckPlan {
    param(
        [string[]]$ChangedPaths = @(),
        [ValidateSet('auto', 'full')]
        [string]$Tier = 'auto',
        [string[]]$ExplicitChecks = @(),
        [switch]$WebBench,
        [switch]$ProviderLive
    )

    $normalizedSet = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    $normalizedPaths = New-Object System.Collections.Generic.List[string]
    foreach ($p in $ChangedPaths) {
        $n = Normalize-RepoPath -Path $p
        if ($n.Length -gt 0 -and $normalizedSet.Add($n)) {
            $normalizedPaths.Add($n)
        }
    }
    $paths = @($normalizedPaths.ToArray())

    $extMatches = @($paths | Where-Object { $_ -like 'extension/*' })
    # tests/verify_i18n_and_ext_v5.py also checks the desktop bundles in assets/locales.
    $extI18nMatches = @(
        $paths | Where-Object {
            $_ -like 'extension/*' -or
            $_ -like 'assets/locales/*' -or
            $_ -eq 'tests/verify_i18n_and_ext_v5.py'
        }
    )
    $multimapUnitMatches = @(
        $paths | Where-Object {
            $_ -like 'extension/*' -or
            $_ -eq 'tests/multimap_unit.mjs' -or
            $_ -like 'tests/fixtures/multimap/*' -or
            $_ -eq 'rust/crates/gh-core/src/map_bridge.rs'
        }
    )
    $providerFixturesMatches = @(
        $paths | Where-Object {
            $_ -like 'extension/*' -or
            $_ -eq 'tests/provider_dom_fixtures.mjs' -or
            $_ -like 'tests/fixtures/providers/*' -or
            $_ -like 'tests/fixtures/multimap/*'
        }
    )
    $extSmokeMatches = @(
        $paths | Where-Object {
            $_ -like 'extension/*' -or
            $_ -eq 'tests/extension_browser_smoke.mjs' -or
            $_ -eq 'tests/extension_dom_fixtures.mjs' -or
            $_ -like 'tests/fixtures/hoyolab_popup/*'
        }
    )
    # assets/** files are compiled into the crates (include_str!/include_bytes!: locales,
    # category aliases, tray icons, zoom thumb template), so they need the Rust checks too.
    $rustMatches = @($paths | Where-Object { $_ -like 'rust/*' -or $_ -like 'assets/*' })
    $framesMatches = @(
        $paths | Where-Object {
            $_ -like 'rust/crates/gh-sim/*' -or
            $_ -eq 'rust/crates/gh-core/src/ui_layout.rs' -or
            $_ -like 'rust/crates/gh-cv/*' -or
            $_ -eq 'assets/zoom_thumb_template.png' -or
            $_ -like 'tests/frame_fixtures/*'
        }
    )
    $blackboxMatches = @(
        $paths | Where-Object {
            $_ -like 'rust/crates/gh-net/*' -or
            $_ -like 'rust/crates/gh-app/*' -or
            $_ -eq 'tests/test_blackbox_contracts.py' -or
            $_ -like 'tests/blackbox_contracts/*'
        }
    )
    $qaMatches = @(
        $paths | Where-Object {
            $_ -like 'rust/crates/gh-cv/*' -or
            $_ -like 'rust/crates/gh-core/*' -or
            $_ -like 'rust/crates/gh-app/*' -or
            $_ -like 'rust/crates/gh-sim/*' -or
            $_ -like 'rust/crates/gh-net/src/hd_assets*' -or
            $_ -eq 'assets/zoom_thumb_template.png' -or
            $_ -eq 'tests/qa_baseline.json' -or
            $_ -eq 'tests/verified_gt.json' -or
            $_ -like 'tests/scenarios/*'
        }
    )
    $webBenchRecommendMatches = @(
        $paths | Where-Object {
            $_ -like 'extension/*' -or
            $_ -like 'rust/crates/gh-net/src/sync_server*' -or
            $_ -eq 'rust/crates/gh-app/src/tracking_loop.rs' -or
            $_ -eq 'rust/crates/gh-app/src/callbacks.rs'
        }
    )

    $explicitList = @(Normalize-CheckNames -RawChecks $ExplicitChecks)
    $plan = New-Object System.Collections.Generic.List[object]

    foreach ($name in $script:CheckOrder) {
        $selected = $false
        $reason = ''

        if ($explicitList.Count -gt 0) {
            if ($explicitList -contains $name) {
                $selected = $true
                $reason = 'explicit -Checks'
            }
        } elseif ($Tier -eq 'full') {
            if ($name -eq 'web-bench') {
                if ($WebBench) {
                    $selected = $true
                    $reason = '-WebBench'
                }
            } elseif ($name -eq 'provider-live') {
                if ($ProviderLive) {
                    $selected = $true
                    $reason = '-ProviderLive'
                }
            } else {
                $selected = $true
                $reason = 'tier full'
            }
        } else {
            switch ($name) {
                'guards' {
                    $selected = $true
                    $reason = 'always'
                }
                'audit' {
                    $selected = $true
                    $reason = 'always'
                }
                'ext-i18n' {
                    if ($extI18nMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $extI18nMatches
                    }
                }
                'multimap-unit' {
                    if ($multimapUnitMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $multimapUnitMatches
                    }
                }
                'clippy' {
                    if ($rustMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $rustMatches
                    }
                }
                'nextest' {
                    if ($rustMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $rustMatches
                    }
                }
                'frames' {
                    if ($framesMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $framesMatches
                    }
                }
                'provider-fixtures' {
                    if ($providerFixturesMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $providerFixturesMatches
                    }
                }
                'ext-smoke' {
                    if ($extSmokeMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $extSmokeMatches
                    }
                }
                'blackbox' {
                    if ($blackboxMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $blackboxMatches
                    }
                }
                'qa' {
                    if ($qaMatches.Count -gt 0) {
                        $selected = $true
                        $reason = Format-MatchReason -MatchedPaths $qaMatches
                    }
                }
                'web-bench' {
                    if ($WebBench) {
                        $selected = $true
                        $reason = '-WebBench'
                    }
                }
                'provider-live' {
                    if ($ProviderLive) {
                        $selected = $true
                        $reason = '-ProviderLive'
                    }
                }
            }
        }

        $plan.Add([PSCustomObject]@{
            Name     = $name
            Selected = $selected
            Reason   = $reason
        })
    }

    $webBenchSelected = [bool](($plan | Where-Object { $_.Name -eq 'web-bench' } | Select-Object -First 1).Selected)
    $recommendWebBench = ($explicitList.Count -eq 0) -and ($Tier -eq 'auto') -and (-not $webBenchSelected) -and ($webBenchRecommendMatches.Count -gt 0)

    return [PSCustomObject]@{
        Plan              = @($plan.ToArray())
        ChangedPaths      = $paths
        RecommendWebBench = $recommendWebBench
        WebBenchMatches   = $webBenchRecommendMatches
    }
}

function Evaluate-Guards {
    param(
        [bool]$RustTargetExists,
        [int]$BaseIgnoreCount,
        [int]$CurrentIgnoreCount,
        [string[]]$ChangedBaselineFiles = @(),
        [switch]$AllowBaselineChange
    )

    $errors = New-Object System.Collections.Generic.List[string]
    $notes = New-Object System.Collections.Generic.List[string]

    if ($RustTargetExists) {
        $errors.Add('rust/target must not exist (keep Cargo target-dir outside rust/target).')
    }

    if ($CurrentIgnoreCount -gt $BaseIgnoreCount) {
        $delta = $CurrentIgnoreCount - $BaseIgnoreCount
        $errors.Add("$($script:RustIgnoreAttr) count under rust/**/*.rs increased: current=$CurrentIgnoreCount > base=$BaseIgnoreCount (+$delta).")
    } else {
        $notes.Add("$($script:RustIgnoreAttr) count under rust/**/*.rs: current=$CurrentIgnoreCount, base=$BaseIgnoreCount (ok).")
    }

    $normBaseline = @(
        $ChangedBaselineFiles |
            ForEach-Object { Normalize-RepoPath -Path $_ } |
            Where-Object { $_ -in $script:ProtectedBaselineFiles } |
            Select-Object -Unique
    )

    if ($AllowBaselineChange) {
        $notes.Add('-AllowBaselineChange was used (must only be used with explicit user approval).')
    }

    if ($normBaseline.Count -gt 0) {
        if ($AllowBaselineChange) {
            $notes.Add("Baseline/GT change(s) permitted via -AllowBaselineChange: $($normBaseline -join ', ').")
        } else {
            $errors.Add("Protected baseline/GT file(s) changed vs base ref: $($normBaseline -join ', '). Pass -AllowBaselineChange only with explicit user approval.")
        }
    } else {
        $notes.Add('Protected baseline/GT files unchanged vs base ref (ok).')
    }

    return [PSCustomObject]@{
        Passed = ($errors.Count -eq 0)
        Errors = @($errors.ToArray())
        Notes  = @($notes.ToArray())
    }
}

function Count-RustIgnoreInDir {
    param([string]$RustDir)

    if (-not (Test-Path -LiteralPath $RustDir -PathType Container)) {
        return 0
    }
    $targetPrefix = (Join-Path $RustDir 'target') + [System.IO.Path]::DirectorySeparatorChar
    $count = 0
    $files = Get-ChildItem -LiteralPath $RustDir -Filter '*.rs' -Recurse -File -ErrorAction SilentlyContinue
    foreach ($f in $files) {
        if ($f.FullName.StartsWith($targetPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
            continue
        }
        $content = [System.IO.File]::ReadAllText($f.FullName, [System.Text.Encoding]::UTF8)
        $count += [regex]::Matches($content, [regex]::Escape($script:RustIgnoreAttr)).Count
    }
    return $count
}

function Count-RustIgnoreAtRef {
    param(
        [string]$RepoRoot,
        [string]$Ref
    )

    $output = @(& git -C $RepoRoot grep -F -o $script:RustIgnoreAttr $Ref -- 'rust/*.rs' 2>&1)
    $rc = $LASTEXITCODE
    if ($rc -eq 1) {
        return 0
    }
    if ($rc -ne 0) {
        throw "git grep failed for ref '$Ref' (exit $rc): $($output -join "`n")"
    }
    $nonEmpty = @($output | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) })
    return $nonEmpty.Count
}

function Read-CargoConfigValues {
    param([string]$ConfigPath)

    $targetDir = $null
    $linker = $null
    if (-not (Test-Path -LiteralPath $ConfigPath -PathType Leaf)) {
        return [PSCustomObject]@{
            TargetDir = $null
            Linker    = $null
        }
    }

    $section = ''
    $lines = [System.IO.File]::ReadAllLines($ConfigPath, [System.Text.Encoding]::UTF8)
    foreach ($rawLine in $lines) {
        $line = $rawLine.Trim()
        if ($line.Length -eq 0 -or $line.StartsWith('#')) {
            continue
        }
        if ($line -match '^\[([^\]]+)\]') {
            $section = ($Matches[1].Trim() -replace '["'']', '')
            continue
        }
        if ($line -match '^([A-Za-z0-9_-]+)\s*=\s*(?:"((?:[^"\\]|\\.)*)"|''([^'']*)'')') {
            $key = $Matches[1].Trim()
            $val = if ($null -ne $Matches[2] -and $Matches[2].Length -gt 0 -or $line -match '=\s*"') {
                $Matches[2] -replace '\\\\', '\' -replace '\\"', '"'
            } else {
                $Matches[3]
            }
            if ($section -eq 'build' -and $key -eq 'target-dir') {
                $targetDir = $val
            } elseif ($section -eq 'target.x86_64-pc-windows-msvc' -and $key -eq 'linker') {
                $linker = $val
            }
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($targetDir) -and -not [System.IO.Path]::IsPathRooted($targetDir)) {
        $configRepoRoot = Split-Path -Path (Split-Path -Path $ConfigPath -Parent) -Parent
        $targetDir = [System.IO.Path]::GetFullPath((Join-Path $configRepoRoot $targetDir))
    }

    return [PSCustomObject]@{
        TargetDir = $targetDir
        Linker    = $linker
    }
}

function Resolve-MainCheckout {
    param([string]$RepoRoot)

    $lines = @(& git -C $RepoRoot worktree list --porcelain 2>$null)
    if ($LASTEXITCODE -eq 0) {
        foreach ($line in $lines) {
            $s = [string]$line
            if ($s.StartsWith('worktree ')) {
                $wt = $s.Substring('worktree '.Length).Trim()
                if ($wt.Length -gt 0) {
                    return [System.IO.Path]::GetFullPath($wt)
                }
            }
        }
    }
    return [System.IO.Path]::GetFullPath($RepoRoot)
}

function Resolve-FallbackPath {
    param(
        [string]$RepoRoot,
        [string]$MainCheckout,
        [string]$RelativePath,
        [ValidateSet('Leaf', 'Container')]
        [string]$PathType
    )

    $localCandidate = Join-Path $RepoRoot $RelativePath
    if (Test-Path -LiteralPath $localCandidate -PathType $PathType) {
        return [System.IO.Path]::GetFullPath($localCandidate)
    }
    if (-not [string]::IsNullOrWhiteSpace($MainCheckout)) {
        $mainCandidate = Join-Path $MainCheckout $RelativePath
        if (Test-Path -LiteralPath $mainCandidate -PathType $PathType) {
            return [System.IO.Path]::GetFullPath($mainCandidate)
        }
    }
    return $null
}

function Get-WorktreeTargetName {
    param([string]$RepoRoot)
    $full = [System.IO.Path]::GetFullPath($RepoRoot).TrimEnd('\')
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $hash = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($full.ToLowerInvariant()))).Replace('-', '').Substring(0, 8).ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
    $leaf = (Split-Path -Path $full -Leaf).ToLowerInvariant() -replace '[^a-z0-9._-]', '_'
    if ($leaf.Length -gt 48) { $leaf = $leaf.Substring(0, 48) }
    return "$leaf-$hash"
}

function Resolve-CheckEnvironment {
    param([string]$RepoRoot)

    $fullRepoRoot = [System.IO.Path]::GetFullPath($RepoRoot)
    $mainCheckout = Resolve-MainCheckout -RepoRoot $fullRepoRoot

    $localCargoConfig = Join-Path $fullRepoRoot '.cargo\config.toml'
    $mainCargoConfig = Join-Path $mainCheckout '.cargo\config.toml'
    $exportedCargoTargetDir = $false
    $exportedCargoLinker = $false

    if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR) -and -not (Test-Path -LiteralPath $localCargoConfig -PathType Leaf)) {
        if (Test-Path -LiteralPath $mainCargoConfig -PathType Leaf) {
            $cfg = Read-CargoConfigValues -ConfigPath $mainCargoConfig
            if (-not [string]::IsNullOrWhiteSpace($cfg.TargetDir)) {
                $env:CARGO_TARGET_DIR = $cfg.TargetDir
                $exportedCargoTargetDir = $true
            }
            if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER) -and -not [string]::IsNullOrWhiteSpace($cfg.Linker)) {
                $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $cfg.Linker
                $exportedCargoLinker = $true
            }
        }
    }

    # Linked worktrees must not share the main CARGO_TARGET_DIR: cargo hashes workspace crates by their
    # path relative to the workspace root, so every worktree writes the same build units (e.g.
    # debug\build\gh-core\<hash>) and freshness is judged against whichever tree built last. A run
    # could then test another worktree's code and still pass. Give each linked worktree its own
    # subdirectory of the shared target dir (cold the first time, incremental after); the main
    # checkout keeps using the shared dir. Remove it with -PruneWorktreeTargets once the worktree is gone.
    $worktreeTargetDir = $null
    if (-not [string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR) -and -not [string]::IsNullOrWhiteSpace($mainCheckout) -and
        -not [string]::Equals([System.IO.Path]::GetFullPath($mainCheckout).TrimEnd('\'), $fullRepoRoot.TrimEnd('\'), [System.StringComparison]::OrdinalIgnoreCase)) {
        $sharedTarget = [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR).TrimEnd('\')
        if ($sharedTarget -notmatch '(?i)[\\/]worktrees[\\/][^\\/]+$') {
            $worktreeTargetDir = Join-Path (Join-Path $sharedTarget 'worktrees') (Get-WorktreeTargetName -RepoRoot $fullRepoRoot)
            $env:CARGO_TARGET_DIR = $worktreeTargetDir
        }
    }

    $ffmpegCmd = Get-Command ffmpeg -ErrorAction SilentlyContinue | Select-Object -First 1
    $ffmpegExe = if ($null -ne $ffmpegCmd) { $ffmpegCmd.Source } else { $null }

    return [PSCustomObject]@{
        RepoRoot               = $fullRepoRoot
        MainCheckout           = $mainCheckout
        WorktreeTargetDir      = $worktreeTargetDir
        CargoTargetDir         = $env:CARGO_TARGET_DIR
        CargoLinker            = $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER
        ExportedCargoTargetDir = $exportedCargoTargetDir
        ExportedCargoLinker    = $exportedCargoLinker
        PythonExe              = Resolve-FallbackPath -RepoRoot $fullRepoRoot -MainCheckout $mainCheckout -RelativePath '.venv\Scripts\python.exe' -PathType 'Leaf'
        DataDir                = Resolve-FallbackPath -RepoRoot $fullRepoRoot -MainCheckout $mainCheckout -RelativePath 'data' -PathType 'Container'
        QaSessionsDir          = Resolve-FallbackPath -RepoRoot $fullRepoRoot -MainCheckout $mainCheckout -RelativePath 'scratch\qa_sessions' -PathType 'Container'
        WebScenarioFile        = Resolve-FallbackPath -RepoRoot $fullRepoRoot -MainCheckout $mainCheckout -RelativePath 'scratch\web_scenario_all.json' -PathType 'Leaf'
        FfmpegExe              = $ffmpegExe
    }
}

function Get-MergeBase {
    param(
        [string]$RepoRoot,
        [string]$BaseRef
    )

    $null = & git -C $RepoRoot rev-parse --verify --quiet "$BaseRef^{commit}" 2>$null
    if ($LASTEXITCODE -ne 0) {
        throw "Base ref '$BaseRef' does not resolve to a valid commit."
    }
    $mb = (& git -C $RepoRoot merge-base HEAD $BaseRef 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "git merge-base HEAD $BaseRef failed: $($mb -join "`n")"
    }
    $sha = ([string]($mb | Select-Object -First 1)).Trim()
    if ([string]::IsNullOrWhiteSpace($sha)) {
        throw "git merge-base HEAD $BaseRef returned an empty commit SHA."
    }
    return $sha
}

function Get-ChangedFiles {
    param(
        [string]$RepoRoot,
        [string]$MergeBase
    )

    $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    $list = New-Object System.Collections.Generic.List[string]

    $commands = @(
        , @('-c', 'core.quotepath=false', 'diff', '--name-only', "$MergeBase...HEAD")
        , @('-c', 'core.quotepath=false', 'diff', '--name-only')
        , @('-c', 'core.quotepath=false', 'diff', '--cached', '--name-only')
        , @('-c', 'core.quotepath=false', 'ls-files', '--others', '--exclude-standard')
    )

    foreach ($gitArgs in $commands) {
        $out = @(& git -C $RepoRoot @gitArgs 2>&1)
        if ($LASTEXITCODE -ne 0) {
            throw "git $($gitArgs -join ' ') failed (exit $LASTEXITCODE): $($out -join "`n")"
        }
        foreach ($line in $out) {
            $norm = Normalize-RepoPath -Path ([string]$line)
            if ($norm.Length -gt 0 -and $seen.Add($norm)) {
                $list.Add($norm)
            }
        }
    }

    return $list.ToArray()
}

function Get-ChangedBaselineFiles {
    param(
        [string]$RepoRoot,
        [string]$MergeBase
    )

    $out = @(& git -C $RepoRoot -c core.quotepath=false diff --name-only $MergeBase -- @($script:ProtectedBaselineFiles) 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "git diff for baseline files failed (exit $LASTEXITCODE): $($out -join "`n")"
    }
    $found = @(
        $out |
            ForEach-Object { Normalize-RepoPath -Path ([string]$_) } |
            Where-Object { $_.Length -gt 0 } |
            Select-Object -Unique
    )
    return $found
}

function Format-Duration {
    param([Nullable[double]]$Seconds)

    if ($null -eq $Seconds) {
        return '-'
    }
    return ('{0:F2}s' -f $Seconds)
}

function Invoke-CheckRunner {
    param(
        [object[]]$Plan,
        [hashtable]$Executors,
        [string[]]$NoticeLines = @(),
        [switch]$Quiet
    )

    $rows = New-Object System.Collections.Generic.List[object]
    $failedCheck = $null
    $firstFailureCode = 0
    $totalStopwatch = [System.Diagnostics.Stopwatch]::StartNew()

    foreach ($item in $Plan) {
        $name = [string]$item.Name
        if (-not $item.Selected) {
            $rows.Add([PSCustomObject]@{
                Name            = $name
                Status          = 'NOT SELECTED'
                DurationSeconds = $null
                ExitCode        = $null
            })
            continue
        }

        if ($null -ne $failedCheck) {
            $rows.Add([PSCustomObject]@{
                Name            = $name
                Status          = 'NOT RUN'
                DurationSeconds = $null
                ExitCode        = $null
            })
            continue
        }

        if (-not $Quiet) {
            [Console]::Out.WriteLine('')
            [Console]::Out.WriteLine("===> [$name] ($($item.Reason))")
        }

        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $outcome = @{ ExitCode = 0 }
        try {
            if (-not $Executors.ContainsKey($name)) {
                throw "No executor registered for check '$name'."
            }
            & $Executors[$name] $outcome
            $exitCode = [int]$outcome.ExitCode
        } catch {
            $exitCode = 1
            if (-not $Quiet) {
                [Console]::Out.WriteLine("[$name] ERROR: $($_.Exception.Message)")
            }
        } finally {
            $sw.Stop()
        }

        $dur = [math]::Round($sw.Elapsed.TotalSeconds, 3)
        if ($exitCode -eq 0) {
            $rows.Add([PSCustomObject]@{
                Name            = $name
                Status          = 'PASS'
                DurationSeconds = $dur
                ExitCode        = 0
            })
        } else {
            $failedCheck = $name
            $firstFailureCode = if ($exitCode -ne 0) { $exitCode } else { 1 }
            $rows.Add([PSCustomObject]@{
                Name            = $name
                Status          = 'FAIL'
                DurationSeconds = $dur
                ExitCode        = $firstFailureCode
            })
        }
    }

    $totalStopwatch.Stop()
    $totalSeconds = [math]::Round($totalStopwatch.Elapsed.TotalSeconds, 3)

    $ranRows = @($rows | Where-Object { $null -ne $_.DurationSeconds })
    $slowest = if ($ranRows.Count -gt 0) {
        $ranRows | Sort-Object -Property DurationSeconds -Descending | Select-Object -First 1
    } else {
        $null
    }

    $resultLine = if ($null -eq $failedCheck) { 'RESULT: PASS' } else { "RESULT: FAIL ($failedCheck)" }

    $summaryLines = New-Object System.Collections.Generic.List[string]
    $summaryLines.Add('')
    $summaryLines.Add('=== Check Summary ===')
    $summaryLines.Add(('{0,-17}  {1,-12}  {2,10}' -f 'Check', 'Status', 'Duration'))
    $summaryLines.Add(('{0,-17}  {1,-12}  {2,10}' -f '-----------------', '------------', '----------'))
    foreach ($r in $rows) {
        $durStr = Format-Duration -Seconds $r.DurationSeconds
        $summaryLines.Add(('{0,-17}  {1,-12}  {2,10}' -f $r.Name, $r.Status, $durStr))
    }
    $summaryLines.Add("Total time: $(Format-Duration -Seconds $totalSeconds)")
    if ($null -ne $slowest) {
        $summaryLines.Add("Slowest step: $($slowest.Name) ($(Format-Duration -Seconds $slowest.DurationSeconds))")
    } else {
        $summaryLines.Add('Slowest step: none')
    }
    foreach ($notice in $NoticeLines) {
        if (-not [string]::IsNullOrWhiteSpace($notice)) {
            $summaryLines.Add($notice)
        }
    }
    $summaryLines.Add($resultLine)

    if (-not $Quiet) {
        foreach ($line in $summaryLines) {
            [Console]::Out.WriteLine($line)
        }
    }

    return [PSCustomObject]@{
        Passed         = ($null -eq $failedCheck)
        FailedCheck    = $failedCheck
        ExitCode       = if ($null -eq $failedCheck) { 0 } else { $firstFailureCode }
        ResultLine     = $resultLine
        Rows           = @($rows.ToArray())
        TotalSeconds   = $totalSeconds
        SlowestName    = if ($null -ne $slowest) { $slowest.Name } else { $null }
        SlowestSeconds = if ($null -ne $slowest) { $slowest.DurationSeconds } else { $null }
        SummaryLines   = @($summaryLines.ToArray())
    }
}

function Invoke-NativeStream {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList = @()
    )

    $global:LASTEXITCODE = 0
    & $FilePath @ArgumentList | ForEach-Object { [Console]::Out.WriteLine([string]$_) }
    return [int]$LASTEXITCODE
}

function Resolve-CargoTargetDirectory {
    param([string]$RepoRoot)

    if (-not [string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
        return [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
    }
    $manifestPath = Join-Path $RepoRoot 'rust\Cargo.toml'
    $jsonText = (& cargo metadata --manifest-path $manifestPath --no-deps --format-version 1) -join "`n"
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed (exit $LASTEXITCODE)."
    }
    $meta = $jsonText | ConvertFrom-Json
    if ([string]::IsNullOrWhiteSpace($meta.target_directory)) {
        throw 'cargo metadata did not report target_directory.'
    }
    return [System.IO.Path]::GetFullPath($meta.target_directory)
}

# Parallel agents run check.ps1 from several worktrees that share one CARGO_TARGET_DIR, and cargo
# copies each tree's binaries to the same final path (cv-bench\gh-sim.exe, debug\GenshinHelper.exe).
# Building and then running that shared file lets run A execute the binary run B built a moment
# later (testing B's code), and on Windows a running exe cannot be replaced, so B's build fails with
# "Access is denied". So: build and copy the binaries under one cross-process lock per target dir,
# then run the private copy. The lock is a named mutex, so waiting is event-driven.
function Invoke-LockedBuildAndCopy {
    param(
        [string]$RepoRoot,
        [string[]]$BuildArgs,
        [string[]]$RelativeExes,
        [string]$Label
    )

    $targetDir = Resolve-CargoTargetDirectory -RepoRoot $RepoRoot
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $hash = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($targetDir.ToLowerInvariant()))).Replace('-', '').Substring(0, 16)
    } finally {
        $sha.Dispose()
    }
    $mutex = [System.Threading.Mutex]::new($false, "Local\GenshinHelperCheckBuild-$hash")
    $acquired = $false
    try {
        try {
            $acquired = $mutex.WaitOne(0)
            if (-not $acquired) {
                [Console]::Out.WriteLine("[$Label] Waiting for another check run to finish building in $targetDir ...")
                $acquired = $mutex.WaitOne([TimeSpan]::FromMinutes(30))
            }
        } catch [System.Threading.AbandonedMutexException] {
            $acquired = $true
        }
        if (-not $acquired) {
            [Console]::Out.WriteLine("[$Label] FAIL: build lock for $targetDir still held after 30 min.")
            return @{ ExitCode = 1 }
        }

        $rc = Invoke-NativeStream -FilePath 'cargo' -ArgumentList $BuildArgs
        if ($rc -ne 0) {
            return @{ ExitCode = $rc }
        }
        $copyDir = Join-Path ([System.IO.Path]::GetTempPath()) ('gh-check-bin-' + [guid]::NewGuid().ToString('N').Substring(0, 12))
        New-Item -ItemType Directory -Path $copyDir | Out-Null
        $exes = New-Object System.Collections.Generic.List[string]
        foreach ($rel in $RelativeExes) {
            $src = Join-Path $targetDir $rel
            if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
                [Console]::Out.WriteLine("[$Label] FAIL: built exe not found at $src")
                Remove-Item -LiteralPath $copyDir -Recurse -Force -ErrorAction SilentlyContinue
                return @{ ExitCode = 1 }
            }
            $dst = Join-Path $copyDir (Split-Path -Path $rel -Leaf)
            Copy-Item -LiteralPath $src -Destination $dst
            $exes.Add($dst)
        }
        return @{ ExitCode = 0; CopyDir = $copyDir; Exes = $exes.ToArray() }
    } finally {
        if ($acquired) {
            $mutex.ReleaseMutex()
        }
        $mutex.Dispose()
    }
}

function Build-RealExecutors {
    param(
        [object]$EnvInfo,
        [string]$MergeBase,
        [string[]]$QaOnly = @(),
        [switch]$AllowBaselineChange
    )

    $repoRoot = $EnvInfo.RepoRoot
    $manifestPath = Join-Path $repoRoot 'rust\Cargo.toml'

    return @{
        'guards' = {
            param($outcome)
            $rustTargetExists = Test-Path -LiteralPath (Join-Path $repoRoot 'rust\target')
            $baseIgnoreCount = Count-RustIgnoreAtRef -RepoRoot $repoRoot -Ref $MergeBase
            $currentIgnoreCount = Count-RustIgnoreInDir -RustDir (Join-Path $repoRoot 'rust')
            $changedBaselines = @(Get-ChangedBaselineFiles -RepoRoot $repoRoot -MergeBase $MergeBase)

            $res = Evaluate-Guards `
                -RustTargetExists $rustTargetExists `
                -BaseIgnoreCount $baseIgnoreCount `
                -CurrentIgnoreCount $currentIgnoreCount `
                -ChangedBaselineFiles $changedBaselines `
                -AllowBaselineChange:$AllowBaselineChange

            foreach ($note in $res.Notes) {
                [Console]::Out.WriteLine("[guards] $note")
            }
            foreach ($err in $res.Errors) {
                [Console]::Out.WriteLine("[guards] FAIL: $err")
            }

            # App locale keys: each must be read by app code or listed as reviewed-unused, so a
            # planned-but-unbuilt feature (like the silent Alt+F12 toast) cannot hide as a dead key.
            $localeExit = 1
            if ([string]::IsNullOrWhiteSpace($EnvInfo.PythonExe)) {
                [Console]::Out.WriteLine('[guards] FAIL: .venv\Scripts\python.exe not found; cannot check app locale keys.')
            } else {
                $localeExit = Invoke-NativeStream -FilePath $EnvInfo.PythonExe -ArgumentList @('-I', (Join-Path $repoRoot 'scripts\check_locale_keys.py'), '--repo', $repoRoot)
            }
            $outcome.ExitCode = if ($res.Passed -and $localeExit -eq 0) { 0 } else { 1 }
        }.GetNewClosure()

        'audit' = {
            param($outcome)
            if ([string]::IsNullOrWhiteSpace($EnvInfo.PythonExe)) {
                [Console]::Out.WriteLine('[audit] FAIL: .venv\Scripts\python.exe not found in current tree or main checkout.')
                $outcome.ExitCode = 1
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath $EnvInfo.PythonExe -ArgumentList @((Join-Path $repoRoot 'scripts\audit_release.py'), '--strict-ip')
        }.GetNewClosure()

        'ext-i18n' = {
            param($outcome)
            if ([string]::IsNullOrWhiteSpace($EnvInfo.PythonExe)) {
                [Console]::Out.WriteLine('[ext-i18n] FAIL: .venv\Scripts\python.exe not found in current tree or main checkout.')
                $outcome.ExitCode = 1
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath $EnvInfo.PythonExe -ArgumentList @((Join-Path $repoRoot 'tests\verify_i18n_and_ext_v5.py'))
        }.GetNewClosure()

        'multimap-unit' = {
            param($outcome)
            if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[multimap-unit] FAIL: node not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath 'node' -ArgumentList @((Join-Path $repoRoot 'tests\multimap_unit.mjs'))
        }.GetNewClosure()

        'clippy' = {
            param($outcome)
            if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[clippy] FAIL: cargo not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath 'cargo' -ArgumentList @('clippy', '--manifest-path', $manifestPath, '--workspace', '--all-targets', '--', '-D', 'warnings')
        }.GetNewClosure()

        'nextest' = {
            param($outcome)
            if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[nextest] FAIL: cargo not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath 'cargo' -ArgumentList @('nextest', 'run', '--manifest-path', $manifestPath, '--workspace')
        }.GetNewClosure()

        'frames' = {
            param($outcome)
            if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[frames] FAIL: cargo not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            $bin = Invoke-LockedBuildAndCopy -RepoRoot $repoRoot -Label 'frames' -RelativeExes @('cv-bench\gh-sim.exe') `
                -BuildArgs @('build', '-q', '--manifest-path', $manifestPath, '--profile', 'cv-bench', '-p', 'gh-sim')
            if ($bin.ExitCode -ne 0) {
                $outcome.ExitCode = $bin.ExitCode
                return
            }
            try {
                $outcome.ExitCode = Invoke-NativeStream -FilePath $bin.Exes[0] -ArgumentList @('frames')
            } finally {
                Remove-Item -LiteralPath $bin.CopyDir -Recurse -Force -ErrorAction SilentlyContinue
            }
        }.GetNewClosure()

        'provider-fixtures' = {
            param($outcome)
            if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[provider-fixtures] FAIL: node not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath 'node' -ArgumentList @((Join-Path $repoRoot 'tests\provider_dom_fixtures.mjs'))
        }.GetNewClosure()

        'ext-smoke' = {
            param($outcome)
            if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[ext-smoke] FAIL: node not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            $domRc = Invoke-NativeStream -FilePath 'node' -ArgumentList @((Join-Path $repoRoot 'tests\extension_dom_fixtures.mjs'))
            if ($domRc -ne 0) {
                $outcome.ExitCode = $domRc
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath 'node' -ArgumentList @((Join-Path $repoRoot 'tests\extension_browser_smoke.mjs'))
        }.GetNewClosure()

        'blackbox' = {
            param($outcome)
            if ([string]::IsNullOrWhiteSpace($EnvInfo.PythonExe)) {
                [Console]::Out.WriteLine('[blackbox] FAIL: .venv\Scripts\python.exe not found in current tree or main checkout.')
                $outcome.ExitCode = 1
                return
            }
            if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[blackbox] FAIL: cargo not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            [Console]::Out.WriteLine('[blackbox] Building GenshinHelper.exe and GenshinHelperHost.exe from current tree...')
            $targetDir = Resolve-CargoTargetDirectory -RepoRoot $repoRoot
            if ($targetDir -match '(?i)[\\/]dist([\\/]|$)') {
                [Console]::Out.WriteLine("[blackbox] FAIL: Refusing to test binaries from a target dir under dist/: $targetDir")
                $outcome.ExitCode = 1
                return
            }
            $bin = Invoke-LockedBuildAndCopy -RepoRoot $repoRoot -Label 'blackbox' `
                -RelativeExes @('debug\GenshinHelper.exe', 'debug\GenshinHelperHost.exe') `
                -BuildArgs @('build', '--manifest-path', $manifestPath, '-p', 'gh-app', '-p', 'gh-host', '--bins')
            if ($bin.ExitCode -ne 0) {
                $outcome.ExitCode = $bin.ExitCode
                return
            }
            $serverExe = $bin.Exes[0]
            $hostExe = $bin.Exes[1]
            try {
                [Console]::Out.WriteLine("[blackbox] Testing private copies built from this tree: server=$serverExe host=$hostExe")
                $outcome.ExitCode = Invoke-NativeStream -FilePath $EnvInfo.PythonExe -ArgumentList @((Join-Path $repoRoot 'tests\test_blackbox_contracts.py'), '--server-exe', $serverExe, '--host-exe', $hostExe)
            } finally {
                Remove-Item -LiteralPath $bin.CopyDir -Recurse -Force -ErrorAction SilentlyContinue
            }
        }.GetNewClosure()

        'qa' = {
            param($outcome)
            if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[qa] FAIL: cargo not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            if ([string]::IsNullOrWhiteSpace($EnvInfo.QaSessionsDir)) {
                [Console]::Out.WriteLine('[qa] FAIL: scratch\qa_sessions directory not found in current tree or main checkout.')
                $outcome.ExitCode = 1
                return
            }
            if ([string]::IsNullOrWhiteSpace($EnvInfo.DataDir)) {
                [Console]::Out.WriteLine('[qa] FAIL: data directory not found in current tree or main checkout.')
                $outcome.ExitCode = 1
                return
            }
            if ([string]::IsNullOrWhiteSpace($EnvInfo.FfmpegExe)) {
                [Console]::Out.WriteLine('[qa] FAIL: ffmpeg not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            $qaArgs = New-Object System.Collections.Generic.List[string]
            foreach ($a in @('qa', '--sessions', $EnvInfo.QaSessionsDir, '--data-dir', $EnvInfo.DataDir, '--ffmpeg', $EnvInfo.FfmpegExe)) {
                $qaArgs.Add($a)
            }
            foreach ($sess in $QaOnly) {
                $qaArgs.Add('--only')
                $qaArgs.Add($sess)
            }
            $bin = Invoke-LockedBuildAndCopy -RepoRoot $repoRoot -Label 'qa' -RelativeExes @('cv-bench\gh-sim.exe') `
                -BuildArgs @('build', '-q', '--manifest-path', $manifestPath, '--profile', 'cv-bench', '-p', 'gh-sim')
            if ($bin.ExitCode -ne 0) {
                $outcome.ExitCode = $bin.ExitCode
                return
            }
            try {
                $outcome.ExitCode = Invoke-NativeStream -FilePath $bin.Exes[0] -ArgumentList @($qaArgs.ToArray())
            } finally {
                Remove-Item -LiteralPath $bin.CopyDir -Recurse -Force -ErrorAction SilentlyContinue
            }
        }.GetNewClosure()

        'web-bench' = {
            param($outcome)
            if ([string]::IsNullOrWhiteSpace($EnvInfo.WebScenarioFile)) {
                [Console]::Out.WriteLine('[web-bench] FAIL: scratch\web_scenario_all.json not found in current tree or main checkout.')
                $outcome.ExitCode = 1
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath 'pwsh' -ArgumentList @('-NoProfile', '-File', (Join-Path $repoRoot 'scripts\web_bench.ps1'), '-Scenario', $EnvInfo.WebScenarioFile)
        }.GetNewClosure()

        'provider-live' = {
            param($outcome)
            if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
                [Console]::Out.WriteLine('[provider-live] FAIL: node not found on PATH.')
                $outcome.ExitCode = 1
                return
            }
            $outcome.ExitCode = Invoke-NativeStream -FilePath 'node' -ArgumentList @((Join-Path $repoRoot 'tests\provider_live_smoke.mjs'))
        }.GetNewClosure()
    }
}

function Invoke-SelfTest {
    $failures = New-Object System.Collections.Generic.List[string]

    function Assert-True {
        param([bool]$Condition, [string]$Label)
        if ($Condition) {
            $script:SelfTestPassed++
        } else {
            $script:SelfTestFailures.Add($Label)
            [Console]::Out.WriteLine("  [FAIL] $Label")
        }
    }

    $script:SelfTestPassed = 0
    $script:SelfTestFailures = $failures

    [Console]::Out.WriteLine('Running scripts/check.ps1 self-tests...')

    # 1. Path -> selected checks table
    $selectionCases = @(
        @{
            Name     = 'README only -> guards+audit only'
            Paths    = @('README.md')
            Expected = @('guards', 'audit')
            RecWb    = $false
        }
        @{
            Name     = 'extension/** -> ext-i18n, multimap-unit, provider-fixtures, ext-smoke + web-bench recommendation'
            Paths    = @('extension/popup.js')
            Expected = @('guards', 'audit', 'ext-i18n', 'multimap-unit', 'provider-fixtures', 'ext-smoke')
            RecWb    = $true
        }
        @{
            Name     = 'assets/locales/** -> ext-i18n, clippy, nextest (bundles are include_str! into gh-core)'
            Paths    = @('assets/locales/en.json')
            Expected = @('guards', 'audit', 'ext-i18n', 'clippy', 'nextest')
            RecWb    = $false
        }
        @{
            Name     = 'tests/verify_i18n_and_ext_v5.py -> ext-i18n'
            Paths    = @('tests/verify_i18n_and_ext_v5.py')
            Expected = @('guards', 'audit', 'ext-i18n')
            RecWb    = $false
        }
        @{
            Name     = 'assets/zoom_thumb_template.png -> clippy, nextest, frames, qa (CV template compiled into gh-cv)'
            Paths    = @('assets/zoom_thumb_template.png')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'frames', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'tests/multimap_unit.mjs -> multimap-unit'
            Paths    = @('tests/multimap_unit.mjs')
            Expected = @('guards', 'audit', 'multimap-unit')
            RecWb    = $false
        }
        @{
            Name     = 'tests/fixtures/multimap/** -> multimap-unit, provider-fixtures'
            Paths    = @('tests/fixtures/multimap/transform_vectors.json')
            Expected = @('guards', 'audit', 'multimap-unit', 'provider-fixtures')
            RecWb    = $false
        }
        @{
            Name     = 'tests/provider_dom_fixtures.mjs -> provider-fixtures'
            Paths    = @('tests/provider_dom_fixtures.mjs')
            Expected = @('guards', 'audit', 'provider-fixtures')
            RecWb    = $false
        }
        @{
            Name     = 'tests/fixtures/providers/** -> provider-fixtures'
            Paths    = @('tests/fixtures/providers/appsample_state.json')
            Expected = @('guards', 'audit', 'provider-fixtures')
            RecWb    = $false
        }
        @{
            Name     = 'rust/crates/gh-core/src/map_bridge.rs -> multimap-unit, clippy, nextest, qa'
            Paths    = @('rust/crates/gh-core/src/map_bridge.rs')
            Expected = @('guards', 'audit', 'multimap-unit', 'clippy', 'nextest', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'tests/extension_browser_smoke.mjs -> ext-smoke'
            Paths    = @('tests/extension_browser_smoke.mjs')
            Expected = @('guards', 'audit', 'ext-smoke')
            RecWb    = $false
        }
        @{
            Name     = 'tests/extension_dom_fixtures.mjs -> ext-smoke'
            Paths    = @('tests/extension_dom_fixtures.mjs')
            Expected = @('guards', 'audit', 'ext-smoke')
            RecWb    = $false
        }
        @{
            Name     = 'tests/fixtures/hoyolab_popup/** -> ext-smoke'
            Paths    = @('tests/fixtures/hoyolab_popup/en.html')
            Expected = @('guards', 'audit', 'ext-smoke')
            RecWb    = $false
        }
        @{
            Name     = 'rust/** (gh-hud) -> clippy, nextest only'
            Paths    = @('rust/crates/gh-hud/src/lib.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest')
            RecWb    = $false
        }
        @{
            Name     = 'rust/crates/gh-net/** (sync_server) -> clippy, nextest, blackbox + web-bench rec'
            Paths    = @('rust/crates/gh-net/src/sync_server.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'blackbox')
            RecWb    = $true
        }
        @{
            Name     = 'rust/crates/gh-net/src/hd_assets* -> clippy, nextest, blackbox, qa'
            Paths    = @('rust/crates/gh-net/src/hd_assets.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'blackbox', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'rust/crates/gh-app/src/tracking_loop.rs -> clippy, nextest, blackbox, qa + web-bench rec'
            Paths    = @('rust/crates/gh-app/src/tracking_loop.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'blackbox', 'qa')
            RecWb    = $true
        }
        @{
            Name     = 'rust/crates/gh-app/src/callbacks.rs -> clippy, nextest, blackbox, qa + web-bench rec'
            Paths    = @('rust/crates/gh-app/src/callbacks.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'blackbox', 'qa')
            RecWb    = $true
        }
        @{
            Name     = 'tests/test_blackbox_contracts.py -> blackbox'
            Paths    = @('tests/test_blackbox_contracts.py')
            Expected = @('guards', 'audit', 'blackbox')
            RecWb    = $false
        }
        @{
            Name     = 'tests/blackbox_contracts/** -> blackbox'
            Paths    = @('tests/blackbox_contracts/test_tier1_routes.py')
            Expected = @('guards', 'audit', 'blackbox')
            RecWb    = $false
        }
        @{
            Name     = 'rust/crates/gh-cv/** -> clippy, nextest, frames, qa'
            Paths    = @('rust/crates/gh-cv/src/locator.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'frames', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'rust/crates/gh-core/** (non-ui_layout) -> clippy, nextest, qa'
            Paths    = @('rust/crates/gh-core/src/lib.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'rust/crates/gh-core/src/ui_layout.rs -> clippy, nextest, frames, qa'
            Paths    = @('rust/crates/gh-core/src/ui_layout.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'frames', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'rust/crates/gh-sim/** -> clippy, nextest, frames, qa'
            Paths    = @('rust/crates/gh-sim/src/qa.rs')
            Expected = @('guards', 'audit', 'clippy', 'nextest', 'frames', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'tests/frame_fixtures/** -> frames'
            Paths    = @('tests/frame_fixtures/21x9_2560x1080_hud_shift.json')
            Expected = @('guards', 'audit', 'frames')
            RecWb    = $false
        }
        @{
            Name     = 'tests/qa_baseline.json -> qa'
            Paths    = @('tests/qa_baseline.json')
            Expected = @('guards', 'audit', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'tests/verified_gt.json -> qa'
            Paths    = @('tests/verified_gt.json')
            Expected = @('guards', 'audit', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'tests/scenarios/** -> qa'
            Paths    = @('tests/scenarios/map_switch.json')
            Expected = @('guards', 'audit', 'qa')
            RecWb    = $false
        }
        @{
            Name     = 'mixed paths (README + extension + gh-cv)'
            Paths    = @('README.md', 'extension/i18n.js', 'rust/crates/gh-cv/src/minimap_tracker.rs')
            Expected = @('guards', 'audit', 'ext-i18n', 'multimap-unit', 'clippy', 'nextest', 'frames', 'provider-fixtures', 'ext-smoke', 'qa')
            RecWb    = $true
        }
        @{
            Name     = 'Windows backslash separators'
            Paths    = @('extension\page_main.js', 'rust\crates\gh-net\src\hd_assets_cache.rs', 'tests\blackbox_contracts\sandbox.py', 'tests\scenarios\s1.json')
            Expected = @('guards', 'audit', 'ext-i18n', 'multimap-unit', 'clippy', 'nextest', 'provider-fixtures', 'ext-smoke', 'blackbox', 'qa')
            RecWb    = $true
        }
    )

    foreach ($tc in $selectionCases) {
        $sel = Select-CheckPlan -ChangedPaths $tc.Paths -Tier 'auto'
        $got = @($sel.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name })
        $gotStr = $got -join ','
        $expStr = $tc.Expected -join ','
        Assert-True ($gotStr -eq $expStr) "selection [$($tc.Name)]: expected=[$expStr] got=[$gotStr]"
        Assert-True ($sel.RecommendWebBench -eq $tc.RecWb) "selection [$($tc.Name)] RecommendWebBench: expected=$($tc.RecWb) got=$($sel.RecommendWebBench)"
    }

    # Tier full, -WebBench, -ProviderLive, explicit -Checks ordering
    $fullPlan = Select-CheckPlan -ChangedPaths @('README.md') -Tier 'full'
    $fullGot = (@($fullPlan.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name }) -join ',')
    $fullExp = 'guards,audit,ext-i18n,multimap-unit,clippy,nextest,frames,provider-fixtures,ext-smoke,blackbox,qa'
    Assert-True ($fullGot -eq $fullExp) "tier full without -WebBench/-ProviderLive: expected=[$fullExp] got=[$fullGot]"

    $fullWbPlan = Select-CheckPlan -ChangedPaths @('README.md') -Tier 'full' -WebBench
    $fullWbGot = (@($fullWbPlan.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name }) -join ',')
    $fullWbExp = "$fullExp,web-bench"
    Assert-True ($fullWbGot -eq $fullWbExp) "tier full with -WebBench: expected=[$fullWbExp] got=[$fullWbGot]"

    $fullPlPlan = Select-CheckPlan -ChangedPaths @('README.md') -Tier 'full' -ProviderLive
    $fullPlGot = (@($fullPlPlan.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name }) -join ',')
    $fullPlExp = "$fullExp,provider-live"
    Assert-True ($fullPlGot -eq $fullPlExp) "tier full with -ProviderLive: expected=[$fullPlExp] got=[$fullPlGot]"

    $fullBothPlan = Select-CheckPlan -ChangedPaths @('README.md') -Tier 'full' -WebBench -ProviderLive
    $fullBothGot = (@($fullBothPlan.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name }) -join ',')
    $fullBothExp = "$fullExp,web-bench,provider-live"
    Assert-True ($fullBothGot -eq $fullBothExp) "tier full with -WebBench and -ProviderLive: expected=[$fullBothExp] got=[$fullBothGot]"

    $autoWbPlan = Select-CheckPlan -ChangedPaths @('extension/popup.js') -Tier 'auto' -WebBench
    $autoWbGot = (@($autoWbPlan.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name }) -join ',')
    Assert-True ($autoWbGot -eq 'guards,audit,ext-i18n,multimap-unit,provider-fixtures,ext-smoke,web-bench') "auto with -WebBench: got=[$autoWbGot]"
    Assert-True (-not $autoWbPlan.RecommendWebBench) 'auto with -WebBench should not duplicate recommendation'

    $autoPlPlan = Select-CheckPlan -ChangedPaths @('README.md') -Tier 'auto' -ProviderLive
    $autoPlGot = (@($autoPlPlan.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name }) -join ',')
    Assert-True ($autoPlGot -eq 'guards,audit,provider-live') "auto with -ProviderLive: got=[$autoPlGot]"

    $explicitPlan = Select-CheckPlan -ChangedPaths @('README.md') -Tier 'auto' -ExplicitChecks @('qa,frames,guards', 'clippy,nextest')
    $explicitGot = (@($explicitPlan.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name }) -join ',')
    Assert-True ($explicitGot -eq 'guards,clippy,nextest,frames,qa') "explicit -Checks preserves fixed order (frames after nextest): got=[$explicitGot]"

    $explicitPlPlan = Select-CheckPlan -ChangedPaths @('README.md') -Tier 'auto' -ExplicitChecks @('provider-live,provider-fixtures,multimap-unit,guards')
    $explicitPlGot = (@($explicitPlPlan.Plan | Where-Object { $_.Selected } | ForEach-Object { $_.Name }) -join ',')
    Assert-True ($explicitPlGot -eq 'guards,multimap-unit,provider-fixtures,provider-live') "explicit -Checks preserves multimap/provider order: got=[$explicitPlGot]"

    # Extra args parser for repeatable -QaOnly
    $parsedExtra = Parse-ExtraArgs -RawArgs @('-QaOnly', 'sess_A', '-QaOnly', 'sess_B,sess_C', '-QaOnly:sess_D')
    Assert-True (($parsedExtra.QaOnly -join ',') -eq 'sess_A,sess_B,sess_C,sess_D') "Parse-ExtraArgs repeatable -QaOnly: got=[$($parsedExtra.QaOnly -join ',')]"

    # 2. Guards evaluation tests
    $gClean = Evaluate-Guards -RustTargetExists $false -BaseIgnoreCount 26 -CurrentIgnoreCount 26 -ChangedBaselineFiles @()
    Assert-True $gClean.Passed 'guards: clean state passes'

    $gTarget = Evaluate-Guards -RustTargetExists $true -BaseIgnoreCount 26 -CurrentIgnoreCount 26 -ChangedBaselineFiles @()
    Assert-True (-not $gTarget.Passed) 'guards: rust/target existing fails'

    $gIgnoreInc = Evaluate-Guards -RustTargetExists $false -BaseIgnoreCount 26 -CurrentIgnoreCount 27 -ChangedBaselineFiles @()
    Assert-True (-not $gIgnoreInc.Passed) "guards: $($script:RustIgnoreAttr) count increase fails"

    $gIgnoreDec = Evaluate-Guards -RustTargetExists $false -BaseIgnoreCount 26 -CurrentIgnoreCount 25 -ChangedBaselineFiles @()
    Assert-True $gIgnoreDec.Passed "guards: $($script:RustIgnoreAttr) count decrease passes"

    $gBaseFail = Evaluate-Guards -RustTargetExists $false -BaseIgnoreCount 26 -CurrentIgnoreCount 26 -ChangedBaselineFiles @('tests\qa_baseline.json')
    Assert-True (-not $gBaseFail.Passed) 'guards: qa_baseline.json change without -AllowBaselineChange fails'

    $gGtFail = Evaluate-Guards -RustTargetExists $false -BaseIgnoreCount 26 -CurrentIgnoreCount 26 -ChangedBaselineFiles @('tests/verified_gt.json')
    Assert-True (-not $gGtFail.Passed) 'guards: verified_gt.json change without -AllowBaselineChange fails'

    $gBaseAllow = Evaluate-Guards -RustTargetExists $false -BaseIgnoreCount 26 -CurrentIgnoreCount 26 -ChangedBaselineFiles @('tests/qa_baseline.json') -AllowBaselineChange
    Assert-True $gBaseAllow.Passed 'guards: baseline change allowed with -AllowBaselineChange'
    Assert-True (($gBaseAllow.Notes -join ' ') -match '-AllowBaselineChange') 'guards: notes record -AllowBaselineChange usage'

    # Temp-dir tests for Count-RustIgnoreInDir and Read-CargoConfigValues (cleaned up in finally)
    $tmpRoot = Join-Path $env:TEMP ('gh_check_selftest_' + [Guid]::NewGuid().ToString('N'))
    try {
        $fakeRust = Join-Path $tmpRoot 'rust\crates\demo\src'
        New-Item -ItemType Directory -Path $fakeRust -Force | Out-Null
        $rsFile = Join-Path $fakeRust 'lib.rs'
        $ignAttr = $script:RustIgnoreAttr
        [System.IO.File]::WriteAllText($rsFile, "#[test]`n$ignAttr = ""slow""]`nfn a() {}`n$ignAttr]`nfn b() {}`n", [System.Text.Encoding]::UTF8)
        $cnt1 = Count-RustIgnoreInDir -RustDir (Join-Path $tmpRoot 'rust')
        Assert-True ($cnt1 -eq 2) "Count-RustIgnoreInDir initial count: expected=2 got=$cnt1"

        [System.IO.File]::AppendAllText($rsFile, "$ignAttr]`nfn c() {}`n", [System.Text.Encoding]::UTF8)
        $cnt2 = Count-RustIgnoreInDir -RustDir (Join-Path $tmpRoot 'rust')
        Assert-True ($cnt2 -eq 3) "Count-RustIgnoreInDir after adding ${ignAttr}: expected=3 got=$cnt2"

        $fakeCargoDir = Join-Path $tmpRoot '.cargo'
        New-Item -ItemType Directory -Path $fakeCargoDir -Force | Out-Null
        $fakeCargoToml = Join-Path $fakeCargoDir 'config.toml'
        $expectedTargetDir = [System.IO.Path]::GetFullPath((Join-Path $tmpRoot 'fake_cargo_target'))
        $escapedTargetDir = $expectedTargetDir.Replace('\', '\\')
        $tomlBody = @(
            '# synthetic config'
            '[build]'
            "target-dir = `"$escapedTargetDir`""
            ''
            '[target.x86_64-pc-windows-msvc]'
            'linker = "lld-link.exe"'
        ) -join "`n"
        [System.IO.File]::WriteAllText($fakeCargoToml, $tomlBody, [System.Text.Encoding]::UTF8)
        $parsedCargo = Read-CargoConfigValues -ConfigPath $fakeCargoToml
        Assert-True ($parsedCargo.TargetDir -eq $expectedTargetDir) "Read-CargoConfigValues target-dir: expected=$expectedTargetDir got=$($parsedCargo.TargetDir)"
        Assert-True ($parsedCargo.Linker -eq 'lld-link.exe') "Read-CargoConfigValues linker: got=$($parsedCargo.Linker)"

        # Per-worktree target names: stable across slash style / case / trailing separator (git worktree
        # list prints forward slashes), distinct for two worktrees with the same leaf name.
        $wtA = Get-WorktreeTargetName -RepoRoot 'C:/work/x/.t3/worktrees/Genshin-Helper/crew-abc'
        $wtB = Get-WorktreeTargetName -RepoRoot 'c:\WORK\x\.t3\worktrees\genshin-helper\CREW-ABC\'
        $wtC = Get-WorktreeTargetName -RepoRoot 'D:\other\crew-abc'
        Assert-True ($wtA -eq $wtB) "Get-WorktreeTargetName must ignore slash style/case: $wtA vs $wtB"
        Assert-True ($wtA -ne $wtC) "Get-WorktreeTargetName must differ for different paths with the same leaf: $wtA vs $wtC"
        Assert-True ($wtA -match '^crew-abc-[0-9a-f]{8}$') "Get-WorktreeTargetName format: $wtA"
    } finally {
        if (Test-Path -LiteralPath $tmpRoot) {
            Remove-Item -LiteralPath $tmpRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    # 3. Runner tests (fake steps + seeded fault)
    $runnerPlan = (Select-CheckPlan -ChangedPaths @('rust/crates/gh-cv/src/lib.rs') -Tier 'auto').Plan
    # Selected: guards, audit, clippy, nextest, frames, qa

    $passInvoked = New-Object System.Collections.Generic.List[string]
    $passExecs = @{}
    foreach ($name in $script:CheckOrder) {
        $n = $name
        $passExecs[$n] = {
            param($outcome)
            $passInvoked.Add($n)
            $outcome.ExitCode = 0
        }.GetNewClosure()
    }
    $passRun = Invoke-CheckRunner -Plan $runnerPlan -Executors $passExecs -Quiet
    Assert-True ($passRun.ExitCode -eq 0) "runner all-pass: ExitCode expected=0 got=$($passRun.ExitCode)"
    Assert-True ($passRun.ResultLine -eq 'RESULT: PASS') "runner all-pass: ResultLine expected='RESULT: PASS' got='$($passRun.ResultLine)'"
    Assert-True ($passRun.SummaryLines[-1] -eq 'RESULT: PASS') "runner all-pass: last summary line is RESULT: PASS"
    Assert-True (($passInvoked -join ',') -eq 'guards,audit,clippy,nextest,frames,qa') "runner all-pass: invoked=[$($passInvoked -join ',')]"

    # Seeded fault: flip clippy from 0 -> 1
    $failInvoked = New-Object System.Collections.Generic.List[string]
    $failExecs = @{}
    foreach ($name in $script:CheckOrder) {
        $n = $name
        $failExecs[$n] = {
            param($outcome)
            $failInvoked.Add($n)
            $outcome.ExitCode = if ($n -eq 'clippy') { 1 } else { 0 }
        }.GetNewClosure()
    }
    $failRun = Invoke-CheckRunner -Plan $runnerPlan -Executors $failExecs -Quiet
    Assert-True ($failRun.ExitCode -ne 0) "runner seeded-fault: ExitCode expected!=0 got=$($failRun.ExitCode)"
    Assert-True ($failRun.ResultLine -eq 'RESULT: FAIL (clippy)') "runner seeded-fault: ResultLine expected='RESULT: FAIL (clippy)' got='$($failRun.ResultLine)'"
    Assert-True ($failRun.SummaryLines[-1] -eq 'RESULT: FAIL (clippy)') "runner seeded-fault: last summary line is RESULT: FAIL (clippy)"
    Assert-True (($failInvoked -join ',') -eq 'guards,audit,clippy') "runner seeded-fault: stopped at clippy, invoked=[$($failInvoked -join ',')]"

    $statusByCheck = @{}
    foreach ($row in $failRun.Rows) {
        $statusByCheck[$row.Name] = $row.Status
    }
    Assert-True ($statusByCheck['guards'] -eq 'PASS') 'runner seeded-fault: guards=PASS'
    Assert-True ($statusByCheck['audit'] -eq 'PASS') 'runner seeded-fault: audit=PASS'
    Assert-True ($statusByCheck['ext-i18n'] -eq 'NOT SELECTED') 'runner seeded-fault: ext-i18n=NOT SELECTED'
    Assert-True ($statusByCheck['clippy'] -eq 'FAIL') 'runner seeded-fault: clippy=FAIL'
    Assert-True ($statusByCheck['nextest'] -eq 'NOT RUN') 'runner seeded-fault: nextest=NOT RUN'
    Assert-True ($statusByCheck['frames'] -eq 'NOT RUN') 'runner seeded-fault: frames=NOT RUN'
    Assert-True ($statusByCheck['qa'] -eq 'NOT RUN') 'runner seeded-fault: qa=NOT RUN'
    Assert-True ($passRun.ResultLine -ne $failRun.ResultLine -and $passRun.ExitCode -ne $failRun.ExitCode) 'runner seeded-fault: flipping fake step flips RESULT and exit code'

    if ($failures.Count -eq 0) {
        [Console]::Out.WriteLine("All $($script:SelfTestPassed) assertions passed.")
        [Console]::Out.WriteLine('SELFTEST PASS')
        return 0
    } else {
        [Console]::Out.WriteLine("$($failures.Count) assertion(s) failed ($($script:SelfTestPassed) passed).")
        [Console]::Out.WriteLine('SELFTEST FAIL')
        return 1
    }
}

# --- Main Entrypoint ---
try {
    $extra = Parse-ExtraArgs -RawArgs $args
    $qaOnlyList = @($extra.QaOnly)
    $explicitChecks = @(Normalize-CheckNames -RawChecks $Checks)
} catch {
    [Console]::Error.WriteLine("[check.ps1] ERROR: $($_.Exception.Message)")
    exit 2
}

if ($PruneWorktreeTargets) {
    # Delete <shared CARGO_TARGET_DIR>\worktrees\<name> folders whose linked worktree no longer exists.
    $pruneRoot = [System.IO.Path]::GetFullPath($PSScriptRoot + '\..')
    $mainCo = Resolve-MainCheckout -RepoRoot $pruneRoot
    $null = Resolve-CheckEnvironment -RepoRoot $mainCo
    # The main checkout usually gets its target dir from its own .cargo/config.toml, not the env var.
    $sharedTargetDir = Resolve-CargoTargetDirectory -RepoRoot $mainCo
    $wtRoot = Join-Path $sharedTargetDir 'worktrees'
    if (-not (Test-Path -LiteralPath $wtRoot -PathType Container)) {
        [Console]::Out.WriteLine("[prune] $wtRoot does not exist; nothing to prune.")
        exit 0
    }
    $live = @{}
    foreach ($line in @(& git -C $mainCo worktree list --porcelain 2>$null)) {
        $s = [string]$line
        if ($s.StartsWith('worktree ')) {
            $live[(Get-WorktreeTargetName -RepoRoot $s.Substring('worktree '.Length).Trim())] = $true
        }
    }
    $removed = 0
    foreach ($d in @(Get-ChildItem -LiteralPath $wtRoot -Directory)) {
        if ($live.ContainsKey($d.Name)) {
            [Console]::Out.WriteLine("[prune] keep   $($d.Name) (worktree still exists)")
            continue
        }
        Remove-Item -LiteralPath $d.FullName -Recurse -Force
        [Console]::Out.WriteLine("[prune] removed $($d.Name)")
        $removed++
    }
    [Console]::Out.WriteLine("[prune] removed $removed orphaned worktree target dir(s).")
    exit 0
}

if ($SelfTest) {
    $rc = Invoke-SelfTest
    exit $rc
}

$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
Push-Location -LiteralPath $RepoRoot
try {
    $mergeBase = Get-MergeBase -RepoRoot $RepoRoot -BaseRef $Base
    $changedPaths = @(Get-ChangedFiles -RepoRoot $RepoRoot -MergeBase $mergeBase)
    $selection = Select-CheckPlan -ChangedPaths $changedPaths -Tier $Tier -ExplicitChecks $explicitChecks -WebBench:$WebBench -ProviderLive:$ProviderLive

    $recommendLine = if ($selection.RecommendWebBench) {
        "RECOMMEND: Run with -WebBench (close GenshinHelper first to free port 8765) -- changed: $(Format-MatchReason -MatchedPaths $selection.WebBenchMatches)"
    } else {
        $null
    }

    if ($DryRun) {
        foreach ($item in $selection.Plan) {
            if ($item.Selected) {
                [Console]::Out.WriteLine("$($item.Name): selected ($($item.Reason))")
            } else {
                [Console]::Out.WriteLine("$($item.Name): not selected")
            }
        }
        if ($null -ne $recommendLine) {
            [Console]::Out.WriteLine($recommendLine)
        }
        exit 0
    }

    $envInfo = Resolve-CheckEnvironment -RepoRoot $RepoRoot
    if ($envInfo.ExportedCargoTargetDir) {
        [Console]::Out.WriteLine("[env] Exported CARGO_TARGET_DIR=$($envInfo.CargoTargetDir) (from main checkout .cargo/config.toml)")
    }
    if ($envInfo.ExportedCargoLinker) {
        [Console]::Out.WriteLine("[env] Exported CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=$($envInfo.CargoLinker) (from main checkout .cargo/config.toml)")
    }
    if ($envInfo.WorktreeTargetDir) {
        [Console]::Out.WriteLine("[env] Linked worktree: using its own CARGO_TARGET_DIR=$($envInfo.WorktreeTargetDir) (shared dir would mix build units between worktrees)")
    }

    $executors = Build-RealExecutors `
        -EnvInfo $envInfo `
        -MergeBase $mergeBase `
        -QaOnly $qaOnlyList `
        -AllowBaselineChange:$AllowBaselineChange

    $notices = if ($null -ne $recommendLine) { @($recommendLine) } else { @() }
    $runResult = Invoke-CheckRunner -Plan $selection.Plan -Executors $executors -NoticeLines $notices

    exit $runResult.ExitCode
} catch {
    [Console]::Error.WriteLine("[check.ps1] ERROR: $($_.Exception.Message)")
    exit 2
} finally {
    Pop-Location
}
