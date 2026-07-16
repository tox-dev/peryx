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
git_common=$(git rev-parse --path-format=absolute --git-common-dir)
cd "$root"
if command -v sha256sum >/dev/null; then hash=(sha256sum); else hash=(shasum -a 256); fi
definition=$("${hash[@]}" .github/codspeed/Dockerfile | cut -d ' ' -f 1)
source_inputs=(
  .cargo
  .github/codspeed
  .github/workflows/codspeed.yml
  ci/run-codspeed-local.sh
  ci/run-codspeed.sh
  crates
  Cargo.lock
  Cargo.toml
  rust-toolchain.toml
)
source_key=$(
  {
    git ls-files -s -- "${source_inputs[@]}"
    git diff --binary HEAD -- "${source_inputs[@]}"
    git ls-files --others --exclude-standard -z -- "${source_inputs[@]}" | xargs -0 "${hash[@]}"
  } | "${hash[@]}" | cut -d ' ' -f 1
)
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
  --security-opt seccomp=unconfined
  --env CODSPEED_OAUTH_TOKEN
  --env CODSPEED_SKIP_UPLOAD
  --env CODSPEED_FORCE_REBUILD=true
  --env "CODSPEED_SOURCE_KEY=$source_key"
  --env CARGO_PROFILE_RELEASE_LTO=false
  --env GLIBC_TUNABLES=glibc.cpu.name=generic:glibc.malloc.arena_max=1
  --env XDG_CONFIG_HOME=/codspeed-config
  --volume "$config_volume:/codspeed-config"
  --volume "$git_common:$git_common:ro"
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
