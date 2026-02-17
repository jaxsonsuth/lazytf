# lazytf

Terminal UI for Terraform workflows.

## Install

```bash
brew tap jaxsonsuth/lazytf
brew install lazytf
```

## Quick Start

1. Create a config file (`lazyterraform.yaml`, `Config.yaml`, or `config.yaml`).
2. Run `lazytf` from your Terraform repo root, or pass an explicit config path.
3. Authenticate with `a`, refresh workspaces with `r`, then run `p` for plan.

```bash
lazytf --config /path/to/config.yaml
```

## Config Reference

`accounts` is a map keyed by the name you want to see in the UI.

- `aws_profile` (required): AWS CLI profile name.
- `composition_path` (required): Terraform composition directory.
- `region` (optional): AWS region exported to `AWS_REGION` and `AWS_DEFAULT_REGION`.
- `var_files` (optional): list of tfvars files used for `plan`/`apply`.

Path behavior:

- Relative config paths are resolved from the config file directory.
- `var_files` paths are resolved from each account `composition_path`.
- `composition_path` supports glob patterns (`*`, `?`, `[]`) and uses the first directory match.

Example:

```yaml
accounts:
  non-prod:
    aws_profile: "non-prod-org-admin"
    composition_path: "compositions/non-prod-453612985887/us-west-2"
    region: "us-west-2"
    var_files:
      - "vars/non-prod.tfvars"

  prod:
    aws_profile: "prod-org-admin"
    composition_path: "compositions/prod-123456789012/us-west-2"
    region: "us-west-2"
    var_files:
      - "vars/prod.tfvars"
```

## Keybindings

Global:

- `q`: quit
- `Ctrl+C`: graceful quit
- `c`: cancel running command (press again to force kill)
- `?`: toggle help modal

Layout and focus:

- `z`: toggle output fullscreen
- `Esc`: exit fullscreen/help modal
- `Tab`/`Shift+Tab` or `h`/`l`: move focus between panels

Navigation:

- `j`/`k` or arrow keys: move selection
- `PgUp`/`PgDn` or mouse wheel: scroll output
- `g`/`G` or `Home`/`End`: output top/bottom

Actions:

- `a`: AWS SSO login
- `s`: auth check
- `r`: refresh workspaces
- `i`: terraform init
- `p`: terraform plan
- `A` then `y`: terraform apply

## Safety Model

- Startup is relaxed: the UI can open with invalid paths so you can inspect configuration.
- Execution is strict: plan/apply/workspace commands are blocked until path preflight checks pass.
- Cancel is two-stage: first `c` sends SIGINT and waits for Terraform cleanup, second `c` force-kills.
- Apply always requires explicit confirmation (`A` then `y`).

## Known Limitations

- Output is raw Terraform stream with semantic coloring; structured plan view is planned for `v0.2.0`.
- One operation runs at a time.

## Maintainers

See `docs/HOMEBREW_RELEASE.md` for release and tap maintenance details.
