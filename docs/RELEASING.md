# Releasing Imperio and pushing updates

Everything lives on GitHub:

| What | Where |
|---|---|
| Source | https://github.com/devaccindia-hash/imperio (public) |
| Website + update manifest | GitHub Pages: https://devaccindia-hash.github.io/imperio/ (built from `site/` by `.github/workflows/pages.yml`) |
| Downloads (`Imperio.exe`) | GitHub Releases: `https://github.com/devaccindia-hash/imperio/releases/download/v<version>/Imperio.exe`; the website's button uses `.../releases/latest/download/Imperio.exe` |

The app checks `https://devaccindia-hash.github.io/imperio/latest.json` at start
(and every few hours), verifies its Ed25519 signature against the public key
embedded in `apps/imperio/src/keys.rs`, downloads the release asset, checks
its SHA-256, replaces itself and restarts. No servers of ours, no bandwidth
limits.

## The easy way (no code)

Double-click **`PUBLISH-UPDATE.bat`**. It asks:

1. the new version (Enter accepts the suggestion, e.g. 0.3.8 → 0.3.9);
2. one sentence describing what changed (shown to users).

The first time it also asks for the private signing seed (kept in
`%USERPROFILE%\.imperio\signing.key`). It then builds, signs `site/latest.json`,
creates GitHub Release `v<version>` with `Imperio.exe` attached, commits,
pushes, waits for GitHub Pages to publish, and confirms. Requires the GitHub
CLI to be logged in once: `gh auth login`.

## One-time setup (already done unless noted)

- **Signing key.** `Imperio.exe keygen` generated the pair on 2026-11-08; the
  public half is in `keys.rs`. Keep the private seed in a password manager.
  Never commit it (`.gitignore` blocks `*.key` and `*signing key*`).
- **GitHub CLI.** `winget install GitHub.cli`, then `gh auth login`.
- **GitHub Pages.** Repository → Settings → Pages → Source: *GitHub Actions*.
  (Or: `gh api -X POST repos/devaccindia-hash/imperio/pages -f build_type=workflow`.)
  The repository must be public for Pages and public release downloads on a
  free account.
- **Custom domain (optional).** Settings → Pages → Custom domain, plus a CNAME
  at your DNS provider. Then change `DEFAULT_URL` in `apps/imperio/src/update.rs`
  and `UPGRADE_URL` in `apps/imperio/src/license.rs`, and `-PagesUrl` in the
  publish script. Keep the old URL serving `latest.json` for a while so
  already-installed copies still find updates.

## Manual steps (what the button does)

```
$env:IMPERIO_SIGNING_KEY = "<private seed>"
# bump version in Cargo.toml, then:
.\tools\release.ps1 -Notes "What changed"        # build, sign latest.json, gh release create
git add Cargo.toml Cargo.lock site/latest.json
git commit -m "Release 0.4.0"; git push          # Pages redeploys site + manifest
```

`release.ps1 -SkipBuild` re-signs/re-uploads without rebuilding;
`-SkipRelease` builds and signs only.

## Licence keys (Pro)

    Imperio.exe license-sign --key <private seed> --name "Jane Doe" --plan pro --expires 2027-12-31

Options: `--expires YYYY-MM-DD` (omit for perpetual), `--seats N`. Send the
`IMP-…` key; the customer pastes it under **Sharing settings → Plan**. Keys are
verified offline; revocation is by expiry only.

## Checklist before a public release

- `cargo test --workspace` green; one two-PC run.
- Version bumped; notes written for humans.
- Authenticode certificate when available: sign `Imperio.exe` before
  `release.ps1` uploads it, so SmartScreen warnings fade.
- Private seed backed up.
