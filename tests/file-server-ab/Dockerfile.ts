FROM toolchain
ARG PNPM_VERSION=10.34.5
ARG PNPM_REGISTRY=https://registry.npmmirror.com
ARG PNPM_NETWORK_CONCURRENCY=32

WORKDIR /srv/nuwax-file-server
COPY package.json pnpm-lock.yaml ./
RUN --mount=type=cache,id=file-server-ab-ts-pnpm-${PNPM_VERSION},target=/pnpm-cache,sharing=locked \
    npm_config_store_dir=/pnpm-cache pnpm install --prod --frozen-lockfile \
      --prefer-offline --registry="${PNPM_REGISTRY}" \
      --network-concurrency="${PNPM_NETWORK_CONCURRENCY}"
COPY . .

ENV NODE_ENV=ab \
    PORT=60000 \
    PROJECT_SOURCE_DIR=/data/project-workspace \
    COMPUTER_WORKSPACE_DIR=/data/computer-workspace \
    USERAPP_WORKSPACE_DIR=/data/userapp-workspace \
    INIT_PROJECT_DIR=/data/init-project \
    UPLOAD_PROJECT_DIR=/data/project-zips \
    DIST_TARGET_DIR=/data/project-nginx \
    LOG_BASE_DIR=/data/logs/project \
    COMPUTER_LOG_DIR=/data/logs/computer \
    FILE_SERVER_LOG_DIR=/data/logs/file-server \
    TEMPLATE_CACHE_DIR=/data/cache/templates \
    NODE_MODULES_LOCAL_DIR=/data/cache/node-modules \
    GIT_ENABLED=true \
    GIT_USE_NATIVE=true \
    GIT_DEFAULT_AUTHOR_NAME="File Server A-B" \
    GIT_DEFAULT_AUTHOR_EMAIL=ab@example.invalid \
    DEPLOYMENT_MODE=docker-compose \
    PNPM_PRUNE_ENABLED=false

EXPOSE 60000
CMD ["node", "src/server.js"]
