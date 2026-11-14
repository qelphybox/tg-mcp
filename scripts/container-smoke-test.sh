#!/usr/bin/env bash
# Container smoke test: runs an image the way a self-hoster would, with no
# configuration beyond fake Telegram, and fails on the first broken promise.
#
#   scripts/container-smoke-test.sh tg-mcp:ci
#
# Checks: the image is under the size budget (uncompressed), /healthz answers,
# the OAuth issuer is the localhost default, the server runs as non-root, and
# the database lands on the /data volume. Then, as on a hosting platform, with
# PORT=9000 and Railway's public domain: /healthz on 9000 and the detected issuer. Needs docker, curl and python3; no
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
  if [ "${status:-0}" != 0 ]; then
    for c in "$name" "$name-platform"; do
      docker inspect "$c" >/dev/null 2>&1 || continue
      echo "--- $c log ---" >&2
      docker logs "$c" >&2 2>&1 || true
    done
  fi
  docker rm -f "$name" "$name-platform" ${created:+"$created"} >/dev/null 2>&1 || true
  docker volume rm -f "$volume" >/dev/null 2>&1 || true
}
trap 'status=$?; cleanup' EXIT

# Size: the flattened filesystem, i.e. what the image takes once unpacked.
# (`docker image inspect` reports compressed sizes on some image stores.)
created="$(docker create "$image")"
size_bytes="$(docker export "$created" | wc -c)"
docker rm "$created" >/dev/null
created=
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

# wait_healthy <container> <base url>
wait_healthy() {
  for _ in $(seq 1 30); do
    curl -fsS "$2/healthz" >/dev/null 2>&1 && return 0
    [ "$(docker inspect -f '{{.State.Running}}' "$1")" = true ] || fail "$1 exited"
    sleep 1
  done
  curl -fsS "$2/healthz" >/dev/null || fail "/healthz did not answer within 30 s at $2"
}

# issuer_is <base url> <expected issuer>
issuer_is() {
  local issuer
  issuer="$(curl -fsS "$1/.well-known/oauth-authorization-server" \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["issuer"])')"
  [ "$issuer" = "$2" ] || fail "issuer is $issuer, expected $2"
  pass "issuer is $issuer"
}

base="http://127.0.0.1:$port"
wait_healthy "$name" "$base"
pass "/healthz answers"
issuer_is "$base" "http://localhost:8080"

# The uid inside the container, as the daemon's ps sees it (also on Docker Desktop).
uid="$(docker top "$name" -o pid,uid | awk 'NR == 2 { print $2 }')"
[ -n "$uid" ] || fail "could not read the server's uid"
[ "$uid" != 0 ] && [ "$uid" != root ] || fail "the server runs as root"
pass "the server runs as uid $uid"

docker cp "$name:/data/tg-mcp.db" - >/dev/null 2>&1 || fail "no database at /data/tg-mcp.db"
pass "the database is at /data/tg-mcp.db"

# As on a hosting platform: it assigns PORT and publishes its domain.
docker run -d --name "$name-platform" \
  -e TG_MCP_FAKE_TELEGRAM=1 \
  -e PORT=9000 \
  -e RAILWAY_PUBLIC_DOMAIN=tg-mcp-smoke.up.railway.app \
  -p "127.0.0.1:$((port + 1)):9000" \
  "$image" >/dev/null
platform_base="http://127.0.0.1:$((port + 1))"
wait_healthy "$name-platform" "$platform_base"
pass "/healthz answers on PORT=9000"
issuer_is "$platform_base" "https://tg-mcp-smoke.up.railway.app"
