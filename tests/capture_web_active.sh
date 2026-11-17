#!/bin/bash
# Run from the user's terminal: sudo bash tests/capture_web_active.sh
# Captures only en0 TCP/UDP DNS traffic. The browser probe runs as SUDO_USER.
set -euo pipefail
umask 077
export PATH=/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin

if [[ ${EUID} -ne 0 || -z ${SUDO_USER:-} || ${SUDO_USER} == root || -z ${SUDO_UID:-} ]]; then
  echo 'Run with sudo from your normal login account: sudo bash tests/capture_web_active.sh' >&2
  exit 2
fi

node=/opt/homebrew/bin/node
if [[ ! -x "$node" ]]; then
  echo 'Required Node executable is missing: /opt/homebrew/bin/node (Node >= 22)' >&2
  exit 2
fi
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
probe="$script_dir/web_dns_probe.mjs"
if [[ ! -f "$probe" ]]; then
  echo "DNS probe is missing: $probe" >&2
  exit 2
fi
owner_gid="$(/usr/bin/id -g "$SUDO_USER")"
if [[ "$(/usr/bin/id -u "$SUDO_USER")" != "$SUDO_UID" ]]; then
  echo 'SUDO_USER and SUDO_UID do not match.' >&2
  exit 2
fi

evidence_dir="$(/usr/bin/mktemp -d /private/tmp/cim-web-dns.XXXXXX)"
/bin/chmod 0700 "$evidence_dir"
# Create and open every root-written output while the directory is still
# root-owned. After ownership transfer, root only uses these open descriptors;
# it never follows user-replaceable paths to write, chmod, chown, or read logs.
for file in dns.pcap tcpdump.log capture.json probe.json probe.log; do
  : >"$evidence_dir/$file"
  /bin/chmod 0600 "$evidence_dir/$file"
  /usr/sbin/chown "$SUDO_UID:$owner_gid" "$evidence_dir/$file"
