# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/tg-mcp /tg-mcp \
    && mkdir /data-empty

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tg-mcp /usr/local/bin/tg-mcp
# A fresh named volume copies this dir's ownership, so SQLite can write as nonroot.
COPY --from=build --chown=65532:65532 /data-empty /data
ENV TG_MCP_BIND=0.0.0.0:8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/tg-mcp"]
