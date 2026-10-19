#!/bin/sh
# Creates, once, the local "rustbeaver-dev" certificate in the "login" keychain.
# With it the binary always gets the same signature, so the Keychain's "Always Allow"
# carries over to later builds (without it, every build is a new app to macOS).
set -eu
if security find-certificate -c rustbeaver-dev >/dev/null 2>&1; then
  echo "rustbeaver-dev already exists."
  exit 0
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=rustbeaver-dev" \
  -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "basicConstraints=critical,CA:false" 2>/dev/null
# Explicit algorithms: `security` cannot import OpenSSL 3's default (AES/SHA-256).
openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -name rustbeaver-dev \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -out "$tmp/id.p12" -passout pass:rustbeaver
# -T: only codesign may use the private key without prompting.
security import "$tmp/id.p12" -k "$HOME/Library/Keychains/login.keychain-db" -P rustbeaver -T /usr/bin/codesign
echo "Done. The next cargo run signs the app; on first access, type your password and click Always Allow."
