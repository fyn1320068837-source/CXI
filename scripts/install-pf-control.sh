#!/bin/bash
# Run once: sudo bash scripts/install-pf-control.sh
# Installs only the fixed CIM allow/block files. It never disables PF, reloads
# the main ruleset, changes sudoers, or loads the allowed policy.
# Stop all CIM instances before running. No PF states are flushed: old
# stateful data connections are not revoked merely by replacing rules.
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin

if [[ $EUID -ne 0 || -z ${SUDO_USER:-} || ${SUDO_USER} == root || -z ${SUDO_UID:-} ]]; then
  echo 'Run with sudo from the CIM login account.' >&2
  exit 2
fi
if [[ "$(/usr/bin/id -u "$SUDO_USER")" != "$SUDO_UID" ]]; then
  echo 'SUDO_USER and SUDO_UID do not match.' >&2
  exit 2
fi

anchor_dir=/etc/pf.anchors
allow_path=/etc/pf.anchors/com.cim.allow
block_path=/etc/pf.anchors/com.cim.block
backup_root=/private/var/root/cim-pf-backups
staging=''
backup_dir=''
pending_rule=''
mutation_started=0

check_root_path() {
  local item=$1 expected=$2 owner mode
  if [[ -L "$item" || ( "$expected" == directory && ! -d "$item" ) \
    || ( "$expected" == file && ! -f "$item" ) ]]; then
    echo "Refusing symlink or unexpected path type: $item" >&2
    return 1
  fi
  owner="$(/usr/bin/stat -f '%u' "$item")"
  mode="$(/usr/bin/stat -f '%Lp' "$item")"
  if [[ "$owner" != 0 ]] || (( (8#$mode & 0022) != 0 )); then
    echo "Refusing non-root-owned or group/other-writable path: $item" >&2
    return 1
  fi
}
atomic_install_rule() {
  local source=$1 target=$2
  case "$target" in
    "$allow_path"|"$block_path") ;;
    *) echo 'Refusing unexpected PF rule target.' >&2; return 1 ;;
  esac
  check_root_path "$anchor_dir" directory
  check_root_path "$target" file
  # A running daemon may read either target at any time. Rename a complete
  # file on the same filesystem so it can never load a truncated policy.
  pending_rule="$(/usr/bin/mktemp "$anchor_dir/.cim-control.XXXXXX")"
  /usr/bin/install -o root -g wheel -m 0644 "$source" "$pending_rule"
  /bin/mv -f "$pending_rule" "$target"
  pending_rule=''
}
finish() {
  local result=$?
  trap - EXIT INT TERM
  set +e
  if [[ "$result" != 0 && "$mutation_started" == 1 ]]; then
    if /sbin/pfctl -a com.cim -f "$backup_dir/new-block.conf"; then
      echo 'Installation failed; the new blocked policy was reloaded.' >&2
    else
      echo 'Installation failed and blocked-policy reload also failed; inspect PF immediately.' >&2
    fi
  fi
  if [[ -n "$pending_rule" ]]; then /bin/rm -f "$pending_rule"; fi
  if [[ -n "$staging" ]]; then /bin/rm -rf "$staging"; fi
  if [[ -n "$backup_dir" ]]; then
    printf 'Root-only backup: %s\n' "$backup_dir"
    printf 'Restore original files and load blocked policy: sudo bash %s/restore.sh\n' "$backup_dir"
  fi
  exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

check_root_path "$anchor_dir" directory
check_root_path "$allow_path" file
check_root_path "$block_path" file
staging="$(/usr/bin/mktemp -d /private/tmp/cim-pf-control.XXXXXX)"
/bin/chmod 0700 "$staging"
/usr/sbin/chown root:wheel "$staging"

# Fixed embedded rules: pfctl never consumes a user-writable workspace file.
cat > "$staging/allow.conf" <<'EOF'
# CIM healthy policy: local proxy only, with incoming loopback control states.
pass in quick on lo0 inet proto tcp from 127.0.0.1 to 127.0.0.1 group cimgrp flags S/SA keep state
pass out quick proto tcp from any to 127.0.0.1 port 7890 group cimgrp flags any no state
pass out quick proto tcp from any to 127.0.0.1 port 7891 group cimgrp flags any no state
pass out quick proto tcp from any to ::1 port 7890 group cimgrp flags any no state
pass out quick proto tcp from any to ::1 port 7891 group cimgrp flags any no state
block return out quick from any to any group cimgrp
EOF
cat > "$staging/block.conf" <<'EOF'
# CIM blocked policy: only incoming loopback control states remain.
pass in quick on lo0 inet proto tcp from 127.0.0.1 to 127.0.0.1 group cimgrp flags S/SA keep state
block return out quick from any to any group cimgrp
EOF
/sbin/pfctl -n -a com.cim -f "$staging/allow.conf"
/sbin/pfctl -n -a com.cim -f "$staging/block.conf"
/sbin/pfctl -s info > "$staging/pf-info-before.txt"
if ! /usr/bin/grep -Eq '^Status: Enabled([[:space:]]|$)' "$staging/pf-info-before.txt"; then
  echo 'PF is not enabled; refusing to claim installation provides enforcement.' >&2
  exit 1
fi
/sbin/pfctl -s rules > "$staging/root-rules-before.txt"
if ! /usr/bin/grep -Eq '^anchor "com\.cim" all$' "$staging/root-rules-before.txt"; then
  echo 'The live main ruleset does not reference the CIM anchor; it was not changed.' >&2
  exit 1
fi

check_root_path /private/var/root directory
if [[ ! -e "$backup_root" && ! -L "$backup_root" ]]; then
  /bin/mkdir -m 0700 "$backup_root"
fi
check_root_path "$backup_root" directory
backup_mode="$(/usr/bin/stat -f '%Lp' "$backup_root")"
if (( (8#$backup_mode & 0077) != 0 )); then
  echo 'Backup directory must be private to root (0700).' >&2
  exit 1
fi
backup_dir="$(/usr/bin/mktemp -d "$backup_root/control.XXXXXX")"
/usr/bin/install -o root -g wheel -m 0600 "$allow_path" "$backup_dir/original-allow.conf"
/usr/bin/install -o root -g wheel -m 0600 "$block_path" "$backup_dir/original-block.conf"
/usr/bin/install -o root -g wheel -m 0600 "$staging/block.conf" "$backup_dir/new-block.conf"
/usr/bin/install -o root -g wheel -m 0600 "$staging/pf-info-before.txt" "$backup_dir/pf-info-before.txt"
/usr/bin/install -o root -g wheel -m 0600 "$staging/root-rules-before.txt" "$backup_dir/root-rules-before.txt"
/sbin/pfctl -a com.cim -s rules > "$backup_dir/anchor-before.txt"
cat > "$backup_dir/restore.sh" <<EOF
#!/bin/bash
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
if [[ \$EUID -ne 0 ]]; then echo 'Run this recovery script with sudo.' >&2; exit 2; fi
echo 'Stop all CIM instances before recovery. Existing data states are not flushed or revoked.' >&2
pending_rule=''
trap 'if [[ -n "\$pending_rule" ]]; then /bin/rm -f "\$pending_rule"; fi' EXIT
atomic_restore_rule() {
  local source=\$1 target=\$2
  case "\$target" in
    '$allow_path'|'$block_path') ;;
    *) echo 'Refusing unexpected recovery target.' >&2; return 1 ;;
  esac
  if [[ -L "\$target" || ! -f "\$target" ]]; then
    echo 'Refusing symlink or unexpected recovery target type.' >&2
    return 1
  fi
  pending_rule="\$(/usr/bin/mktemp '$anchor_dir/.cim-control.XXXXXX')"
  /usr/bin/install -o root -g wheel -m 0644 "\$source" "\$pending_rule"
  /bin/mv -f "\$pending_rule" "\$target"
  pending_rule=''
}
/sbin/pfctl -n -a com.cim -f '$backup_dir/original-allow.conf'
/sbin/pfctl -n -a com.cim -f '$backup_dir/original-block.conf'
/sbin/pfctl -a com.cim -f '$backup_dir/original-block.conf'
atomic_restore_rule '$backup_dir/original-allow.conf' '$allow_path'
atomic_restore_rule '$backup_dir/original-block.conf' '$block_path'
/sbin/pfctl -a com.cim -f '$backup_dir/original-block.conf'
echo 'Original CIM files restored; only the old blocked policy was loaded.'
echo 'A running daemon can reapply its healthy policy; stop managed instances before recovery.'
EOF
/bin/chmod 0700 "$backup_dir/restore.sh"

mutation_started=1
# Managed instances must already be stopped to retire old stateful sockets.
# Do not use -F states: -a must not be assumed to scope state-table clearing.
# Block before atomically replacing each disk file.
/sbin/pfctl -a com.cim -f "$staging/block.conf"
atomic_install_rule "$staging/allow.conf" "$allow_path"
atomic_install_rule "$staging/block.conf" "$block_path"
# Keep the final installer action restrictive, even if a running daemon raced
# its own transition during installation. The daemon must be redeployed next.
/sbin/pfctl -a com.cim -f "$staging/block.conf"
/sbin/pfctl -a com.cim -s rules
echo 'Installed incoming loopback control rules; only the blocked policy was loaded.'
echo 'A running daemon can reapply its healthy policy; this installer does not lock its transitions.'
echo 'No PF states were flushed; stopped CIM instances are required to retire old data connections.'
echo 'PF was not disabled and the main ruleset was not reloaded. Deploy the updated daemon and run live regression next.'
