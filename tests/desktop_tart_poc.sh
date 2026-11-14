#!/usr/bin/env bash
# Tart/Apple VM PoC for Claude Desktop.
#
# The default `check` mode is read-only. It never installs Tart, creates a VM,
# changes the host proxy, or changes PF. Desktop remains UNSUPPORTED until the
# healthy/blocked/recovered evidence is collected from inside a real guest.

set -euo pipefail

MODE="${1:-check}"
VM_NAME="${CIM_DESKTOP_VM_NAME:-cim-desktop-poc}"
VM_IMAGE="${CIM_DESKTOP_VM_IMAGE:-ghcr.io/cirruslabs/macos-sonoma-vanilla:latest}"
TART_BIN="${CIM_TART_BIN:-tart}"
SOCKS_PORT="${CIM_DESKTOP_SOCKS_PORT:-7891}"
HTTP_PORT="${CIM_DESKTOP_HTTP_PORT:-7890}"
PROXY_HOST="${CIM_DESKTOP_PROXY_HOST:-}"
LOG_DIR="${CIM_DESKTOP_VM_LOG_DIR:-${TMPDIR:-/tmp}/cim-desktop-tart}"

say() { printf '[desktop-tart] %s\n' "$*"; }
fail() { say "FAIL: $*" >&2; exit 1; }
blocked() { say "BLOCKED: $*" >&2; exit 2; }

host_gateway() {
  route -n get default 2>/dev/null | awk '/gateway:/{print $2; exit}'
}

host_platform_checks() {
  [[ "$(uname -s)" == "Darwin" ]] || blocked "Tart PoC requires macOS"
  [[ "$(uname -m)" == "arm64" ]] || blocked "Apple VM PoC requires an arm64 host"
  [[ -d /System/Library/Frameworks/Virtualization.framework ]] || \
    blocked "Virtualization.framework is not available"
}

host_checks() {
  host_platform_checks
  command -v "$TART_BIN" >/dev/null 2>&1 || \
    blocked "Tart is not installed; install it with: brew install openai/tools/tart"
}

vm_exists() {
  "$TART_BIN" list 2>/dev/null | awk 'NR > 1 {print $1}' | grep -Fxq "$VM_NAME"
}

vm_ip() {
  "$TART_BIN" ip "$VM_NAME" 2>/dev/null | head -n 1 | tr -d '[:space:]'
}

proxy_listener() {
  local port="$1"
  if ! command -v lsof >/dev/null 2>&1; then
    say "lsof unavailable; cannot inspect host proxy listener"
    return 1
  fi
  lsof -nP -a -iTCP:"$port" -sTCP:LISTEN 2>/dev/null || true
}

guest_exec() {
  # Tart's guest agent is preferred because it avoids storing VM credentials.
  if "$TART_BIN" exec --help 2>&1 | grep -q -- '--'; then
    "$TART_BIN" exec "$VM_NAME" -- "$@"
    return
  fi

  local ip
  ip="$(vm_ip)"
  [[ -n "$ip" ]] || fail "VM has no IP; start it and enable guest SSH or Tart guest agent"
  command -v ssh >/dev/null 2>&1 || fail "ssh is required for guest verification"
  local user="${CIM_DESKTOP_VM_USER:-admin}"
  ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null "$user@$ip" -- "$@"
}

check_mode() {
  host_platform_checks
  say "host: $(sw_vers -productVersion 2>/dev/null || true) $(uname -m)"
  say "Virtualization.framework: available"
  if ! command -v "$TART_BIN" >/dev/null 2>&1; then
    say "Tart: missing"
    say "install: brew install openai/tools/tart"
    say "check complete; no system state was changed"
    exit 2
  fi
  say "Tart: $($TART_BIN --version 2>/dev/null || echo available)"

  if vm_exists; then
    say "VM: $VM_NAME exists"
    local ip
    ip="$(vm_ip || true)"
    if [[ -n "$ip" ]]; then
      say "VM IP: $ip"
    else
      say "VM state: stopped (no IP)"
    fi
  else
    say "VM: $VM_NAME does not exist"
    say "next: $0 prepare"
  fi

  local gateway
  gateway="$(host_gateway || true)"
  say "host default gateway: ${gateway:-unknown}"
  if [[ -z "$PROXY_HOST" ]]; then
    say "proxy host: unset (set CIM_DESKTOP_PROXY_HOST to a host-reachable address)"
  else
    say "proxy host: $PROXY_HOST"
  fi
  say "HTTP proxy port: $HTTP_PORT; SOCKS5 proxy port: $SOCKS_PORT"
  say "host SOCKS listener:"
  proxy_listener "$SOCKS_PORT"
  say "host HTTP listener:"
  proxy_listener "$HTTP_PORT"
  say "check complete; no system state was changed"
}

