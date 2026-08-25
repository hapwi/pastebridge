#!/usr/bin/env bash
# Import MACOS_SELFSIGN_P12_* into a temporary keychain (macOS CI).
set -euo pipefail

if [[ -z "${MACOS_SELFSIGN_P12_BASE64:-}" || -z "${MACOS_SELFSIGN_P12_PASSWORD:-}" ]]; then
  echo "error: MACOS_SELFSIGN_P12_BASE64 and MACOS_SELFSIGN_P12_PASSWORD must be set" >&2
  exit 1
fi

TMP="${RUNNER_TEMP:-/tmp}"
KEYCHAIN="$TMP/pastebridge-codesign.keychain-db"
P12="$TMP/hapwi-pastebridge.p12"
KC_PASS="$(openssl rand -hex 16)"
export P12

python3 -c 'import base64, os; open(os.environ["P12"], "wb").write(base64.b64decode(os.environ["MACOS_SELFSIGN_P12_BASE64"]))'

security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
security create-keychain -p "$KC_PASS" "$KEYCHAIN"
security set-keychain-settings -lut 21600 "$KEYCHAIN"
security unlock-keychain -p "$KC_PASS" "$KEYCHAIN"
security import "$P12" -k "$KEYCHAIN" -P "$MACOS_SELFSIGN_P12_PASSWORD" \
  -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KC_PASS" "$KEYCHAIN"

EXISTING=()
while IFS= read -r line; do
  line="${line//\"/}"
  line="${line#"${line%%[![:space:]]*}"}"
  line="${line%"${line##*[![:space:]]}"}"
  [[ -n "$line" ]] && EXISTING+=("$line")
done < <(security list-keychains -d user)
security list-keychains -d user -s "$KEYCHAIN" "${EXISTING[@]}"

rm -f "$P12"
echo "Imported hapwi Pastebridge into $KEYCHAIN"
