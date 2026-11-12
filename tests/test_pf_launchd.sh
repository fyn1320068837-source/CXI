#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  echo "SKIP: PF/launchd test requires macOS"
  exit 0
fi

service="gui/$(id -u)/com.cim.daemon"
if ! launchctl print "$service" >/dev/null 2>&1; then
  echo "SKIP: $service is not loaded"
  exit 0
fi

launchctl print "$service" | grep -Eq 'state = (running|xpcproxy)'
sudo -n /sbin/pfctl -a com.cim -s rules >/dev/null

status="$(curl -fsS http://127.0.0.1:8080/api/status)"
python3 - "$status" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
assert data["firewall"]["installed"] is True, data
assert data["proxy"]["healthy"] is True, data
assert data["firewall"]["mode"] == "allowed", data
print("PASS: launchd daemon, sudoers pfctl, proxy health and PF allowed state")
PY
