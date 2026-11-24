#!/usr/bin/env bash
# Clean up the OrbStack reference lab: finished Kubernetes Jobs/pods and stale
# local Docker images. Dry-run by default; pass --apply to actually delete.
#
# Safety rules:
#   * only completed/failed Jobs older than JOB_AGE_HOURS (default 2) are removed;
#   * only orphan pods older than that cutoff are removed;
#   * every image referenced by a Kubernetes pod or a running container is kept;
#   * the newest image in each matchengine-* family is kept (so a rollback tag
#     still exists), plus :local and all third-party/base images;
#   * stopped containers are pruned only when older than CONTAINER_AGE_H hours;
#   * the Docker build cache is never touched.
#
# Usage:
#   ./scripts/cleanup-orbstack.sh            # print the plan
#   ./scripts/cleanup-orbstack.sh --apply    # execute it
set -uo pipefail

APPLY=0
JOB_AGE_HOURS="${JOB_AGE_HOURS:-2}"
CONTAINER_AGE_H="${CONTAINER_AGE_H:-24}"

# A reboot can leave a shell without /opt/homebrew/bin or /usr/local/bin on
# PATH. Resolve the CLIs explicitly so the script works in a bare shell too.
find_cli() {
  local name="$1" candidate
  if command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
    return
  fi
  for candidate in \
    "/opt/homebrew/bin/$name" \
    "/usr/local/bin/$name" \
    "$HOME/.orbstack/bin/$name" \
    "/Applications/OrbStack.app/Contents/MacOS/xbin/$name"; do
    if [ -x "$candidate" ]; then echo "$candidate"; return; fi
  done
}
KUBECTL="$(find_cli kubectl)"
DOCKER="$(find_cli docker)"
if [ -z "$KUBECTL" ] || [ -z "$DOCKER" ]; then
  echo "kubectl or docker not found; install OrbStack/Docker Desktop" >&2
  exit 1
fi
export KUBECTL DOCKER

case "${1:-}" in
  --apply) APPLY=1 ;;
  --dry-run|"") ;;
  *) echo "usage: $0 [--apply]" >&2; exit 2 ;;
esac

run() {
  if [ "$APPLY" -eq 1 ]; then
    "$@"
  else
    printf 'DRY-RUN:'; printf ' %q' "$@"; printf '\n'
  fi
}

echo "== Kubernetes Jobs older than ${JOB_AGE_HOURS}h =="
jobs_plan=$(python3 - "$JOB_AGE_HOURS" <<'PY'
import datetime, json, os, subprocess, sys
cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=float(sys.argv[1]))
data = json.loads(subprocess.check_output([os.environ["KUBECTL"], "get", "jobs", "-A", "-o", "json"]))
for job in data.get("items", []):
    status = job.get("status", {})
    if not (status.get("succeeded") or status.get("failed")):
        continue
    ts = job["metadata"].get("creationTimestamp")
    if not ts:
        continue
    created = datetime.datetime.fromisoformat(ts.replace("Z", "+00:00"))
    if created < cutoff:
        print(job["metadata"]["namespace"], job["metadata"]["name"], sep="\t")
PY
)
if [ -n "$jobs_plan" ]; then
  printf '%s\n' "$jobs_plan" | while IFS=$'\t' read -r ns name; do
    run "$KUBECTL" -n "$ns" delete job "$name"
  done
else
  echo "  none"
fi

echo "== Orphan pods older than ${JOB_AGE_HOURS}h =="
pods_plan=$(python3 - "$JOB_AGE_HOURS" <<'PY'
import datetime, json, os, subprocess, sys
cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=float(sys.argv[1]))
data = json.loads(subprocess.check_output([os.environ["KUBECTL"], "get", "pods", "-A", "-o", "json"]))
for pod in data.get("items", []):
    meta = pod["metadata"]
    if meta.get("ownerReferences"):
        continue
    if pod.get("status", {}).get("phase") not in ("Succeeded", "Failed"):
        continue
    ts = meta.get("creationTimestamp")
    if not ts:
        continue
    if datetime.datetime.fromisoformat(ts.replace("Z", "+00:00")) < cutoff:
        print(meta["namespace"], meta["name"], sep="\t")
