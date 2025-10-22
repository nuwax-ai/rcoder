# 多阶段构建：编译阶段 + 运行阶段
FROM rust:1.90 AS builder

# 设置工作目录
WORKDIR /app

# 安装编译依赖
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    clang \
    cmake \
    build-essential

# 复制源码
COPY . .

# 预编译依赖，只编译 rcoder binary
RUN cargo build --release --bin rcoder --target-dir /app/target

# 轻量级运行时镜像
FROM debian:bookworm-slim

# 安装运行时依赖
RUN apt-get update && apt-get install -y \
    curl \
    jq \
    libssl3 \
    ca-certificates \
    bash \
    curl \
    build-essential \
    pkg-config \
    libssl-dev



# 创建应用用户和目录
RUN useradd -m -u 1000 rcoder && \
    mkdir -p /app/logs /app/workspace

# 从 builder 阶段复制编译好的二进制和启动脚本
COPY --from=builder /app/target/release/rcoder /usr/local/bin/
COPY --from=builder /app/docker/start-rcoder.sh /app/start-rcoder.sh
RUN chmod +x /app/start-rcoder.sh

# 设置环境变量
ENV RUST_LOG=info
ENV PORT=8087
ENV DOCKER_SOCKET_PATH=/var/run/docker.sock
ENV RCODER_WORKSPACE=/app/workspace
ENV RCODER_LOGS=/app/logs

# 暴露端口
EXPOSE 8087

# 健康检查
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD curl -f http://localhost:8087/health || exit 1

# 使用 Command 启动服务
CMD ["/bin/bash", "/app/start-rcoder.sh"]
