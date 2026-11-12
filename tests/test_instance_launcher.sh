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
group="${CIM_ISOLATION_GROUP:-cimgrp}"
gid="$(dscl . -read "/Groups/${group}" PrimaryGroupID | awk '/PrimaryGroupID:/ {print $2}')"
[[ "$gid" =~ ^[0-9]+$ ]]

identity="$(sudo -n -E "$helper" \
  --uid "$uid" \
  --gid "$gid" \
  --user "$user" \
  -- /usr/bin/id -a)"

grep -Fq "uid=${uid}(${user})" <<<"$identity"
grep -Fq "egid=${gid}(${group})" <<<"$identity"
echo "PASS: instance helper drops to uid=${uid} and egid=${gid}(${group})"
