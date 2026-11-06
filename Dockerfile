# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/tg-mcp /tg-mcp

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tg-mcp /usr/local/bin/tg-mcp
ENV TG_MCP_BIND=0.0.0.0:8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/tg-mcp"]
