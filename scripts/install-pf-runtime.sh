#!/bin/bash
# Install only the additional runtime PF permissions needed by CIM.
# Does not reload /etc/pf.conf, disable PF, or change unrelated anchors.
set -euo pipefail

if [[ $EUID -ne 0 || -z "${SUDO_USER:-}" || "$SUDO_USER" == root ]]; then
    echo 'Run using sudo from the CIM login user.' >&2
    exit 1
fi
case "$SUDO_USER" in
    *[!a-zA-Z0-9_-]*) echo 'Unsupported sudo username.' >&2; exit 1 ;;
esac

runtime_tmp="$(mktemp -d /private/tmp/cim-pf-runtime.XXXXXX)"
trap 'rm -rf "$runtime_tmp"' EXIT
runtime_rules=/etc/sudoers.d/cim_pf_runtime
cat > "$runtime_tmp/rules" <<EOF
# CIM runtime PF verification: read global status and enable only.
$SUDO_USER ALL=(root) NOPASSWD: /sbin/pfctl -s info
$SUDO_USER ALL=(root) NOPASSWD: /sbin/pfctl -s rules
$SUDO_USER ALL=(root) NOPASSWD: /sbin/pfctl -e
EOF
/usr/sbin/visudo -cf "$runtime_tmp/rules"
mkdir -p /etc/sudoers.d
if [[ -e "$runtime_rules" ]]; then
    cp -p "$runtime_rules" "$runtime_tmp/previous"
fi
/usr/bin/install -o root -g wheel -m 0440 "$runtime_tmp/rules" "$runtime_rules"
if ! /usr/sbin/visudo -cf /etc/sudoers; then
    if [[ -e "$runtime_tmp/previous" ]]; then
        cp -p "$runtime_tmp/previous" "$runtime_rules"
    else
        rm -f "$runtime_rules"
    fi
    exit 1
fi

# Refuse to treat rules on disk as a live anchor. Avoid a global rules reload.
if ! /sbin/pfctl -s rules | /usr/bin/grep -Eq '^anchor "com\.cim" all$'; then
    echo 'Live root rules do not reference com.cim; not enabling automatically.' >&2
    exit 1
fi
if ! /sbin/pfctl -s info | /usr/bin/grep -Eq '^Status: Enabled([[:space:]]|$)'; then
    /sbin/pfctl -e
fi
/sbin/pfctl -s info | /usr/bin/grep '^Status:'
echo 'Installed CIM PF runtime permissions; global PF enabled, com.cim anchor present.'
