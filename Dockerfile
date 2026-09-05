# syntax=docker/dockerfile:1
FROM docker.io/library/rust:1.97.0-alpine3.24@sha256:ec9c91e77119ce498cd1e87d96d77e0f75b2cee21655a29bc2bf75a51a2b20a4 AS builder

ARG CARGO_BUILD_FLAGS="--locked --release"
ARG STABBUR_BUILD_GIT_SHA="unknown"

WORKDIR /usr/src/stabbur

RUN apk add --no-cache build-base cmake perl

# Keep these manifest-only copies in exact parity with Cargo workspace members.
# scripts/check-docker-manifests.py enforces that invariant in CI.
COPY Cargo.toml Cargo.lock ./
COPY crates/stabbur-auth-core/Cargo.toml ./crates/stabbur-auth-core/Cargo.toml
COPY crates/stabbur-builder-autopkg/Cargo.toml ./crates/stabbur-builder-autopkg/Cargo.toml
COPY crates/stabbur-builder-core/Cargo.toml ./crates/stabbur-builder-core/Cargo.toml
COPY crates/stabbur-domain/Cargo.toml ./crates/stabbur-domain/Cargo.toml
COPY crates/stabbur-jobs-core/Cargo.toml ./crates/stabbur-jobs-core/Cargo.toml
COPY crates/stabbur-storage-conformance/Cargo.toml ./crates/stabbur-storage-conformance/Cargo.toml
COPY crates/stabbur-storage-core/Cargo.toml ./crates/stabbur-storage-core/Cargo.toml
COPY crates/stabbur-storage-runtime/Cargo.toml ./crates/stabbur-storage-runtime/Cargo.toml
COPY crates/stabbur-storage-sqlite/Cargo.toml ./crates/stabbur-storage-sqlite/Cargo.toml
COPY crates/stabbur-store-core/Cargo.toml ./crates/stabbur-store-core/Cargo.toml
COPY crates/stabbur-store-fs/Cargo.toml ./crates/stabbur-store-fs/Cargo.toml

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/stabbur/target \
    mkdir -p src && \
    printf '//! Dependency-cache placeholder.\n\n/// Build-only placeholder.\npub fn placeholder() {}\n' > src/lib.rs && \
    printf 'fn main() {}\n' > src/main.rs && \
    find crates -mindepth 2 -maxdepth 2 -name Cargo.toml \
      -exec sh -c 'mkdir -p "$(dirname "$1")/src"; printf "//! Dependency-cache placeholder.\\n\\n/// Build-only placeholder.\\npub fn placeholder() {" > "$(dirname "$1")/src/lib.rs"; printf "}\\n" >> "$(dirname "$1")/src/lib.rs"' sh {} \; && \
    cargo build ${CARGO_BUILD_FLAGS} --bin stabbur-server && \
    rm -rf src && \
    find crates -mindepth 2 -maxdepth 2 -type d -name src -exec rm -rf {} +

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/stabbur/target \
    STABBUR_BUILD_GIT_SHA="${STABBUR_BUILD_GIT_SHA}" \
    find src crates -path '*/src/*' -type f -exec touch {} + && \
    cargo build ${CARGO_BUILD_FLAGS} --bin stabbur-server && \
    cp target/release/stabbur-server /tmp/stabbur-server && \
    strip /tmp/stabbur-server

FROM scratch AS release-artifacts

COPY --from=builder /tmp/stabbur-server /stabbur-server

FROM docker.io/library/alpine:3.24.1@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b

ARG STABBUR_UID="10001"
ARG STABBUR_GID="10001"

RUN apk add --no-cache ca-certificates && \
    addgroup -S -g "${STABBUR_GID}" stabbur && \
    adduser -S -D -H -u "${STABBUR_UID}" -G stabbur stabbur && \
    mkdir -p /var/lib/stabbur && \
    chown stabbur:stabbur /var/lib/stabbur

COPY --from=builder /tmp/stabbur-server /usr/local/bin/stabbur-server
COPY entrypoint.sh /entrypoint.sh

ENV STABBUR_BIND="0.0.0.0:8080" \
    STABBUR_DATA_DIR="/var/lib/stabbur" \
    STABBUR_STORAGE_BACKEND="sqlite"

EXPOSE 8080
VOLUME ["/var/lib/stabbur"]
USER stabbur:stabbur
ENTRYPOINT ["/entrypoint.sh"]
