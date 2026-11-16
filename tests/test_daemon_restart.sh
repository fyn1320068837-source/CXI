#!/usr/bin/env bash
# Validate the launchd restart contract. Set CIM_DAEMON_RESTART_INSTANCE to
# exercise the real instance-stop path during a daemon kickstart.
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
restart_instance="${CIM_DAEMON_RESTART_INSTANCE:-}"
log_file="${CIM_DAEMON_LOG:-$HOME/.cim/logs/daemon.log}"
log_start_line=0
if [[ -n "$restart_instance" && -r "$log_file" ]]; then
  log_start_line="$(wc -l < "$log_file" | tr -d ' ')"
fi
started_instance=0
cleanup() {
  if [[ "$started_instance" == "1" ]]; then
    curl -fsS --max-time 20 -X POST "$BASE_URL/api/instances/$restart_instance/stop" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
if [[ -n "$restart_instance" ]]; then
  if [[ ! "$restart_instance" =~ ^(code|desktop|vscode|web)$ ]]; then
    echo "FAIL: unsupported CIM_DAEMON_RESTART_INSTANCE=$restart_instance" >&2
    exit 1
  fi
  preflight="$(curl -fsS --max-time 20 "$BASE_URL/api/status")"
  python3 - "$preflight" <<'PY'
import json, sys
data = json.loads(sys.argv[1])
proxy = data.get("proxy", {})
firewall = data.get("firewall", {})
if not proxy.get("healthy") or firewall.get("mode") != "allowed":
    raise SystemExit(
        "FAIL: daemon restart instance mode requires healthy proxy and allowed PF "
        f"(healthy={proxy.get('healthy')}, mode={firewall.get('mode')})"
    )
PY
  launch_payload="$(curl -fsS --max-time 20 -X POST "$BASE_URL/api/instances/$restart_instance/launch")"
  python3 - "$launch_payload" "$restart_instance" <<'PY'
import json, sys
payload, kind = json.loads(sys.argv[1]), sys.argv[2]
assert payload.get("success"), (kind, payload)
PY
  started_instance=1
fi
launchctl kickstart -k "$SERVICE"
wait_for_health
new_pid="$(read_pid || true)"

python3 - "$BASE_URL/api/status" "$old_pid" "$new_pid" "$restart_instance" <<'PY'
import json
import subprocess
import sys
import urllib.request

url, old_pid, new_pid, restart_instance = sys.argv[1:]
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

if restart_instance:
    instance = next(item for item in instances if item.get("kind") == restart_instance)
    assert instance.get("status") == "stopped", (restart_instance, data)

# launchd must own one daemon after kickstart; avoid counting the shell script.
processes = subprocess.run(
    ["pgrep", "-x", "cim"], capture_output=True, text=True, check=False
).stdout.splitlines()
assert len(processes) <= 1, processes
suffix = f", {restart_instance} stopped" if restart_instance else ""
print(f"PASS: daemon restarted ({old_pid or 'unknown'} -> {new_pid or 'unknown'}), status schema valid{suffix}")
PY

if [[ -n "$restart_instance" && -r "$log_file" ]]; then
  python3 - "$log_file" "$log_start_line" <<'PY'
from pathlib import Path
import sys

path, start_line = sys.argv[1], int(sys.argv[2])
lines = Path(path).read_text(errors="replace").splitlines()[start_line:]
try:
    shutdown = next(i for i, line in enumerate(lines) if "Shutdown signal received" in line)
    blocked = next(
        i for i, line in enumerate(lines[shutdown:], start=shutdown)
        if "PF fail-closed rules enabled" in line
    )
    stopped = next(
        i for i, line in enumerate(lines[shutdown:], start=shutdown)
        if "Stopping sing-box" in line
    )
except StopIteration as error:
    raise SystemExit(f"FAIL: missing daemon shutdown ordering evidence in {path}: {error}")
assert blocked < stopped, (blocked, stopped)
print("PASS: shutdown log proves PF blocked before sing-box stop")
PY
fi
