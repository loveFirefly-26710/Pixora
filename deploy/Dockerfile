# Pixora 应用镜像。
#
# 多阶段构建：构建阶段装 musl 目标并编译出静态链接的二进制，
# 运行阶段用 scratch —— 最终镜像 20MB 上下，冷启动毫秒级。
#
# 本机不需要装 musl 目标：本地开发用 msvc/gnu 目标跑 `cargo run`，
# 出镜像时才进容器编 musl（交叉编译交给 Docker）。
#
# 架构按构建机自身决定（`uname -m`）：这与 `docker buildx --platform` 的
# 语义一致——buildx 会把整个构建阶段放到目标架构上跑（原生运行器上就是原生，
# 加了 QEMU 才是模拟）。所以同一个 Dockerfile 既能出 amd64 也能出 arm64 镜像：
#
#   docker buildx build --platform linux/amd64,linux/arm64 -f deploy/Dockerfile .
#
# 用 `uname -m` 而不是 buildx 注入的 `TARGETARCH`：前者在 `docker build`、
# `docker compose build`、多平台 buildx 三种入口下都成立。

# ---------- 构建阶段 ----------
FROM rust:1.98 AS build

# musl 目标 + 前端构建工具。架构名对齐 rustc 的三元组后缀
RUN set -eu; \
    case "$(uname -m)" in \
      x86_64)  MUSL=x86_64-unknown-linux-musl ;; \
      aarch64) MUSL=aarch64-unknown-linux-musl ;; \
      *) echo "不支持的架构：$(uname -m)（只出了 x86_64 与 aarch64 两套）" >&2; exit 1 ;; \
    esac; \
    echo "$MUSL" > /tmp/musl-target; \
    rustup target add "$MUSL"; \
    apt-get update && apt-get install -y --no-install-recommends \
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
# 想把这份镜像里的前端指向别的后端时，构建时给这个参数（默认同源）
ARG PIXORA_API_BASE=""
RUN cd web && PIXORA_API_BASE="$PIXORA_API_BASE" npm run build

RUN cargo build --release --target "$(cat /tmp/musl-target)"

# ---------- 运行阶段 ----------
FROM scratch

LABEL org.opencontainers.image.title="Pixora" \
      org.opencontainers.image.description="自托管的图片托管服务：单二进制、内容寻址、低内存占用" \
      org.opencontainers.image.licenses="MIT"

COPY --from=build /src/target/*/release/pixora /pixora
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
