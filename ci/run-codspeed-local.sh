#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 login | peryx-ecosystem-oci | peryx-ecosystem-pypi" >&2
}

case "${1:-}" in
  -h | --help) usage; exit ;;
  login | peryx-ecosystem-oci | peryx-ecosystem-pypi) command=$1 ;;
  *) usage; exit 2 ;;
esac

root=$(git rev-parse --show-toplevel)
cd "$root"
if command -v sha256sum >/dev/null; then
  definition=$(sha256sum .github/codspeed/Dockerfile | cut -d ' ' -f 1)
else
  definition=$(shasum -a 256 .github/codspeed/Dockerfile | cut -d ' ' -f 1)
fi
registry_image=ghcr.io/tox-dev/peryx-codspeed
tag="$registry_image:definition-$definition"
digest=$(docker buildx imagetools inspect "$tag" --format '{{.Manifest.Digest}}' 2>/dev/null || true)
if [[ "$digest" == sha256:* ]]; then
  image="$registry_image@$digest"
else
  image="peryx-codspeed:definition-$definition"
  docker buildx build \
    --file .github/codspeed/Dockerfile \
    --load \
    --platform linux/arm64 \
    --tag "$image" \
    .
fi

config_volume=peryx-codspeed-config
target_volume="peryx-codspeed-target-${definition:0:12}"
docker volume create "$config_volume" >/dev/null
docker volume create "$target_volume" >/dev/null
tty=()
if [[ -t 0 && -t 1 ]]; then tty=(-it); fi

container=(
  docker run "${tty[@]}" --rm
  --platform linux/arm64
  --env CODSPEED_OAUTH_TOKEN
  --env CARGO_PROFILE_RELEASE_LTO=thin
  --env GLIBC_TUNABLES=glibc.cpu.name=generic:glibc.malloc.arena_max=1
  --env XDG_CONFIG_HOME=/codspeed-config
  --volume "$config_volume:/codspeed-config"
  --volume "$target_volume:/__w/peryx/peryx/target"
  --volume "$root:/__w/peryx/peryx"
  --workdir /__w/peryx/peryx
  "$image"
)
if [[ "$command" == login ]]; then
  "${container[@]}" codspeed auth login
else
  "${container[@]}" ci/run-codspeed.sh "$command"
fi
