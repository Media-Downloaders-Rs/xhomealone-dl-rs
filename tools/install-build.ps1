param([switch]$Launch)
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot)).TrimEnd([IO.Path]::DirectorySeparatorChar)
$source = Join-Path $root 'target/release/xhomealone-dl.exe'
$destination = Join-Path $root 'xhomealone-dl.exe'
if (Get-Process -Name 'xhomealone-dl' -ErrorAction SilentlyContinue) {
    throw 'Cierra XHomeAlone antes de actualizar; no se detendra ninguna descarga.'
}
if (-not (Test-Path -LiteralPath $source)) { throw 'Compila primero con cargo build --release --locked.' }
foreach ($path in @($source,$destination)) {
    if (-not ([IO.Path]::GetFullPath($path)).StartsWith($root + [IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)) { throw 'Ruta fuera del repositorio.' }
}
$temporary = Join-Path $root ('target/xhomealone-dl-install-' + [Guid]::NewGuid().ToString('N') + '.exe')
[IO.File]::Copy($source,$temporary)
$expected = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
if ((Get-FileHash -LiteralPath $temporary -Algorithm SHA256).Hash -ne $expected) { throw 'No coincide el hash de la copia.' }
if (Test-Path -LiteralPath $destination) {
    $backupDirectory = Join-Path $root 'target/previous-builds'
    [IO.Directory]::CreateDirectory($backupDirectory) | Out-Null
    $backup = Join-Path $backupDirectory ('xhomealone-dl-' + [Guid]::NewGuid().ToString('N') + '.exe')
    [IO.File]::Replace($temporary,$destination,$backup)
} else { [IO.File]::Move($temporary,$destination) }
if ((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ne $expected) { throw 'No coincide el hash del ejecutable instalado.' }
Write-Output 'XHomeAlone actualizado. Las descargas y SQLite se conservan.'
if ($Launch) { Start-Process -FilePath $destination -WorkingDirectory $root -WindowStyle Normal }
