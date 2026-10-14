FROM rust:1.96-slim-trixie AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release

FROM debian:trixie-slim
# CA certs: reqwest loads them at startup, and webhooks go to businesses' https URLs.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/dodo_payments /app/target/release/mock_psp /usr/local/bin/
EXPOSE 8080
CMD ["dodo_payments"]
