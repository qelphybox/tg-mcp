# syntax=docker/dockerfile:1
#
# Multi-arch (linux/amd64, linux/arm64) without emulation: the build stage runs on
# the builder's own platform and cross-compiles for the target one.

FROM --platform=$BUILDPLATFORM rust:1-slim-bookworm AS build
ARG TARGETARCH
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu; gcc_pkg=gcc-x86-64-linux-gnu; libc_pkg=libc6-dev-amd64-cross ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gcc_pkg=gcc-aarch64-linux-gnu; libc_pkg=libc6-dev-arm64-cross ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$triple" > /triple; \
    if [ "$(uname -m)" != "${triple%%-*}" ]; then \
      apt-get update && apt-get install -y --no-install-recommends "$gcc_pkg" "$libc_pkg" \
      && rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$triple"
# Linkers and C compilers (bundled SQLite) for cross builds; unused for native ones.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eu; triple="$(cat /triple)"; \
    if [ "$(uname -m)" = "${triple%%-*}" ]; then \
      unset CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER CC_x86_64_unknown_linux_gnu \
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER CC_aarch64_unknown_linux_gnu; \
    fi; \
    cargo build --release --locked --target "$triple" \
    && cp "target/$triple/release/tg-mcp" /tg-mcp \
    && mkdir /data-empty

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tg-mcp /usr/local/bin/tg-mcp
# A fresh named volume copies this dir's ownership, so SQLite can write as nonroot.
COPY --from=build --chown=65532:65532 /data-empty /data
ENV TG_MCP_BIND=0.0.0.0:8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/tg-mcp"]
