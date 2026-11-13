#!/usr/bin/env bash
set -euo pipefail

# This test is intentionally opt-in: it talks to the configured upstream
# proxy, launches real desktop processes, and can open a browser window.
if [[ "${CIM_RUN_REAL_PROXY:-0}" != "1" ]]; then
  echo "SKIP: set CIM_RUN_REAL_PROXY=1 to run the real proxy integration test"
  exit 0
fi

BASE_URL="${CIM_BASE_URL:-http://127.0.0.1:8080}"
curl_json() {
  curl -fsS --max-time 20 "$@"
}

status="$(curl_json "$BASE_URL/api/status")"
proxy_test="$(curl_json -X POST "$BASE_URL/api/proxy/test")"

python3 - "$status" "$proxy_test" <<'PY'
import json, sys
status, proxy_test = map(json.loads, sys.argv[1:])
assert status["proxy"]["healthy"], status
assert proxy_test["healthy"], proxy_test
assert proxy_test.get("exit_ip"), proxy_test
assert proxy_test.get("geo", {}).get("timezone"), proxy_test
assert proxy_test.get("geo", {}).get("language"), proxy_test
print("proxy: healthy, exit_ip:", proxy_test["exit_ip"])
print("geo:", proxy_test["geo"])
PY
expected_tz="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["geo"]["timezone"])' "$proxy_test")"
expected_lang="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["geo"]["language"])' "$proxy_test")"
expected_locale="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["geo"]["locale"])' "$proxy_test")"

IFS=',' read -r -a instances <<< "${CIM_REAL_PROXY_INSTANCES:-web}"
for kind in "${instances[@]}"; do
  [[ -n "$kind" ]] || continue
  echo "launching $kind"
  launch="$(curl_json -X POST "$BASE_URL/api/instances/$kind/launch")"
  python3 - "$launch" "$kind" <<'PY'
import json, sys
payload, kind = json.loads(sys.argv[1]), sys.argv[2]
assert payload.get("success"), (kind, payload)
PY

  for _ in {1..20}; do
    status="$(curl_json "$BASE_URL/api/status")"
    if python3 - "$status" "$kind" <<'PY'
import json, sys
data, kind = json.loads(sys.argv[1]), sys.argv[2]
instance = next(item for item in data["instances"] if item["kind"] == kind)
if instance["status"] == "running" and instance["validation"] == "pass":
    print(instance.get("pid") or "")
    raise SystemExit(0)
raise SystemExit(1)
PY
    then
      break
    fi
    sleep 1
  done

  status="$(curl_json "$BASE_URL/api/status")"
  pid="$(python3 - "$status" "$kind" <<'PY'
import json, sys
data, kind = json.loads(sys.argv[1]), sys.argv[2]
instance = next(item for item in data["instances"] if item["kind"] == kind)
assert instance["validation"] == "pass", instance
assert instance.get("validation_checked_at"), instance
details = instance.get("validation_details", "")
assert "direct=0" in details and "proxied=" in details, instance
print(instance["pid"])
PY
)"
  [[ "$pid" =~ ^[0-9]+$ ]]
  if [[ "$kind" == "web" ]]; then
    command_line="$(ps -p "$pid" -o command=)"
    grep -F -- '--proxy-server=socks5://127.0.0.1:7891' <<< "$command_line"
    grep -F -- "--lang=$expected_lang" <<< "$command_line"
    grep -F -- '--force-webrtc-ip-handling-policy=disable_non_proxied_udp' <<< "$command_line"
    grep -F -- '--disable-quic' <<< "$command_line"
  elif [[ "$kind" == "code" ]]; then
    process_env="$(ps eww -p "$pid")"
    grep -F "TZ=$expected_tz" <<< "$process_env"
    grep -F "LANG=$expected_locale" <<< "$process_env"
    grep -F 'ALL_PROXY=socks5://127.0.0.1:7891' <<< "$process_env"
  fi
  echo "lsof for $kind pid $pid"
  pids=("$pid")
  for ((index = 0; index < ${#pids[@]}; index++)); do
    children="$(pgrep -P "${pids[$index]}" 2>/dev/null || true)"
    while read -r child; do
      [[ -n "$child" ]] || continue
      pids+=("$child")
    done <<< "$children"
  done
  for process_pid in "${pids[@]}"; do
    lsof -nP -a -i -p "$process_pid" 2>/dev/null || true
    if lsof -nP -a -iUDP:53 -p "$process_pid" 2>/dev/null | grep -q 'UDP'; then
      echo "FAIL: direct DNS UDP socket detected for $kind pid $process_pid" >&2
      exit 1
    fi
  done
  evidence="$(curl_json "$BASE_URL/api/validation/$kind")"
  python3 - "$evidence" <<'PY'
import json, sys
payload = json.loads(sys.argv[1])
assert payload.get("success"), payload
assert payload.get("evidence"), payload
latest = payload["evidence"][0]
assert latest["status"] == "pass", latest
assert latest["direct_count"] == 0, latest
assert latest["proxied_count"] > 0, latest
PY
  if [[ "$kind" == "web" ]]; then
    security="$(curl_json "$BASE_URL/api/security/web")"
    python3 - "$security" <<'PY'
import json, sys
payload = json.loads(sys.argv[1])
assert payload.get("success"), payload
assert payload.get("verdict") == "PASS", payload
assert payload.get("web", {}).get("direct") == 0, payload
assert payload.get("checks", {}).get("direct_zero"), payload
assert payload.get("checks", {}).get("validation_pass"), payload
PY
  fi
  curl_json -X POST "$BASE_URL/api/instances/$kind/stop" >/dev/null
done

echo "PASS: real proxy, GeoIP, locale and lsof validation"
