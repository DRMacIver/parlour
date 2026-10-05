# syntax=docker/dockerfile:1
#
# Parlour's image (DESIGN.md §10): one static binary, with the templates,
# the stylesheet and the JavaScript islands embedded, in an image that
# holds nothing else but the CA certificates its SMTP connections check
# against. Build it from the repository's root:
#
#     docker build -t parlour .
#
# deploy/compose.yaml builds and runs it; see docs/admin-guide.md.

# The islands, which the server embeds (web/dist).
FROM node:26-alpine AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# The server, linked statically against musl. Queries are checked against
# the committed server/.sqlx, so no database is needed to build.
FROM rust:1.99-alpine AS server
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY rust-toolchain.toml ./
COPY server/ server/
COPY --from=web /src/web/dist web/dist
WORKDIR /src/server
ENV SQLX_OFFLINE=true
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/server/target \
    cargo build --release --locked \
    && cp target/release/parlour /parlour
# Where uploads go, owned by the user the server runs as, so that a new
# volume mounted there starts out theirs.
RUN mkdir -p /data/uploads

FROM scratch
COPY --from=server /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=server /parlour /parlour
COPY --from=server --chown=65532:65532 /data /data
USER 65532:65532
ENV PARLOUR_BIND=0.0.0.0:8080 \
    PARLOUR_UPLOADS_DIR=/data/uploads \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
EXPOSE 8080
ENTRYPOINT ["/parlour"]
CMD ["serve"]
