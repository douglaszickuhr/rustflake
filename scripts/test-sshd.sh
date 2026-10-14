#!/usr/bin/env bash
# Sobe o bastion de teste (rb-sshd) na mesma rede do rb-pg. Chaves de teste ficam em target/test-ssh.
set -euo pipefail
cd "$(dirname "$0")/.."
D=target/test-ssh
mkdir -p "$D"
[ -f "$D/id_plain" ] || ssh-keygen -q -t ed25519 -N '' -C rb-plain -f "$D/id_plain"
[ -f "$D/id_pass" ] || ssh-keygen -q -t ed25519 -N 'segredo' -C rb-pass -f "$D/id_pass"
[ -f "$D/id_agent" ] || ssh-keygen -q -t ed25519 -N '' -C rb-agent -f "$D/id_agent"
cat "$D/id_plain.pub" "$D/id_pass.pub" "$D/id_agent.pub" > "$D/authorized_keys"
docker network inspect rb-net >/dev/null 2>&1 || docker network create rb-net >/dev/null
docker network connect rb-net rb-pg 2>/dev/null || true
docker build -q -t rb-sshd -f scripts/sshd.Dockerfile scripts >/dev/null
docker rm -f rb-sshd >/dev/null 2>&1 || true
docker run -d --name rb-sshd --network rb-net -p 127.0.0.1:52222:22 \
  -v "$PWD/$D/authorized_keys:/home/rb/.ssh/authorized_keys:ro" rb-sshd >/dev/null
for _ in $(seq 50); do nc -z 127.0.0.1 52222 >/dev/null 2>&1 && break; sleep 0.1; done
echo "export RB_TEST_SSH=127.0.0.1:52222"
