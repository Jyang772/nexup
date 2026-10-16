FROM rust:1.96-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 nexup \
    && useradd --uid 10001 --gid nexup --no-create-home nexup \
    && mkdir /data && chown nexup:nexup /data
COPY --from=build /build/target/release/nexup /usr/local/bin/nexup
USER 10001:10001
ENV NEXUP_LISTEN=0.0.0.0:8080 NEXUP_DATA_DIR=/data
EXPOSE 8080
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD curl --noproxy '*' -fsS http://127.0.0.1:8080/health || exit 1
ENTRYPOINT ["nexup"]
CMD ["serve"]
