#!/bin/sh
# Static checks of deploy/: `systemd-analyze verify` on the units (with the
# binaries they start), the security score, and Quadlet's generator.
#   tests/deploy-check.sh BIN_DIR [SYSTEMD_ROOT] [QUADLET]
# SYSTEMD_ROOT: a directory where systemd 252+ is unpacked (usr/bin/systemd-analyze,
# lib/systemd/system, usr/lib/systemd/user); default: the host's own systemd.
# Runs inside `unshare -rm` so it needs no root.
set -eu
BIN=$1; SD=${2:-}; Q=${3:-/usr/libexec/podman/quadlet}
REPO=$(cd "$(dirname "$0")/.." && pwd)
SA=${SD:+$SD/usr/bin/}systemd-analyze
[ -n "$SD" ] && export LD_LIBRARY_PATH="$SD/usr/lib/x86_64-linux-gnu:$SD/lib/x86_64-linux-gnu:$SD/usr/lib/x86_64-linux-gnu/systemd"
R=$(mktemp -d); trap 'rm -rf "$R"' EXIT
mkdir -p "$R/usr/local/bin" "$R/etc/systemd/system" "$R/home/u/.local/bin" "$R/home/u/.config/systemd/user"
cp "$BIN/ocl-supervisor" "$BIN/opencode-node" "$R/usr/local/bin/"; cp "$BIN/opencode-node" "$R/home/u/.local/bin/"
cp "$REPO/deploy/systemd/ocl-supervisor.service" "$REPO/deploy/systemd/opencode-node.service" "$R/etc/systemd/system/"
cp "$REPO/deploy/systemd/opencode-node.user.service" "$R/home/u/.config/systemd/user/opencode-node.service"
[ -n "$SD" ] && { mkdir -p "$R/lib/systemd" "$R/usr/lib/systemd"; cp -r "$SD/lib/systemd/system" "$R/lib/systemd/"; cp -r "$SD/usr/lib/systemd/system" "$R/usr/lib/systemd/" 2>/dev/null || true; }
cat > "$R/run.sh" <<SH
set -e
mount -t tmpfs none /run; mkdir -p /run/user/0
cd "$R/etc/systemd/system"
"$SA" verify --root="$R" ocl-supervisor.service opencode-node.service && echo "PASS system units verify"
"$SA" security --offline=true --root="$R" ocl-supervisor.service opencode-node.service | grep Overall
cd "$R/home/u/.config/systemd/user"
HOME="$R/home/u" XDG_RUNTIME_DIR=/run/user/0 SYSTEMD_UNIT_PATH="$R/home/u/.config/systemd/user:${SD:-}/usr/lib/systemd/user" "$SA" --user verify ./opencode-node.service && echo "PASS user unit verifies"
SH
unshare -rm sh "$R/run.sh"
if [ -x "$Q" ]; then
  mkdir -p "$R/q"; cp "$REPO/deploy/quadlet/ocl-supervisor.container" "$R/q/"
  U=$(QUADLET_UNIT_DIRS="$R/q" "$Q" -dryrun 2>/dev/null | grep '^ExecStart=.*podman run')
  echo "$U" | grep -q -- '--read-only' && echo "$U" | grep -q -- '--cap-drop all' && echo "$U" | grep -q -- 'no-new-privileges' \
    && echo "PASS quadlet generates a read-only, no-capability container unit"
fi
