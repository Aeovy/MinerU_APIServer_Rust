#!/bin/sh
set -eu

OUTPUT_ROOT="${MINERU_API_OUTPUT_ROOT:-/app/output}"

if [ "$(id -u)" = "0" ]; then
    mkdir -p "$OUTPUT_ROOT"
    chown mineru:mineru "$OUTPUT_ROOT"
    exec gosu mineru "$@"
fi

exec "$@"
