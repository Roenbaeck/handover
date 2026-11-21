#!/usr/bin/env bash
# Build an air-gap bundle on the connected side (PLAN.md §16):
#   1. vendor every crate so the enclave can rebuild offline;
#   2. build the linux/arm64 image (Positorium compiles from source in the builder);
#   3. pull and pin the router image;
#   4. save everything into one zstd-compressed tarball.
# Usage: deploy/bundle.sh [output.tar.zst]
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-handover-bundle-$(date +%Y%m%d).tar.zst}"
router="${VLLM_ROUTER_IMAGE:-handover/vllm-router:bundle}"
platform="${PLATFORM:-linux/arm64}"

echo "==> vendoring crates"
cargo vendor --locked vendor > /dev/null

echo "==> building handover:bundle for $platform"
docker buildx build --platform "$platform" -f deploy/Dockerfile -t handover:bundle --load .

if [ -n "${VLLM_ROUTER_IMAGE:-}" ]; then
  echo "==> pulling $router for $platform"
  docker pull --platform "$platform" "$router"
else
  echo "==> building $router for $platform from github.com/vllm-project/router"
  docker buildx build --platform "$platform" -t "$router" --load \
    -f docker/Dockerfile.router https://github.com/vllm-project/router.git
fi
digest="$(docker image inspect --format '{{index .RepoDigests 0}}' "$router" 2>/dev/null || echo "$router")"
echo "    router pinned at $digest"

echo "==> smoke test under emulation"
docker run --rm --platform "$platform" --entrypoint handover-proxy handover:bundle --version

echo "==> saving to $out"
docker save handover:bundle "$router" | zstd -T0 -19 -o "$out"
tar -czf "${out%.tar.zst}-source.tgz" --exclude=target --exclude=spike/target --exclude=.git .
echo "done: $out (images) and ${out%.tar.zst}-source.tgz (vendored source)"
echo "inside the enclave: zstd -dc $out | docker load"
