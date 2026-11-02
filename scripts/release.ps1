param(
    [switch]$Publish,
    [string]$NotesFile
)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath (Split-Path -Parent $PSScriptRoot)
$repo = 'hojin7958/HyperSearch'
$version = [regex]::Match((Get-Content Cargo.toml -Raw), '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Cargo.toml needs a stable x.y.z version.' }
$tag = "v$version"
if ($Publish) {
    if (-not $NotesFile -or -not (Test-Path -LiteralPath $NotesFile -PathType Leaf)) { throw 'Use -NotesFile with reviewed release notes.' }
    $NotesFile = (Resolve-Path -LiteralPath $NotesFile).Path
    if (git status --porcelain) { throw 'Commit the release source and Cargo.lock before publishing.' }
    $remote = git remote get-url origin
    if ($LASTEXITCODE -ne 0 -or $remote -notmatch '^https://github\.com/hojin7958/HyperSearch(?:\.git)?$') { throw 'Check the origin repository before publishing.' }
    gh release view $tag --repo $repo *> $null
    if ($LASTEXITCODE -eq 0) { throw "$tag already exists. Increase Cargo.toml version first." }
}
$env:CARGO_HOME = Join-Path $PWD '.tools\cargo'
$env:RUSTUP_HOME = Join-Path $PWD '.tools\rustup'
$localCargo = Join-Path $env:CARGO_HOME 'bin'
if (Test-Path -LiteralPath (Join-Path $localCargo 'cargo.exe')) {
    $env:PATH = $localCargo + ';' + $env:PATH
} else {
    Remove-Item Env:CARGO_HOME
    Remove-Item Env:RUSTUP_HOME
}
cargo fmt --all --check
if ($LASTEXITCODE -ne 0) { throw 'Formatting check failed.' }
cargo test --locked --release -j1 -- --test-threads=1
if ($LASTEXITCODE -ne 0) { throw 'Release tests failed.' }
cargo build --locked --release --bin hyperfinder -j1
if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
$output = Join-Path $PWD "_workspace\releases\$tag"
New-Item -ItemType Directory -Force -Path $output | Out-Null
$executable = Join-Path $output 'Hyperfinder.exe'
Copy-Item -LiteralPath target\release\hyperfinder.exe -Destination $executable -Force
$actualVersion = (Get-Item -LiteralPath $executable).VersionInfo.ProductVersion
if ($actualVersion -ne $version) { throw "Executable version mismatch: $actualVersion / $version" }
$zip = Join-Path $output "Hyperfinder-$version-Windows-x64.zip"
Compress-Archive -LiteralPath $executable -DestinationPath $zip -Force
$checksums = Join-Path $output 'SHA256SUMS.txt'
@($executable, $zip) | ForEach-Object {
    '{0}  {1}' -f (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant(), (Split-Path -Leaf $_)
} | Set-Content -LiteralPath $checksums -Encoding ascii
if ($Publish) {
    $branch = git branch --show-current
    git push origin "${branch}:main"
    if ($LASTEXITCODE -ne 0) { throw 'Source push failed.' }
    git tag -a $tag -m "Hyperfinder $version"
    if ($LASTEXITCODE -ne 0) { throw 'Tag creation failed.' }
    git push origin "refs/tags/$tag"
    if ($LASTEXITCODE -ne 0) { throw 'Tag push failed.' }
    # Keep the release hidden until all three files are uploaded.
    gh release create $tag $executable $zip $checksums --repo $repo --verify-tag --draft --title "Hyperfinder $version" --notes-file $NotesFile
    if ($LASTEXITCODE -ne 0) { throw 'Draft release creation failed.' }
    gh release edit $tag --repo $repo --draft=false --latest
    if ($LASTEXITCODE -ne 0) { throw 'Release publication failed; inspect the draft.' }
}
Write-Output "Release files: $output"
