<#
.SYNOPSIS
  Build a release of Imperio, sign the update manifest, and publish the exe
  as a GitHub Release asset.

.DESCRIPTION
  1. Builds Imperio.exe (static, release) and copies it to dist\.
  2. Writes site\latest.json with the version from Cargo.toml, the GitHub
     Release download URL, the SHA-256 of the exe and your notes, and signs it
     with the private key from IMPERIO_SIGNING_KEY.
  3. Creates GitHub Release v<version> with Imperio.exe attached (gh CLI).
  Committing site\latest.json and pushing is done by publish-update.ps1
  (GitHub Pages then serves the manifest and the website).

.EXAMPLE
  $env:IMPERIO_SIGNING_KEY = "<private seed from `imperio keygen`>"
  .\tools\release.ps1 -Notes "Faster connections, tray icon, session recording"
#>
param(
  [string]$Notes = "",
  [string]$GitHubRepo = "devaccindia-hash/imperio",
  [switch]$SkipBuild,
  [switch]$SkipRelease
)
$ErrorActionPreference = "Stop"
$repoDir = Resolve-Path (Join-Path $PSScriptRoot "..")
$env:Path = "C:\Program Files\GitHub CLI;$env:USERPROFILE\.cargo\bin;" + $env:Path

if (-not $env:IMPERIO_SIGNING_KEY) {
  Write-Error "Set IMPERIO_SIGNING_KEY to the private seed first (see docs\RELEASING.md)."
}

$version = (Select-String -Path (Join-Path $repoDir "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
Write-Host "Releasing Imperio $version"

if (-not $SkipBuild) {
  $env:RUSTFLAGS = "-C target-feature=+crt-static"
  Push-Location $repoDir
  # cargo prints progress on stderr; run it through cmd so Windows PowerShell
  # does not turn that into a terminating error.
  & cmd /c "cargo build --release -p imperio -p imperio-host -p imperio-viewer 2>&1"
  if ($LASTEXITCODE -ne 0) { Pop-Location; Write-Error "build failed" }
  Pop-Location
}

$exe = Join-Path $repoDir "target\release\imperio.exe"
$dist = Join-Path $repoDir "dist"
Copy-Item $exe (Join-Path $dist "Imperio.exe") -Force
Copy-Item (Join-Path $repoDir "target\release\imperio-host.exe") (Join-Path $dist "imperio-host.exe") -Force
Copy-Item (Join-Path $repoDir "target\release\imperio-viewer.exe") (Join-Path $dist "imperio-viewer.exe") -Force

# --- manifest: the download URL is the versioned release asset -----------------
$url = "https://github.com/$GitHubRepo/releases/download/v$version/Imperio.exe"
$manifest = @{ version = $version; url = $url; sha256 = ""; notes = $Notes; sig = "" } | ConvertTo-Json
$latest = Join-Path $repoDir "site\latest.json"
[System.IO.File]::WriteAllText($latest, $manifest, (New-Object System.Text.UTF8Encoding $false))

# Imperio.exe is a GUI-subsystem program: wait for it explicitly.
$sign = Start-Process -FilePath $exe -ArgumentList @("release-sign", "`"$latest`"", "--key", $env:IMPERIO_SIGNING_KEY, "--exe", "`"$exe`"") -Wait -NoNewWindow -PassThru
if ($sign.ExitCode -ne 0) { Write-Error "signing failed (exit $($sign.ExitCode))" }
$signed = Get-Content $latest -Raw | ConvertFrom-Json
if (-not $signed.sig) { Write-Error "latest.json was not signed" }
Write-Host "signed latest.json: version $($signed.version), sha256 $($signed.sha256)"

# --- GitHub Release with the exe attached -------------------------------------
if (-not $SkipRelease) {
  $tag = "v$version"
  $asset = Join-Path $dist "Imperio.exe"
  $notesArg = if ($Notes) { $Notes } else { "Imperio $version" }
  # gh writes progress to stderr; run it as a process so Windows PowerShell
  # does not treat that as an error, and check the exit code ourselves.
  $probe = Start-Process -FilePath "gh" -ArgumentList @("release", "view", $tag, "--repo", $GitHubRepo) -Wait -NoNewWindow -PassThru -RedirectStandardOutput "$env:TEMP\imperio-gh-view.txt" -RedirectStandardError "$env:TEMP\imperio-gh-view.err"
  if ($probe.ExitCode -eq 0) {
    Write-Host "Release $tag exists; replacing its Imperio.exe"
    $args = @("release", "upload", $tag, "`"$asset`"", "--clobber", "--repo", $GitHubRepo)
  } else {
    $args = @("release", "create", $tag, "`"$asset`"", "--repo", $GitHubRepo, "--title", "`"Imperio $version`"", "--notes", "`"$notesArg`"")
  }
  $gh = Start-Process -FilePath "gh" -ArgumentList $args -Wait -NoNewWindow -PassThru
  if ($gh.ExitCode -ne 0) { Write-Error "GitHub release failed (exit $($gh.ExitCode)); is 'gh auth login' done?" }
  Write-Host "Release asset: $url" -ForegroundColor Green
}
