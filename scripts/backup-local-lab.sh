#!/usr/bin/env bash
# Run the scheduled backup path on the local lab without object storage.
#
# `deploy/backup.yaml` uploads bundles with rclone, which treats a plain filesystem path as a
# local remote. This script mounts one shared PVC into both CronJobs and points RCLONE_REMOTE at
# it, so the full create -> verify -> drill -> sign -> publish -> retention path runs without an
# S3 server. The PVC lives on the same node and cluster as the data: this is a demo convenience,
# not a backup destination. Production deployments point RCLONE_REMOTE at object storage and do
# not use this script.
#
# Usage:
#   scripts/backup-local-lab.sh [--namespace match-control] [--storage-class local-path]
#   scripts/backup-local-lab.sh --uninstall
#
# The script is idempotent. It creates the two Secrets the Jobs mount when they are missing
# (generating a random 32-byte signing key), then patches both CronJobs. Reapply
# `k8s_control_plane/deploy/backup.yaml` to drop the lab wiring.
#
# After the first run, bootstrap a bundle so the hourly status job has data:
#   kubectl -n match-control create job --from=cronjob/match-backup match-backup-bootstrap
#   kubectl -n match-control logs job/match-backup-bootstrap -f
set -euo pipefail

NAMESPACE="${BACKUP_NAMESPACE:-match-control}"
STORAGE_CLASS="${BACKUP_STORAGE_CLASS:-local-path}"
INSTALL=1
PVC_NAME="match-backup-remote"
MOUNT_PATH="/remote"

usage() {
  sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --namespace)
      NAMESPACE="$2"
      shift 2
      ;;
    --storage-class)
      STORAGE_CLASS="$2"
      shift 2
      ;;
    --uninstall)
      INSTALL=0
      shift
      ;;
    -h | --help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

log() { printf '%s\n' "$*"; }

require_cronjob() {
  if ! kubectl -n "$NAMESPACE" get cronjob "$1" >/dev/null 2>&1; then
    echo "CronJob $NAMESPACE/$1 is missing; apply k8s_control_plane/deploy/backup.yaml first" >&2
    exit 1
  fi
}

patch_cronjob() {
  local name=$1 container=$2
  kubectl -n "$NAMESPACE" patch cronjob "$name" --type=strategic -p "$(cat <<JSON
{
  "spec": {
    "jobTemplate": {
      "spec": {
        "template": {
          "spec": {
            "containers": [
              {
                "name": "$container",
                "env": [
                  { "name": "RCLONE_REMOTE", "value": "$MOUNT_PATH" }
                ],
                "volumeMounts": [
                  { "name": "remote-local-lab", "mountPath": "$MOUNT_PATH" }
                ]
              }
            ],
            "volumes": [
              {
                "name": "remote-local-lab",
                "persistentVolumeClaim": { "claimName": "$PVC_NAME" }
              }
            ]
          }
        }
      }
    }
  }
}
JSON
)"
}

if [ "$INSTALL" = "0" ]; then
  for name in match-backup match-backup-status; do
    require_cronjob "$name"
    kubectl -n "$NAMESPACE" patch cronjob "$name" --type=json -p "$(cat <<JSON
[
  { "op": "remove", "path": "/spec/jobTemplate/spec/template/spec/containers/0/volumeMounts/1" },
  { "op": "remove", "path": "/spec/jobTemplate/spec/template/spec/volumes/5" },
  { "op": "remove", "path": "/spec/jobTemplate/spec/template/spec/containers/0/env/7" }
]
JSON
)" || log "note: $name was not carrying the lab wiring"
  done
  kubectl -n "$NAMESPACE" delete pvc "$PVC_NAME" --ignore-not-found
  log "removed the local-lab wiring; reapply k8s_control_plane/deploy/backup.yaml to be safe"
  exit 0
fi

if ! kubectl -n "$NAMESPACE" get secret match-backup-signing-key >/dev/null 2>&1; then
  key_file=$(mktemp "${TMPDIR:-/tmp}/match-backup-key.XXXXXX")
  openssl rand -out "$key_file" 32
  kubectl -n "$NAMESPACE" create secret generic match-backup-signing-key \
    --from-file=signing.key="$key_file" >/dev/null
  rm -f "$key_file"
  log "created Secret $NAMESPACE/match-backup-signing-key (random 32-byte key)"
else
  log "Secret $NAMESPACE/match-backup-signing-key already exists"
fi

if ! kubectl -n "$NAMESPACE" get secret match-backup-rclone >/dev/null 2>&1; then
  printf '# lab config: no remotes; RCLONE_REMOTE uses a local path\n' >"${TMPDIR:-/tmp}/match-backup-rclone.conf"
  kubectl -n "$NAMESPACE" create secret generic match-backup-rclone \
    --from-file=rclone.conf="${TMPDIR:-/tmp}/match-backup-rclone.conf" >/dev/null
  log "created Secret $NAMESPACE/match-backup-rclone (empty config)"
else
  log "Secret $NAMESPACE/match-backup-rclone already exists"
fi

kubectl apply -f - >/dev/null <<YAML
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $PVC_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/name: match-backup
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: $STORAGE_CLASS
  resources:
    requests:
      storage: 1Gi
YAML
log "PVC $NAMESPACE/$PVC_NAME ready (storageClass $STORAGE_CLASS)"

require_cronjob match-backup
require_cronjob match-backup-status
patch_cronjob match-backup backup
patch_cronjob match-backup-status status
log "patched both CronJobs: RCLONE_REMOTE=$MOUNT_PATH on $PVC_NAME"
# The production window (02:17 UTC) falls while a laptop lab is asleep; with
# startingDeadlineSeconds=600 the controller skips the missed run entirely, so
# no fresh bundle is published and every hourly status check then fails as
# "newest backup is stale". Give the lab a wake-time schedule and a full-day
# deadline so one awake moment in 24h is enough to run the backup.
kubectl -n "$NAMESPACE" patch cronjob match-backup --type merge -p \
  '{"spec":{"schedule":"17 6 * * *","startingDeadlineSeconds":86400}}' >/dev/null
log "lab backup schedule: 17 6 * * * with a 24h starting deadline"
log "next: kubectl -n $NAMESPACE create job --from=cronjob/match-backup match-backup-bootstrap"
