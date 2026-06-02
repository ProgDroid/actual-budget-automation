# --- Rust build stage ---
# Pinned to -bookworm so the build glibc matches the node:22-slim runtime (Debian
# bookworm, glibc 2.36). Bare `rust:1-slim` tracks the latest Debian (trixie,
# glibc 2.39), which links symbols the runtime libc lacks: `aba: ... version
# `GLIBC_2.39' not found`. Keep both stages on the same Debian codename.
# Digest resolved 2026-06-02 via: docker buildx imagetools inspect rust:1-slim-bookworm
FROM rust:1-slim-bookworm@sha256:b5f842fac1e3b4ff718a652a8e0173b62d9403ec826ef4998880b9347db30684 AS rust-build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin aba

# --- Node deps stage ---
# Digest resolved 2026-06-02 via: docker buildx imagetools inspect node:22-slim
FROM node:22-slim@sha256:7af03b14a13c8cdd38e45058fd957bf00a72bbe17feac43b1c15a689c029c732 AS node-deps
WORKDIR /app/actual-shim
COPY actual-shim/package.json actual-shim/package-lock.json ./
RUN npm ci --omit=dev

# --- Runtime ---
FROM node:22-slim@sha256:7af03b14a13c8cdd38e45058fd957bf00a72bbe17feac43b1c15a689c029c732 AS runtime
WORKDIR /app
COPY --from=rust-build /build/target/release/aba /usr/local/bin/aba
COPY actual-shim/index.js actual-shim/package.json /app/actual-shim/
COPY --from=node-deps /app/actual-shim/node_modules /app/actual-shim/node_modules
ENV ABA_SHIM_DIR=/app/actual-shim
ENV ACTUAL_DATA_DIR=/data/actual-cache
USER node
ENTRYPOINT ["aba"]
CMD ["run"]
