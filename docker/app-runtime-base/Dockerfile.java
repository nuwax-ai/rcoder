# Java 应用运行时
ARG BASE_IMAGE=app-runtime-base:latest
FROM ${BASE_IMAGE}

# 安装 Java 17
RUN apt-get update && apt-get install -y \
    openjdk-17-jdk \
    maven \
    gradle \
    && rm -rf /var/lib/apt/lists/* \
    && ARCH=$(dpkg --print-architecture) \
    && ln -s /usr/lib/jvm/java-17-openjdk-${ARCH} /usr/lib/jvm/java-17-openjdk-current

ENV JAVA_HOME=/usr/lib/jvm/java-17-openjdk-current

# 安装 Arthas (Java 诊断工具)
RUN curl -fsSL https://arthas.aliyun.com/arthas-boot.jar -o /usr/local/bin/arthas-boot.jar \
    && chmod +x /usr/local/bin/arthas-boot.jar \
    && echo '#!/bin/bash' > /usr/local/bin/arthas \
    && echo 'java -jar /usr/local/bin/arthas-boot.jar "$@"' >> /usr/local/bin/arthas \
    && chmod +x /usr/local/bin/arthas

# 暴露端口
EXPOSE 8080 7681

# 启动命令
CMD ["/usr/local/bin/start-ttyd.sh"]
