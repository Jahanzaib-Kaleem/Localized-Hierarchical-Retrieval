FROM node:22-bookworm-slim AS studio-build
WORKDIR /build/studio
COPY studio/package.json ./
RUN npm install --no-audit --no-fund
COPY studio/ ./
RUN npm run build

FROM rust:1.90-bookworm AS rust-build
WORKDIR /build
COPY rust/ ./rust/
RUN cargo build --manifest-path rust/Cargo.toml --release --bin lhr

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 lhr \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin lhr \
    && mkdir -p /data /opt/lhr/studio \
    && chown -R lhr:lhr /data
COPY --from=rust-build /build/rust/target/release/lhr /usr/local/bin/lhr
COPY --from=studio-build /build/studio/dist/ /opt/lhr/studio/
COPY --chown=lhr:lhr docker/entrypoint.sh /usr/local/bin/lhr-entrypoint
RUN chmod 0755 /usr/local/bin/lhr-entrypoint
ENV LHR_ROOT=/data \
    LHR_STUDIO_DIR=/opt/lhr/studio
VOLUME ["/data"]
EXPOSE 8787
USER lhr
ENTRYPOINT ["/usr/local/bin/lhr-entrypoint"]
