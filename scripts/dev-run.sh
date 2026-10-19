#!/bin/sh
# Cargo runner (.cargo/config.toml): signs the app before running it so the Keychain keeps "Always Allow".
# Prefers an "Apple Development" certificate: with a team id the Keychain trusts the team, so rebuilds
# don't prompt. Otherwise the local "rustflake-dev" one (scripts/dev-cert.sh), which still prompts once
# per rebuild. Without either, it just runs.
bin="$1"
if [ "$(basename "$bin")" = rustflake ]; then
  id=$(security find-identity -v -p codesigning 2>/dev/null | awk '/"Apple Development:/ {print $2; exit}')
  if [ -z "$id" ] && security find-certificate -c rustflake-dev >/dev/null 2>&1; then
    id=rustflake-dev
  fi
  if [ -n "$id" ]; then
    codesign --force --sign "$id" --identifier dev.rustflake "$bin" 2>/dev/null || echo "warning: could not sign $bin" >&2
  fi
fi
exec "$@"
