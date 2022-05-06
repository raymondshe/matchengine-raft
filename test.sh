#!/bin/sh

set -o errexit

kill() {
    if [ "$(uname)" = "Darwin" ]; then
        SERVICE='raft-key-value'
        if pgrep -xq -- "${SERVICE}"; then
            pkill -f "${SERVICE}"
        fi
    else
        set +e # killall will error if finds no process to kill
        killall raft-key-value
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
    echo "Adding node 2 and node 3 as learners, to receive log from leader node 1"
    echo
    rpc 2100$1/init '{}'
    echo "Node $1 added as leaner"

    if test $1 -ne 1 
    then
        rpc 2100$1/add-learner       '[1, "127.0.0.1:21001"]'
        echo "Node 1 added as leaner"
    fi
    if test $1 -ne 2 
    then
        rpc 2100$1/add-learner       '[2, "127.0.0.1:21002"]'
        echo "Node 2 added as leaner"
    fi
    if test $1 -ne 3
    then
        rpc 2100$1/add-learner       '[3, "127.0.0.1:21003"]'
        echo "Node 3 added as leaner"
    fi
}

place_order() {
##############################################################{
    echo "Place order on leader"
    for i in {1..1500}
    do
        echo
        JSON='{"Place": { "order": {"live": "Limit", "side": "Buy", "price": 400'
        JSON+="$i.0,"
        JSON+='"volume": 4.4, "id": '
        JSON+="$i,"
        JSON+=' "sequance": '
        JSON+="$i"
        JSON+='} }}'
        echo $JSON
        rpc 2100$1/write "$JSON"
    done 

    echo "Data written"
}

get_metrics() {
    echo "Get metrics from the leader"
    echo
    rpc 2100$1/metrics
}

kill_node() {
    case $1 in 
    "1")
        pkill -f 'raft-key-value --id 1'
        ;;
    "2")
        pkill -f 'raft-key-value --id 2'
        ;;
    "3")
        pkill -f 'raft-key-value --id 3'
        ;;
    esac
}

start_node() {
    nohup ./target/debug/raft-key-value  --id $1 --http-addr 127.0.0.1:2100$1 > n1.log &
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
    rpc 2100$2/read  '"orderbook_sequance"'
    ;;
"build-cluster")
    build_cluster $2
    ;;
  *)
    "Nothing is done!"
    ;;
esac
