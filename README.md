# lazytf

Terminal UI for Terraform workflows.

## Install

```bash
brew tap jaxsonsuth/lazytf
brew install lazytf
```

## Notes

- This formula currently builds `lazytf` from source.
- The `lazytf` release workflow can update the Homebrew tap formula automatically when `HOMEBREW_TAP_TOKEN` is configured in this repository.

## Config

Create a config file in your Terraform repo root (`lazyterraform.yaml`, `Config.yaml`, or `config.yaml`) and run `lazytf` from that repo, or pass an explicit path:

```bash
lazytf --config /path/to/config.yaml
```

See `docs/HOMEBREW_RELEASE.md` for release and tap maintenance details.