prepare_mode() {
  host_checks
  vm_exists && fail "VM $VM_NAME already exists"
  say "cloning $VM_IMAGE to $VM_NAME (large download; explicit prepare requested)"
  "$TART_BIN" clone "$VM_IMAGE" "$VM_NAME"
  say "VM created; enable Remote Login or the Tart guest agent before verify"
}

run_mode() {
  host_checks
  vm_exists || fail "VM $VM_NAME does not exist; run: $0 prepare"
  mkdir -p "$LOG_DIR"
  local log="$LOG_DIR/$VM_NAME.log"
  say "starting $VM_NAME; log: $log"
  "$TART_BIN" run "$VM_NAME" --no-graphics=false >"$log" 2>&1 &
  say "tart run pid: $!"
  say "wait for the guest login/agent, then run: $0 verify healthy"
}

verify_mode() {
  local phase="${2:-healthy}"
  host_checks
  vm_exists || fail "VM $VM_NAME does not exist"
  local ip
  ip="$(vm_ip || true)"
  [[ -n "$ip" ]] || fail "VM is not running or has no IP"
  say "phase=$phase vm=$VM_NAME ip=$ip"
  say "guest proxy configuration"
  guest_exec sh -lc 'scutil --proxy 2>/dev/null || networksetup -getwebproxy Wi-Fi 2>/dev/null || true'
  say "guest route and resolver"
  guest_exec sh -lc 'route -n get default 2>/dev/null || true; scutil --dns 2>/dev/null | sed -n "1,80p" || true'

  if [[ -n "$PROXY_HOST" ]]; then
    say "proxy reachability from guest"
    guest_exec curl --connect-timeout 5 --max-time 15 \
      --proxy "http://$PROXY_HOST:$HTTP_PORT" https://api.ipify.org
    printf '\n'
  else
    say "proxy reachability skipped: CIM_DESKTOP_PROXY_HOST is unset"
  fi

  say "direct path probe (must be blocked by the guest fail-closed policy)"
  if guest_exec curl --connect-timeout 3 --max-time 5 --noproxy '*' \
      https://api.ipify.org >/dev/null 2>&1; then
    [[ "$phase" == "healthy" ]] && fail "direct path succeeded; guest fail-closed is not active"
    say "WARNING: direct path succeeded during phase=$phase"
  else
    say "direct path blocked"
  fi

  if [[ "$phase" == "blocked" ]]; then
    say "blocked phase evidence collected; proxy must be stopped before this command"
  elif [[ "$phase" == "recovered" ]]; then
    say "recovered phase evidence collected; compare exit IP with the healthy phase"
  else
    say "healthy phase evidence collected"
  fi
}

stop_mode() {
  host_checks
  vm_exists || fail "VM $VM_NAME does not exist"
  say "stopping $VM_NAME"
  "$TART_BIN" stop "$VM_NAME"
}

case "$MODE" in
  check) check_mode ;;
  prepare) prepare_mode ;;
  run) run_mode ;;
  verify) verify_mode "$@" ;;
  stop) stop_mode ;;
  *)
    cat >&2 <<'USAGE'
Usage: tests/desktop_tart_poc.sh <check|prepare|run|verify|stop> [healthy|blocked|recovered]

Default check is read-only. prepare/run/stop are explicit VM lifecycle actions.
verify never stops the host proxy; run it once per phase after manually changing
the proxy state and the guest fail-closed policy.

Environment:
  CIM_DESKTOP_VM_NAME       VM name (default: cim-desktop-poc)
  CIM_DESKTOP_VM_IMAGE      macOS image for prepare
  CIM_DESKTOP_PROXY_HOST    host-reachable address of the CIM HTTP proxy
  CIM_DESKTOP_VM_USER       SSH user if Tart guest agent is unavailable
USAGE
    exit 64
    ;;
esac
