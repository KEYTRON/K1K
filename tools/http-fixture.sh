#!/bin/sh
# The host side of the network test: a tiny HTTP server on 127.0.0.1:18791 that
# QEMU's user-mode network reaches as 10.0.2.2:18791 from the guest.
#   tools/http-fixture.sh start <build-dir>
#   tools/http-fixture.sh stop  <build-dir>
set -eu
cmd=$1
dir=$2
case $cmd in
start)
    mkdir -p "$dir/httpd"
    echo K1K-NET-OK > "$dir/httpd/k1k-net.txt"
    python3 -m http.server 18791 --bind 127.0.0.1 --directory "$dir/httpd" \
        > "$dir/httpd.log" 2>&1 &
    echo $! > "$dir/httpd.pid"
    # Wait until it listens.
    i=0
    while [ $i -lt 50 ]; do
        python3 -c 'import socket;socket.create_connection(("127.0.0.1",18791),0.2).close()' 2>/dev/null && exit 0
        i=$((i + 1))
        sleep 0.1
    done
    echo "http fixture did not start" >&2
    exit 1
    ;;
stop)
    if [ -f "$dir/httpd.pid" ]; then
        kill "$(cat "$dir/httpd.pid")" 2>/dev/null || true
        rm -f "$dir/httpd.pid"
    fi
    ;;
esac
