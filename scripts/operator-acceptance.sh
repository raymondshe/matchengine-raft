#!/usr/bin/env bash
#
# End-to-end acceptance harness for the Kubernetes control plane.
#
# The harness drives the manager API and then checks the *live* result: rendered
# manifests, Kubernetes objects, operator status and the engines themselves.
# It never trusts the manager's simulated observation.
#
# Everything it creates is named with a private prefix (default `acc`), lives in
# the demo namespace (default `match-eu-1`) and is removed on exit unless
# --keep is given. Existing fleet shards are never touched.
#
# Usage:
#   scripts/operator-acceptance.sh [run] [options]
#   scripts/operator-acceptance.sh render [--shard NAME] [options]
#   scripts/operator-acceptance.sh cleanup [options]
#
# Run `scripts/operator-acceptance.sh --help` for the full option list.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# ---------------------------------------------------------------------------
# defaults
# ---------------------------------------------------------------------------

ACTION=run
PHASES="containers,leader,roll,stopstart,supervisor,reserved,migration,adoption"
NAMESPACE=match-eu-1
CONTROL_NS=match-control
OPERATOR_DEPLOY=match-operator
MANAGER_SVC=match-manager
MANAGER_PORT=8099
LOCAL_PORT=18099
MANAGER_OVERRIDE=
PREFIX=acc
BASE_PORT=26200
IMAGE=${MATCHENGINE_ACCEPTANCE_IMAGE:-matchengine-raft:k8s-reorg-v1-aeron-arm64}
TIMEOUT=900
ROLL_TIMEOUT=900
KEEP=0
RENDER_SHARD=
RENDER_EXPECT=
REPLICAS=3
SYMBOL_BASE=91

SHARD_C1="$PREFIX-c1"
SHARD_SUP="$PREFIX-sup"
SHARD_RES="$PREFIX-res"
SHARD_M1="$PREFIX-m1"
SHARD_M2="$PREFIX-m2"
SHARD_ADOPT="$PREFIX-adopt"

SYM_C1="${PREFIX}c1"
SYM_SUP="${PREFIX}sup"
SYM_SUP2="${PREFIX}sup2"
SYM_MIG="${PREFIX}mig"
SYM_ADOPT="${PREFIX}adopt"

PORT_C1=$((BASE_PORT + 1))
PORT_SUP=$((BASE_PORT + 11))
PORT_SUP2=$((BASE_PORT + 12))
PORT_RES=$((BASE_PORT + 21))
PORT_M1=$((BASE_PORT + 31))
PORT_M2=$((BASE_PORT + 41))
PORT_ADOPT=$((BASE_PORT + 51))

MANAGER=
PF_PID=
OPERATOR_SCALED=0
DID_CREATE=0
CLEANED=0
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/operator-acceptance.XXXXXX")
STARTED_AT=$SECONDS

usage() {
    cat <<'EOF'
Run the control-plane operator acceptance harness against a live cluster.

Commands:
  run                 Create, exercise and clean up acceptance resources (default).
  render              Assert the rendered templates for existing shards; no mutation.
  cleanup             Remove every acceptance resource, including retained PVCs.

Phases (run --phases containers,leader,roll,stopstart,supervisor,reserved,migration,adoption):
  containers          Create a containers-mode shard, assert its templates, wait for
                      readiness and write/read an order.
  leader              Transfer leadership through the engine console action and
                      prove the book survives.
  roll                Roll to the digest-pinned image, assert one-pod-at-a-time
                      progress and quorum-safe readiness, then roll back to the tag.
  stopstart           Shut the symbol down, prove PVC identities survive, start it
                      again and prove the sequence/book survived.
  supervisor          Create a supervisor-mode shard, add and stop a second symbol
                      without restarting the first child (PID continuity).
  reserved            Create and drop a reserved shard (no workload).
  migration           Migrate a symbol to a second shard with a non-overlapping
                      Raft ID range, then roll it back.
  adoption            Scale the operator down, apply rendered manifests by hand,
                      scale it up and prove pods/PVCs/StatefulSet were adopted
                      without recreation.

Options:
  --phases LIST             Comma-separated phase list (default: all).
  --namespace NS            Workload namespace (default match-eu-1).
  --control-namespace NS    Namespace of the manager/operator (default match-control).
  --operator NAME           Operator Deployment name (default match-operator).
  --manager URL             Use an existing manager URL instead of port-forwarding.
  --local-port PORT         Local port for the manager port-forward (default 18099).
  --prefix PREFIX           Acceptance name prefix, [a-z0-9]+ (default acc).
  --base-port PORT          First acceptance HTTP port (default 26200).
  --image IMAGE             Engine image for new shards.
  --timeout SECONDS         General readiness timeout (default 900).
  --roll-timeout SECONDS    Image rollout timeout (default 900).
  --keep                    Leave acceptance resources in place for inspection.
  --shard NAME              render: check only this shard.
  --expect FAMILY           render: force containers|supervisor|reserved.
  --dry-run                 Alias for the render command.
  -h, --help                Show this help.

Environment:
  MATCHENGINE_ACCEPTANCE_IMAGE   Default for --image.

Examples:
  ./scripts/operator-acceptance.sh
  ./scripts/operator-acceptance.sh run --phases containers,leader,roll
  ./scripts/operator-acceptance.sh render
  ./scripts/operator-acceptance.sh render --shard fx-sup-01 --expect supervisor
  ./scripts/operator-acceptance.sh cleanup
EOF
}

# ---------------------------------------------------------------------------
# output and assertions
# ---------------------------------------------------------------------------

step() { printf '\n== %s ==\n' "$*"; }
ok() { printf '  ok   %s\n' "$*"; }
note() { printf '  --   %s\n' "$*"; }
fail() {
    printf '\nFAIL: %s\n' "$*" >&2
    printf 'Temporary logs: %s\n' "$TMP_DIR" >&2
    exit 1
}

assert_eq() { # actual expected message
    if [ "$1" != "$2" ]; then
        fail "$3: expected '$2', got '$1'"
    fi
    ok "$3"
}

assert_nonempty() { # value message
    if [ -z "$1" ]; then
        fail "$2"
    fi
    ok "$2"
}

assert_contains() { # haystack needle message
    case "$1" in
    *"$2"*) ok "$3" ;;
    *) fail "$3: '$2' not found in '$1'" ;;
    esac
}

require_value() {
    [ "$#" -ge 2 ] || fail "option $1 requires a value"
}

# ---------------------------------------------------------------------------
# argument parsing
# ---------------------------------------------------------------------------

while [ "$#" -gt 0 ]; do
    case $1 in
    run | render | cleanup)
        ACTION=$1
        ;;
    --phases)
        require_value "$@"
        PHASES=$2
        shift
        ;;
    --namespace)
        require_value "$@"
        NAMESPACE=$2
        shift
        ;;
    --control-namespace)
        require_value "$@"
        CONTROL_NS=$2
        shift
        ;;
    --operator)
        require_value "$@"
        OPERATOR_DEPLOY=$2
        shift
        ;;
    --manager)
        require_value "$@"
        MANAGER_OVERRIDE=$2
        shift
        ;;
    --local-port)
        require_value "$@"
        LOCAL_PORT=$2
        shift
        ;;
    --prefix)
        require_value "$@"
        PREFIX=$2
        shift
        ;;
    --base-port)
        require_value "$@"
        BASE_PORT=$2
        shift
        ;;
    --image)
        require_value "$@"
        IMAGE=$2
        shift
        ;;
    --timeout)
        require_value "$@"
        TIMEOUT=$2
        shift
        ;;
    --roll-timeout)
        require_value "$@"
        ROLL_TIMEOUT=$2
        shift
        ;;
    --keep)
        KEEP=1
        ;;
    --shard)
        require_value "$@"
        RENDER_SHARD=$2
        shift
        ;;
    --expect)
        require_value "$@"
        RENDER_EXPECT=$2
        shift
        ;;
    --dry-run)
        ACTION=render
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        printf 'error: unknown option %s\n\n' "$1" >&2
        usage >&2
        exit 2
        ;;
    esac
    shift
done

case "$PREFIX" in
*[!a-z0-9]* | "")
    fail "--prefix must match [a-z0-9]+, got '$PREFIX'"
    ;;
esac

SHARD_C1="$PREFIX-c1"
SHARD_SUP="$PREFIX-sup"
SHARD_RES="$PREFIX-res"
SHARD_M1="$PREFIX-m1"
SHARD_M2="$PREFIX-m2"
SHARD_ADOPT="$PREFIX-adopt"

SYM_C1="${PREFIX}c1"
SYM_SUP="${PREFIX}sup"
SYM_SUP2="${PREFIX}sup2"
SYM_MIG="${PREFIX}mig"
SYM_ADOPT="${PREFIX}adopt"

SYM_ID_C1=$SYMBOL_BASE
SYM_ID_SUP=$((SYMBOL_BASE + 1))
SYM_ID_SUP2=$((SYMBOL_BASE + 2))
SYM_ID_MIG=$((SYMBOL_BASE + 3))
SYM_ID_ADOPT=$((SYMBOL_BASE + 4))

PORT_C1=$((BASE_PORT + 1))
PORT_SUP=$((BASE_PORT + 11))
PORT_SUP2=$((BASE_PORT + 12))
PORT_RES=$((BASE_PORT + 21))
PORT_M1=$((BASE_PORT + 31))
PORT_M2=$((BASE_PORT + 41))
PORT_ADOPT=$((BASE_PORT + 51))

if [ "$((PORT_ADOPT + 1000))" -gt 65535 ]; then
    fail "--base-port is too high: raft port for the adoption shard is $((PORT_ADOPT + 1000))"
fi

KNOWN_PHASES="containers leader roll stopstart supervisor reserved migration adoption"
for phase in $(printf '%s' "$PHASES" | tr ',' ' '); do
    case " $KNOWN_PHASES " in
    *" $phase "*) ;;
    *) fail "unknown phase '$phase'; known phases: $KNOWN_PHASES" ;;
    esac
done

phase_selected() {
    case ",$PHASES," in
    *",$1,"*) return 0 ;;
    *) return 1 ;;
    esac
}

# ---------------------------------------------------------------------------
# exit handling
# ---------------------------------------------------------------------------

