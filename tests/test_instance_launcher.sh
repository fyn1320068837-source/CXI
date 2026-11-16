#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  echo "SKIP: instance launcher test requires macOS"
  exit 0
fi

helper="/usr/local/libexec/cim-instance-launch"
if [[ ! -x "$helper" ]]; then
  echo "SKIP: $helper is not installed"
  exit 0
fi

if ! sudo -n -l 2>/dev/null | grep -Fq "$helper"; then
  echo "SKIP: sudoers does not authorize $helper"
  exit 0
fi

user="$(id -un)"
uid="$(id -u)"
group="cimgrp"
gid="$(dscl . -read "/Groups/${group}" PrimaryGroupID | awk '/PrimaryGroupID:/ {print $2}')"
[[ "$gid" =~ ^[0-9]+$ ]]

if sudo -n -E "$helper" \
  --uid "$uid" \
  --gid "$gid" \
  --user "$user" \
  -- /usr/bin/id -a >/dev/null 2>&1; then
  echo "FAIL: arbitrary /usr/bin/id was accepted by the launcher allowlist" >&2
  exit 1
fi

if sudo -n -E "$helper" \
  --uid "$uid" \
  --gid "$((gid + 1))" \
  --user "$user" \
  -- /Applications/Claude.app/Contents/MacOS/Claude >/dev/null 2>&1; then
  echo "FAIL: non-cimgrp GID was accepted by the launcher" >&2
  exit 1
fi

echo "PASS: launcher rejects arbitrary programs and non-cimgrp GIDs (cimgrp=${gid})"
