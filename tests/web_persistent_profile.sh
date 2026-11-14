#!/usr/bin/env bash
set -euo pipefail

# This test is intentionally opt-in: it launches a real Chromium instance and
# verifies that the isolated Web profile survives a stop/start cycle.
if [[ "${CIM_RUN_REAL_PROXY:-0}" != "1" ]]; then
  echo "SKIP: set CIM_RUN_REAL_PROXY=1 to run the persistent Web profile test"
  exit 0
fi

BASE_URL="${CIM_BASE_URL:-http://127.0.0.1:8080}"
PROFILE_DIR="${CIM_WEB_PROFILE_DIR:-$HOME/.cim/instances/web/profile}"
SESSION_ROOT="${CIM_WEB_SESSION_ROOT:-$HOME/.cim/instances/web/sessions}"

curl_json() {
  curl -fsS --max-time 20 "$@"
}

wait_for_web_pid() {
  local status pid
  for _ in {1..20}; do
    status="$(curl_json "$BASE_URL/api/status")"
    pid="$(python3 - "$status" <<'PY'
import json, sys
data = json.loads(sys.argv[1])
instance = next(item for item in data["instances"] if item["kind"] == "web")
if instance["status"] == "running" and instance.get("pid"):
    print(instance["pid"])
PY
)"
    if [[ "$pid" =~ ^[0-9]+$ ]]; then
      echo "$pid"
      return 0
    fi
    sleep 1
  done
  echo "timed out waiting for Web instance" >&2
  return 1
}

process_tree_command() {
  # Chrome may reparent helper processes during startup. The profile path is
  # unique to this test, so inspect the complete process list rather than
  # relying only on the API root PID's descendant tree.
  ps -axo command= 2>/dev/null || true
}

started=0
cleanup() {
  if [[ "$started" == "1" ]]; then
    curl_json -X POST "$BASE_URL/api/instances/web/stop" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

first_launch="$(curl_json -X POST "$BASE_URL/api/instances/web/launch")"
python3 - "$first_launch" <<'PY'
import json, sys
payload = json.loads(sys.argv[1])
assert payload.get("success"), payload
PY
started=1
first_pid="$(wait_for_web_pid)"
first_cmd="$(process_tree_command "$first_pid")"
grep -Fq -- "--user-data-dir=$PROFILE_DIR" <<< "$first_cmd" || {
  echo "FAIL: first Web process did not use persistent profile $PROFILE_DIR" >&2
  exit 1
}

curl_json -X POST "$BASE_URL/api/instances/web/stop" >/dev/null
started=0
[[ -d "$PROFILE_DIR" ]] || {
  echo "FAIL: Web profile was removed after stop: $PROFILE_DIR" >&2
  exit 1
}
if find "$SESSION_ROOT" -mindepth 1 -maxdepth 1 -type d -print -quit 2>/dev/null | grep -q .; then
  echo "FAIL: transient Web session marker survived stop: $SESSION_ROOT" >&2
  exit 1
fi

second_launch="$(curl_json -X POST "$BASE_URL/api/instances/web/launch")"
python3 - "$second_launch" <<'PY'
import json, sys
payload = json.loads(sys.argv[1])
assert payload.get("success"), payload
PY
started=1
second_pid="$(wait_for_web_pid)"
second_cmd="$(process_tree_command "$second_pid")"
grep -Fq -- "--user-data-dir=$PROFILE_DIR" <<< "$second_cmd" || {
  echo "FAIL: second Web process did not reuse persistent profile $PROFILE_DIR" >&2
  exit 1
}

echo "PASS: Web profile persisted across stop/start: $PROFILE_DIR"
