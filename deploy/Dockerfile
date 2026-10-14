# Pixora 应用镜像。
#
# 多阶段构建：构建阶段装 musl 目标并编译出静态链接的二进制，
# 运行阶段用 scratch —— 最终镜像 20MB 上下，冷启动毫秒级。
#
# 本机不需要装 musl 目标：本地开发用 msvc/gnu 目标跑 `cargo run`，
# 出镜像时才进容器编 musl（交叉编译交给 Docker）。

# ---------- 构建阶段 ----------
FROM rust:1.98 AS build

# musl 目标 + 前端构建工具
RUN rustup target add x86_64-unknown-linux-musl \
    && apt-get update && apt-get install -y --no-install-recommends \
       musl-tools nodejs npm \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# 先拷清单与锁文件，让依赖层能被缓存下来
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
COPY web/package.json web/package-lock.json ./web/
RUN cd web && npm ci

# 再拷源码（前端源码变化不影响依赖层的缓存）
COPY web ./web
RUN cd web && npm run build

RUN cargo build --release --target x86_64-unknown-linux-musl

# ---------- 运行阶段 ----------
FROM scratch

COPY --from=build /src/target/x86_64-unknown-linux-musl/release/pixora /pixora
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

ENV TZ=Asia/Shanghai \
    PIXORA_DATA_DIR=/data \
    PIXORA_BIND=0.0.0.0:8080 \
    RUST_LOG=info

# 数据目录在 compose 里挂载；这里声明一下，让 `docker run` 也知道该挂哪
VOLUME /data
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/pixora", "--health-probe"]

ENTRYPOINT ["/pixora"]
