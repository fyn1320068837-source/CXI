#!/bin/bash
# Apply one stable macOS code-signing identity to CIM binaries.
# Rust's default ad-hoc signature is cdhash-based, so every rebuild looks
# like a new application to macOS TCC. A reused identity keeps the requirement
# stable across normal release builds.

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    exit 0
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
IDENTITY_FILE="${HOME}/.cim/codesign-identity"
identity="${CIM_CODESIGN_IDENTITY:-}"

if [[ -z "$identity" && -f "$IDENTITY_FILE" ]]; then
    identity="$(sed -n '1p' "$IDENTITY_FILE")"
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
    # `security find-identity` exits non-zero when the login keychain has no
    # usable identity. Keep that case on the ad-hoc fallback path instead of
    # aborting the daemon restart under `set -e`.
    available_identities="$(security find-identity -v -p codesigning 2>/dev/null || true)"
    if [[ -n "$identity" ]] && ! grep -Fq "$identity" <<<"$available_identities"; then
        identity=""
    fi
    if [[ -z "$identity" ]]; then
        identity="$(awk '/Apple Development:/ && $0 !~ /CSSMERR/ { print $2; exit }' <<<"$available_identities")"
    fi
    if [[ -n "$identity" ]]; then
        mkdir -p "${HOME}/.cim"
        printf '%s\n' "$identity" > "$IDENTITY_FILE"
        chmod 600 "$IDENTITY_FILE"
    fi
fi

if [[ -z "$identity" ]]; then
    echo "⚠ 未找到可复用的 macOS 代码签名身份，保留 ad-hoc 签名" >&2
    exit 0
fi

sign_one() {
    local binary="$1"
    local identifier="$2"
    [[ -f "$binary" ]] || return 0
    codesign --force --sign "$identity" --identifier "$identifier" \
        --timestamp=none "$binary"
    echo "  固定签名: ${binary} (${identifier})"
}

sign_one "${1:-${ROOT_DIR}/target/release/cim}" "com.cim.daemon"
sign_one "${2:-${ROOT_DIR}/target/release/cim-instance-launch}" "com.cim.instance-launch"
