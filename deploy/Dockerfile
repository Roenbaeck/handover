# syntax=docker/dockerfile:1.7
# Builds handover-proxy and handover-mock. Multi-arch: build linux/arm64 for the
# DGX Sparks with `docker buildx build --platform linux/arm64 -f deploy/Dockerfile .`.
# With a vendored tree (deploy/bundle.sh) the build needs no network.

FROM rust:1.98-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    if [ -d vendor ]; then \
      mkdir -p .cargo && printf '[source.crates-io]\nreplace-with = "vendored"\n[source.vendored]\ndirectory = "vendor"\n' > .cargo/config.toml; \
      OFFLINE=--offline; \
    fi; \
    cargo build --release --locked $OFFLINE -p handover-proxy -p handover-mock \
    && mkdir -p /out && cp target/release/handover-proxy target/release/handover-mock /out/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /var/lib/handover handover \
    && mkdir -p /var/lib/handover && chown handover /var/lib/handover
COPY --from=build /out/handover-proxy /out/handover-mock /usr/local/bin/
USER handover
VOLUME /var/lib/handover
EXPOSE 8443
ENTRYPOINT ["handover-proxy"]
CMD ["--config", "/etc/handover/handover.toml"]
