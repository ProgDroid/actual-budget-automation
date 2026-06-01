# --- Rust build stage ---
FROM rust:1-slim AS rust-build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin aba

# --- Node deps stage ---
FROM node:22-slim AS node-deps
WORKDIR /app/actual-shim
COPY actual-shim/package.json actual-shim/package-lock.json ./
RUN npm ci --omit=dev

# --- Runtime ---
FROM node:22-slim AS runtime
WORKDIR /app
COPY --from=rust-build /build/target/release/aba /usr/local/bin/aba
COPY actual-shim/index.js actual-shim/package.json /app/actual-shim/
COPY --from=node-deps /app/actual-shim/node_modules /app/actual-shim/node_modules
ENV ABA_SHIM_DIR=/app/actual-shim
ENV ACTUAL_DATA_DIR=/data/actual-cache
USER node
ENTRYPOINT ["aba"]
CMD ["run"]
