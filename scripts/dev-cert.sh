#!/bin/sh
# Cria, uma vez, o certificado local "rustbeaver-dev" no keychain "login".
# Com ele o binário sai sempre com a mesma assinatura, e o "Permitir Sempre" do Keychain
# vale para os próximos builds (sem ele, cada build é um app novo para o macOS).
set -eu
if security find-certificate -c rustbeaver-dev >/dev/null 2>&1; then
  echo "rustbeaver-dev já existe."
  exit 0
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=rustbeaver-dev" \
  -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "basicConstraints=critical,CA:false" 2>/dev/null
# Algoritmos explícitos: o padrão do OpenSSL 3 (AES/SHA-256) o `security` não importa.
openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -name rustbeaver-dev \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -out "$tmp/id.p12" -passout pass:rustbeaver
# -T: só o codesign usa a chave privada sem perguntar.
security import "$tmp/id.p12" -k "$HOME/Library/Keychains/login.keychain-db" -P rustbeaver -T /usr/bin/codesign
echo "Pronto. O próximo cargo run assina o app; no primeiro acesso, digite a senha e clique em Permitir Sempre."
