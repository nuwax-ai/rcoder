// Shared dependency stays inside BuildKit; only runnable images are exported.
variable "RCODER_ROOT" { default = "" }
variable "AB_TS_CONTEXT" { default = "" }
variable "AB_RUST_IMAGE_REF" { default = "" }
variable "AB_TS_IMAGE_REF" { default = "" }
variable "AB_RUST_BUILDER_IMAGE" { default = "" }
variable "AB_NODE_IMAGE_DIGEST" { default = "" }
variable "AB_PNPM_VERSION" { default = "" }
variable "AB_PNPM_REGISTRY" { default = "" }
variable "AB_PNPM_NETWORK_CONCURRENCY" { default = "" }
variable "AB_UID" { default = "" }
variable "AB_GID" { default = "" }

group "default" { targets = ["rust", "typescript"] }
target "toolchain" {
  context = "${RCODER_ROOT}/tests/file-server-ab"
  dockerfile = "Dockerfile.base"
  args = {
    RUST_BUILDER_IMAGE = AB_RUST_BUILDER_IMAGE
    NODE_RUNTIME_IMAGE = AB_NODE_IMAGE_DIGEST
    PNPM_VERSION = AB_PNPM_VERSION
    PNPM_REGISTRY = AB_PNPM_REGISTRY
    PNPM_NETWORK_CONCURRENCY = AB_PNPM_NETWORK_CONCURRENCY
    AB_UID = AB_UID
    AB_GID = AB_GID
  }
  output = ["type=cacheonly"]
}
target "rust" {
  context = RCODER_ROOT
  dockerfile = "tests/file-server-ab/Dockerfile.rust"
  contexts = { toolchain = "target:toolchain" }
  tags = [AB_RUST_IMAGE_REF]
  output = ["type=docker"]
}
target "typescript" {
  context = AB_TS_CONTEXT
  dockerfile = "Dockerfile"
  contexts = { toolchain = "target:toolchain" }
  args = {
    PNPM_VERSION = AB_PNPM_VERSION
    PNPM_REGISTRY = AB_PNPM_REGISTRY
    PNPM_NETWORK_CONCURRENCY = AB_PNPM_NETWORK_CONCURRENCY
  }
  tags = [AB_TS_IMAGE_REF]
  output = ["type=docker"]
}