done
exec 3>"$evidence_dir/capture.json"
exec 4>"$evidence_dir/probe.log"
exec 5>"$evidence_dir/tcpdump.log"
exec 6<"$evidence_dir/tcpdump.log"
capture_pid=''
directory_given=0
capture_started_at=''
listening_confirmed_at=''
probe_started_at=''
probe_finished_at=''
capture_stop_requested_at=''
capture_finished_at=''
probe_result=null
capture_result=null
alive_before_probe=false
alive_after_probe=false
stopped_by_runner=false
early_capture_exit=false
now() {
  # Use the same millisecond UTC format as probe.json, without running Node
  # as root. Second-only timestamps could falsely end before the probe did.
  /usr/bin/sudo -H -u "$SUDO_USER" -- /usr/bin/env \
    PATH=/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin \
    "$node" -p 'new Date().toISOString()'
}
json_time() {
  if [[ -n "$1" ]]; then printf '"%s"' "$1"; else printf 'null'; fi
}
finish() {
  local result=$?
  local capture_log captured received dropped_kernel dropped_interface
  local window_complete=false scope_ready=false
  trap - EXIT INT TERM
  set +e
  if [[ -n "$capture_pid" ]]; then
    if /bin/kill -0 "$capture_pid" 2>/dev/null; then
      capture_stop_requested_at="$(now)"
      if /bin/kill -INT "$capture_pid" 2>/dev/null; then
        stopped_by_runner=true
      else
        early_capture_exit=true
      fi
    else
      early_capture_exit=true
    fi
    wait "$capture_pid"
    capture_result=$?
    capture_finished_at="$(now)"
  fi
  # Read the original tcpdump inode, not a path the user can replace.
  capture_log="$(/bin/cat <&6)"
  captured="$(printf '%s\n' "$capture_log" | /usr/bin/awk '/^[0-9]+ packets? captured$/ { n=$1 } END { print n ~ /^[0-9]+$/ ? n : "null" }')"
  received="$(printf '%s\n' "$capture_log" | /usr/bin/awk '/^[0-9]+ packets? received by filter$/ { n=$1 } END { print n ~ /^[0-9]+$/ ? n : "null" }')"
  dropped_kernel="$(printf '%s\n' "$capture_log" | /usr/bin/awk '/^[0-9]+ packets? dropped by kernel$/ { n=$1 } END { print n ~ /^[0-9]+$/ ? n : "null" }')"
  dropped_interface="$(printf '%s\n' "$capture_log" | /usr/bin/awk '/^[0-9]+ packets? dropped by interface$/ { n=$1 } END { print n ~ /^[0-9]+$/ ? n : "null" }')"
  if [[ -n "$capture_started_at" && -n "$listening_confirmed_at" \
    && -n "$probe_started_at" && -n "$probe_finished_at" \
    && -n "$capture_stop_requested_at" && -n "$capture_finished_at" \
    && "$alive_before_probe" == true && "$alive_after_probe" == true \
    && "$stopped_by_runner" == true && "$early_capture_exit" == false && "$capture_result" == 0 ]]; then
    window_complete=true
  fi
  if [[ "$result" == 0 && "$window_complete" == true && "$probe_result" == 0 && "$captured" != null \
    && "$received" != null && "$dropped_kernel" == 0 \
    && ( "$dropped_interface" == null || "$dropped_interface" == 0 ) ]]; then
    scope_ready=true
  elif [[ "$result" == 0 ]]; then
    # A successful Node exit cannot hide premature capture termination,
    # missing tcpdump counters, or dropped packets.
    result=1
  fi
  # On early setup failure, transfer the still root-owned directory once.
  # Normal completion makes no privileged path changes after transfer.
  if [[ "$directory_given" == 0 ]]; then
    /usr/sbin/chown "$SUDO_UID:$owner_gid" "$evidence_dir" || result=1
  fi
  {
    printf '{\n  "interface": "en0",\n  "filter": "port 53",\n'
    printf '  "capture_started_at": %s,\n' "$(json_time "$capture_started_at")"
    printf '  "listening_confirmed_at": %s,\n' "$(json_time "$listening_confirmed_at")"
    printf '  "probe_started_at": %s,\n' "$(json_time "$probe_started_at")"
    printf '  "probe_finished_at": %s,\n' "$(json_time "$probe_finished_at")"
    printf '  "capture_stop_requested_at": %s,\n' "$(json_time "$capture_stop_requested_at")"
    printf '  "capture_finished_at": %s,\n' "$(json_time "$capture_finished_at")"
    printf '  "capture_pid": %s,\n' "${capture_pid:-null}"
    printf '  "probe_exit_status": %s,\n  "capture_exit_status": %s,\n' "$probe_result" "$capture_result"
    printf '  "script_exit_status": %s,\n' "$result"
    printf '  "alive_before_probe": %s,\n  "alive_after_probe": %s,\n' "$alive_before_probe" "$alive_after_probe"
    printf '  "stopped_by_runner": %s,\n  "early_capture_exit": %s,\n' "$stopped_by_runner" "$early_capture_exit"
    printf '  "packets_captured": %s,\n  "packets_received_by_filter": %s,\n' "$captured" "$received"
    printf '  "packets_dropped_by_kernel": %s,\n  "packets_dropped_by_interface": %s,\n' "$dropped_kernel" "$dropped_interface"
    printf '  "capture_window_complete": %s,\n  "scope_ready_for_review": %s,\n' "$window_complete" "$scope_ready"
    printf '  "verdict": "not_evaluated",\n'
    printf '  "limitations": "en0 TCP/UDP port 53 only; review the unique domain and raw pcap; no process attribution or global no-leak guarantee"\n}\n'
  } >&3
  if [[ $? != 0 ]]; then
    echo 'Failed to save capture metadata.' >&2
    result=1
  fi
  printf '\nOriginal tcpdump summary:\n%s\n' "$capture_log"
  # Read the user-owned probe report only after dropping privileges.
  /usr/bin/sudo -H -u "$SUDO_USER" -- "$node" -e '
    try {
      const p = JSON.parse(require("node:fs").readFileSync(process.argv[1], "utf8"));
      console.log("Browser probe:", p.status, "request sent:", p.request_dispatched === true,
        "temporary tab closed:", p.temporary_tab_closed === true);
      if (p.error) console.log("Probe reason:", p.error);
    } catch { console.log("Browser probe report unavailable; inspect probe.log."); }
  ' "$evidence_dir/probe.json"
  printf '\nEvidence directory: %s\n' "$evidence_dir"
  printf 'Capture window complete: %s; scope ready for manual review: %s.\n' "$window_complete" "$scope_ready"
  printf 'Capture and probe finished with exit %s. This is evidence collection, not a PASS verdict.\n' "$result"
  exec 3>&- 4>&- 5>&- 6<&-
  exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Open capture outputs before handing directory ownership to the normal user.
capture_started_at="$(now)"
/usr/sbin/tcpdump -i en0 -nn -U -w "$evidence_dir/dns.pcap" 'port 53' \
  >&5 2>&1 3>&- 4>&- 5>&- 6<&- &
capture_pid=$!
listening=0
for ((attempt=0; attempt<50; attempt++)); do
  if /usr/bin/grep -q 'listening on en0' "$evidence_dir/tcpdump.log"; then
    listening=1
    listening_confirmed_at="$(now)"
    break
  fi
  if ! /bin/kill -0 "$capture_pid" 2>/dev/null; then
    echo 'tcpdump stopped before capture was ready; see tcpdump.log.' >&2
    exit 1
  fi
  /bin/sleep 0.1
done
if [[ "$listening" != 1 ]]; then
  echo 'tcpdump did not confirm listening within 5 seconds; browser probe was not started.' >&2
  exit 1
fi

/usr/sbin/chown "$SUDO_UID:$owner_gid" "$evidence_dir"
directory_given=1
if ! /bin/kill -0 "$capture_pid" 2>/dev/null; then
  early_capture_exit=true
  echo 'tcpdump exited before the probe; capture is incomplete.' >&2
  exit 1
fi
alive_before_probe=true
printf 'Capturing en0 TCP/UDP port 53; running the temporary-tab probe as %s.\n' "$SUDO_USER"
probe_started_at="$(now)"
set +e
/usr/bin/sudo -H -u "$SUDO_USER" -- /usr/bin/env \
  PATH=/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin \
  "$node" "$probe" "$evidence_dir/probe.json" >&4 2>&1
probe_result=$?
set -e
probe_finished_at="$(now)"
if /bin/kill -0 "$capture_pid" 2>/dev/null; then
  alive_after_probe=true
else
  early_capture_exit=true
  echo 'tcpdump exited during the probe; capture is incomplete even if Node succeeded.' >&2
  exit 1
fi
exit "$probe_result"
