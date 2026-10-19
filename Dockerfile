# ---- builder: static musl binary (rust:alpine targets musl by default) ----
FROM rust:1-alpine AS builder
# musl-dev provides the C toolchain that rustls' `ring` crypto backend compiles against.
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY static ./static
RUN cargo build --release --locked

# ---- runtime: the binary (frontend baked in) + ffmpeg for the HLS re-encode ----
# Unlike shopping-list this can't be `scratch`: ffmpeg turns the broadcast
# (MPEG-2/AC-3) into H.264/AAC that iPhones and browsers can play.
FROM alpine:3.22
RUN apk add --no-cache ffmpeg \
    && if [ "$(apk --print-arch)" = "x86_64" ]; then \
         # Optional Intel iGPU encoding (HWACCEL=vaapi): i965 for older chips, iHD for newer.
         apk add --no-cache libva-intel-driver intel-media-driver; \
       fi \
    && adduser -D -u 1000 tvh \
    && mkdir -p /data && chown tvh:tvh /data
COPY --from=builder /app/target/release/tvh-proxy /usr/local/bin/tvh-proxy
USER tvh
ENV PORT=9982 DATA_DIR=/data
VOLUME /data
EXPOSE 9982
HEALTHCHECK --interval=5m --timeout=10s --start-period=30s --retries=3 \
    CMD ["tvh-proxy", "healthcheck"]
ENTRYPOINT ["tvh-proxy"]
