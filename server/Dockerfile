# Build context: the server/ directory
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY ui ./ui
RUN cargo build --release --locked || cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data fleet \
    && mkdir -p /data && chown fleet /data
COPY --from=build /src/target/release/p4fleet /usr/local/bin/p4fleet
USER fleet
ENV FLEET_DATA_DIR=/data FLEET_LISTEN=0.0.0.0:8790
VOLUME /data
EXPOSE 8790
ENTRYPOINT ["/usr/local/bin/p4fleet"]
