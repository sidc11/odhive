#!/usr/bin/env bash
# Smoke test for a release binary inside a Linux distro container (CI only).
# Usage: smoke-linux.sh /path/to/odhive
# Runs as a non-root user. Uses bash /dev/tcp so no extra packages are needed.
set -euo pipefail

bin="$1"
log="$(mktemp)"
# The container user has no passwd entry; give it a private home.
HOME="$(mktemp -d)"
export HOME
unset XDG_CONFIG_HOME XDG_DATA_HOME

"$bin" --version
# In the foreground: plain `odhive` would start it detached and exit.
"$bin" --port 7069 serve --foreground >"$log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
  grep -q 'Open OdHive' "$log" && break
  sleep 0.1
done
grep -q 'Open OdHive (link valid for 60 s, once): http://localhost:7069/login?t=' "$log" || { cat "$log"; exit 1; }

status_for_host() {
  exec 3<>/dev/tcp/127.0.0.1/7069
  printf 'GET / HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n\r\n' "$1" >&3
  head -n1 <&3 | tr -d '\r'
  exec 3>&-
}

ok="$(status_for_host localhost:7069)"
bad="$(status_for_host evil.com)"
echo "localhost -> $ok"
echo "evil.com  -> $bad"
[[ "$ok" == "HTTP/1.1 200 OK" ]]
[[ "$bad" == "HTTP/1.1 403 Forbidden" ]]

# First run creates the config (0600) and the workspace layout.
[[ "$(stat -c %a "$HOME/.config/odhive/config.toml")" == "600" ]]
[[ -d "$HOME/OdHive/sources" && -d "$HOME/OdHive/projects" ]]

echo "smoke test passed on $(. /etc/os-release && echo "$PRETTY_NAME")"
