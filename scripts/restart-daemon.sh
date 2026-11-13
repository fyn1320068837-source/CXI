#!/bin/bash
# Build and restart the launchd-managed CIM daemon.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd -P)"
RUNTIME_BIN="${HOME}/.cim/bin/cim"

cd "$ROOT_DIR"
cargo build --release
"${ROOT_DIR}/scripts/sign-macos-binaries.sh" \
  "${ROOT_DIR}/target/release/cim" \
  "${ROOT_DIR}/target/release/cim-instance-launch"
mkdir -p "${HOME}/.cim/bin"
ln -sfn "$ROOT_DIR/target/release/cim" "$RUNTIME_BIN"

launchctl kickstart -k "gui/$(id -u)/com.cim.daemon"
for _ in $(seq 1 45); do
    if curl -fsS --max-time 2 http://127.0.0.1:8080/health >/dev/null; then
        echo "✅ 守护进程已使用最新 release 重启"
        exit 0
    fi
    sleep 1
done

echo "❌ 守护进程启动失败，请查看 ~/.cim/logs/daemon.log"
exit 1
