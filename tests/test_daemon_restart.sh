#!/usr/bin/env bash
# Validate the launchd restart contract without touching proxy or instances.
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  echo "SKIP: daemon restart smoke test requires macOS launchd"
  exit 0
fi

SERVICE="gui/$(id -u)/com.cim.daemon"
BASE_URL="${CIM_API_BASE:-http://127.0.0.1:8080}"

if ! launchctl print "$SERVICE" >/dev/null 2>&1; then
  echo "SKIP: $SERVICE is not loaded"
  exit 0
fi

read_pid() {
  launchctl print "$SERVICE" 2>/dev/null | awk '/^[[:space:]]*pid = / {print $3; exit}'
}

wait_for_health() {
  for _ in $(seq 1 60); do
    if curl -fsS --max-time 2 "$BASE_URL/health" 2>/dev/null | grep -q '^OK$'; then
      return 0
    fi
    sleep 1
  done
  return 1
}

old_pid="$(read_pid || true)"
launchctl kickstart -k "$SERVICE"
wait_for_health
new_pid="$(read_pid || true)"

python3 - "$BASE_URL/api/status" "$old_pid" "$new_pid" <<'PY'
import json
import subprocess
import sys
import urllib.request

url, old_pid, new_pid = sys.argv[1:]
with urllib.request.urlopen(url, timeout=3) as response:
    data = json.load(response)

assert isinstance(data.get("proxy"), dict), data
assert isinstance(data.get("firewall"), dict), data
instances = data.get("instances")
assert isinstance(instances, list) and {item.get("kind") for item in instances} == {
    "code", "desktop", "vscode", "web"
}, data

if old_pid and new_pid:
    assert old_pid != new_pid, (old_pid, new_pid)

# launchd must own one daemon after kickstart; avoid counting the shell script.
processes = subprocess.run(
    ["pgrep", "-x", "cim"], capture_output=True, text=True, check=False
).stdout.splitlines()
assert len(processes) <= 1, processes
print(f"PASS: daemon restarted ({old_pid or 'unknown'} -> {new_pid or 'unknown'}), status schema valid")
PY
