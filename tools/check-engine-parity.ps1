param([string]$Repository = (Split-Path -Parent $PSScriptRoot))
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath($Repository).TrimEnd([IO.Path]::DirectorySeparatorChar)
$site = (Split-Path -Leaf $root) -replace '-dl-rs$',''
if (-not (Test-Path -LiteralPath (Join-Path $root 'Cargo.toml'))) { throw 'Cargo manifest missing.' }
$siteDir = Join-Path $root 'src/sites'
$expected = "$site.rs"
if (-not (Test-Path -LiteralPath (Join-Path $siteDir $expected))) { throw "Provider module missing: $expected" }
$allowed = @('mod.rs', $expected, 'archive.rs', 'archive_sources.rs')
$extra = @(Get-ChildItem -LiteralPath $siteDir -File -Filter '*.rs' | Where-Object { $_.Name -notin $allowed })
if ($extra.Count -gt 0) { throw "Foreign provider modules: $($extra.Name -join ', ')" }
$modules = [IO.File]::ReadAllText((Join-Path $siteDir 'mod.rs'))
if ($modules -notmatch ('(?m)^pub mod ' + [regex]::Escape($site) + ';\r?$')) { throw 'Own provider is not exposed.' }
Write-Output "Single-site provider verified: $site"