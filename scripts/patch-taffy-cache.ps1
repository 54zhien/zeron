<#
.SYNOPSIS
Patches zgx's vendored taffy (zgx-taffy) Cache::get in Cargo's git checkout cache
so MinContent probes are never falsely reused as definite-width height measurements.

.DESCRIPTION
When a CSS Grid column uses `minmax(min-content, Nfr)` (such as Zeron's Markdown
tables) and a cell's longest unbreakable token determines the column's final width,
upstream taffy's `Cache::get` matches `known_dimensions.width == Some(cached_size.width)`
against the earlier `AvailableSpace::MinContent` probe (Slot 8). For wrapping text,
a MinContent probe breaks at every UAX #14 opportunity (each CJK character / word on
its own line), producing a huge inflated height that then gets locked in as the row's
height while GPU rendering draws the text normally across far fewer lines.

Patching the fetched checkout in Cargo's git cache keeps `apps/zgx/Cargo.toml` and
`apps/zgx/Cargo.lock` completely untouched so automated merges from `zeronsh/zeron`
never conflict.
#>
param(
    [string]$RepoRoot = (Split-Path $PSScriptRoot -Parent),
    [switch]$CheckOnly
)

$ErrorActionPreference = 'Stop'

$lockPath = Join-Path $RepoRoot 'apps/zgx/Cargo.lock'
if (-not (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
    throw "Missing $lockPath"
}

$lockText = Get-Content -Raw -LiteralPath $lockPath
$lockMatch = [regex]::Match(
    $lockText,
    '(?ms)\[\[package\]\]\s+name = "zgx-taffy"\s+version = "([^"]+)"\s+source = "git\+[^"]*#([0-9a-f]{7,40})"'
)
if (-not $lockMatch.Success) {
    throw "Could not find git-sourced zgx-taffy revision in $lockPath"
}
$taffyVersion = $lockMatch.Groups[1].Value
$zgxRev = $lockMatch.Groups[2].Value
$shortRev = $zgxRev.Substring(0, 7)

$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$checkoutsRoot = Join-Path $cargoHome 'git/checkouts'
if (-not (Test-Path -LiteralPath $checkoutsRoot -PathType Container)) {
    throw "Cargo git checkouts directory not found at $checkoutsRoot. Run 'cargo fetch --locked --manifest-path apps/zgx/Cargo.toml' first."
}

$cacheFiles = @()
foreach ($repoDir in (Get-ChildItem -LiteralPath $checkoutsRoot -Directory -Filter 'zgx-*')) {
    foreach ($revDir in (Get-ChildItem -LiteralPath $repoDir.FullName -Directory)) {
        if ($revDir.Name -eq $shortRev -or $revDir.Name.StartsWith($shortRev)) {
            $candidate = Join-Path $revDir.FullName 'vendor/taffy/src/tree/cache.rs'
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                $cacheFiles += $candidate
            }
        }
    }
}

if ($cacheFiles.Count -eq 0) {
    throw "No zgx-taffy cache.rs found for zgx rev $shortRev under $checkoutsRoot. Run 'cargo fetch --locked --manifest-path apps/zgx/Cargo.toml' first."
}

$marker = '// fork-fix: min-content-cache'

$oldBlock = @'
        match run_mode {
            RunMode::PerformLayout => self
                .final_layout_entry
                .filter(|entry| {
                    let cached_size = entry.content.size;
                    (known_dimensions.width == entry.known_dimensions.width
                        || known_dimensions.width == Some(cached_size.width))
                        && (known_dimensions.height == entry.known_dimensions.height
                            || known_dimensions.height == Some(cached_size.height))
                        && (known_dimensions.width.is_some()
                            || entry.available_space.width.is_roughly_equal(available_space.width))
                        && (known_dimensions.height.is_some()
                            || entry.available_space.height.is_roughly_equal(available_space.height))
                })
                .map(|e| e.content),
            RunMode::ComputeSize => {
                for entry in self.measure_entries.iter().flatten() {
                    let cached_size = entry.content;

                    if (known_dimensions.width == entry.known_dimensions.width
                        || known_dimensions.width == Some(cached_size.width))
                        && (known_dimensions.height == entry.known_dimensions.height
                            || known_dimensions.height == Some(cached_size.height))
                        && (known_dimensions.width.is_some()
                            || entry.available_space.width.is_roughly_equal(available_space.width))
                        && (known_dimensions.height.is_some()
                            || entry.available_space.height.is_roughly_equal(available_space.height))
                    {
                        return Some(LayoutOutput::from_outer_size(cached_size));
                    }
                }

                None
            }
            RunMode::PerformHiddenLayout => None,
        }
'@

