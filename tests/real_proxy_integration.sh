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
assert_contains() {
  local label="$1"
  local pattern="$2"
  local haystack="$3"
  if ! grep -Fq -- "$pattern" <<< "$haystack"; then
    echo "FAIL: $label (missing $pattern)" >&2
    return 1
  fi
}

status="$(curl_json "$BASE_URL/api/status")"
proxy_test="$(curl_json -X POST "$BASE_URL/api/proxy/test")"

started_instances=()
cleanup() {
  for started_kind in "${started_instances[@]}"; do
    curl_json -X POST "$BASE_URL/api/instances/$started_kind/stop" >/dev/null 2>&1 || true
  done
}
remove_started() {
  local target="$1"
  local remaining=()
  for started_kind in "${started_instances[@]}"; do
    [[ "$started_kind" == "$target" ]] || remaining+=("$started_kind")
  done
  started_instances=("${remaining[@]}")
}
trap cleanup EXIT

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
  started_instances+=("$kind")

  for attempt in {1..20}; do
    status="$(curl_json "$BASE_URL/api/status")"
    if python3 - "$status" "$kind" "$attempt" <<'PY'
import json, sys
data, kind, attempt = json.loads(sys.argv[1]), sys.argv[2], int(sys.argv[3])
instance = next(item for item in data["instances"] if item["kind"] == kind)
idle_code = (
    kind == "code"
    and instance["status"] == "running"
    and instance["validation"] == "unknown"
    and instance.get("validation_reason") == "no_proxy_connection_observed"
)
if instance["status"] == "running" and (instance["validation"] == "pass" or idle_code):
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
idle_code = (
    kind == "code"
    and instance["validation"] == "unknown"
    and instance.get("validation_reason") == "no_proxy_connection_observed"
)
assert instance["validation"] == "pass" or idle_code, instance
assert instance.get("validation_checked_at"), instance
details = instance.get("validation_details", "")
assert "direct=0" in details and "proxied=" in details, instance
if not idle_code:
    assert instance.get("validation_reason") is None, instance
assert instance.get("validation_direct_endpoints") == [], instance
print(instance["pid"])
PY
)"
  [[ "$pid" =~ ^[0-9]+$ ]]

  # The API PID may be the short-lived sudo/helper process used for PF
  # isolation.  Inspect its complete descendant tree so the assertions cover
  # the actual Code/VS Code/Chrome process as well as the launcher.
  pids=("$pid")
  for ((index = 0; index < ${#pids[@]}; index++)); do
    children="$(pgrep -P "${pids[$index]}" 2>/dev/null || true)"
    while read -r child; do
      [[ -n "$child" ]] || continue
      pids+=("$child")
    done <<< "$children"
  done
  process_tree_cmd=""
  process_tree_env=""
  for process_pid in "${pids[@]}"; do
    process_tree_cmd+="$(ps -p "$process_pid" -o command= 2>/dev/null || true)\n"
    process_tree_env+="$(ps eww -p "$process_pid" -o command= 2>/dev/null || true)\n"
  done

  if [[ "$kind" == "web" ]]; then
    assert_contains "$kind proxy" '--proxy-server=socks5://127.0.0.1:7891' "$process_tree_cmd"
    assert_contains "$kind session dir" "--user-data-dir=$HOME/.cim/instances/web/sessions/" "$process_tree_cmd"
    assert_contains "$kind language" "--lang=$expected_lang" "$process_tree_cmd"
    assert_contains "$kind WebRTC guard" '--force-webrtc-ip-handling-policy=disable_non_proxied_udp' "$process_tree_cmd"
    assert_contains "$kind QUIC guard" '--disable-quic' "$process_tree_cmd"
  elif [[ "$kind" == "code" ]]; then
    assert_contains "$kind HOME" "HOME=$HOME/.cim/instances/code" "$process_tree_env"
    assert_contains "$kind timezone" "TZ=$expected_tz" "$process_tree_env"
    assert_contains "$kind locale" "LANG=$expected_locale" "$process_tree_env"
    assert_contains "$kind HTTP proxy" 'HTTPS_PROXY=http://127.0.0.1:7890' "$process_tree_env"
    assert_contains "$kind SOCKS proxy" 'ALL_PROXY=socks5://127.0.0.1:7891' "$process_tree_env"
  elif [[ "$kind" == "vscode" ]]; then
    assert_contains "$kind data dir flag" '--user-data-dir' "$process_tree_cmd"
    assert_contains "$kind data dir" "$HOME/.cim/instances/vscode/data" "$process_tree_cmd"
    assert_contains "$kind extensions flag" '--extensions-dir' "$process_tree_cmd"
    assert_contains "$kind extensions dir" "$HOME/.cim/instances/vscode/extensions" "$process_tree_cmd"
    assert_contains "$kind proxy" '--proxy-server=http://127.0.0.1:7890' "$process_tree_cmd"
    assert_contains "$kind locale flag" '--locale' "$process_tree_cmd"
    assert_contains "$kind language" "$expected_lang" "$process_tree_cmd"
  fi
  echo "lsof for $kind pid $pid"
  for process_pid in "${pids[@]}"; do
    lsof -nP -a -i -p "$process_pid" 2>/dev/null || true
    if lsof -nP -a -iUDP:53 -p "$process_pid" 2>/dev/null | grep -q 'UDP'; then
      echo "FAIL: direct DNS UDP socket detected for $kind pid $process_pid" >&2
      exit 1
    fi
  done
  evidence="$(curl_json "$BASE_URL/api/validation/$kind")"
  python3 - "$evidence" "$kind" <<'PY'
import json, os, sys
payload, kind = json.loads(sys.argv[1]), sys.argv[2]
assert payload.get("success"), payload
assert payload.get("evidence"), payload
latest = payload["evidence"][0]
assert latest["direct_count"] == 0, latest
if kind != "code" or os.environ.get("CIM_CODE_REQUIRE_TRAFFIC") == "1":
    assert latest["status"] == "pass", latest
    assert latest["proxied_count"] > 0, latest
else:
    assert latest["status"] in ("pass", "unknown"), latest
    print("code has no observed external traffic; direct=0 remains the safety result")
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
  remove_started "$kind"
done

echo "PASS: real proxy, GeoIP, locale and lsof validation"
