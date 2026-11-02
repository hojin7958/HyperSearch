param(
    [ValidateSet('build','run','test','bench')][string]$Action = 'build',
    [string]$Root = 'C:\'
)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$localCargo = Join-Path $PSScriptRoot '.tools\cargo\bin'
if (Test-Path -LiteralPath (Join-Path $localCargo 'cargo.exe')) {
    $env:CARGO_HOME = Join-Path $PSScriptRoot '.tools\cargo'
    $env:RUSTUP_HOME = Join-Path $PSScriptRoot '.tools\rustup'
    $env:PATH = $localCargo + ';' + $env:PATH
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'Rust MSVC toolchain is required: https://rust-lang.org/tools/install/' }
if ($Action -eq 'test') { cargo test --release; exit $LASTEXITCODE }
cargo build --release --bins
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
New-Item -ItemType Directory -Force -Path dist | Out-Null
Copy-Item -LiteralPath target\release\hyperfinder.exe -Destination dist\Hyperfinder.exe -Force
Copy-Item -LiteralPath target\release\qf-bench.exe -Destination dist\qf-bench.exe -Force
Copy-Item -LiteralPath README.md -Destination dist\README.md -Force
# Old releases needed a separate helper. The worker is now in Hyperfinder.exe.
foreach ($obsoleteName in @('qf-indexer.exe', 'qf-indexer.zip')) {
    $obsoletePath = Join-Path $PSScriptRoot ('dist\' + $obsoleteName)
    if (Test-Path -LiteralPath $obsoletePath) {
        Remove-Item -LiteralPath $obsoletePath -Force
    }
}
Compress-Archive -LiteralPath dist\Hyperfinder.exe -DestinationPath dist\Hyperfinder.zip -Force
if ($Action -eq 'run') { & .\dist\Hyperfinder.exe }
if ($Action -eq 'bench') { & .\dist\qf-bench.exe --root $Root --out qa\benchmark.json }
