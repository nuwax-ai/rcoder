#!/usr/bin/env bash
set -euo pipefail

ts_source="${AB_TS_SOURCE:?AB_TS_SOURCE must point at nuwax-file-server}"
ts_ref="${AB_TS_REF:-HEAD}"
context="${AB_TS_CONTEXT:?AB_TS_CONTEXT must point at the temporary build context}"
repo_root="${RCODER_ROOT:?RCODER_ROOT must point at rcoder}"

if [[ ! -f "${ts_source}/src/server.js" ]]; then
  echo "TS server source not found: ${ts_source}/src/server.js" >&2
  exit 1
fi
resolved="$(git -C "${ts_source}" rev-parse "${ts_ref}^{commit}")"
rm -rf -- "${context}"
mkdir -p -- "${context}"
git -C "${ts_source}" archive --format=tar "${resolved}" | tar -xf - -C "${context}"

# The app requires src/env.<NODE_ENV>. Generate an empty, test-only env file;
# all non-secret values are injected by Compose. The archive contains tracked
# files only, so local .env files and untracked workspace content never enter it.
mkdir -p "${context}/src"
: > "${context}/src/env.ab"
cp "${repo_root}/tests/file-server-ab/Dockerfile.ts" "${context}/Dockerfile"
cat > "${context}/.dockerignore" <<'EOF'
.git
.env*
**/.env*
**/node_modules
src/env.development
src/env.production
src/env.test
src/env.*
!src/env.ab
logs
dist
EOF

printf '%s\n' "${resolved}" > "${context}/.ab-source-revision"
echo "Prepared tracked TS source ${resolved} at ${context}"