cleanup_on_exit() {
    local code=$?
    trap - EXIT
    if [ -n "${PF_PID:-}" ]; then
        kill "$PF_PID" 2>/dev/null || true
        wait "$PF_PID" 2>/dev/null || true
    fi
    if [ "${OPERATOR_SCALED:-0}" = "1" ]; then
        kubectl -n "$CONTROL_NS" scale deployment "$OPERATOR_DEPLOY" --replicas=1 >/dev/null 2>&1 || true
    fi
    if [ "${KEEP:-0}" != "1" ] && [ "${DID_CREATE:-0}" = "1" ] && [ "${CLEANED:-0}" != "1" ]; then
        printf '\ncleaning up acceptance resources after exit...\n' >&2
        cleanup_all >/dev/null 2>&1 || true
    fi
    exit "$code"
}
trap cleanup_on_exit EXIT

# ---------------------------------------------------------------------------
# manager access
# ---------------------------------------------------------------------------

manager_healthy() {
    [ -n "${MANAGER:-}" ] && curl -fsS -m 3 "$MANAGER/api/v1/health" >/dev/null 2>&1
}

ensure_manager() {
    if [ -n "$MANAGER_OVERRIDE" ]; then
        MANAGER=${MANAGER_OVERRIDE%/}
        manager_healthy || fail "manager at $MANAGER is not healthy"
        ok "manager reachable at $MANAGER"
        return
    fi
    MANAGER="http://127.0.0.1:$LOCAL_PORT"
    if manager_healthy; then
        ok "manager reachable at $MANAGER"
        return
    fi
    step "starting manager port-forward on $LOCAL_PORT"
    kubectl -n "$CONTROL_NS" port-forward "svc/$MANAGER_SVC" "$LOCAL_PORT:$MANAGER_PORT" \
        >"$TMP_DIR/manager-port-forward.log" 2>&1 &
    PF_PID=$!
    local start=$SECONDS
    while ! manager_healthy; do
        if ((SECONDS - start > 30)); then
            cat "$TMP_DIR/manager-port-forward.log" >&2 || true
            fail "manager port-forward did not become healthy"
        fi
        sleep 1
    done
    ok "manager port-forward ready on $LOCAL_PORT (pid $PF_PID)"
}

api_get() { # path
    local out code
    out=$(mktemp "$TMP_DIR/api.XXXXXX")
    if ! code=$(curl -sS -m 60 -o "$out" -w '%{http_code}' "$MANAGER$1"); then
        rm -f "$out"
        fail "GET $1 failed"
    fi
    if [ "$code" -ge 400 ]; then
        local body
        body=$(cat "$out")
        rm -f "$out"
        fail "GET $1 returned $code: $body"
    fi
    cat "$out"
    rm -f "$out"
}

api_post() { # path body
    local out code
    out=$(mktemp "$TMP_DIR/api.XXXXXX")
    if ! code=$(curl -sS -m 180 -o "$out" -w '%{http_code}' \
        -X POST -H 'content-type: application/json' -d "$2" "$MANAGER$1"); then
        rm -f "$out"
        fail "POST $1 failed"
    fi
    if [ "$code" -ge 400 ]; then
        local body
        body=$(cat "$out")
        rm -f "$out"
        fail "POST $1 returned $code: $body"
    fi
    cat "$out"
    rm -f "$out"
}

api_post_quiet() { # path body
    curl -sS -m 60 -X POST -H 'content-type: application/json' -d "$2" "$MANAGER$1" >/dev/null 2>&1 || true
}

api_exists() { # path
    curl -fsS -m 10 "$MANAGER$1" >/dev/null 2>&1
}

shard_detail() { # shard
    api_get "/api/v1/shards/$1"
}

shard_field() { # shard jq-expression
    local body
    body=$(shard_detail "$1" 2>/dev/null || true)
    jq -r "$2 // empty" <<<"$body" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Kubernetes helpers
# ---------------------------------------------------------------------------

k() {
    kubectl -n "$NAMESPACE" "$@"
}

pod_names() { # shard
    k get pods -l "matching.io/cluster=$1" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null || true
}

pod_json() { # shard
    k get pods -l "matching.io/cluster=$1" -o json 2>/dev/null || printf '{"items":[]}'
}

pod_count() { # shard
    pod_json "$1" | jq -r '.items | length'
}

ready_pod_count() { # shard
    pod_json "$1" | jq -r '[.items[] | select(any(.status.conditions[]?; .type == "Ready" and .status == "True"))] | length'
}

running_pod_count() { # shard
    pod_json "$1" | jq -r '[.items[] | select(.status.phase == "Running")] | length'
}

pvc_uid_map() { # shard
    k get pvc -l "matching.io/cluster=$1" -o json 2>/dev/null |
        jq -r '.items[]? | "\(.metadata.name) \(.metadata.uid)"' | sort
}

pod_uid_map() { # shard
    pod_json "$1" | jq -r '.items[]? | "\(.metadata.name) \(.metadata.uid)"' | sort
}

pod_image_map() { # shard
    pod_json "$1" | jq -r '.items[]? | "\(.metadata.name) \(.spec.containers[0].image)"' | sort
}

sts_field() { # shard field
    k get sts "$1" -o json 2>/dev/null | jq -r ".status.$2 // empty" 2>/dev/null || true
}

deployments_on_shard() { # shard
    k get symboldeployment -o json 2>/dev/null |
        jq -r --arg shard "$1" '.items[]? | select(.spec.shardRef == $shard) | .metadata.name' || true
}

active_deployments_on_shard() { # shard (JSON array, slot order)
    k get symboldeployment -o json 2>/dev/null |
        jq -c --arg shard "$1" \
            '[.items[]? | select(.spec.shardRef == $shard and .spec.desiredState != "Disabled")]
             | sort_by(.spec.placement.slot)' || true
}

deployment_field() { # deployment jq-expression
    k get symboldeployment "$1" -o json 2>/dev/null | jq -r "$2 // empty" 2>/dev/null || true
}

route_field() { # route jq-expression
    k get symbolroute "$1" -o json 2>/dev/null | jq -r "$2 // empty" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# engine probes (exec inside the pod; the engine binds the pod FQDN)
# ---------------------------------------------------------------------------

engine_url() { # shard pod port path
    printf 'http://%s.%s-headless.%s.svc.cluster.local:%s%s' "$2" "$1" "$NAMESPACE" "$3" "$4"
}

engine_curl() { # shard pod container port method path [body] [action]
    local shard=$1 pod=$2 container=$3 port=$4 method=$5 path=$6
    local body=${7:-} action=${8:-0}
    local url args
    url=$(engine_url "$shard" "$pod" "$port" "$path")
    args=(-sS -m 15 -X "$method")
    if [ "$method" = "POST" ]; then
        args+=(-H 'content-type: application/json')
        if [ -n "$body" ]; then
            args+=(-d "$body")
        fi
    fi
    if [ "$action" = "1" ]; then
        args+=(-H 'X-Console-Action: 1')
    fi
    kubectl -n "$NAMESPACE" exec "$pod" -c "$container" -- curl "${args[@]}" "$url"
}

engine_get() { # shard pod container port path
    engine_curl "$1" "$2" "$3" "$4" GET "$5"
}

engine_post() { # shard pod container port path body
    engine_curl "$1" "$2" "$3" "$4" POST "$5" "$6"
}

engine_read() { # shard pod container port key
    local body
    body=$(jq -cn --arg key "$5" '$key')
    engine_post "$1" "$2" "$3" "$4" /read "$body" | jq -r '.Ok // empty'
}

engine_sequence() { # shard pod container port
    engine_read "$1" "$2" "$3" "$4" orderbook_sequence
}

engine_orders() { # shard pod container port
    engine_read "$1" "$2" "$3" "$4" orderbook_orders
}

find_leader_pod() { # shard container port
    local shard=$1 container=$2 port=$3 pod out
    for pod in $(pod_names "$shard"); do
        out=$(engine_get "$shard" "$pod" "$container" "$port" /console/api/node 2>/dev/null || true)
        if [ -n "$out" ] && [ "$(jq -r '.leadership.is_leader // false' <<<"$out" 2>/dev/null || true)" = "true" ]; then
            printf '%s\n' "$pod"
            return 0
        fi
    done
    return 1
}

wait_leader_pod() { # shard container port [timeout]
    local shard=$1 container=$2 port=$3 timeout=${4:-$TIMEOUT}
    local start=$SECONDS pod
    while :; do
        pod=$(find_leader_pod "$shard" "$container" "$port" 2>/dev/null || true)
        if [ -n "$pod" ]; then
            printf '%s\n' "$pod"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            return 1
        fi
        sleep 2
    done
}

node_field() { # shard pod container port jq-expression
    local out
    out=$(engine_get "$1" "$2" "$3" "$4" /console/api/node 2>/dev/null || true)
    jq -r "$5 // empty" <<<"$out" 2>/dev/null || true
}

place_order() { # shard pod container port symbol id price volume client_id
    local body
    body=$(jq -cn \
        --argjson symbol "$5" --argjson id "$6" \
        --arg price "$7" --arg volume "$8" --arg client "$9" \
        '{Place:{order:{live:"Limit",side:"Buy",symbol_id:$symbol,price:$price,
          volume:$volume,id:$id,tif:"GTC",client_order_id:$client},
          client_order_id:$client}}')
    engine_post "$1" "$2" "$3" "$4" /write "$body"
}

# ---------------------------------------------------------------------------
# manager models
# ---------------------------------------------------------------------------

shard_body() { # name mode port
    jq -cn \
        --arg name "$1" --arg ns "$NAMESPACE" --arg mode "$2" \
        --arg image "$IMAGE" --argjson port "$3" --argjson replicas "$REPLICAS" \
        '{name:$name,namespace:$ns,spec:{
            cell:"eu-1",environment:"acceptance",class:"Packed",replicas:$replicas,
            packing:{mode:$mode,maxProcessesPerPod:4},
            image:$image,
            release:{wireVersion:7,snapshotVersion:11,mixedVersionSafe:false},
            storage:{className:"local-path",size:"1Gi"},
            defaultProcessResources:{
                requests:{cpu:"50m",memory:"64Mi"},
                limits:{cpu:"500m",memory:"512Mi"}},
            raft:{walFsync:{mode:"every"},readPolicy:"local_lease",
                  snapshot:{everyMinutes:30}},
            network:{httpBasePort:$port}}}'
}

