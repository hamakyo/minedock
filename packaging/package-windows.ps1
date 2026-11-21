param(
    [string]$OutputDirectory = (Join-Path (Get-Location) "dist"),
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = (& cargo metadata --no-deps --format-version 1 | ConvertFrom-Json).packages |
        Where-Object { $_.name -eq "minedock-app" } |
        Select-Object -First 1 -ExpandProperty version
}

$workspace = (Get-Location).Path
$releaseDirectory = Join-Path $workspace "target\release"
$stageDirectory = Join-Path $OutputDirectory "minedock-$Version-windows-x64"
$archive = Join-Path $OutputDirectory "minedock-$Version-windows-x64.zip"

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
if (Test-Path -LiteralPath $stageDirectory) {
    Remove-Item -LiteralPath $stageDirectory -Recurse -Force
}
if (Test-Path -LiteralPath $archive) {
    Remove-Item -LiteralPath $archive -Force
}

& cargo build --locked --release -p minedock-app
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed"
}

$executable = Join-Path $releaseDirectory "minedock-app.exe"
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "release executable was not produced: $executable"
}

New-Item -ItemType Directory -Force -Path $stageDirectory | Out-Null
Copy-Item -LiteralPath $executable -Destination (Join-Path $stageDirectory "MineDock.exe")
Copy-Item -LiteralPath (Join-Path $workspace "README.md") -Destination (Join-Path $stageDirectory "README.md")
Copy-Item -LiteralPath (Join-Path $workspace "README.ja.md") -Destination (Join-Path $stageDirectory "README.ja.md")
Set-Content -LiteralPath (Join-Path $stageDirectory "VERSION") -Value $Version -NoNewline

Compress-Archive -LiteralPath $stageDirectory -DestinationPath $archive -CompressionLevel Optimal
$hash = Get-FileHash -LiteralPath $archive -Algorithm SHA256
Set-Content -LiteralPath "$archive.sha256" -Value "$($hash.Hash.ToLowerInvariant())  $(Split-Path $archive -Leaf)"

Write-Output "Created $archive"
Write-Output "Created $archive.sha256"
