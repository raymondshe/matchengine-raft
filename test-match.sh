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

export RUST_LOG=debug 

##############################################################{
echo "Place order on leader"
sleep 1
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
    rpc 21003/write "$JSON"
done 

echo "Data written"
