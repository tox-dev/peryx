#!/usr/bin/env bash
set -euo pipefail

package=${1:?Rust package to benchmark}
jobs=${2:-4}

mkdir -p .tox/codspeed/cargo
if [[ -n ${GITHUB_EVENT_PATH:-} ]]; then
  PERYX_CODSPEED_EVENT_PATH=$GITHUB_EVENT_PATH
else
  PERYX_CODSPEED_EVENT_PATH=$PWD/.tox/codspeed/event.json
  printf '{}\n' > "$PERYX_CODSPEED_EVENT_PATH"
fi
export PERYX_CODSPEED_EVENT_PATH

if [[ -n ${PERYX_CODSPEED_REGISTRY_TOKEN:-} ]]; then
  DOCKER_CONFIG=$(mktemp -d)
  export DOCKER_CONFIG
  trap 'rm -rf "$DOCKER_CONFIG"' EXIT
  printf '%s' "$PERYX_CODSPEED_REGISTRY_TOKEN" | docker login ghcr.io \
    --username "${PERYX_CODSPEED_REGISTRY_ACTOR:?registry actor is required}" \
    --password-stdin
fi

if [[ -z ${PERYX_CODSPEED_IMAGE:-} ]]; then
  docker compose --profile codspeed build codspeed
fi
docker compose --profile codspeed run --rm codspeed "$package" "$jobs"
