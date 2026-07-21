# Go 应用运行时
ARG BASE_IMAGE=app-runtime-base:latest
FROM ${BASE_IMAGE}

ENV GO_VERSION=1.26.4

# 安装 Go
RUN ARCH=$(dpkg --print-architecture) && \
    wget -q https://go.dev/dl/go${GO_VERSION}.linux-${ARCH}.tar.gz -O /tmp/go.tar.gz \
    && tar -C /usr/local -xzf /tmp/go.tar.gz \
    && rm /tmp/go.tar.gz
ENV PATH="/usr/local/go/bin:${PATH}"
ENV GOPATH="/root/go"
ENV PATH="${GOPATH}/bin:${PATH}"

# 暴露端口
EXPOSE 8080 7681

# 启动命令
CMD ["/usr/local/bin/start-ttyd.sh"]
