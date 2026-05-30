#!/bin/sh
# Docker entrypoint for WiFi-DensePose sensing server.
#
# Supports two usage patterns:
#
# 1. No arguments — use defaults from environment:
#      docker run -e CSI_SOURCE=esp32 ruvnet/wifi-densepose:latest
#
# 2. Pass CLI flags directly:
#      docker run ruvnet/wifi-densepose:latest --source esp32 --tick-ms 500
#      docker run ruvnet/wifi-densepose:latest --model /app/models/my.rvf
#
# Environment variables:
#   CSI_SOURCE   — data source: auto (default), esp32, wifi, simulated
#   MODELS_DIR   — directory to scan for .rvf model files (default: data/models)
set -e

# Route to cog-ha-matter (ADR-116) when invoked as:
#   docker run <image> cog-ha-matter [--flags]
# or via the short alias `ha-matter`. Strips the keyword and execs the
# Home Assistant + Matter cog binary, defaulting --sensing-url to the
# co-located sensing-server endpoint so docker-compose deployments work
# out of the box.
case "${1:-}" in
    cog-ha-matter|ha-matter)
        shift
        exec /app/cog-ha-matter \
            --sensing-url "${SENSING_URL:-http://127.0.0.1:3000}" \
            "$@"
        ;;
    homecore|homecore-server)
        # Route to the HOMECORE native Rust port of Home Assistant
        # (ADRs 126-134, v0.10.0). Default bind matches HA at :8123.
        shift
        exec /app/homecore-server \
            --bind "${HOMECORE_BIND:-0.0.0.0:8123}" \
            "$@"
        ;;
esac

# ── #864: secure-by-default API auth for the sensing server ──────────────────
#
# The sensing server publishes a live RF-sensing REST API and WebSocket stream.
# Historically the Docker image shipped with RUVIEW_API_TOKEN empty, which makes
# bearer auth a no-op and exposes `/api/v1/*` and `/ws/sensing` to anyone who can
# reach the published ports. We now fail closed: if no token is supplied we
# generate a strong random one and print it, so the stream is never anonymous by
# default. Operators on a trusted, isolated LAN can opt back into the open
# posture explicitly with RUVIEW_ALLOW_UNAUTHENTICATED=1.
generate_token() {
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 32
    elif [ -r /proc/sys/kernel/random/uuid ]; then
        # Two UUIDs (dashes stripped) → 64 hex chars of kernel randomness.
        printf '%s%s' \
            "$(cat /proc/sys/kernel/random/uuid)" \
            "$(cat /proc/sys/kernel/random/uuid)" | tr -d '-'
    else
        head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
    fi
}

if [ -z "${RUVIEW_API_TOKEN:-}" ]; then
    case "${RUVIEW_ALLOW_UNAUTHENTICATED:-}" in
        1|true|TRUE|yes|YES)
            echo "WARNING: RUVIEW_ALLOW_UNAUTHENTICATED is set — the sensing API and" >&2
            echo "         /ws/sensing stream will run UNAUTHENTICATED. Only do this on a" >&2
            echo "         trusted, isolated network (issue #864)." >&2
            ;;
        *)
            RUVIEW_API_TOKEN="$(generate_token)"
            export RUVIEW_API_TOKEN
            echo "============================================================" >&2
            echo " RuView: no RUVIEW_API_TOKEN supplied — generated one for you:" >&2
            echo "   RUVIEW_API_TOKEN=${RUVIEW_API_TOKEN}" >&2
            echo "" >&2
            echo "   REST: Authorization: Bearer <token>" >&2
            echo "   WS:   ws://<host>:3001/ws/sensing?token=<token>" >&2
            echo "" >&2
            echo " Pin your own with -e RUVIEW_API_TOKEN=..., or run open on a" >&2
            echo " trusted LAN with -e RUVIEW_ALLOW_UNAUTHENTICATED=1 (issue #864)." >&2
            echo "============================================================" >&2
            ;;
    esac
fi

# If the first argument looks like a flag (starts with -), prepend the
# server binary so users can just pass flags:
#   docker run <image> --source esp32 --tick-ms 500
if [ "${1#-}" != "$1" ] || [ -z "$1" ]; then
    set -- /app/sensing-server \
        --source "${CSI_SOURCE:-auto}" \
        --tick-ms 100 \
        --ui-path /app/ui \
        --http-port 3000 \
        --ws-port 3001 \
        --bind-addr 0.0.0.0 \
        "$@"
fi

exec "$@"
