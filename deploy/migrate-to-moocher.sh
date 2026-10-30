#!/usr/bin/env bash
# One-time upgrade of a rootless install from the opencode-cluster names to moocher.
#
#   bash deploy/migrate-to-moocher.sh <dir with moo, moocher-hub, moocher-node, moo-bridge>
#
# What it does:
#   1. stops live sessions with the old `ocl` CLI (their containers carry the old
#      labels and names, so the new node would not find them), then the old services;
#   2. installs the new binaries into /usr/local/bin (sudo) and removes the old ones;
#   3. writes moocher-hub.service / moocher-node.service next to your existing user
#      units, keeping everything you customised (EnvironmentFile, flags, paths);
#   4. tags localhost/ocl-agent:<v> as localhost/moocher-agent:<v> (no rebuild needed);
#   5. starts the new services.
#
# Config and data stay where they are: with no ~/.moocher, the binaries use
# ~/.opencode-cluster. Dashboard users log in again (the cookie was renamed).
set -euo pipefail
BIN=${1:?usage: migrate-to-moocher.sh <dir with the new binaries>}
for b in moo moocher-hub moocher-node; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b" >&2; exit 1; }
done
U=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
OLD=$HOME/.opencode-cluster

if command -v ocl >/dev/null && [ -r "$OLD/cli-token" ]; then
  echo "stopping live sessions..."
  export OCL_TOKEN=${OCL_TOKEN:-$(cat "$OLD/cli-token")}
  for s in $(ocl sessions --json 2>/dev/null | python3 -c 'import sys,json;[print(s["id"]) for s in json.load(sys.stdin) if s["state"] not in ("stopped","lost","dead")]' 2>/dev/null || true); do
    ocl stop "$s" >/dev/null 2>&1 && echo "  stopped $s" || true
  done
  sleep 3
fi
systemctl --user disable --now opencode-node ocl-supervisor 2>/dev/null || true
if command -v podman >/dev/null; then
  ids=$(podman ps -aq --filter label=ocl.node 2>/dev/null || true)
  [ -n "$ids" ] && podman rm -f $ids >/dev/null && echo "removed old containers"
  for img in $(podman images --format '{{.Repository}}:{{.Tag}}' | grep '^localhost/ocl-agent:' || true); do
    new=${img/ocl-agent/moocher-agent}
    podman image exists "$new" || { podman tag "$img" "$new" && echo "tagged $new"; }
  done
fi

echo "installing binaries (sudo)..."
sudo install -m 755 "$BIN"/moo "$BIN"/moocher-hub "$BIN"/moocher-node /usr/local/bin/
[ -x "$BIN/moo-bridge" ] && sudo install -m 755 "$BIN/moo-bridge" /usr/local/bin/
sudo rm -f /usr/local/bin/ocl /usr/local/bin/ocl-supervisor /usr/local/bin/opencode-node /usr/local/bin/ocl-bridge

rename_unit() { # old new
  [ -f "$U/$1.service" ] || return 0
  sed -e 's/ocl-supervisor/moocher-hub/g; s/opencode-node/moocher-node/g; s/ocl-bridge/moo-bridge/g; s/OCL_/MOO_/g' \
      -e 's/opencode-cluster supervisor/moocher hub/; s/opencode-cluster node/moocher node/' \
      "$U/$1.service" > "$U/$2.service"
  mv "$U/$1.service" "$U/$1.service.old"
  echo "wrote $U/$2.service (old one kept as $1.service.old)"
}
rename_unit ocl-supervisor moocher-hub
rename_unit opencode-node moocher-node
systemctl --user daemon-reload
[ -f "$U/moocher-hub.service" ] && systemctl --user enable --now moocher-hub && sleep 2
[ -f "$U/moocher-node.service" ] && systemctl --user enable --now moocher-node

if grep -rqs 'ocl-agent\|OCL_' "$OLD"/*.yaml "$OLD"/agents "$OLD"/templates 2>/dev/null; then
  echo "note: these config files still name ocl-agent or OCL_ variables:"
  grep -rls 'ocl-agent\|OCL_' "$OLD"/*.yaml "$OLD"/agents "$OLD"/templates 2>/dev/null | sed 's/^/  /'
fi
echo "done. The CLI is now \`moo\` (MOO_TOKEN / MOO_URL replace OCL_TOKEN / OCL_URL)."
