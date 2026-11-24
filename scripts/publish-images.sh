#!/usr/bin/env bash
# Build, transfer and pin the images used by the Kubernetes control plane.
#
#   ./scripts/publish-images.sh                          # list what would be built
#   ./scripts/publish-images.sh --build                  # build local images
#   ./scripts/publish-images.sh --save                   # build + docker save tarballs
#   ./scripts/publish-images.sh --load                   # docker load from tarballs
#   ./scripts/publish-images.sh --registry localhost:5000 --push --pin
#
# The lab has no registry: --save/--load is the explicit alternative to a
# kubelet image GC removing local-only tags (the 2026-11-18 rollout failure).
# --pin renders the production overlay's image patch from real repo digests.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="$ROOT/dist/images"
VERSION="$(python3 -c 'import json;print(json.load(open("trading_terminal/package.json"))["version"])')"
PLATFORM="${PLATFORM:-linux/arm64}"
BUILD=0; SAVE=0; LOAD=0; PUSH=0; PIN=0
REGISTRY=""

IMAGES=(
  "matchengine-k8s-control-plane|k8s_control_plane/Dockerfile|k8s_control_plane|"
  "matchengine-trading-terminal|trading_terminal/Dockerfile|trading_terminal|"
  "matchengine-raft|match_engine/docker/k8s.Dockerfile|match_engine|CARGO_FEATURES=market-data-nats"
  "matchengine-marketdata|market_data_system/docker/k8s-history.Dockerfile|.|_HISTORY=1"
  "matchengine-raft-demo|order_system/docker/k8s.Dockerfile|order_system|"
)

while [ $# -gt 0 ]; do
  case "$1" in
    --build) BUILD=1; shift ;;
    --save) BUILD=1; SAVE=1; shift ;;
    --load) LOAD=1; shift ;;
    --push) PUSH=1; shift ;;
    --pin) PIN=1; shift ;;
    --registry) REGISTRY="${2:-}"; shift 2 ;;
    *) echo "usage: $0 [--build|--save|--load|--push|--pin] [--registry HOST]" >&2; exit 2 ;;
  esac
done

mkdir -p "$OUT"
echo "version=$VERSION platform=$PLATFORM output=$OUT"
for spec in "${IMAGES[@]}"; do echo "  ${spec%%|*}:$VERSION"; done

if [ "$LOAD" -eq 1 ]; then
  for archive in "$OUT"/*.tar; do
    [ -f "$archive" ] || continue
    echo "loading $archive"
    docker load -i "$archive"
  done
fi

if [ "$BUILD" -eq 1 ]; then
  for spec in "${IMAGES[@]}"; do
    IFS='|' read -r name dockerfile context arg <<<"$spec"
    echo "building $name:$VERSION"
    cmd=(docker build --platform "$PLATFORM" -f "$dockerfile" -t "$name:$VERSION")
    if [ -n "$arg" ] && [ "$arg" != "_HISTORY=1" ]; then
      cmd+=(--build-arg "$arg")
    fi
    cmd+=("$context")
    "${cmd[@]}" >/dev/null
    if [ "$SAVE" -eq 1 ]; then
      docker save "$name:$VERSION" -o "$OUT/$name-$VERSION.tar"
      echo "  saved $OUT/$name-$VERSION.tar"
    fi
  done
fi

if [ "$PUSH" -eq 1 ] || [ "$PIN" -eq 1 ]; then
  [ -n "$REGISTRY" ] || { echo "--push/--pin needs --registry HOST" >&2; exit 1; }
fi

if [ "$PUSH" -eq 1 ]; then
  : >"$OUT/digests.env"
  for spec in "${IMAGES[@]}"; do
    name="${spec%%|*}"
    remote="$REGISTRY/$name:$VERSION"
    docker tag "$name:$VERSION" "$remote"
    docker push "$remote" >/dev/null
    digest="$(docker buildx imagetools inspect "$remote" --format '{{.Manifest.Digest}}' 2>/dev/null || true)"
    if [ -z "$digest" ]; then
      digest="$(docker inspect --format '{{index .RepoDigests 0}}' "$remote" | sed 's/.*@//' || true)"
    fi
    echo "$name=$digest" >>"$OUT/digests.env"
    echo "pushed $remote digest=$digest"
  done
fi

if [ "$PIN" -eq 1 ]; then
  python3 - "$REGISTRY" "$OUT/digests.env" <<'PY'
import pathlib, re, sys
registry, env_path = sys.argv[1], pathlib.Path(sys.argv[2])
digests = {}
if env_path.exists():
    digests = dict(line.strip().split('=', 1) for line in env_path.read_text().splitlines() if '=' in line)
missing = [n for n in ('matchengine-k8s-control-plane', 'matchengine-trading-terminal') if not digests.get(n)]
if missing:
    raise SystemExit('--pin needs pushed digests for: ' + ', '.join(missing) + ' (run with --push first)')
tpl = pathlib.Path('k8s_control_plane/overlays/production/images.patch.yaml.example')
def fill(match):
    image = match.group(1)
    name = image.split('/')[-1]
    digest = digests.get(name, 'REPLACE_ME').removeprefix('sha256:')
    return f'image: {registry}/{image}@sha256:{digest}'
text = re.sub(r'image: REGISTRY/([^@\s]+)@sha256:REPLACE_ME', fill, tpl.read_text())
out = pathlib.Path('k8s_control_plane/overlays/production/images.patch.yaml')
out.write_text(text)
print('wrote', out)
PY
fi

if [ "$BUILD" -eq 0 ] && [ "$LOAD" -eq 0 ] && [ "$PUSH" -eq 0 ] && [ "$PIN" -eq 0 ]; then
  echo "dry run only — pass --build, --save, --load or --push/--pin to act"
fi
