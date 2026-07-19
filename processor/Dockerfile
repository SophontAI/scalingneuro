FROM debian:bookworm-slim AS dcm2niix-build

ARG DCM2NIIX_VERSION=1.0.20260416
ARG DCM2NIIX_SHA256=dc87a34b8284df2700a5aee433c4ba7ea56b999ac774fcf684962de5e898670d

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates cmake curl git libopenjp2-7-dev pkg-config zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN curl -fsSLo source.tar.gz "https://github.com/rordenlab/dcm2niix/archive/refs/tags/v${DCM2NIIX_VERSION}.tar.gz" \
    && echo "${DCM2NIIX_SHA256}  source.tar.gz" | sha256sum -c - \
    && tar -xzf source.tar.gz --strip-components=1 \
    && cmake -S . -B build \
        -DCMAKE_BUILD_TYPE=Release \
        -DUSE_STATIC_RUNTIME=OFF \
        -DUSE_OPENJPEG=System \
        -DUSE_JPEGLS=ON \
        -DUSE_JNIFTI=OFF \
        -DZLIB_IMPLEMENTATION=System \
    && cmake --build build --parallel \
    && build/bin/dcm2niix --version | grep -F "v${DCM2NIIX_VERSION}"

FROM python:3.12.11-slim-bookworm

LABEL org.opencontainers.image.title="Scaling Neuro processor" \
      org.opencontainers.image.version="0.1.1" \
      org.opencontainers.image.source="https://github.com/SophontAI/scalingneuro"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libopenjp2-7 tini zlib1g zstd \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 65532 processor \
    && useradd --uid 65532 --gid 65532 --no-create-home --home-dir /nonexistent processor
COPY --from=dcm2niix-build /build/build/bin/dcm2niix /usr/local/bin/dcm2niix
WORKDIR /app
COPY processor/requirements.lock ./requirements.lock
RUN python -m pip install --no-cache-dir --require-hashes -r requirements.lock
COPY processor/scaling_neuro_processor ./scaling_neuro_processor
COPY processor/scripts/entrypoint.sh ./scripts/entrypoint.sh
COPY docs/dcm2niix-license.txt ./THIRD-PARTY-LICENSES/dcm2niix-license.txt
ENV PYTHONPATH=/app
RUN dcm2niix --version | grep -F 'v1.0.20260416' \
    && chmod 0555 /app/scripts/entrypoint.sh

USER 65532:65532
ENTRYPOINT ["/usr/bin/tini", "--", "/app/scripts/entrypoint.sh"]
