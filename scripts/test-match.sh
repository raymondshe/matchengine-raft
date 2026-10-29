#!/bin/sh

set -o errexit

# Always run from the repository root.
cd "$(dirname "$0")/.."

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

export RUST_LOG=debug 

##############################################################{
echo "Place order on leader"
sleep 1
i=1
while [ "$i" -le 1500 ]
do
    echo
    JSON='{"Place": { "order": {"live": "Limit", "side": "Buy", "price": 400'"$i"'.0, "volume": 4.4, "id": '"$i"', "sequence": '"$i"' } }}'
    echo "$JSON"
    rpc 21001/write "$JSON"
    i=$((i + 1))
done

echo "Data written"