symbol_body() { # name id shard allow_restart
    jq -cn \
        --arg name "$1" --arg ns "$NAMESPACE" --arg shard "$3" \
        --argjson id "$2" --argjson restart "$4" \
        '{symbol:{name:$name,namespace:$ns,spec:{
            symbolId:$id,
            base:{code:"USD",class:"fiat",decimals:2},
            quote:{code:"JPY",class:"fiat",decimals:2},
            priceDecimals:3,priceTick:"0.001",baseLot:"0.01",
            feeAssetPolicy:"received",stp:"off",
            owner:"acceptance",generation:1}},
          shard:$shard,allowRestart:$restart}'
}

create_shard() { # name mode port
    local body
    DID_CREATE=1
    body=$(api_post /api/v1/shards "$(shard_body "$1" "$2" "$3")")
    assert_eq "$(jq -r '.shard.status.phase // empty' <<<"$body")" "Reserved" \
        "shard $1 starts Reserved"
}

create_symbol_raw() { # shard name id allow_restart
    DID_CREATE=1
    api_post /api/v1/symbols "$(symbol_body "$2" "$3" "$1" "$4")"
}

create_symbol() { # shard name id allow_restart
    local response
    response=$(create_symbol_raw "$@")
    assert_eq "$(jq -r '.placement.shard // empty' <<<"$response")" "$1" \
        "symbol $2 placed on $1"
}

# ---------------------------------------------------------------------------
# rendered-template assertions
# ---------------------------------------------------------------------------

assert_render_family() { # shard family
    local shard=$1 family=$2
    local manifest kinds shard_json replicas base mode slots
    local count i sym port offset container args env_mode

    manifest=$(api_get "/api/v1/shards/$shard/manifests?profile=local")
    kinds=$(jq -r '.items | map(.kind) | sort | join(",")' <<<"$manifest")
    shard_json=$(k get symbolshard "$shard" -o json)
    replicas=$(jq -r '.spec.replicas' <<<"$shard_json")
    base=$(jq -r '.spec.network.httpBasePort' <<<"$shard_json")
    mode=$(jq -r '.spec.packing.mode' <<<"$shard_json")
    slots=$(jq -r '.spec.packing.maxProcessesPerPod' <<<"$shard_json")

    jq -e --arg shard "$shard" \
        '[.items[] | select(.metadata.labels["matching.io/cluster"] != $shard)] | length == 0' \
        <<<"$manifest" >/dev/null ||
        fail "$shard render: every object must carry matching.io/cluster=$shard"

    case "$family" in
    reserved)
        assert_eq "$kinds" "ConfigMap,NetworkPolicy,PodDisruptionBudget" \
            "$shard reserved render has no workload objects"
        jq -e '.items[] | select(.kind == "ConfigMap") | .data["processes.json"] | fromjson | .processes | length == 0' \
            <<<"$manifest" >/dev/null ||
            fail "$shard reserved render: the process list must be empty"
        assert_eq "$(jq -r '.items[] | select(.kind == "PodDisruptionBudget") | .spec.minAvailable' <<<"$manifest")" \
            "$([ "$replicas" -gt 1 ] && echo $((replicas - 1)) || echo 1)" \
            "$shard reserved PDB keeps quorum"
        ;;
    containers | supervisor)
        assert_eq "$kinds" "ConfigMap,NetworkPolicy,PodDisruptionBudget,Service,Service,StatefulSet" \
            "$shard $family render has the expected object kinds"
        assert_eq "$(jq -r '.items[] | select(.kind == "PodDisruptionBudget") | .spec.minAvailable' <<<"$manifest")" \
            "$([ "$replicas" -gt 1 ] && echo $((replicas - 1)) || echo 1)" \
            "$shard PDB keeps quorum"
        assert_eq "$(jq -r '.items[] | select(.kind == "Service" and .spec.clusterIP == "None") | .metadata.name' <<<"$manifest")" \
            "$shard-headless" \
            "$shard headless service is named as expected"
        ;;
    *)
        fail "unknown render family '$family'"
        ;;
    esac

    [ "$family" = "containers" ] || [ "$family" = "supervisor" ] || return 0

    local sts
    sts=$(jq '.items[] | select(.kind == "StatefulSet")' <<<"$manifest")
    assert_eq "$(jq -r '.spec.replicas' <<<"$sts")" "$replicas" \
        "$shard StatefulSet has $replicas replicas"
    assert_eq "$(jq -r '.spec.serviceName' <<<"$sts")" "$shard-headless" \
        "$shard StatefulSet points at the headless service"
    assert_eq "$(jq -r '.spec.updateStrategy.type' <<<"$sts")" "OnDelete" \
        "$shard StatefulSet rolls one pod at a time"
    assert_eq "$(jq -r '.spec.volumeClaimTemplates[0].metadata.name' <<<"$sts")" "data" \
        "$shard StatefulSet keeps a data volume claim"

    local deployments
    deployments=$(active_deployments_on_shard "$shard")
    count=$(jq 'length' <<<"$deployments")
    [ "$count" -ge 1 ] || fail "$shard render has no active process"
    ok "$shard render covers $count active process(es)"

    if [ "$family" = "containers" ]; then
        assert_eq "$(jq -r '.spec.template.spec.containers | length' <<<"$sts")" "$count" \
            "$shard StatefulSet has one container per active symbol"
        for i in $(seq 0 $((count - 1))); do
            sym=$(jq -r ".[$i].spec.symbolRef" <<<"$deployments")
            port=$(jq -r ".[$i].spec.placement.httpPort" <<<"$deployments")
            slot=$(jq -r ".[$i].spec.placement.slot" <<<"$deployments")
            offset=$(jq -r ".[$i].spec.raftIdOffset" <<<"$deployments")
            container=$(jq -c --arg name "$sym" \
                '.spec.template.spec.containers[] | select(.name == $name)' <<<"$sts")
            [ -n "$container" ] || fail "$shard render: missing container $sym"
            args=$(jq -r '.args | join(" ")' <<<"$container")
            assert_contains "$args" "--http-addr \$(POD_FQDN):$port" \
                "$shard/$sym binds the pod FQDN on port $port"
            assert_contains "$args" "--id-offset $offset" \
                "$shard/$sym uses Raft ID offset $offset"
            assert_contains "$args" "--instance-prefix $sym" \
                "$shard/$sym uses its own data prefix"
            assert_contains "$args" "--data-dir /data" \
                "$shard/$sym stores data on the PVC"
            jq -e --arg name "$sym" --arg http "http-$slot" --arg raft "raft-$slot" \
                '.spec.template.spec.containers[] | select(.name == $name)
                 | .ports[] | select(.name == $http or .name == $raft)' \
                <<<"$sts" >/dev/null ||
                fail "$shard/$sym does not expose HTTP and Raft ports"
        done
        assert_eq "$(jq -r '.items[] | select(.kind == "Service" and .spec.clusterIP == "None") | .spec.ports | length' <<<"$manifest")" \
            "$((count * 2))" \
            "$shard headless service exposes HTTP and Raft per process"
        env_mode=$(jq -r '.spec.raft.walFsync.mode' <<<"$shard_json")
        if [ "$env_mode" = "every" ]; then
            jq -e --arg name "$(jq -r '.[0].spec.symbolRef' <<<"$deployments")" \
                '.spec.template.spec.containers[] | select(.name == $name)
                 | .env[] | select(.name == "RAFT_WAL_FSYNC_MS" and .value == "0")' \
                <<<"$sts" >/dev/null ||
                fail "$shard render: RAFT_WAL_FSYNC_MS=0 is not set for fsync=every"
        fi
    else
        container=$(jq -c '.spec.template.spec.containers[] | select(.name == "supervisor")' <<<"$sts")
        [ -n "$container" ] || fail "$shard render: missing supervisor container"
        assert_eq "$(jq -r '.command[0]' <<<"$container")" "/usr/local/bin/match_supervisor" \
            "$shard runs the supervisor binary"
        args=$(jq -r '.args | join(" ")' <<<"$container")
        assert_contains "$args" "--config /etc/match/processes.json" \
            "$shard supervisor reads the rendered process list"
        assert_contains "$args" "--max-processes $slots" \
            "$shard supervisor honours the slot cap"
        assert_contains "$args" "--engine-bin /usr/local/bin/matchengine-raft" \
            "$shard supervisor launches the engine binary"
        jq -e '.ports[] | select(.name == "supervisor" and .containerPort == 9090)' \
            <<<"$container" >/dev/null ||
            fail "$shard supervisor does not expose its control port"

        local config
        config=$(jq -r '.items[] | select(.kind == "ConfigMap") | .data["processes.json"]' <<<"$manifest")
        assert_eq "$(jq -r '.processes | length' <<<"$config")" "$count" \
            "$shard process list matches the active symbols"
        for i in $(seq 0 $((count - 1))); do
            sym=$(jq -r ".[$i].spec.symbolRef" <<<"$deployments")
            port=$(jq -r ".[$i].spec.placement.httpPort" <<<"$deployments")
            offset=$(jq -r ".[$i].spec.raftIdOffset" <<<"$deployments")
            jq -e --arg name "$sym" --argjson http "$port" --argjson raft "$((port + 1000))" \
                --argjson offset "$offset" \
                '.processes[] | select(.name == $name and .httpPort == $http
                   and .raftPort == $raft and .raftIdOffset == $offset)' \
                <<<"$config" >/dev/null ||
                fail "$shard process list entry for $sym is wrong"
        done
        assert_eq "$(jq -r '.items[] | select(.kind == "Service" and .spec.clusterIP == "None") | .spec.ports | length' <<<"$manifest")" \
            "$((slots * 2))" \
            "$shard headless service exposes every supervisor slot"
    fi
}

# ---------------------------------------------------------------------------
# readiness waits
# ---------------------------------------------------------------------------

wait_shard_phase() { # shard phase [timeout]
    local shard=$1 want=$2 timeout=${3:-$TIMEOUT}
    local start=$SECONDS phase
    while :; do
        phase=$(shard_field "$shard" '.shard.status.phase')
        if [ "$phase" = "$want" ]; then
            ok "shard $shard reports phase $want"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "shard $shard reports phase '$phase', expected $want"
        fi
        sleep 2
    done
}

