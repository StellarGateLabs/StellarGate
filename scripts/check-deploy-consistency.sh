#!/usr/bin/env bash
#
# Cross-checks the deploy assets against each other and against Dockerfile so
# they cannot silently drift apart as the app evolves (issues #444, #447):
#
#   - Dockerfile's EXPOSE'd port matches the port both compose files publish
#     the app on.
#   - The healthcheck binary path baked into the runtime image matches what
#     every compose file's healthcheck actually invokes.
#   - deploy/stellargate.env.example never carries a real-looking secret in
#     place of a placeholder.
#
# Run locally with: bash scripts/check-deploy-consistency.sh

set -euo pipefail

fail=0
note() { printf '\033[1;31mdrift:\033[0m %s\n' "$*"; fail=1; }

dockerfile_port=$(grep -oE '^EXPOSE [0-9]+' Dockerfile | grep -oE '[0-9]+')
dockerfile_bin=$(grep -oE '/usr/local/bin/[a-z]+' Dockerfile | sort -u | head -1)

for compose in docker-compose.yml deploy/docker-compose.prod.yml; do
    if ! grep -qE "(^|[\"':])${dockerfile_port}([\"'/:]|$)" "$compose"; then
        note "$compose does not reference port $dockerfile_port (from Dockerfile EXPOSE)"
    fi
    if ! grep -q "$dockerfile_bin" "$compose"; then
        note "$compose healthcheck does not invoke $dockerfile_bin (from Dockerfile)"
    fi
done

env_example="deploy/stellargate.env.example"
for var in WEBHOOK_SECRET ADMIN_PROVISIONING_SECRET; do
    value=$(grep -E "^${var}=" "$env_example" | head -1 | cut -d= -f2-)
    if [[ -n "$value" && "$value" != REPLACE_ME_* ]]; then
        note "$env_example: $var=$value does not look like the REPLACE_ME_ placeholder"
    fi
done

if [[ $fail -eq 0 ]]; then
    echo "deploy assets are consistent with Dockerfile"
fi
exit $fail
