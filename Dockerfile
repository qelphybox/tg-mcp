# syntax=docker/dockerfile:1
#
# Static musl binary on distroless/static, for linux/amd64 and linux/arm64 (#101).
# No emulation: the build stage runs on the builder's own platform and
# cross-compiles for the target one, with zig (through cargo-zigbuild) as the C
# compiler (bundled SQLite, mimalloc) and the linker for both targets.

FROM --platform=$BUILDPLATFORM rust:1-slim-bookworm AS build
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3-pip \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install --no-cache-dir --break-system-packages ziglang==0.15.2 cargo-zigbuild==0.24.0
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-musl ;; \
      arm64) triple=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$triple" > /triple; \
    rustup target add "$triple"
WORKDIR /src
COPY . .
# The Built-in app (ADR-0007) comes only as BuildKit secrets, never as ARGs
# (those show in `docker history` and provenance):
#   --secret id=tg_builtin_api_id,env=... --secret id=tg_builtin_api_hash,env=...
# Without them the binary has no Built-in app, as with `cargo build`.
# Pass TG_MCP_BUILTIN_APP=required with them (release builds do): it fails the
# build without them, and keeps layers cached with and without the secrets
# apart (secrets are not part of the cache key).
ARG TG_MCP_BUILTIN_APP=optional
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=secret,id=tg_builtin_api_id,env=TG_MCP_BUILTIN_API_ID \
    --mount=type=secret,id=tg_builtin_api_hash,env=TG_MCP_BUILTIN_API_HASH \
    set -eu; triple="$(cat /triple)"; \
    if [ "$TG_MCP_BUILTIN_APP" = required ] \
       && { [ -z "${TG_MCP_BUILTIN_API_ID:-}" ] || [ -z "${TG_MCP_BUILTIN_API_HASH:-}" ]; }; then \
      echo "TG_MCP_BUILTIN_APP=required, but the tg_builtin_api_id / tg_builtin_api_hash secrets are missing" >&2; \
      exit 1; \
    fi; \
    cargo zigbuild --release --locked --target "$triple" \
    && cp "target/$triple/release/tg-mcp" /tg-mcp \
    && mkdir /data-empty

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /tg-mcp /usr/local/bin/tg-mcp
# A fresh named volume copies this dir's ownership, so SQLite can write as nonroot.
# No VOLUME instruction: some hosting platforms reject images that declare one.
COPY --from=build --chown=65532:65532 /data-empty /data
# mimalloc commits its arenas eagerly by default: about 11 MB RSS at idle
# instead of 2. Committing on demand keeps it at glibc's level (#101).
# No TG_MCP_BIND: the server listens on the platform's PORT, else 8080 (#102).
ENV TG_MCP_DB_PATH=/data/tg-mcp.db \
    MIMALLOC_ARENA_EAGER_COMMIT=0
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/tg-mcp"]