wait_shard_ready() { # shard [timeout]
    local shard=$1 timeout=${2:-$TIMEOUT}
    local start=$SECONDS body phase complete
    while :; do
        body=$(shard_detail "$shard" 2>/dev/null || true)
        phase=$(jq -r '.shard.status.phase // empty' <<<"$body" 2>/dev/null || true)
        complete=$(jq -r '.shard.status.rollout.complete // false' <<<"$body" 2>/dev/null || true)
        if [ "$phase" = "Ready" ] && [ "$complete" = "true" ]; then
            ok "shard $shard is Ready and its rollout is complete"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "shard $shard did not become Ready (phase=$phase rollout.complete=$complete)"
        fi
        sleep 2
    done
}

wait_pods_ready() { # shard count [timeout]
    local shard=$1 want=$2 timeout=${3:-$TIMEOUT}
    local start=$SECONDS got
    while :; do
        got=$(ready_pod_count "$shard")
        if [ "$got" = "$want" ]; then
            ok "shard $shard has $want/$want ready pods"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "shard $shard has $got/$want ready pods after ${timeout}s"
        fi
        sleep 2
    done
}

wait_pods_running() { # shard count [timeout]
    local shard=$1 want=$2 timeout=${3:-$TIMEOUT}
    local start=$SECONDS got
    while :; do
        got=$(running_pod_count "$shard")
        if [ "$got" = "$want" ]; then
            ok "shard $shard has $want/$want running pods"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "shard $shard has $got/$want running pods after ${timeout}s"
        fi
        sleep 2
    done
}

wait_pods_gone() { # shard [timeout]
    local shard=$1 timeout=${2:-180}
    local start=$SECONDS got
    while :; do
        got=$(pod_count "$shard")
        if [ "$got" = "0" ]; then
            ok "shard $shard has no pods"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "shard $shard still has $got pod(s) after ${timeout}s"
        fi
        sleep 2
    done
}

wait_deployment_ready() { # deployment [timeout]
    local dep=$1 timeout=${2:-$TIMEOUT}
    local start=$SECONDS ready leader
    while :; do
        ready=$(deployment_field "$dep" '.status.ready')
        leader=$(deployment_field "$dep" '.status.leaderPod')
        if [ "$ready" = "true" ] && [ -n "$leader" ]; then
            ok "deployment $dep is ready with leader $leader"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "deployment $dep did not become ready (ready=$ready leader=$leader)"
        fi
        sleep 2
    done
}

wait_deployment_disabled() { # deployment [timeout]
    local dep=$1 timeout=${2:-180}
    local start=$SECONDS state
    while :; do
        state=$(deployment_field "$dep" '.spec.desiredState')
        if [ "$state" = "Disabled" ]; then
            ok "deployment $dep is Disabled"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "deployment $dep did not become Disabled (state=$state)"
        fi
        sleep 2
    done
}

# ---------------------------------------------------------------------------
# cleanup
# ---------------------------------------------------------------------------

cleanup_shard() { # shard
    local shard=$1
    local deps symbols routes dep symbol route state
    local start

    deps=$(deployments_on_shard "$shard")
    symbols=
    for dep in $deps; do
        symbol=$(deployment_field "$dep" '.spec.symbolRef')
        [ -n "$symbol" ] && symbols="$symbols $symbol"
    done
    symbols=$(printf '%s\n' $symbols 2>/dev/null | sort -u || true)
    routes=
    for dep in $deps; do
        route=$(k get symbolroute -o json 2>/dev/null | jq -r --arg dep "$dep" \
            '.items[]? | select(.spec.activeDeploymentRef == $dep
              or .spec.previousDeploymentRef == $dep) | .metadata.name' || true)
        [ -n "$route" ] && routes="$routes $route"
    done
    routes=$(printf '%s\n' $routes 2>/dev/null | sort -u || true)

    if manager_healthy; then
        for symbol in $symbols; do
            api_post_quiet "/api/v1/symbols/$symbol/shutdown" '{"allowRestart":true}'
        done
    fi

    for dep in $deps; do
        state=$(deployment_field "$dep" '.spec.desiredState')
        if [ -n "$state" ] && [ "$state" != "Disabled" ]; then
            k patch symboldeployment "$dep" --type merge \
                -p '{"spec":{"desiredState":"Disabled"}}' >/dev/null 2>&1 || true
        fi
    done
    for route in $routes; do
        state=$(route_field "$route" '.spec.state')
        if [ -n "$state" ] && [ "$state" != "Disabled" ] && [ "$state" != "Active" ]; then
            k patch symbolroute "$route" --type merge \
                -p '{"spec":{"state":"Disabled"}}' >/dev/null 2>&1 || true
        fi
    done

    start=$SECONDS
    while ((SECONDS - start < 120)); do
        [ "$(pod_count "$shard")" = "0" ] && break
        sleep 2
    done

    if manager_healthy; then
        api_post_quiet "/api/v1/shards/$shard/drop" '{"retainStorage":false}'
    fi

    k delete symbolshard "$shard" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    for dep in $deps; do
        k delete symboldeployment "$dep" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    done
    for route in $routes; do
        k delete symbolroute "$route" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    done
    for symbol in $symbols; do
        k delete tradingsymbol "$symbol" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    done

    k delete sts,svc,pdb,cm,networkpolicy -l "matching.io/cluster=$shard" \
        --ignore-not-found --wait=false >/dev/null 2>&1 || true
    k delete pods -l "matching.io/cluster=$shard" --ignore-not-found \
        --grace-period=10 --timeout=60s >/dev/null 2>&1 || true
    k delete pvc -l "matching.io/cluster=$shard" --ignore-not-found \
        --timeout=120s >/dev/null 2>&1 || true
    local pod
    for pod in 0 1 2 3; do
        k delete pvc "data-$shard-$pod" --ignore-not-found --timeout=60s >/dev/null 2>&1 || true
    done
}

