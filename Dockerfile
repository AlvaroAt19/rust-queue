FROM rust:1.71-alpine

WORKDIR /app

COPY . .

RUN apk add musl-dev

RUN cargo build --release

CMD ./target/release/rust-queue