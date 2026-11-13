# Build inside Debian: an Arch glibc-linked executable is not a Debian binary.
FROM rust:1.95.0-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends dpkg-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked --no-default-features -p saiku-cli && \
    cargo test --locked -p saiku-core -p saiku-dsh -p saiku-tui -p saiku-cli --features saiku-cli/test-runtime
CMD ["bash", "packaging/debian/build.sh", "/src/target/release/saiku", "/out"]