$newBlock = @'
        // fork-fix: min-content-cache
        // Never reuse a cached entry produced under AvailableSpace::MinContent (or a
        // Definite constraint narrower than the returned size) when queried with
        // known_dimensions equal to that returned size. For wrapping text, MinContent
        // breaks at every UAX #14 opportunity (e.g. every CJK character on its own
        // line), so its cached height is vastly taller than laying out at that width.
        #[inline]
        fn matches_axis(
            known: Option<f32>,
            entry_known: Option<f32>,
            cached: f32,
            entry_available: AvailableSpace,
            available: AvailableSpace,
        ) -> bool {
            if known.is_some() {
                known == entry_known
                    || (known == Some(cached)
                        && match entry_available {
                            AvailableSpace::MaxContent => true,
                            AvailableSpace::Definite(avail) => avail >= cached,
                            AvailableSpace::MinContent => false,
                        })
            } else {
                known == entry_known && entry_available.is_roughly_equal(available)
            }
        }

        match run_mode {
            RunMode::PerformLayout => self
                .final_layout_entry
                .filter(|entry| {
                    let cached_size = entry.content.size;
                    matches_axis(
                        known_dimensions.width,
                        entry.known_dimensions.width,
                        cached_size.width,
                        entry.available_space.width,
                        available_space.width,
                    ) && matches_axis(
                        known_dimensions.height,
                        entry.known_dimensions.height,
                        cached_size.height,
                        entry.available_space.height,
                        available_space.height,
                    )
                })
                .map(|e| e.content),
            RunMode::ComputeSize => {
                for entry in self.measure_entries.iter().flatten() {
                    let cached_size = entry.content;

                    if matches_axis(
                        known_dimensions.width,
                        entry.known_dimensions.width,
                        cached_size.width,
                        entry.available_space.width,
                        available_space.width,
                    ) && matches_axis(
                        known_dimensions.height,
                        entry.known_dimensions.height,
                        cached_size.height,
                        entry.available_space.height,
                        available_space.height,
                    ) {
                        return Some(LayoutOutput::from_outer_size(cached_size));
                    }
                }

                None
            }
            RunMode::PerformHiddenLayout => None,
        }
'@

$oldNorm = $oldBlock.Replace("`r`n", "`n")
$newNorm = $newBlock.Replace("`r`n", "`n")
$patchedAny = $false

foreach ($file in $cacheFiles) {
    $raw = [IO.File]::ReadAllText($file)
    if ($raw.Contains($marker)) {
        Write-Host "zgx-taffy ($taffyVersion @ $shortRev) already patched: $file"
        continue
    }

    $usesCrlf = $raw.Contains("`r`n")
    $norm = $raw.Replace("`r`n", "`n")
    if (-not $norm.Contains($oldNorm)) {
        # If upstream already fixed Cache::get to guard MinContent, allow clean skip.
        $getIdx = $norm.IndexOf('pub fn get(')
        $storeIdx = $norm.IndexOf('pub fn store(')
        if ($getIdx -ge 0 -and $storeIdx -gt $getIdx) {
            $getBody = $norm.Substring($getIdx, $storeIdx - $getIdx)
            if ($getBody.Contains('MinContent')) {
                Write-Host "Upstream zgx-taffy ($taffyVersion @ $shortRev) already handles MinContent in Cache::get; skipping patch: $file"
                continue
            }
        }
        throw "Unexpected Cache::get layout in $file (zgx-taffy $taffyVersion @ $shortRev); please review scripts/patch-taffy-cache.ps1."
    }

    if ($CheckOnly) {
        throw "zgx-taffy ($taffyVersion @ $shortRev) is NOT patched yet: $file"
    }

    $updated = $norm.Replace($oldNorm, $newNorm)
    if ($usesCrlf) {
        $updated = $updated.Replace("`n", "`r`n")
    }
    [IO.File]::WriteAllText($file, $updated, [Text.UTF8Encoding]::new($false))
    $patchedAny = $true
    Write-Host "Patched zgx-taffy ($taffyVersion @ $shortRev): $file"
}

if ($patchedAny) {
    # Clear zgx-taffy fingerprints in apps/zgx/target so restored caches always rebuild it.
    $targetDir = Join-Path $RepoRoot 'apps/zgx/target'
    if (Test-Path -LiteralPath $targetDir -PathType Container) {
        foreach ($profile in @('debug', 'release')) {
            $fpDir = Join-Path $targetDir "$profile/.fingerprint"
            if (Test-Path -LiteralPath $fpDir -PathType Container) {
                Get-ChildItem -LiteralPath $fpDir -Directory -Filter 'zgx-taffy-*' | ForEach-Object {
                    Remove-Item -LiteralPath $_.FullName -Recurse -Force
                    Write-Host "Cleared stale fingerprint: $($_.FullName)"
                }
            }
        }
    }
}
