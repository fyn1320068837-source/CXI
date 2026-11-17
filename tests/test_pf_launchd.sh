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
sudo -n /sbin/pfctl -s info | grep -Eq '^Status: Enabled([[:space:]]|$)'
sudo -n /sbin/pfctl -s rules | grep -Eq '^anchor "com\.cim" (quick )?all$'
rules="$(sudo -n /sbin/pfctl -a com.cim -s rules)"
printf '%s\n' "$rules" | grep -Eq '^pass in quick on lo0 inet proto tcp from 127\.0\.0\.1 to 127\.0\.0\.1 group = [0-9]+ flags S/SA'
printf '%s\n' "$rules" | grep -Eq 'pass out quick inet proto tcp .*127\.0\.0\.1.*port = 7890.*group = '
printf '%s\n' "$rules" | grep -Eq 'pass out quick inet proto tcp .*127\.0\.0\.1.*port = 7891.*group = '
printf '%s\n' "$rules" | grep -Eq 'pass out quick inet6 proto tcp .*::1.*port = 7890.*group = '
printf '%s\n' "$rules" | grep -Eq 'pass out quick inet6 proto tcp .*::1.*port = 7891.*group = '
printf '%s\n' "$rules" | grep -Eq 'block return out quick all group = '
[[ "$(printf '%s\n' "$rules" | grep -Ec '^pass out quick .* no state$')" == 4 ]]

status="$(curl -fsS http://127.0.0.1:8080/api/status)"
python3 - "$status" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
assert data["firewall"]["installed"] is True, data
assert data["firewall"]["global_enabled"] is True, data
assert data["firewall"]["root_anchor_loaded"] is True, data
assert data["firewall"]["global_verified"] is True, data
assert data["proxy"]["healthy"] is True, data
assert data["firewall"]["mode"] == "allowed", data
print("PASS: launchd daemon, PF global engine/root anchor/control channel, proxy health and PF allowed state")
PY
