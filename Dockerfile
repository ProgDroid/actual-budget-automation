# --- Rust build stage ---
# Digest resolved 2026-06-02 via: docker buildx imagetools inspect rust:1-slim
FROM rust:1-slim@sha256:26abcef3d79b8d890c4ceb17093154573e1f6479cf6dd7c1450043b8458350f6 AS rust-build
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
