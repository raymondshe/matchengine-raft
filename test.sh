#!/bin/sh

set -o errexit

kill() {
    if [ "$(uname)" = "Darwin" ]; then
        SERVICE='matchengine-raft'
        if pgrep -xq -- "${SERVICE}"; then
            pkill -f "${SERVICE}"
        fi
    else
        set +e # killall will error if finds no process to kill
        killall matchengine-raft
        set -e
    fi
}

rpc() {
    local uri=$1
    local body="$2"

    echo '---'" rpc(:$uri, $body)"

    {
        if [ ".$body" = "." ]; then
            curl --silent "127.0.0.1:$uri"
        else
            curl --silent "127.0.0.1:$uri" -H "Content-Type: application/json" -d "$body"
        fi
    } | {
        if type jq > /dev/null 2>&1; then
            jq
        else
            cat
        fi
    }

    echo
    echo
}

build_cluster() {
    local node=${1:-1}
    echo "Initializing node $node and adding the other nodes as learners"
    echo
    rpc 2100"$node"/init '[]'
    echo "Node $node initialized"

    if test "$node" -ne 1
    then
        rpc 2100"$node"/add-learner       '[1, "127.0.0.1:21001"]'
        echo "Node 1 added as learner"
    fi
    if test "$node" -ne 2
    then
        rpc 2100"$node"/add-learner       '[2, "127.0.0.1:21002"]'
        echo "Node 2 added as learner"
    fi
    if test "$node" -ne 3
    then
        rpc 2100"$node"/add-learner       '[3, "127.0.0.1:21003"]'
        echo "Node 3 added as learner"
    fi
}

place_order() {
    echo "Place order on leader"
    local node=${1:-1}
    local i=1
    while [ "$i" -le 1500 ]
    do
        echo
        JSON='{"Place": { "order": {"live": "Limit", "side": "Buy", "price": 400'"$i"'.0, "volume": 4.4, "id": '"$i"', "sequence": '"$i"' } }}'
        echo "$JSON"
        rpc 2100"$node"/write "$JSON"
        i=$((i + 1))
    done

    echo "Data written"
}

get_metrics() {
    local node=${1:-1}
    echo "Get metrics from the leader"
    echo
    rpc 2100"$node"/metrics
}

kill_node() {
    case $1 in 
    "1")
        pkill -f 'matchengine-raft --id 1'
        ;;
    "2")
        pkill -f 'matchengine-raft --id 2'
        ;;
    "3")
        pkill -f 'matchengine-raft --id 3'
        ;;
    esac
}

start_node() {
    nohup ./target/debug/matchengine-raft  --id $1 --http-addr 127.0.0.1:2100$1 > n$1.log 2>&1 &
}

change_membership () {
    local members=${1:-[1,2,3]}
    echo "Changing membership to $members"
    echo
    rpc 21001/change-membership "$members"
    echo "Membership changed"
}

clean() {
    echo "Cleaning"
    surname=`date +%Y%m%dT%H%M%S`
    dir=${RAFT_DATA_DIR:-/tmp/matchengine-raft}
    if [ -d "$dir" ]; then
        mv "$dir" "$dir.$surname"
    fi
    mkdir -p "$dir"
}

export RUST_LOG=debug 
export RUST_LIB_BACKTRACE=1
export RUST_BACKTRACE=1

echo "Run command $1"
case $1 in
"place-order")
    place_order $2
    ;;
"metrics")
    get_metrics $2
    ;;

"kill-node")
    kill_node $2
    ;;
"kill")
    kill
    ;;
"start-node")
    start_node $2
    ;;
"get-seq")
    rpc 2100"${2:-1}"/read  '"orderbook_sequence"'
    ;;
"build-cluster")
    build_cluster $2
    ;;
"change-membership")
	change_membership $2
    ;;

"clean")
    clean
    ;;
  *)
    "Nothing is done!"
    ;;
esac
