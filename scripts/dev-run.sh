#!/bin/sh
# Runner do cargo (.cargo/config.toml): assina o app com o certificado local antes de rodar.
# Sem o certificado (scripts/dev-cert.sh), só roda.
bin="$1"
if [ "$(basename "$bin")" = rustbeaver ] && security find-certificate -c rustbeaver-dev >/dev/null 2>&1; then
  codesign --force --sign rustbeaver-dev --identifier dev.rustbeaver "$bin" 2>/dev/null || echo "aviso: não consegui assinar $bin" >&2
fi
exec "$@"
