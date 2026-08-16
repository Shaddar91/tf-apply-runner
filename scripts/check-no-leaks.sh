#!/usr/bin/env bash
#check-no-leaks.sh — refuse to ship this folder if it holds a host path or an over-commented file.
#Scans the WHOLE dir (git-ignored files too), skipping only .git/ target/ node_modules/ and binaries.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
CONF="${TF_APPLY_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/tf-apply}"
SL=/  #assemble the host-path prefixes at runtime so this script never contains the literals it hunts

if [ "${1:-}" = "--install-hook" ]; then
  hook="$REPO/.git/hooks/pre-commit"
  printf '%s\n' '#!/usr/bin/env bash' 'exec "$(git rev-parse --show-toplevel)/scripts/check-no-leaks.sh"' >"$hook"
  chmod +x "$hook"
  echo "installed $hook"
  exit 0
fi

fail=0
prune=(--exclude-dir=.git --exclude-dir=target --exclude-dir=node_modules)

leaks="$(grep -rHnIE "${prune[@]}" -f <(
  printf '%s\n' "${SL}home${SL}" "${SL}Users${SL}" "${SL}root${SL}" "${SL}mnt${SL}c${SL}Users${SL}"
  [ -f "$CONF/leakpatterns" ] && grep -vE '^[[:space:]]*(#|$)' "$CONF/leakpatterns"
) . 2>/dev/null || true)"
if [ -n "$leaks" ]; then
  echo "LEAK — host path or deny-listed pattern:"
  echo "$leaks"
  fail=1
fi

while IFS= read -r f; do
  case "$f" in
    *.rs) c="$(grep -cE '^[[:space:]]*//' "$f")" ;;
    *.sh|*.bash|*.yml|*.yaml|*.toml|*.service|*.conf|*.example|*Dockerfile|*.dockerignore|*.gitignore)
      c=$(( $(grep -cE '^[[:space:]]*#' "$f") - $(grep -cE '^#!' "$f") )) ;;
    *) continue ;;
  esac
  t="$(wc -l <"$f")"
  [ "$t" -gt 0 ] || continue
  if [ $((10 * c)) -gt "$t" ]; then
    printf 'COMMENT-BUDGET — %s at %d%% (%d/%d)\n' "${f#./}" $((100 * c / t)) "$c" "$t"
    fail=1
  fi
done < <(find . -type f -not -path './.git/*' -not -path './target/*' -not -path './node_modules/*')

[ "$fail" -eq 0 ] && echo "OK — no host paths, no over-budget files."
exit "$fail"
