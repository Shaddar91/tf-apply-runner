# tf-apply-runner

A long-lived Rust service that runs Terraform on behalf of pipeline agents and hands
them back **only a run id** — never the plan/apply output, never credentials.

## Contract

```
POST /apply  {"dir": "<absolute path>", "apply": <bool>}  ->  {"run_id": "<uuid>"}
GET  /health                                              ->  200 OK
```

- `dir` must resolve under one of the roots in `TF_ALLOWED_ROOTS` (colon-separated,
  fail-closed: empty/unset rejects every request). Out-of-allowlist paths are rejected
  **without executing** Terraform.
- `apply=false` runs `terraform plan`; `apply=true` runs `terraform apply`. The full
  plan/apply log is streamed to disk under `RUNS_DIR/<run_id>/`, and is **never**
  returned in the response body.
- On failure the service writes a `terraform-expert` task into the pipeline's
  `_unrouted/` directory so a fresh agent fixes the HCL with a clean context.

## Architecture — two binaries, one workspace

| Crate          | Binary         | Role |
|----------------|----------------|------|
| `tf-apply`     | `tf-apply`     | Long-lived HTTP executor. Binds `127.0.0.1:TF_RUNNER_PORT` (default 1937), spawns `terraform` against the requested directory, streams the log to `RUNS_DIR`, returns `{run_id}`. Holds the AWS profile and the `X-Runner-Token`. |
| `tf-apply-mcp` | `tf-apply-mcp` | Per-agent MCP **stdio door**. Forwards the typed call over loopback HTTP to `tf-apply` and hands the agent back nothing but the run id. Executes **no** Terraform in-process and holds **no** credentials. |

The door is bound to `terraform-expert` and `devops-specialist` agents only; no other
agent can see the tool. Terraform output stays on disk — an agent gets the run id and
must read the log file out-of-band if it is authorized to.

## Configuration

No secret or host path is tracked. The container and the host-native unit read their
runtime config from files the operator creates **outside the repo tree**, so the repository
is safe to publish as-is. Every value has a placeholder template in `.env.example`,
`deploy/paths.conf.example`, and the `deploy/tf-apply-runner.service` unit.

## Local setup

Site config lives OUTSIDE the repo tree. Create two files under `~/.config/tf-apply/`
(both `chmod 600`), plus one systemd drop-in for the host-native unit. No tracked file
carries a host path or secret.

| Out-of-tree file | Purpose | Template |
|---|---|---|
| `~/.config/tf-apply/runner.env` | runtime config + secrets — `X-Runner-Token`, AWS profile, `TF_ALLOWED_ROOTS`, `RUNS_DIR`, `UNROUTED_DIR` | `.env.example` |
| `~/.config/tf-apply/compose.env` | `docker compose` `${...}` interpolation — `TF_RUNNER_UID`/`GID`, the three volume host paths, and `TF_RUNNER_ENV_FILE` (absolute path to `runner.env`) | `.env.example` |
| `~/.config/systemd/user/tf-apply-runner.service.d/paths.conf` | systemd `ReadWritePaths=` for the host-native unit (the trees `ProtectSystem=strict` must leave writable) | `deploy/paths.conf.example` |

Run state (per-run logs) is written out of tree at `~/.local/state/tf-apply/runs/` (`RUNS_DIR`).

Launch the container through the wrapper, which points `docker compose` at the out-of-tree env-file:

```bash
scripts/compose.sh up -d      # docker compose --env-file ~/.config/tf-apply/compose.env up -d
scripts/compose.sh config     # render the effective config
scripts/compose.sh logs -f
```

## Leak guard

`scripts/check-no-leaks.sh` scans the whole folder — git-ignored files included, skipping only
`.git/`, `target/`, `node_modules/`, and binaries — and exits non-zero on any absolute host path
(home, Users, root, mnt/c/Users trees) or any regex from the optional out-of-tree file
`~/.config/tf-apply/leakpatterns` (keep site words such as a client name there, never in the repo).
It also flags any source/config file whose comment lines exceed 10% of its lines.
Run `scripts/check-no-leaks.sh --install-hook` to wire it as the git pre-commit hook.

## Build

```bash
cargo build --release
# -> target/release/tf-apply      (the HTTP executor)
# -> target/release/tf-apply-mcp  (the MCP stdio door)
```

## Status

Complete and running. The `tf-apply` executor and the `tf-apply-mcp` door are both
implemented — routes, path allowlist, per-directory locks, token auth, and the scrubbed
failure emitter — and the service runs as the containerized `tf-apply-runner` on loopback
`127.0.0.1:1937`. Every tracked file is placeholder-only; real site paths and secrets live
in the out-of-tree files under `~/.config/tf-apply/` listed in [Local setup](#local-setup).
