# 在低版本 glibc (2.31) 环境编译，保证二进制同时兼容 deepin V23 与 V25
# 构建命令： docker build -t lithescape-builder .
FROM rust:1.83-bullseye

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential perl pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

RUN cargo build --release

RUN mkdir -p /out \
    && cp target/release/lithescape /out/lithescape \
    && strip /out/lithescape || true

CMD ["/bin/bash"]
