FROM rust:1.86.0
WORKDIR /app
COPY . .
RUN cargo build --release
RUN ls -la target/release
CMD ["./target/release/wraith-bot"]
