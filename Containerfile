FROM docker.io/library/debian:bullseye AS build

RUN --mount=type=cache,target=/var/cache/apt,id=apt-cache-bullseye \
    --mount=type=cache,target=/var/lib/apt/lists,id=apt-lists-bullseye \
    apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    build-essential \
    pkg-config \
    gcc-x86-64-linux-gnu \
    gcc-aarch64-linux-gnu \
    libc6-dev-arm64-cross

ENV CARGO_HOME=/usr/local/cargo RUSTUP_HOME=/usr/local/rustup
ENV PATH="/usr/local/cargo/bin:${PATH}"

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path

RUN rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu

ENV CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

WORKDIR /app

# Cargo.lock is only committed on release commits, hence the glob
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# The .deb's glibc floor is read off each binary rather than guessed: it moves
# with the base image, and too low a floor dies at exec on a symbol version.
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git \
    --mount=type=cache,target=/app/target,id=cargo-target \
    set -eu; \
    for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do \
        cargo build --release --target "$target" --bin bcachefs-updatedb; \
    done; \
    mkdir /out; \
    cp target/x86_64-unknown-linux-gnu/release/bcachefs-updatedb /out/bcachefs-updatedb.amd64; \
    cp target/aarch64-unknown-linux-gnu/release/bcachefs-updatedb /out/bcachefs-updatedb.arm64; \
    for arch in amd64 arm64; do \
        readelf -W --dyn-syms "/out/bcachefs-updatedb.$arch" \
          | sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' \
          | sort -Vu | tail -1 > "/out/glibc-floor.$arch"; \
        if readelf -Wd "/out/bcachefs-updatedb.$arch" | grep -q libzstd; then \
            echo "libzstd linked dynamically; add libzstd1 to Depends" >&2; exit 1; \
        fi; \
    done

FROM scratch
COPY --from=build /out/ /
