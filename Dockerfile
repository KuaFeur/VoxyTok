# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
ENV PORT=8080 RUST_LOG=voxytok=info VOXYTOK_DATA_DIR=/home/container/data \
    POSTGRES_DB=voxytok POSTGRES_USER=voxytok
RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql postgresql-client gosu ca-certificates bash curl \
 && rm -rf /var/lib/apt/lists/* \
 && (getent group voxytok >/dev/null || groupadd --system voxytok) \
 && (getent passwd voxytok >/dev/null || useradd --system --gid voxytok --home-dir /home/container --no-create-home voxytok) \
 && mkdir -p /home/container/data /var/lib/postgresql /var/run/postgresql /var/log/postgresql \
 && chown -R postgres:postgres /var/lib/postgresql /var/run/postgresql /var/log/postgresql \
 && chown -R voxytok:voxytok /home/container
COPY --from=builder /build/target/release/voxytok /usr/local/bin/voxytok
COPY docker/entrypoint.sh /usr/local/bin/voxytok-entrypoint
RUN chmod 0755 /usr/local/bin/voxytok /usr/local/bin/voxytok-entrypoint
WORKDIR /home/container
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/voxytok-entrypoint"]
