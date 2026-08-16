#!/usr/bin/env bash
set -euo pipefail
CONF="${TF_APPLY_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/tf-apply}"
cd "$(dirname "$0")/.."
exec docker compose --env-file "$CONF/compose.env" "$@"
