#!/bin/bash
# Syntax-only verification; cimgrp must exist. No rules are loaded.
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin

rules_dir="$(/usr/bin/mktemp -d /private/tmp/cim-pf-syntax.XXXXXX)"
trap '/bin/rm -rf "$rules_dir"' EXIT
cat > "$rules_dir/block.conf" <<'EOF'
# Retain incoming loopback control states; block instance-initiated traffic.
pass in quick on lo0 inet proto tcp from 127.0.0.1 to 127.0.0.1 group cimgrp flags S/SA keep state
block return out quick from any to any group cimgrp
EOF
cat > "$rules_dir/allow.conf" <<'EOF'
# Incoming control states plus instance-initiated proxy connections only.
pass in quick on lo0 inet proto tcp from 127.0.0.1 to 127.0.0.1 group cimgrp flags S/SA keep state
pass out quick proto tcp from any to 127.0.0.1 port 7890 group cimgrp flags any no state
pass out quick proto tcp from any to 127.0.0.1 port 7891 group cimgrp flags any no state
pass out quick proto tcp from any to ::1 port 7890 group cimgrp flags any no state
pass out quick proto tcp from any to ::1 port 7891 group cimgrp flags any no state
block return out quick from any to any group cimgrp
EOF

/sbin/pfctl -n -a com.cim -f "$rules_dir/block.conf"
/sbin/pfctl -n -a com.cim -f "$rules_dir/allow.conf"
echo 'PASS: syntax only, including incoming control keep-state and proxy flags-any/no-state rules.'
echo 'No live rules changed. PF enforcement and browser connectivity still require runtime tests.'
