ARG NODE_RUNTIME_IMAGE=node:22-bookworm-slim
FROM ${NODE_RUNTIME_IMAGE}
ARG PNPM_VERSION=10.34.5

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git procps \
    && npm install --global pnpm@${PNPM_VERSION} \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /srv/nuwax-file-server
COPY package.json pnpm-lock.yaml ./
RUN pnpm install --prod --frozen-lockfile
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
