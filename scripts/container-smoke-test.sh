#!/usr/bin/env bash
# Container smoke test: runs an image the way a self-hoster would, with no
# configuration beyond fake Telegram, and fails on the first broken promise.
#
#   scripts/container-smoke-test.sh tg-mcp:ci
#
# Checks: the image is under the size budget (uncompressed), /healthz answers,
# the OAuth issuer is the localhost default, the server runs as non-root, and
# the database lands on the /data volume. Needs docker, curl and python3; no
# Telegram credentials (forks run it too). CI: .github/workflows/ci.yml.
set -euo pipefail

image="${1:?usage: $0 <image>}"
budget_mb="${SMOKE_SIZE_BUDGET_MB:-25}"
port="${SMOKE_HOST_PORT:-18080}"
name="tg-mcp-smoke-$$"
volume="tg-mcp-smoke-$$"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

cleanup() {
  if [ "${status:-0}" != 0 ] && docker inspect "$name" >/dev/null 2>&1; then
    echo "--- container log ---" >&2
    docker logs "$name" >&2 2>&1 || true
  fi
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker volume rm -f "$volume" >/dev/null 2>&1 || true
}
trap 'status=$?; cleanup' EXIT

# Size: the flattened filesystem, i.e. what the image takes once unpacked.
# (`docker image inspect` reports compressed sizes on some image stores.)
created="$(docker create "$image")"
size_bytes="$(docker export "$created" | wc -c)"
docker rm "$created" >/dev/null
size_mb="$(python3 -c "print(f'{$size_bytes / 1048576:.1f}')")"
[ "$size_bytes" -le $((budget_mb * 1048576)) ] \
  || fail "image is ${size_mb} MB uncompressed, over the ${budget_mb} MB budget"
pass "image is ${size_mb} MB uncompressed (budget ${budget_mb} MB)"

docker volume create "$volume" >/dev/null
docker run -d --name "$name" \
  -e TG_MCP_FAKE_TELEGRAM=1 \
  -v "$volume:/data" \
  -p "127.0.0.1:$port:8080" \
  "$image" >/dev/null

base="http://127.0.0.1:$port"
for _ in $(seq 1 30); do
  curl -fsS "$base/healthz" >/dev/null 2>&1 && break
  [ "$(docker inspect -f '{{.State.Running}}' "$name")" = true ] || fail "container exited"
  sleep 1
done
curl -fsS "$base/healthz" >/dev/null || fail "/healthz did not answer within 30 s"
pass "/healthz answers"

issuer="$(curl -fsS "$base/.well-known/oauth-authorization-server" \
  | python3 -c 'import json, sys; print(json.load(sys.stdin)["issuer"])')"
[ "$issuer" = "http://localhost:8080" ] || fail "issuer is $issuer, expected http://localhost:8080"
pass "issuer is $issuer"

pid="$(docker inspect -f '{{.State.Pid}}' "$name")"
uid="$(awk '/^Uid:/ { print $2 }' "/proc/$pid/status")"
[ "$uid" != 0 ] || fail "the server runs as root"
pass "the server runs as uid $uid"

docker cp "$name:/data/tg-mcp.db" - >/dev/null 2>&1 || fail "no database at /data/tg-mcp.db"
pass "the database is at /data/tg-mcp.db"
