#!/bin/sh

# Start the local walkthrough cluster and leave it running.
#
# This is the same launcher as test-cluster.sh, with --keep, debug logging and
# a separate default data dir, so it does not collide with the disposable
# test-cluster run.

set -eu

# Always run from the repository root.
cd "$(dirname "$0")/.."

export RUST_LOG=${RUST_LOG:-debug}

# Respect an explicit RAFT_DATA_DIR or --data-dir; otherwise use the historical
# open-cluster root and clear it, matching the previous script.
if [ -n "${RAFT_DATA_DIR:-}" ]; then
    exec ./scripts/test-cluster.sh --keep "$@"
fi

data_dir_arg=0
for arg in "$@"; do
    case $arg in
    -d | --data-dir) data_dir_arg=1 ;;
    esac
done
if [ "$data_dir_arg" -eq 1 ]; then
    exec ./scripts/test-cluster.sh --keep "$@"
fi

rm -rf /tmp/matchengine-raft-open-cluster
exec ./scripts/test-cluster.sh --keep --data-dir /tmp/matchengine-raft-open-cluster "$@"
