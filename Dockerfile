# syntax=docker/dockerfile:1.7
# Selvedge: one image for every role. workerd is the official release from npm,
# checked against its published sha512 integrity hash.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p selvedge && cp target/release/selvedge /selvedge

FROM debian:bookworm-slim AS workerd
ARG TARGETARCH
ARG WORKERD_VERSION=1.20261106.1
ARG WORKERD_SHA512_AMD64=yRAKuderf90qBO/K8r6uIwVdNo5Akh2Fv9a6KPbiSABMCXdBR7zkF+fCG/dbaHqs9YXMbIhroJmcqRZUiMpHSQ==
ARG WORKERD_SHA512_ARM64=QkXgRV35gihA75nRlLPzcSGPHt86wTJS9jPIZbTGGgG/V1Qh4psc8zLztnzZMLEBPPnr7aAb6S7gseH9c/i4kw==
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) pkg=linux-64; sum=$WORKERD_SHA512_AMD64 ;; \
      arm64) pkg=linux-arm64; sum=$WORKERD_SHA512_ARM64 ;; \
      *) echo "unsupported architecture $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /w.tgz "https://registry.npmjs.org/@cloudflare/workerd-$pkg/-/workerd-$pkg-$WORKERD_VERSION.tgz"; \
    echo "$sum" | base64 -d | od -An -tx1 | tr -d ' \n' > /expected; \
    sha512sum /w.tgz | cut -d' ' -f1 | tr -d '\n' > /actual; \
    cmp -s /expected /actual || { echo "workerd checksum mismatch" >&2; exit 1; }; \
    tar -xzf /w.tgz -C / package/bin/workerd; mv /package/bin/workerd /workerd

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --home /var/lib/selvedge selvedge
COPY --from=workerd /workerd /usr/local/bin/workerd
COPY --from=build /selvedge /usr/local/bin/selvedge
ENV SELVEDGE_WORKERD=/usr/local/bin/workerd \
    SELVEDGE_STATE_DIR=/var/lib/selvedge \
    SELVEDGE_RUN_DIR=/run/selvedge \
    SELVEDGE_LOG_JSON=true
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/selvedge"]
