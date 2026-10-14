# Builds a small image that runs `bbs serve`. The proto compiles with protox,
# so the builder needs no protoc.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends sqlite3 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 999 --user-group --create-home --home-dir /var/lib/bbs bbs
COPY --from=build /src/target/release/bbs /usr/local/bin/bbs
# Numeric, so Kubernetes can verify runAsNonRoot without a runAsUser override.
USER 999:999
# Logs go to stderr for `docker logs`; the state dir is a volume so the database
# survives restarts.
ENV HOME=/var/lib/bbs BBS_LOG_STDERR=1
VOLUME /var/lib/bbs
EXPOSE 7777
ENTRYPOINT ["bbs", "serve", "--state-dir", "/var/lib/bbs"]
CMD ["--listen", "0.0.0.0:7777"]
