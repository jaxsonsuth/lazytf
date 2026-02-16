# Homebrew Release Notes

This project ships a manual release workflow for macOS arm64 and can auto-update the Homebrew tap formula.

## 1) Create a release artifact

1. Open `Actions` in `https://github.com/jaxsonsuth/lazytf`.
2. Run `Release (macOS arm64)` workflow.
3. Set tag input to something like `v0.1.0`.
4. Wait for the workflow to finish.

The release uploads:

- `lazytf-aarch64-apple-darwin.tar.gz`
- `checksums.txt`

## 2) Optional one-time setup for automatic tap updates

If you want releases to update Homebrew automatically:

1. In `jaxsonsuth/lazytf`, open `Settings` -> `Secrets and variables` -> `Actions`.
2. Add a repository secret named `HOMEBREW_TAP_TOKEN`.
3. Use a GitHub token that has write access to `jaxsonsuth/homebrew-lazytf`.

With that secret configured, every release run updates and pushes:

- `Formula/lazytf.rb` in `jaxsonsuth/homebrew-lazytf`

## 3) Manual tap update (fallback)

Use a separate tap repository, for example:

- `https://github.com/jaxsonsuth/homebrew-lazytf`

Create/update this file in the tap repo:

- `Formula/lazytf.rb`

Start from template:

- `packaging/homebrew/lazytf.rb.template`

Set values:

- `url` to match the release tag.
- `sha256` from `checksums.txt`.
- `version` to match the tag version (without `v`).

## 4) Install

On any macOS arm64 machine:

```bash
brew tap jaxsonsuth/lazytf
brew install lazytf
```
