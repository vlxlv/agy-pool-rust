---
name: agy-pool-release
description: Release and version-bump procedure for vlxlv/agy-pool-rust. Use whenever bumping the package version, tagging v*, or publishing release assets.
---

# Release & Version Bump Skill (`vlxlv/agy-pool-rust`)

## Mandatory Invariants

1. **Keep ONLY ONE version-bump commit on `main`**:
   - `git log --grep="^chore: bump version" --oneline main` MUST list at most **1** commit.
   - **Never** stack bump commits across releases, and never fold a version bump into a feature/fix commit.
2. **The bump commit touches ONLY `Cargo.toml` and `Cargo.lock`** and has the exact subject `chore: bump version to <X.Y.Z>`.
3. **Published tags are immutable**: never move or delete an existing `v*` tag unless explicitly told to re-tag that version. After a rewrite, older tags keep pointing at their original (now off-`main`) bump commits; that is expected.
4. **Builds run in GitHub Actions only**: `release.yml` builds and publishes on a `v*` tag. Never run release/LTO builds on the live gateway host.

## Procedure

1. Start from an up-to-date, clean `main` whose CI is green:
   ```bash
   git switch main && git pull --ff-only && git status --short
   gh run list --branch main --limit 3
   ```
2. Find the existing bump commit:
   ```bash
   git log --grep="^chore: bump version" --oneline
   ```
3. If one exists (`OLD`) and it is not `HEAD`, drop it from `main` by replaying the later commits onto its parent:
   ```bash
   git rebase --rebase-merges --onto OLD^ OLD main
   ```
   If `OLD` is already `HEAD`, there is nothing new to release; stop.
4. Create the single bump commit at the tip:
   ```bash
   sed -i 's/^version = ".*"/version = "<X.Y.Z>"/' Cargo.toml   # [package].version is the only line starting with `version =`
   cargo update --workspace                                    # refreshes the agy-pool entry in Cargo.lock
   git add Cargo.toml Cargo.lock
   git commit -m "chore: bump version to <X.Y.Z>"
   ```
5. Verify before pushing:
   - `git log --grep="^chore: bump version" --oneline | wc -l` equals `1`, and it is `HEAD`.
   - `git diff --name-only HEAD^ HEAD` lists exactly `Cargo.lock` and `Cargo.toml`.
   - `Cargo.toml` `[package].version` and the `name = "agy-pool"` entry in `Cargo.lock` both read `<X.Y.Z>` (`release.yml` rejects a tag that does not match `Cargo.toml`).
6. Push `main`. If step 3 rewrote history, first make sure no open PR is based on the old commits (`gh pr list`), then:
   ```bash
   git push --force-with-lease origin main
   ```
   Wait for CI on the new `HEAD` to pass.
7. Tag (annotated, message = bare version, matching earlier tags) and push only the tag:
   ```bash
   git tag -a "v<X.Y.Z>" -m "<X.Y.Z>"
   git push origin "refs/tags/v<X.Y.Z>"
   ```
8. Watch `.github/workflows/release.yml` to completion and confirm the GitHub Release has exactly these 5 assets: `agy-pool-linux-amd64`, `agy-pool-linux-arm64`, `agy-pool_<X.Y.Z>_amd64.deb`, `agy-pool_<X.Y.Z>_arm64.deb`, `SHA256SUMS`.
