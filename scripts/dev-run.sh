#!/bin/sh
# Cargo runner (.cargo/config.toml): signs the app with the local certificate before running it.
# Without the certificate (scripts/dev-cert.sh), it just runs.
bin="$1"
if [ "$(basename "$bin")" = rustflake ] && security find-certificate -c rustflake-dev >/dev/null 2>&1; then
  codesign --force --sign rustflake-dev --identifier dev.rustflake "$bin" 2>/dev/null || echo "warning: could not sign $bin" >&2
fi
exec "$@"
