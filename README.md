# tern

Team GitHub activity dashboard. Syncs an org's pull requests, reviews,
comments and commits into a local SQLite file every 15 minutes and serves
charts per member on `127.0.0.1:8088`. Design: `docs/Team GitHub Activity Dashboard.md`.

## Install

```sh
brew install oxhive/tap/tern     # macOS arm64, Linux x86_64/arm64
# or from source
cargo install --path .
```

## Run

```sh
tern init                                        # writes config.toml + members.toml
$EDITOR ~/.config/tern/config.toml ~/.config/tern/members.toml
chmod 600 ~/.config/tern/app.pem
export TERN_GITHUB_KEY_PATH=~/.config/tern/app.pem
tern validate
tern backfill --since 2026-01-01
tern                                             # sync ticker + dashboard
```

## Release

`just release-patch` (or `-minor`, `-major`) bumps the version, tags `v*`
and pushes; `.github/workflows/publish.yml` builds the binaries, creates the
GitHub Release and updates the `oxhive/tap` Homebrew formula.
