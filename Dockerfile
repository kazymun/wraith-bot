FROM rust:latest
WORKDIR /app
COPY . .
RUN cargo update
RUN cargo build --release
CMD ["./target/release/wraith-bot"]
