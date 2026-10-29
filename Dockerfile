# openfront.video: the web service (web/) and the renderer it runs
# (engine-rs's openfront-video, with its maps and assets built in).
#
#   docker build -t openfront-video .
#   docker run -p 8080:8080 -v openfront-videos:/data openfront-video
#
# Put a TLS-terminating proxy in front for https://openfront.video.

# syntax=docker/dockerfile:1
FROM rust:1.97-bookworm AS build
# NASM lets OpenH264 build its assembly (much faster encoding).
RUN apt-get update && apt-get install -y --no-install-recommends nasm && rm -rf /var/lib/apt/lists/*

# The engine version the Rust port follows; build.rs takes the maps, sprites,
# font and theme from it. Only those folders are checked out.
ARG OPENFRONT_COMMIT=9f4a28f16fae951df0daf727e5e64b34ce8b77d1
WORKDIR /src
RUN git init -q OpenFrontIO \
 && cd OpenFrontIO \
 && git remote add origin https://github.com/openfrontio/OpenFrontIO.git \
 && git sparse-checkout set --no-cone \
      /resources/maps/ /resources/atlases/ /resources/fonts/ /resources/tribeNameThemes.json '/src/client/render/gl/*.json' \
 && git fetch -q --depth 1 --filter=blob:none origin "$OPENFRONT_COMMIT" \
 && git checkout -q FETCH_HEAD

# Cargo's downloads and build folders live in BuildKit caches that outlast
# this layer, so a source change recompiles only what changed (dependencies
# and the compressed maps stay built). The binaries are copied out of the
# cache in the same step, since the cache isn't part of the image. Debug info
# only slows the link here.
ENV CARGO_PROFILE_RELEASE_DEBUG=false
COPY engine-rs engine-rs
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/engine-rs/target \
    cd engine-rs && cargo build --release --locked --bin openfront-video \
 && mkdir -p /out && cp target/release/openfront-video /out/
COPY web web
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/web/target \
    cd web && cargo build --release --locked \
 && cp target/release/openfront-web /out/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --uid 10001 openfront \
 && mkdir /data && chown openfront /data
COPY --from=build /out/openfront-video /out/openfront-web /usr/local/bin/
USER openfront
ENV PORT=8080 \
    DATA_DIR=/data \
    PUBLIC_URL=https://openfront.video \
    OPENFRONT_VIDEO_BIN=/usr/local/bin/openfront-video \
    MAX_RENDERS=1
VOLUME /data
EXPOSE 8080
CMD ["openfront-web"]
