FROM rust:1.71.0 as builder
WORKDIR /tmp
COPY Cargo.toml ./
COPY Cargo.lock ./
COPY src src
RUN cargo build --locked --profile release --package clickhouse_ndc

# todo: figure out how to get rid of dependency libssl.so.1.1
# so we can use multistage builds for a smaller image
# unable to determine where the dependency comes from,
# this may be somewhere upstream?

ENTRYPOINT [ "/tmp/target/release/clickhouse_ndc", "serve", "--configuration", "/config.json"]
