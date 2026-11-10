# Builds the gateway and the uploader. The program itself is deployed with the Solana CLI, not here.
FROM rust:1-bookworm AS build
WORKDIR /src/tools
COPY tools/ ./
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home app
# the launchpad runs `upload` from next to the gateway binary
COPY --from=build /src/tools/target/release/gateway /src/tools/target/release/upload /app/
WORKDIR /app
USER app
ENV PORT=8080
EXPOSE 8080
CMD ["/app/gateway"]