cleanup_all() {
    local shard name
    for shard in "$SHARD_C1" "$SHARD_SUP" "$SHARD_RES" "$SHARD_M1" "$SHARD_M2" "$SHARD_ADOPT"; do
        cleanup_shard "$shard"
    done
    # Catch shards left by an earlier run with a different phase layout.
    for shard in $(k get symbolshard -o json 2>/dev/null |
        jq -r --arg prefix "$PREFIX-" '.items[].metadata.name | select(startswith($prefix))' ||
        true); do
        cleanup_shard "$shard"
    done
    # Retained PVCs outlive the CR by design when the operator uses manual
    # retention (retention-seconds=0). Remove every acceptance-labelled claim,
    # including shards that no longer have a CR.
    for name in $(k get pvc -o json 2>/dev/null |
        jq -r --arg prefix "$PREFIX-"             '.items[] | select((.metadata.labels["matching.io/cluster"] // "") | startswith($prefix))
             | .metadata.name' || true); do
        k delete pvc "$name" --ignore-not-found --timeout=120s >/dev/null 2>&1 || true
    done
    # Last-resort symbol cleanup in case a deployment record was already gone.
    local symbol
    for symbol in "$SYM_C1" "$SYM_SUP" "$SYM_SUP2" "$SYM_MIG" "$SYM_ADOPT"; do
        k delete tradingsymbol "$symbol" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    done
    ok "acceptance resources are gone"
}

create_clean_shard() { # shard
    cleanup_shard "$1"
}

# ---------------------------------------------------------------------------
# shared phase helpers
# ---------------------------------------------------------------------------

ensure_test_order() { # shard container port symbol symbol_id order_id
    local shard=$1 container=$2 port=$3 symbol=$4 symbol_id=$5 order_id=$6
    local leader orders response sequence
    leader=$(wait_leader_pod "$shard" "$container" "$port" 120) ||
        fail "no leader for $shard/$symbol"
    orders=$(engine_orders "$shard" "$leader" "$container" "$port")
    if [ "$orders" = "[]" ] || [ -z "$orders" ]; then
        response=$(place_order "$shard" "$leader" "$container" "$port" \
            "$symbol_id" "$order_id" "1.000" "0.10" "acc-$symbol-1")
        sequence=$(jq -r '.Ok.data.sequence // empty' <<<"$response")
        assert_nonempty "$sequence" "write to $shard/$symbol returns a sequence"
        assert_eq "$(jq -r '.Ok.data.events[0].kind // empty' <<<"$response")" "rested" \
            "order $order_id rests on $shard/$symbol"
        printf '%s\n' "$leader"
    else
        ok "$shard/$symbol already has a resting order"
        printf '%s\n' "$leader"
    fi
}

ensure_containers_ready() {
    if ! api_exists "/api/v1/shards/$SHARD_C1"; then
        create_clean_shard "$SHARD_C1"
        create_shard "$SHARD_C1" containers "$PORT_C1"
        create_symbol "$SHARD_C1" "$SYM_C1" "$SYM_ID_C1" false
    fi
    wait_shard_ready "$SHARD_C1"
    ensure_test_order "$SHARD_C1" "$SYM_C1" "$PORT_C1" "$SYM_C1" "$SYM_ID_C1" 9001 >/dev/null
}

orders_normalized() { # JSON array text
    jq -S -c . <<<"$1"
}

# ---------------------------------------------------------------------------
# phase: containers
# ---------------------------------------------------------------------------

phase_containers() {
    step "phase: containers"
    create_clean_shard "$SHARD_C1"
    create_shard "$SHARD_C1" containers "$PORT_C1"
    assert_render_family "$SHARD_C1" reserved
    create_symbol "$SHARD_C1" "$SYM_C1" "$SYM_ID_C1" false
    assert_render_family "$SHARD_C1" containers
    wait_shard_ready "$SHARD_C1"
    wait_pods_ready "$SHARD_C1" "$REPLICAS"

    local leader health
    leader=$(wait_leader_pod "$SHARD_C1" "$SYM_C1" "$PORT_C1" 120) ||
        fail "no leader after $SHARD_C1 became ready"
    ok "leader is $leader"
    health=$(engine_get "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1" /health)
    assert_eq "$(jq -r '.healthy // false' <<<"$health")" "true" "engine $SHARD_C1/$SYM_C1 is healthy"
    assert_eq "$(jq -r '.ready // false' <<<"$health")" "true" "engine $SHARD_C1/$SYM_C1 is ready"
    assert_eq "$(engine_read "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1" symbol_id)" \
        "$SYM_ID_C1" "engine reports the configured symbol id"
    assert_eq "$(engine_read "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1" orderbook_sequence)" \
        "0" "new book starts at sequence 0"

    ensure_test_order "$SHARD_C1" "$SYM_C1" "$PORT_C1" "$SYM_C1" "$SYM_ID_C1" 9001 >/dev/null
    assert_eq "$(engine_sequence "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")" "1" \
        "book sequence advanced to 1"
    assert_eq "$(engine_orders "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1" | jq -r '.[0].id')" \
        "9001" "the resting order is visible through /read"
}

# ---------------------------------------------------------------------------
# phase: leader transfer
# ---------------------------------------------------------------------------

phase_leader() {
    step "phase: leader transfer"
    ensure_containers_ready
    local leader leader_id target_id target_pod pod pod_id
    local before_sequence before_orders response got sequence

    leader=$(wait_leader_pod "$SHARD_C1" "$SYM_C1" "$PORT_C1" 120) ||
        fail "no leader before transfer"
    leader_id=$(node_field "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1" '.node_id')
    target_id=$(((leader_id + 1) % REPLICAS))
    target_pod=
    for pod in $(pod_names "$SHARD_C1"); do
        pod_id=$(node_field "$SHARD_C1" "$pod" "$SYM_C1" "$PORT_C1" '.node_id')
        if [ "$pod_id" = "$target_id" ]; then
            target_pod=$pod
        fi
    done
    assert_nonempty "$target_pod" "found pod for target voter $target_id"
    before_sequence=$(engine_sequence "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")
    before_orders=$(engine_orders "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")
    assert_eq "$(jq -r 'length' <<<"$before_orders")" "1" \
        "book has an order before the transfer"

    response=$(engine_curl "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1" POST \
        /console/api/actions/transfer-leader "{\"target\":$target_id}" 1)
    assert_eq "$(jq -r '.accepted // false' <<<"$response")" "true" \
        "console accepted the leadership transfer"

    local start=$SECONDS new_leader=
    while :; do
        new_leader=$(wait_leader_pod "$SHARD_C1" "$SYM_C1" "$PORT_C1" 1 2>/dev/null || true)
        if [ -n "$new_leader" ] &&
            [ "$(node_field "$SHARD_C1" "$new_leader" "$SYM_C1" "$PORT_C1" '.node_id')" = "$target_id" ]; then
            break
        fi
        if ((SECONDS - start >= 90)); then
            fail "leadership did not move to voter $target_id"
        fi
        sleep 2
    done
    ok "leadership moved to voter $target_id on $new_leader"
    assert_eq "$(node_field "$SHARD_C1" "$new_leader" "$SYM_C1" "$PORT_C1" '.leadership.leader_id')" \
        "$target_id" "every node reports the new leader id"

    sequence=$(engine_sequence "$SHARD_C1" "$new_leader" "$SYM_C1" "$PORT_C1")
    got=$(engine_orders "$SHARD_C1" "$new_leader" "$SYM_C1" "$PORT_C1")
    assert_eq "$sequence" "$before_sequence" "sequence is unchanged after the transfer"
    assert_eq "$(orders_normalized "$got")" "$(orders_normalized "$before_orders")" \
        "the book is identical on the new leader"
}

# ---------------------------------------------------------------------------
# phase: image rollout
# ---------------------------------------------------------------------------

ROLL_PERCENTS=
ROLL_UPDATED=
ROLL_PROGRESS=
ROLL_MIN_READY=0
ROLL_COMPLETE=false
ROLL_DETAIL=

roll_count_replaced() { # shard before_file
    local shard=$1 before=$2 pods name uid new_uid count=0
    pods=$(pod_json "$shard")
    while IFS=' ' read -r name uid; do
        [ -n "$name" ] || continue
        new_uid=$(jq -r --arg name "$name" \
            '.items[]? | select(.metadata.name == $name) | .metadata.uid' \
            <<<"$pods" 2>/dev/null | head -1)
        if [ -n "$new_uid" ] && [ "$new_uid" != "$uid" ]; then
            count=$((count + 1))
        fi
    done <"$before"
    printf '%s\n' "$count"
}

# Wait for the *pods*, not just the manager status. The StatefulSet status can
# still describe the previous revision right after the upgrade is accepted, so
# `rollout.complete` alone would return before a single pod moved.
roll_wait() { # shard replicas expected_image before_file timeout
    local shard=$1 replicas=$2 image=$3 before=$4 timeout=$5
    local start=$SECONDS updated ready total percent complete detail sts
    local replaced ready_now all_image name pod_image
    ROLL_PERCENTS=
    ROLL_UPDATED=
    ROLL_PROGRESS=
    ROLL_MIN_READY=$replicas
    ROLL_COMPLETE=false
    ROLL_DETAIL=
    while :; do
        sts=$(k get sts "$shard" -o json 2>/dev/null || true)
        if [ -n "$sts" ]; then
            updated=$(jq -r '.status.updatedReplicas // empty' <<<"$sts")
            ready=$(jq -r '.status.readyReplicas // empty' <<<"$sts")
            total=$(jq -r '.spec.replicas // empty' <<<"$sts")
            if [ -n "$updated" ] && [ -n "$total" ]; then
                ROLL_UPDATED="$ROLL_UPDATED $updated"
                if [ "$total" -ne "$replicas" ]; then
                    fail "StatefulSet $shard changed replicas to $total during the rollout"
                fi
            fi
        fi
        replaced=$(roll_count_replaced "$shard" "$before")
        ROLL_PROGRESS="$ROLL_PROGRESS $replaced"
        ready_now=$(ready_pod_count "$shard")
        if [ "$ready_now" -lt "$ROLL_MIN_READY" ]; then
            ROLL_MIN_READY=$ready_now
        fi
        detail=$(shard_detail "$shard" 2>/dev/null || true)
        percent=$(jq -r '.shard.status.rollout.percent // empty' <<<"$detail" 2>/dev/null || true)
        complete=$(jq -r '.shard.status.rollout.complete // false' <<<"$detail" 2>/dev/null || true)
        if [ -n "$percent" ]; then
            ROLL_PERCENTS="$ROLL_PERCENTS $percent"
        fi
        ROLL_COMPLETE=$complete
        ROLL_DETAIL=$detail
        all_image=true
        while read -r name pod_image; do
            [ -n "$name" ] || continue
            if [ "$pod_image" != "$image" ]; then
                all_image=false
            fi
        done <<<"$(pod_image_map "$shard")"
        if [ "$replaced" = "$replicas" ] && [ "$ready_now" = "$replicas" ] &&
            [ "$all_image" = "true" ] && [ "$complete" = "true" ]; then
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            return 1
        fi
        sleep 0.5
    done
}

assert_rollout_progress() { # replicas
    local replicas=$1 intermediate=0 last=-1 value percent_intermediate=0
    for value in $ROLL_PROGRESS; do
        if [ "$value" -lt "$last" ]; then
            fail "pod replacement progress went backwards:$ROLL_PROGRESS"
        fi
        last=$value
        if [ "$value" -gt 0 ] && [ "$value" -lt "$replicas" ]; then
            intermediate=1
        fi
    done
    [ "$intermediate" = "1" ] ||
        fail "the rollout did not replace pods one at a time (pod replacement progress:$ROLL_PROGRESS)"
    ok "pod replacements progressed one at a time (replaced:$(compact_series "$ROLL_PROGRESS"))"
    [ "$ROLL_MIN_READY" -ge "$((replicas - 1))" ] ||
        fail "rollout dropped ready pods to $ROLL_MIN_READY/$replicas"
    ok "ready pods stayed quorum-safe (minimum $ROLL_MIN_READY/$replicas)"
    for value in $ROLL_PERCENTS; do
        if [ "$value" -gt 0 ] && [ "$value" -lt 100 ]; then
            percent_intermediate=1
        fi
    done
    if [ "$percent_intermediate" = "1" ]; then
        ok "manager reported intermediate rollout percentages (percent:$(compact_series "$ROLL_PERCENTS"))"
    else
        note "manager status skipped intermediate percentages (percent:$(compact_series "$ROLL_PERCENTS")); pod replacements were still one at a time"
    fi
}

assert_pods_on_image() { # shard expected_image
    local shard=$1 expected=$2 name image
    while read -r name image; do
        [ -n "$name" ] || continue
        if [ "$image" != "$expected" ]; then
            fail "pod $name runs image '$image', expected '$expected'"
        fi
    done <<<"$(pod_image_map "$shard")"
    ok "every pod runs $expected"
}

assert_pods_replaced() { # shard before_map
    local shard=$1 before=$2 name uid new_uid
    while read -r name uid; do
        [ -n "$name" ] || continue
        new_uid=$(k get pod "$name" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
        if [ -z "$new_uid" ]; then
            fail "pod $name is missing after the rollout"
        fi
        if [ "$new_uid" = "$uid" ]; then
            fail "pod $name was not replaced (uid $uid)"
        fi
    done <<<"$before"
    ok "every pod was replaced (UIDs changed)"
}

compact_series() { # values
    local out= last= value first=1
    for value in $1; do
        if [ "$first" = "1" ] || [ "$value" != "$last" ]; then
            out="$out $value"
            last=$value
            first=0
        fi
    done
    printf '%s\n' "$out"
}

phase_roll() {
    step "phase: digest-pinned image rollout and rollback"
    ensure_containers_ready
    local tag digest images response before_uids before_sequence
    tag=$(shard_field "$SHARD_C1" '.shard.spec.image')
    images=$(api_get "/api/v1/images?shard=$SHARD_C1")
    digest=$(jq -r '.items[] | select(.current == true and .digest != null) | .reference' <<<"$images" | head -1)
    assert_nonempty "$digest" "image inventory returned a digest-pinned reference for $tag"
    note "tag:    $tag"
    note "digest: $digest"
    if [ "$digest" = "$tag" ]; then
        fail "the digest reference equals the tag; the roll would not change the StatefulSet"
    fi

    local leader
    leader=$(wait_leader_pod "$SHARD_C1" "$SYM_C1" "$PORT_C1" 120) ||
        fail "no leader before the rollout"
    before_sequence=$(engine_sequence "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")
    before_uids=$(pod_uid_map "$SHARD_C1")
    assert_nonempty "$before_uids" "recorded the pre-rollout pod UIDs"
    printf '%s\n' "$before_uids" >"$TMP_DIR/roll-before-digest.tsv"

    local since
    since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    response=$(api_post "/api/v1/shards/$SHARD_C1/upgrade" "$(jq -cn \
        --arg image "$digest" \
        '{image:$image,release:{wireVersion:7,snapshotVersion:11,mixedVersionSafe:false}}')")
    assert_eq "$(jq -r '.upgrade.image // empty' <<<"$response")" "$digest" \
        "manager recorded the digest-pinned image"

    if ! roll_wait "$SHARD_C1" "$REPLICAS" "$digest" \
        "$TMP_DIR/roll-before-digest.tsv" "$ROLL_TIMEOUT"; then
        fail "digest rollout of $SHARD_C1 did not complete within ${ROLL_TIMEOUT}s"
    fi
    ok "digest rollout completed"
    assert_rollout_progress "$REPLICAS"
    assert_eq "$(jq -r '.shard.status.rollout.complete' <<<"$ROLL_DETAIL")" "true" \
        "manager reports rollout.complete=true"
    assert_eq "$(jq -r '.shard.status.rollout.percent' <<<"$ROLL_DETAIL")" "100" \
        "manager reports rollout.percent=100"
    assert_pods_on_image "$SHARD_C1" "$digest"
    assert_pods_replaced "$SHARD_C1" "$before_uids"
    wait_pods_ready "$SHARD_C1" "$REPLICAS"

    local logs deletions
    logs=$(kubectl -n "$CONTROL_NS" logs "deploy/$OPERATOR_DEPLOY" --since-time="$since" 2>/dev/null || true)
    deletions=$(printf '%s\n' "$logs" | grep -c "deleted one outdated pod for shard rollout" || true)
    [ -n "$deletions" ] || deletions=0
    if [ "$deletions" -lt 1 ]; then
        fail "operator log has no one-pod-at-a-time rollout deletion since $since"
    fi
    ok "operator logged $deletions one-pod-at-a-time deletion(s)"
    if printf '%s\n' "$logs" | grep -q "transferred leadership before rollout"; then
        ok "operator transferred leadership before deleting a leader"
    else
        note "no leader transfer was needed during this roll"
    fi

    leader=$(wait_leader_pod "$SHARD_C1" "$SYM_C1" "$PORT_C1" 120) ||
        fail "no leader after the digest rollout"
    assert_eq "$(engine_sequence "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")" "$before_sequence" \
        "book sequence survived the digest rollout"
    assert_eq "$(engine_orders "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1" | jq -r '.[0].id')" \
        "9001" "the resting order survived the digest rollout"

    step "phase: roll back to the tag"
    before_uids=$(pod_uid_map "$SHARD_C1")
    printf '%s\n' "$before_uids" >"$TMP_DIR/roll-before-tag.tsv"
    response=$(api_post "/api/v1/shards/$SHARD_C1/upgrade" "$(jq -cn \
        --arg image "$tag" \
        '{image:$image,release:{wireVersion:7,snapshotVersion:11,mixedVersionSafe:false}}')")
    assert_eq "$(jq -r '.upgrade.image // empty' <<<"$response")" "$tag" \
        "manager recorded the tag rollback"
    if ! roll_wait "$SHARD_C1" "$REPLICAS" "$tag" \
        "$TMP_DIR/roll-before-tag.tsv" "$ROLL_TIMEOUT"; then
        fail "rollback of $SHARD_C1 did not complete within ${ROLL_TIMEOUT}s"
    fi
    ok "tag rollback completed"
    assert_rollout_progress "$REPLICAS"
    assert_pods_on_image "$SHARD_C1" "$tag"
    assert_pods_replaced "$SHARD_C1" "$before_uids"
    wait_pods_ready "$SHARD_C1" "$REPLICAS"
    leader=$(wait_leader_pod "$SHARD_C1" "$SYM_C1" "$PORT_C1" 120) ||
        fail "no leader after the rollback"
    assert_eq "$(engine_sequence "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")" "$before_sequence" \
        "book sequence survived the rollback"
    assert_eq "$(engine_orders "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1" | jq -r '.[0].id')" \
        "9001" "the resting order survived the rollback"
}

# ---------------------------------------------------------------------------
# phase: stop and start
# ---------------------------------------------------------------------------

phase_stopstart() {
    step "phase: stop and start"
    ensure_containers_ready
    local leader before_sequence before_orders before_pvcs after_pvcs response
    leader=$(wait_leader_pod "$SHARD_C1" "$SYM_C1" "$PORT_C1" 120) ||
        fail "no leader before shutdown"
    before_sequence=$(engine_sequence "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")
    before_orders=$(engine_orders "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")
    before_pvcs=$(pvc_uid_map "$SHARD_C1")
    assert_nonempty "$before_pvcs" "recorded the PVC identities"

    response=$(api_post "/api/v1/symbols/$SYM_C1/shutdown" '{}')
    assert_eq "$(jq -r '.shutdown.deployment // empty' <<<"$response")" "$SYM_C1-g1" \
        "shutdown targeted $SYM_C1-g1"
    assert_eq "$(jq -r '.shutdown.zeroImpact // false' <<<"$response")" "true" \
        "shutdown reports zero impact"
    wait_deployment_disabled "$SYM_C1-g1"
    assert_eq "$(route_field "$SYM_C1" '.spec.state')" "Disabled" \
        "route is Disabled during shutdown"
    wait_pods_gone "$SHARD_C1" 180

    after_pvcs=$(pvc_uid_map "$SHARD_C1")
    assert_eq "$after_pvcs" "$before_pvcs" "PVC identities survived shutdown"
    if k get sts "$SHARD_C1" >/dev/null 2>&1; then
        fail "StatefulSet $SHARD_C1 still exists after a complete shutdown"
    fi
    ok "StatefulSet was pruned while the PVCs stayed"

    response=$(api_post "/api/v1/symbols/$SYM_C1/start" '{}')
    assert_eq "$(jq -r '.start.deployment // empty' <<<"$response")" "$SYM_C1-g1" \
        "start targeted $SYM_C1-g1"
    wait_shard_ready "$SHARD_C1"
    wait_pods_ready "$SHARD_C1" "$REPLICAS"
    leader=$(wait_leader_pod "$SHARD_C1" "$SYM_C1" "$PORT_C1" 120) ||
        fail "no leader after start"
    assert_eq "$(pvc_uid_map "$SHARD_C1")" "$before_pvcs" \
        "PVC identities survived start"
    assert_eq "$(engine_sequence "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")" "$before_sequence" \
        "book sequence survived stop/start"
    assert_eq "$(orders_normalized "$(engine_orders "$SHARD_C1" "$leader" "$SYM_C1" "$PORT_C1")")" \
        "$(orders_normalized "$before_orders")" "the book survived stop/start"
}

# ---------------------------------------------------------------------------
# phase: supervisor
# ---------------------------------------------------------------------------

supervisor_json() { # pod
    kubectl -n "$NAMESPACE" exec "$1" -c supervisor -- \
        curl -sS -m 5 http://127.0.0.1:9090/processes
}

supervisor_pid() { # pod name
    supervisor_json "$1" | jq -r --arg name "$2" '.running[] | select(.name == $name) | .pid' | head -1
}

supervisor_running_names() { # pod
    supervisor_json "$1" | jq -r '.running[].name' | sort
}

wait_supervisor_names() { # pod expected_names_csv timeout
    local pod=$1 expected=$2 timeout=${3:-300}
    local start=$SECONDS got
    while :; do
        got=$(supervisor_running_names "$pod" 2>/dev/null | paste -sd, - || true)
        if [ "$got" = "$expected" ]; then
            ok "supervisor runs $expected"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "supervisor running processes are '$got', expected '$expected'"
        fi
        sleep 2
    done
}

wait_config_processes() { # shard count timeout
    local shard=$1 count=$2 timeout=${3:-300}
    local start=$SECONDS got
    while :; do
        got=$(k get cm "$shard-shard" -o json 2>/dev/null |
            jq -r '.data["processes.json"] // "{}" | fromjson | .processes | length' 2>/dev/null || true)
        if [ "$got" = "$count" ]; then
            ok "process ConfigMap lists $count process(es)"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "process ConfigMap lists '$got' process(es), expected $count"
        fi
        sleep 2
    done
}

phase_supervisor() {
    step "phase: supervisor"
    create_clean_shard "$SHARD_SUP"
    create_shard "$SHARD_SUP" supervisor "$PORT_SUP"
    create_symbol "$SHARD_SUP" "$SYM_SUP" "$SYM_ID_SUP" false
    assert_render_family "$SHARD_SUP" supervisor
    wait_shard_ready "$SHARD_SUP"
    wait_pods_ready "$SHARD_SUP" "$REPLICAS"

    local pod supervisor_pod pid1 leader2 response
    supervisor_pod=$(pod_names "$SHARD_SUP" | head -1)
    assert_nonempty "$supervisor_pod" "found a supervisor pod"
    wait_supervisor_names "$supervisor_pod" "$SYM_SUP" 300
    pid1=$(supervisor_pid "$supervisor_pod" "$SYM_SUP")
    assert_nonempty "$pid1" "recorded the $SYM_SUP child pid"

    ensure_test_order "$SHARD_SUP" "supervisor" "$PORT_SUP" "$SYM_SUP" "$SYM_ID_SUP" 9101 >/dev/null

    step "phase: supervisor zero-impact add"
    create_symbol "$SHARD_SUP" "$SYM_SUP2" "$SYM_ID_SUP2" false
    wait_config_processes "$SHARD_SUP" 2 300
    assert_render_family "$SHARD_SUP" supervisor
    wait_supervisor_names "$supervisor_pod" "$SYM_SUP,$SYM_SUP2" 300
    assert_eq "$(supervisor_pid "$supervisor_pod" "$SYM_SUP")" "$pid1" \
        "adding $SYM_SUP2 did not restart $SYM_SUP"
    wait_deployment_ready "$SYM_SUP2-g1" 300
    leader2=$(wait_leader_pod "$SHARD_SUP" "supervisor" "$PORT_SUP2" 180) ||
        fail "no leader for $SYM_SUP2"
    assert_eq "$(engine_read "$SHARD_SUP" "$leader2" "supervisor" "$PORT_SUP2" symbol_id)" \
        "$SYM_ID_SUP2" "$SYM_SUP2 reports its own symbol id"
    ensure_test_order "$SHARD_SUP" "supervisor" "$PORT_SUP2" "$SYM_SUP2" "$SYM_ID_SUP2" 9102 >/dev/null

    local leader1
    leader1=$(wait_leader_pod "$SHARD_SUP" "supervisor" "$PORT_SUP" 120) ||
        fail "no leader for $SYM_SUP after adding $SYM_SUP2"
    assert_eq "$(engine_sequence "$SHARD_SUP" "$leader1" "supervisor" "$PORT_SUP")" "1" \
        "$SYM_SUP kept serving while $SYM_SUP2 started"

    step "phase: supervisor zero-impact stop"
    response=$(api_post "/api/v1/symbols/$SYM_SUP2/shutdown" '{}')
    assert_eq "$(jq -r '.shutdown.zeroImpact // false' <<<"$response")" "true" \
        "supervisor shutdown reports zero impact"
    wait_supervisor_names "$supervisor_pod" "$SYM_SUP" 300
    assert_eq "$(supervisor_pid "$supervisor_pod" "$SYM_SUP")" "$pid1" \
        "stopping $SYM_SUP2 did not restart $SYM_SUP"
    wait_deployment_disabled "$SYM_SUP2-g1"
    leader1=$(wait_leader_pod "$SHARD_SUP" "supervisor" "$PORT_SUP" 120) ||
        fail "no leader for $SYM_SUP after stopping $SYM_SUP2"
    assert_eq "$(engine_sequence "$SHARD_SUP" "$leader1" "supervisor" "$PORT_SUP")" "1" \
        "$SYM_SUP still serves after $SYM_SUP2 stopped"

    step "phase: supervisor restart"
    response=$(api_post "/api/v1/symbols/$SYM_SUP2/start" '{}')
    assert_eq "$(jq -r '.start.deployment // empty' <<<"$response")" "$SYM_SUP2-g1" \
        "start targeted $SYM_SUP2-g1"
    wait_supervisor_names "$supervisor_pod" "$SYM_SUP,$SYM_SUP2" 300
    assert_eq "$(supervisor_pid "$supervisor_pod" "$SYM_SUP")" "$pid1" \
        "restarting $SYM_SUP2 did not restart $SYM_SUP"
    leader2=$(wait_leader_pod "$SHARD_SUP" "supervisor" "$PORT_SUP2" 180) ||
        fail "no leader for $SYM_SUP2 after restart"
    assert_eq "$(engine_sequence "$SHARD_SUP" "$leader2" "supervisor" "$PORT_SUP2")" "1" \
        "$SYM_SUP2 recovered its retained order"
}

# ---------------------------------------------------------------------------
# phase: reserved
# ---------------------------------------------------------------------------

phase_reserved() {
    step "phase: reserved shard"
    create_clean_shard "$SHARD_RES"
    create_shard "$SHARD_RES" containers "$PORT_RES"
    assert_render_family "$SHARD_RES" reserved
    wait_shard_phase "$SHARD_RES" Reserved 120
    if k get sts "$SHARD_RES" >/dev/null 2>&1; then
        fail "reserved shard unexpectedly has a StatefulSet"
    fi
    ok "reserved shard has no StatefulSet"

    local response
    response=$(api_post "/api/v1/shards/$SHARD_RES/drop" '{"retainStorage":false}')
    assert_eq "$(jq -r '.drop.mode // empty' <<<"$response")" "reserved" \
        "drop reports reserved mode"
    assert_eq "$(jq -r '.drop.zeroImpact // false' <<<"$response")" "true" \
        "dropping a reserved shard has zero impact"
    local start=$SECONDS
    while k get symbolshard "$SHARD_RES" >/dev/null 2>&1; do
        if ((SECONDS - start >= 120)); then
            fail "reserved shard CR was not removed"
        fi
        sleep 2
    done
    ok "reserved shard record was removed"
}

# ---------------------------------------------------------------------------
# phase: migration
# ---------------------------------------------------------------------------

wait_route_cutover() { # route source target timeout
    local route=$1 source=$2 target=$3 timeout=${4:-$TIMEOUT}
    local start=$SECONDS active previous state source_state
    while :; do
        active=$(route_field "$route" '.spec.activeDeploymentRef')
        previous=$(route_field "$route" '.spec.previousDeploymentRef')
        state=$(route_field "$route" '.spec.state')
        source_state=$(deployment_field "$source" '.spec.desiredState')
        if [ "$active" = "$target" ] && [ "$previous" = "$source" ] &&
            [ "$state" = "Active" ] && [ "$source_state" = "Draining" ]; then
            ok "route $route cut over to $target and drains $source"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "route $route did not cut over (active=$active previous=$previous state=$state source=$source_state)"
        fi
        sleep 2
    done
}

wait_route_rollback() { # route source target timeout
    local route=$1 source=$2 target=$3 timeout=${4:-$TIMEOUT}
    local start=$SECONDS active previous state target_state
    while :; do
        active=$(route_field "$route" '.spec.activeDeploymentRef')
        previous=$(route_field "$route" '.spec.previousDeploymentRef')
        state=$(route_field "$route" '.spec.state')
        target_state=$(deployment_field "$target" '.spec.desiredState')
        if [ "$active" = "$source" ] && { [ -z "$previous" ] || [ "$previous" = "null" ]; } &&
            [ "$state" = "Active" ] && [ "$target_state" = "Disabled" ]; then
            ok "route $route rolled back to $source and disabled $target"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "route $route did not roll back (active=$active previous=$previous state=$state target=$target_state)"
        fi
        sleep 2
    done
}

phase_migration() {
    step "phase: migration"
    create_clean_shard "$SHARD_M1"
    create_clean_shard "$SHARD_M2"
    create_shard "$SHARD_M1" containers "$PORT_M1"
    create_shard "$SHARD_M2" containers "$PORT_M2"
    create_symbol "$SHARD_M1" "$SYM_MIG" "$SYM_ID_MIG" false
    wait_shard_ready "$SHARD_M1"
    ensure_test_order "$SHARD_M1" "$SYM_MIG" "$PORT_M1" "$SYM_MIG" "$SYM_ID_MIG" 9201 >/dev/null

    local source_dep="$SYM_MIG-g1" shadow="$SYM_MIG-shadow"
    local leader source_sequence response
    leader=$(wait_leader_pod "$SHARD_M1" "$SYM_MIG" "$PORT_M1" 120) ||
        fail "no leader on the migration source"
    source_sequence=$(engine_sequence "$SHARD_M1" "$leader" "$SYM_MIG" "$PORT_M1")

    response=$(api_post "/api/v1/symbols/$SYM_MIG/migrate" \
        "$(jq -cn --arg target "$SHARD_M2" '{targetShard:$target}')")
    assert_eq "$(jq -r '.migration.targetShard // empty' <<<"$response")" "$SHARD_M2" \
        "migration targets $SHARD_M2"
    assert_eq "$(jq -r '.migration.targetDeployment // empty' <<<"$response")" "$shadow" \
        "migration created $shadow"
    assert_eq "$(jq -r '.migration.targetHttpPort // empty' <<<"$response")" "$PORT_M2" \
        "shadow uses port $PORT_M2"
    assert_eq "$(deployment_field "$shadow" '.spec.raftIdOffset')" "$REPLICAS" \
        "shadow Raft IDs start after the source generation"
    assert_eq "$(deployment_field "$shadow" '.spec.desiredState')" "Shadow" \
        "shadow starts in Shadow state"

    wait_route_cutover "$SYM_MIG" "$source_dep" "$shadow" "$TIMEOUT"
    wait_deployment_ready "$shadow" "$TIMEOUT"

    local target_leader target_sequence
    target_leader=$(wait_leader_pod "$SHARD_M2" "$SYM_MIG" "$PORT_M2" 180) ||
        fail "no leader on the migration target"
    target_sequence=$(engine_sequence "$SHARD_M2" "$target_leader" "$SYM_MIG" "$PORT_M2")
    assert_eq "$target_sequence" "$source_sequence" \
        "the target took over at the same sequence"
    assert_eq "$(engine_orders "$SHARD_M2" "$target_leader" "$SYM_MIG" "$PORT_M2" | jq -r '.[0].id')" \
        "9201" "the target serves the migrated book"

    local write
    write=$(place_order "$SHARD_M2" "$target_leader" "$SYM_MIG" "$PORT_M2" \
        "$SYM_ID_MIG" 9202 "1.100" "0.20" "acc-$SYM_MIG-2")
    # `orderbook_sequence` is the next sequence, so a write is assigned the
    # current value and the counter advances afterwards.
    assert_eq "$(jq -r '.Ok.data.sequence // empty' <<<"$write")" "$target_sequence" \
        "the target accepts a new write at the current sequence"
    assert_eq "$(engine_sequence "$SHARD_M2" "$target_leader" "$SYM_MIG" "$PORT_M2")" \
        "$((target_sequence + 1))" "the target sequence advances after the write"
    assert_eq "$(engine_orders "$SHARD_M2" "$target_leader" "$SYM_MIG" "$PORT_M2" | jq -r 'length')" \
        "2" "the target book holds both the migrated and the new order"

    step "phase: migration rollback"
    k patch symbolroute "$SYM_MIG" --type merge -p '{"spec":{"state":"Rollback"}}' >/dev/null
    wait_route_rollback "$SYM_MIG" "$source_dep" "$shadow" "$TIMEOUT"
    wait_deployment_ready "$source_dep" "$TIMEOUT"
    leader=$(wait_leader_pod "$SHARD_M1" "$SYM_MIG" "$PORT_M1" 180) ||
        fail "no leader on the migration source after rollback"
    local orders
    orders=$(engine_orders "$SHARD_M1" "$leader" "$SYM_MIG" "$PORT_M1")
    assert_eq "$(jq -r 'length' <<<"$orders")" "2" \
        "the source serves both orders after rollback"
    assert_eq "$(engine_sequence "$SHARD_M1" "$leader" "$SYM_MIG" "$PORT_M1")" "$((target_sequence + 1))" \
        "the source caught up to the post-cutover write"
    assert_eq "$(deployment_field "$shadow" '.spec.desiredState')" "Disabled" \
        "the shadow is disabled after rollback"
}

# ---------------------------------------------------------------------------
# phase: adoption
# ---------------------------------------------------------------------------

wait_operator_replicas() { # replicas timeout
    local want=$1 timeout=$2
    local start=$SECONDS got
    while :; do
        got=$(kubectl -n "$CONTROL_NS" get deployment "$OPERATOR_DEPLOY" \
            -o jsonpath='{.status.availableReplicas}' 2>/dev/null || true)
        [ -n "$got" ] || got=0
        if [ "$got" = "$want" ]; then
            ok "operator has $want available replica(s)"
            return 0
        fi
        if ((SECONDS - start >= timeout)); then
            fail "operator has $got available replica(s), expected $want"
        fi
        sleep 2
    done
}

apply_manifests() { # shard
    local shard=$1 manifest count i
    manifest=$(api_get "/api/v1/shards/$shard/manifests?profile=local")
    count=$(jq '.items | length' <<<"$manifest")
    assert_eq "$count" "6" "$shard render has six objects to adopt"
    for i in $(seq 0 $((count - 1))); do
        jq -c ".items[$i]" <<<"$manifest" | k apply -f - >/dev/null
    done
    ok "applied $count rendered objects without the operator"
}

phase_adoption() {
    step "phase: adoption"
    create_clean_shard "$SHARD_ADOPT"

    step "phase: adoption operator paused"
    OPERATOR_SCALED=1
    kubectl -n "$CONTROL_NS" scale deployment "$OPERATOR_DEPLOY" --replicas=0 >/dev/null
    wait_operator_replicas 0 120

    create_shard "$SHARD_ADOPT" containers "$PORT_ADOPT"
    local placement
    placement=$(create_symbol_raw "$SHARD_ADOPT" "$SYM_ADOPT" "$SYM_ID_ADOPT" false)
    assert_eq "$(jq -r '.placement.shard // empty' <<<"$placement")" "$SHARD_ADOPT" \
        "symbol $SYM_ADOPT placed on $SHARD_ADOPT while the operator was down"
    assert_render_family "$SHARD_ADOPT" containers

    apply_manifests "$SHARD_ADOPT"
    wait_pods_running "$SHARD_ADOPT" "$REPLICAS" 180

    local sts_uid pvcs pods
    sts_uid=$(k get sts "$SHARD_ADOPT" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
    pvcs=$(pvc_uid_map "$SHARD_ADOPT")
    pods=$(pod_uid_map "$SHARD_ADOPT")
    assert_nonempty "$sts_uid" "recorded the adopted StatefulSet UID"
    assert_nonempty "$pvcs" "recorded the adopted PVC identities"
    assert_nonempty "$pods" "recorded the adopted pod UIDs"

    step "phase: adoption operator resumed"
    kubectl -n "$CONTROL_NS" scale deployment "$OPERATOR_DEPLOY" --replicas=1 >/dev/null
    wait_operator_replicas 1 120
    OPERATOR_SCALED=0
    wait_shard_ready "$SHARD_ADOPT"
    wait_pods_ready "$SHARD_ADOPT" "$REPLICAS"

    assert_eq "$(k get sts "$SHARD_ADOPT" -o jsonpath='{.metadata.uid}')" "$sts_uid" \
        "the operator adopted the existing StatefulSet without recreation"
    assert_eq "$(pvc_uid_map "$SHARD_ADOPT")" "$pvcs" \
        "the operator adopted the existing PVCs without recreation"
    assert_eq "$(pod_uid_map "$SHARD_ADOPT")" "$pods" \
        "the operator adopted the existing pods without restarting them"

    ensure_test_order "$SHARD_ADOPT" "$SYM_ADOPT" "$PORT_ADOPT" "$SYM_ADOPT" "$SYM_ID_ADOPT" 9301 >/dev/null
}

# ---------------------------------------------------------------------------
# render command
# ---------------------------------------------------------------------------

assert_order_systems() {
    local list count name manifest shards
    list=$(api_get /api/v1/order-systems)
    count=$(jq -r '.items | length' <<<"$list")
    if [ "$count" = "0" ]; then
        note "no order systems exist; skipping the order-system render checks"
        return 0
    fi
    for name in $(jq -r '.items[].name' <<<"$list"); do
        shards=$(jq -r --arg name "$name" '.items[] | select(.name == $name) | .spec.shards | length' <<<"$list")
        manifest=$(api_get "/api/v1/order-systems/$name/manifests")
        assert_eq "$(jq -r '[.items[] | select(.kind == "Deployment")] | length' <<<"$manifest")" \
            "$shards" "$name renders one Deployment per user shard"
        assert_eq "$(jq -r '[.items[] | select(.kind == "HorizontalPodAutoscaler")] | length' <<<"$manifest")" \
            "$shards" "$name renders one HPA per user shard"
        jq -e '[.items[] | select(.kind == "HorizontalPodAutoscaler")]
            | length > 0
              and all(.[]; .spec.minReplicas == 1
                and .spec.metrics[0].resource.target.averageUtilization == 70)' \
            <<<"$manifest" >/dev/null ||
            fail "$name HPA must keep a one-replica floor and a 70% CPU target"
        ok "$name HPAs keep a one-replica floor and a 70% CPU target"
        jq -e '[.items[] | select(.kind == "Deployment")]
            | length > 0
              and all(.[]; .spec.strategy.rollingUpdate.maxUnavailable == 0
                and .spec.strategy.rollingUpdate.maxSurge == 1
                and (.spec | has("replicas") | not))' \
            <<<"$manifest" >/dev/null ||
            fail "$name Deployments must roll one surge pod, never go unavailable, and leave replicas to the HPA"
        ok "$name Deployments leave replicas to the HPA and roll one surge pod"
    done
}

assert_hpa_floors() {
    local bad
    bad=$(k get hpa -o json 2>/dev/null |
        jq -r '[.items[]? | select((.spec.minReplicas // 1) != 1)] | length' 2>/dev/null || true)
    [ -n "$bad" ] || bad=0
    assert_eq "$bad" "0" "every live HPA keeps a one-replica floor"
}

family_of_shard() { # shard
    local json mode
    json=$(k get symbolshard "$1" -o json 2>/dev/null || true)
    [ -n "$json" ] || fail "unknown shard $1"
    mode=$(jq -r '.spec.packing.mode' <<<"$json")
    if [ "$mode" = "supervisor" ]; then
        printf 'supervisor\n'
        return
    fi
    if [ "$(jq -r '.status.phase // empty' <<<"$json")" = "Reserved" ] &&
        [ -z "$(deployments_on_shard "$1")" ]; then
        printf 'reserved\n'
        return
    fi
    printf 'containers\n'
}

cmd_render() {
    step "render assertions"
    if [ -n "$RENDER_SHARD" ]; then
        local family=${RENDER_EXPECT:-}
        [ -n "$family" ] || family=$(family_of_shard "$RENDER_SHARD")
        assert_render_family "$RENDER_SHARD" "$family"
        ok "rendered templates for $RENDER_SHARD ($family)"
        return
    fi

    local shards containers supervisor reserved
    shards=$(k get symbolshard -o json)
    containers=$(jq -r '.items[] | select(.spec.packing.mode == "containers"
        and .status.phase == "Ready") | .metadata.name' <<<"$shards" | head -1)
    supervisor=$(jq -r '.items[] | select(.spec.packing.mode == "supervisor"
        and .status.phase == "Ready") | .metadata.name' <<<"$shards" | head -1)
    reserved=$(jq -r '.items[] | select(.status.phase == "Reserved") | .metadata.name' \
        <<<"$shards" | head -1)

    assert_nonempty "$containers" "found a containers shard to render"
    assert_render_family "$containers" containers
    ok "rendered templates for containers shard $containers"

    assert_nonempty "$supervisor" "found a supervisor shard to render"
    assert_render_family "$supervisor" supervisor
    ok "rendered templates for supervisor shard $supervisor"

    if [ -n "$reserved" ]; then
        assert_render_family "$reserved" reserved
        ok "rendered templates for reserved shard $reserved"
    else
        note "no reserved shard exists; reserved rendering is covered by the run phase"
    fi

    assert_order_systems
    assert_hpa_floors
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

check_symbol_ids() {
    local used id name
    used=$(k get tradingsymbol -o json 2>/dev/null |
        jq -r '.items[]? | "\(.spec.symbolId) \(.metadata.name)"' || true)
    for id in "$SYM_ID_C1" "$SYM_ID_SUP" "$SYM_ID_SUP2" "$SYM_ID_MIG" "$SYM_ID_ADOPT"; do
        name=$(awk -v id="$id" '$1 == id { print $2; exit }' <<<"$used")
        if [ -n "$name" ] && [ "${name#"$PREFIX"}" = "$name" ]; then
            fail "symbol id $id is already used by $name; choose another --prefix or clean it up"
        fi
    done
    ok "symbol ids $SYM_ID_C1..$SYM_ID_ADOPT are free for acceptance"
}

print_kept() {
    step "kept acceptance resources"
    note "shards: $SHARD_C1 $SHARD_SUP $SHARD_RES $SHARD_M1 $SHARD_M2 $SHARD_ADOPT"
    note "remove them with: ./scripts/operator-acceptance.sh cleanup"
    if [ -n "${PF_PID:-}" ]; then
        note "manager port-forward pid $PF_PID is stopped when this process exits"
    fi
}

run_phases() {
    check_symbol_ids
    cleanup_all
    local phase
    for phase in containers leader roll stopstart supervisor reserved migration adoption; do
        phase_selected "$phase" || continue
        "phase_$phase"
    done
}

main() {
    case "$ACTION" in
    cleanup)
        MANAGER=${MANAGER_OVERRIDE:-http://127.0.0.1:$LOCAL_PORT}
        cleanup_all
        CLEANED=1
        return
        ;;
    esac
    ensure_manager
    case "$ACTION" in
    run)
        run_phases
        if [ "$KEEP" = "1" ]; then
            print_kept
        else
            step "cleanup"
            cleanup_all
            CLEANED=1
        fi
        step "result"
        ok "acceptance completed in $((SECONDS - STARTED_AT))s"
        note "phases: $PHASES"
        note "temporary logs: $TMP_DIR"
        ;;
    render)
        cmd_render
        ;;
    *)
        fail "unknown action $ACTION"
        ;;
    esac
}

main