PY
)
if [ -n "$pods_plan" ]; then
  printf '%s\n' "$pods_plan" | while IFS=$'\t' read -r ns name; do
    run "$KUBECTL" -n "$ns" delete pod "$name"
  done
else
  echo "  none"
fi

echo "== Stale matchengine-* images =="
image_plan=$(python3 <<'PY'
import re, os, subprocess

def lines(*cmd):
    return subprocess.check_output(cmd, text=True).splitlines()

in_use = set()
pods = subprocess.check_output(
    [os.environ["KUBECTL"], "get", "pods", "-A", "-o",
     "jsonpath={range .items[*]}{range .spec.containers[*]}{.image}{\"\\n\"}{end}{range .spec.initContainers[*]}{.image}{\"\\n\"}{end}{end}"],
    text=True).splitlines()
in_use.update(i for i in pods if i)
for line in lines(os.environ["DOCKER"], "ps", "--format", "{{.Image}}"):
    in_use.add(line.strip())

# Only our own versioned families are candidates; third-party/base images stay.
families = {}
images = []
for line in lines(os.environ["DOCKER"], "images", "--format", "{{.Repository}}:{{.Tag}}|{{.ID}}|{{.Size}}"):
    ref, image_id, size = line.split("|", 2)
    name, _, tag = ref.rpartition(":")
    if not name.startswith("matchengine-"):
        continue
    family = (name, re.sub(r"-v\d+$", "", tag))
    version = re.search(r"-v(\d+)$", tag)
    rank = int(version.group(1)) if version else -1
    families.setdefault(family, (-1, None))
    if rank > families[family][0]:
        families[family] = (rank, ref)
    images.append((ref, image_id, size))

for ref, image_id, size in images:
    name, _, tag = ref.rpartition(":")
    family = (name, re.sub(r"-v\d+$", "", tag))
    newest = families[family][1]
    if ref in in_use or ref == newest or tag == "local":
        continue
    print(ref, size, sep="\t")
PY
)
if [ -n "$image_plan" ]; then
  printf '%s\n' "$image_plan" | while IFS=$'\t' read -r ref size; do
    run "$DOCKER" rmi "$ref"
  done
else
  echo "  none"
fi

echo "== Dangling images =="
if [ "$APPLY" -eq 1 ]; then
  run "$DOCKER" image prune -f
else
  "$DOCKER" images -f dangling=true --format '  {{.ID}} {{.Size}}'
fi

echo "== Stopped containers older than ${CONTAINER_AGE_H}h =="
if [ "$APPLY" -eq 1 ]; then
  run "$DOCKER" container prune --filter "until=${CONTAINER_AGE_H}h" -f
else
  python3 - "$CONTAINER_AGE_H" <<'PY'
import datetime, os, re, subprocess, sys
cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=float(sys.argv[1]))
out = subprocess.check_output(
    [os.environ["DOCKER"], "ps", "-a", "--filter", "status=exited",
     "--format", "{{.ID}}|{{.CreatedAt}}|{{.Image}}"], text=True)
count = 0
for line in out.splitlines():
    cid, created, image = line.split("|", 2)
    # Docker prints "2026-11-24 07:50:17 +0800 CST"; the numeric offset wins.
    stamp = re.sub(r"\s+[A-Z]{2,5}$", "", created.strip())
    try:
        when = datetime.datetime.strptime(stamp, "%Y-%m-%d %H:%M:%S %z")
    except ValueError:
        continue
    if when < cutoff:
        print("  ", cid, when.strftime("%Y-%m-%d"), image)
        count += 1
print("   total older than cutoff:", count)
PY
fi

if [ "$APPLY" -eq 0 ]; then
  echo "Dry run only — re-run with --apply to delete."
fi
