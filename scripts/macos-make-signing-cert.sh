#!/usr/bin/env bash
# Create the stable self-signed macOS identity (MMF pattern).
# Run from the repo root. Does not commit the private key.
#
#   ./scripts/macos-make-signing-cert.sh
#
# Then (if you are not using the secrets already set in this repo):
#   gh secret set MACOS_SELFSIGN_P12_BASE64 < cert.p12.b64
#   gh secret set MACOS_SELFSIGN_P12_PASSWORD --body "$PASSWORD"
set -euo pipefail

IDENTITY="hapwi Pastebridge"
OUT="${1:-/tmp/pastebridge-signing}"
mkdir -p "$OUT"
chmod 700 "$OUT"

PASSWORD="${MACOS_SELFSIGN_P12_PASSWORD:-$(openssl rand -hex 24)}"
CNF="$OUT/codesign.cnf"
KEY="$OUT/key.pem"
CERT="$OUT/cert.pem"
P12="$OUT/hapwi-pastebridge.p12"

cat > "$CNF" <<'EOF'
[req]
distinguished_name = req_distinguished_name
x509_extensions = v3_ext
prompt = no

[req_distinguished_name]
CN = hapwi Pastebridge
O = hapwi

[v3_ext]
basicConstraints = CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF

openssl req -new -x509 -days 3650 -nodes -newkey rsa:2048 \
  -keyout "$KEY" -out "$CERT" -config "$CNF"

# SHA1/3DES PKCS#12 so macOS `security import` accepts it.
openssl pkcs12 -export \
  -inkey "$KEY" -in "$CERT" \
  -out "$P12" \
  -name "$IDENTITY" \
  -passout "pass:${PASSWORD}" \
  -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1

base64 -w0 "$P12" > "$OUT/hapwi-pastebridge.p12.b64"
printf '%s\n' "$PASSWORD" > "$OUT/password"
chmod 600 "$KEY" "$CERT" "$P12" "$OUT/hapwi-pastebridge.p12.b64" "$OUT/password"

echo "Wrote $P12"
echo "Identity: $IDENTITY"
echo "Bundle id used at codesign time: com.hapwi.pastebridge"
echo
echo "Set GitHub secrets (do not commit these files):"
echo "  gh secret set MACOS_SELFSIGN_P12_BASE64 < $OUT/hapwi-pastebridge.p12.b64"
echo "  gh secret set MACOS_SELFSIGN_P12_PASSWORD < $OUT/password"
