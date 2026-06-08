#!/usr/bin/env bash
# Run the integration suite against each MySQL release Oracle still
# supports (8.0 in extended support, 8.4 LTS, 9.x Innovation) plus
# MariaDB. Each engine is started in a fresh container bound to host
# port 3306 so the default tests/.env
# (db_url=mysql://root:123456@127.0.0.1:3306) works without local edits.
set -euo pipefail
cd "$(dirname "$0")/.."

# Make sure a Ctrl-C / errexit during a run leaves no container behind.
trap 'docker rm -f mbc-engine >/dev/null 2>&1 || true' EXIT

run_one() {
    local label="$1"
    local image="$2"
    shift 2
    local args=("$@")

    echo
    echo "========================================================================"
    echo "  $label  ($image)"
    echo "========================================================================"

    docker rm -f mbc-engine >/dev/null 2>&1 || true
    docker run -d --name mbc-engine \
        -p 3306:3306 \
        -e MYSQL_ROOT_PASSWORD=123456 \
        -e MARIADB_ROOT_PASSWORD=123456 \
        "$image" \
        "${args[@]}" >/dev/null

    # Wait up to 120s for the server to respond to a TCP ping. Bail
    # early if the container exits during startup (typically a bad
    # CLI flag for that engine version).
    local ready=0
    for i in $(seq 1 120); do
        if docker exec mbc-engine sh -c \
            'mysqladmin -uroot -p"$MYSQL_ROOT_PASSWORD" ping --silent 2>/dev/null \
             || mysqladmin -uroot -p"$MARIADB_ROOT_PASSWORD" ping --silent 2>/dev/null' \
            >/dev/null 2>&1; then
            ready=$i
            break
        fi
        if ! docker inspect -f '{{.State.Running}}' mbc-engine 2>/dev/null | grep -q true; then
            echo "  $label container exited before becoming ready; logs:"
            docker logs mbc-engine 2>&1 | tail -20
            docker rm -f mbc-engine >/dev/null 2>&1 || true
            return 1
        fi
        sleep 1
    done
    if [[ $ready -eq 0 ]]; then
        echo "  $label did not respond to ping within 120s"
        docker logs mbc-engine 2>&1 | tail -20
        docker rm -f mbc-engine >/dev/null 2>&1 || true
        return 1
    fi
    echo "  $label is ready (after ${ready}s)"

    cargo test --test integration_test \
        -- --test-threads=1 --skip parse_file_tests \
        2>&1 | grep -E '^test |test result' | tail -120 || true

    docker rm -f mbc-engine >/dev/null
}

# binlog_row_metadata=FULL is an 8.0.1+ option. Set it on every modern
# MySQL so the metadata tests actually exercise the parsing path;
# without it they skip silently (see the runtime guard in the tests).

# 8.0 LTS — premier support ended 2025-04, extended support to 2032-04.
# default_authentication_plugin is the deprecated 8.0 spelling and is
# tolerated by the 8.0 line; 8.4+ rejects it (see below).
run_one "MySQL 8.0" mysql:8.0.31 \
    --server_id=1 \
    --log_bin=/var/lib/mysql/mysql-bin.log \
    --max_binlog_size=100M \
    --gtid_mode=ON \
    --enforce_gtid_consistency=ON \
    --binlog_format=ROW \
    --binlog_row_metadata=FULL \
    --binlog_rows_query_log_events=ON \
    --default_authentication_plugin=mysql_native_password || true

# 8.4 LTS — premier support to 2029-07. The
# `--default_authentication_plugin` option was renamed to
# `--authentication_policy` and `mysql_native_password` is no longer
# loaded by default; we don't need it here because tests authenticate
# via the modern caching_sha2_password plugin already.
run_one "MySQL 8.4" mysql:8.4 \
    --server_id=1 \
    --log_bin=/var/lib/mysql/mysql-bin.log \
    --max_binlog_size=100M \
    --gtid_mode=ON \
    --enforce_gtid_consistency=ON \
    --binlog_format=ROW \
    --binlog_row_metadata=FULL \
    --binlog_rows_query_log_events=ON || true

# 9.x Innovation — quarterly cadence, latest stable shipped Q1 2026.
# Same flags as 8.4; pin to a specific tag so a future Innovation
# release with a removed flag doesn't silently break this script.
run_one "MySQL 9.x" mysql:9.7 \
    --server_id=1 \
    --log_bin=/var/lib/mysql/mysql-bin.log \
    --max_binlog_size=100M \
    --gtid_mode=ON \
    --enforce_gtid_consistency=ON \
    --binlog_format=ROW \
    --binlog_row_metadata=FULL \
    --binlog_rows_query_log_events=ON || true

# MariaDB always emits its own GTID stream when binlog is on;
# --gtid_mode does not exist as a server option there.
run_one "MariaDB 10.11" mariadb:10.11 \
    --server_id=1 \
    --log_bin=/var/lib/mysql/mysql-bin.log \
    --max_binlog_size=100M \
    --binlog_format=ROW || true
