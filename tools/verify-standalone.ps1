param([string]$Repository = (Split-Path -Parent $PSScriptRoot))
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath($Repository).TrimEnd([IO.Path]::DirectorySeparatorChar)
$manifest = Join-Path $root 'Cargo.toml'
$raw = & cargo metadata --manifest-path $manifest --format-version 1 --no-deps --locked --offline
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
$metadata = ($raw -join "`n") | ConvertFrom-Json
$package = @($metadata.packages | Where-Object { [IO.Path]::GetFullPath($_.manifest_path) -eq $manifest })
if ($package.Count -ne 1) { throw 'Expected one root package.' }
foreach ($dependency in $package[0].dependencies) {
    if ($dependency.path) {
        $resolved = [IO.Path]::GetFullPath($dependency.path)
        if (-not $resolved.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Dependency outside repository: $($dependency.name) -> $resolved"
        }
    }
}
foreach ($target in $package[0].targets) {
    if (-not ([IO.Path]::GetFullPath($target.src_path)).StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Source outside repository: $($target.name)"
    }
}
$readme = Get-Content -LiteralPath (Join-Path $root 'README.md') -Raw -Encoding UTF8
$buildSection = [regex]::Match($readme, '(?s)## Compilar(.*?)(?:\r?\n## |\z)').Groups[1].Value
if ([regex]::IsMatch($buildSection, '\.\.[\\/][A-Za-z0-9_-]+-dl-rs')) {
    throw 'Build documentation still requires a sibling repository.'
}
Write-Output "Standalone package verified: $($package[0].name)"
