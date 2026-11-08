<#
.SYNOPSIS
  No-code update publisher. Run via PUBLISH-UPDATE.bat (double-click) or:
    powershell -File tools\publish-update.ps1 [-Version 0.4.0] [-Notes "..."]

  Asks two questions (new version, what changed), then: bumps the version,
  builds the static Imperio.exe, signs site\latest.json, publishes the exe as
  a GitHub Release, commits and pushes (GitHub Pages redeploys the website
  and the manifest), and confirms the live version. The private signing seed
  is asked for once and kept in %USERPROFILE%\.imperio\signing.key.
#>
param(
  [string]$Version = "",
  [string]$Notes = "",
  [string]$Key = "",
  [string]$GitHubRepo = "devaccindia-hash/imperio",
  [string]$PagesUrl = "https://devaccindia-hash.github.io/imperio",
  [switch]$NoPush
)
$ErrorActionPreference = "Stop"
$repoDir = Split-Path -Parent $PSScriptRoot
$env:Path = "C:\Program Files\GitHub CLI;$env:USERPROFILE\.cargo\bin;" + $env:Path

Write-Host ""
Write-Host "  ===  Publish an Imperio update  ===" -ForegroundColor Cyan
Write-Host ""

# --- preflight ----------------------------------------------------------------
& cmd /c "gh auth status >nul 2>&1"
if ($LASTEXITCODE -ne 0) { throw "GitHub CLI is not logged in. Run:  gh auth login   (once) and try again." }

# --- signing key (asked once, then remembered) ------------------------------
$keyFile = Join-Path $env:USERPROFILE ".imperio\signing.key"
if ($Key) {
  $key = $Key.Trim()
  if (-not (Test-Path $keyFile)) {
    # Remember it so the plain double-click never has to ask.
    New-Item -ItemType Directory -Force (Split-Path $keyFile) | Out-Null
    [IO.File]::WriteAllText($keyFile, $key)
  }
} elseif (Test-Path $keyFile) {
  $key = (Get-Content $keyFile -Raw).Trim()
} else {
  Write-Host "  First time only: paste your PRIVATE signing seed (from 'Imperio.exe keygen')."
  Write-Host "  It is saved to $keyFile so you are never asked again."
  $key = (Read-Host "  Private seed").Trim()
  if (-not $key) { throw "No key given." }
  New-Item -ItemType Directory -Force (Split-Path $keyFile) | Out-Null
  [IO.File]::WriteAllText($keyFile, $key)
}
$env:IMPERIO_SIGNING_KEY = $key

# --- version ------------------------------------------------------------------
$cargo = Join-Path $repoDir "Cargo.toml"
$current = (Select-String -Path $cargo -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$parts = $current.Split('.')
$suggest = "$($parts[0]).$($parts[1]).$([int]$parts[2] + 1)"
if (-not $Version) {
  $answer = Read-Host "  Current version is $current. New version [$suggest]"
  $Version = if ($answer) { $answer.Trim() } else { $suggest }
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version must look like 1.2.3 (got '$Version')." }
if (-not $Notes) {
  $Notes = Read-Host "  What changed? (one sentence your users will see)"
}

# --- bump ---------------------------------------------------------------------
$text = Get-Content $cargo -Raw
$re = [regex]'(?m)^version\s*=\s*"[^"]+"'
$text = $re.Replace($text, "version = `"$Version`"", 1)
[IO.File]::WriteAllText($cargo, $text, (New-Object System.Text.UTF8Encoding $false))
Write-Host "  Version set to $Version" -ForegroundColor Green

# --- build, sign, GitHub Release ------------------------------------------------
& (Join-Path $repoDir "tools\release.ps1") -Notes $Notes -GitHubRepo $GitHubRepo

# --- commit + push (GitHub Pages redeploys site/ including latest.json) ---------
Push-Location $repoDir
& cmd /c "git add Cargo.toml Cargo.lock site/latest.json 2>&1" | Out-Null
& cmd /c "git commit -q -m `"Release $Version`" -m `"$Notes`" 2>&1" | Out-Null
if (-not $NoPush) {
  Write-Host "  Pushing to GitHub..." -ForegroundColor Cyan
  & cmd /c "git push origin HEAD 2>&1" | Select-Object -Last 2 | ForEach-Object { "  $_" }
  if ($LASTEXITCODE -ne 0) { Pop-Location; throw "git push failed" }
}
Pop-Location

# --- verify ---------------------------------------------------------------------
if (-not $NoPush) {
  Write-Host "  Waiting for GitHub Pages to publish the new manifest (usually 1-2 minutes)..."
  $live = $null
  for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Seconds 6
    try {
      $live = (Invoke-WebRequest -UseBasicParsing "$PagesUrl/latest.json" -Headers @{ "Cache-Control" = "no-cache" }).Content | ConvertFrom-Json
      if ($live.version -eq $Version) { break }
    } catch { }
  }
  if ($live -and $live.version -eq $Version) {
    Write-Host ""
    Write-Host "  Published Imperio $Version." -ForegroundColor Green
    Write-Host "  Download: https://github.com/$GitHubRepo/releases/latest/download/Imperio.exe"
    Write-Host "  Users see the update on their next start (or within a few hours in the tray)."
  } else {
    Write-Warning "GitHub Pages has not served $Version yet. Check the 'Deploy website' run at https://github.com/$GitHubRepo/actions"
  }
}
