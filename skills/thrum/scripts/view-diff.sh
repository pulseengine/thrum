#!/usr/bin/env bash
# view-diff.sh -- View code changes produced by a Thrum task.
#
# Usage:
#   view-diff.sh <task-id>
set -euo pipefail

: "${THRUM_API_URL:?THRUM_API_URL must be set}"
: "${THRUM_API_TOKEN:?THRUM_API_TOKEN must be set}"

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <task-id>" >&2
  exit 1
fi

TASK_ID="$1"

curl -sf \
  -H "Authorization: Bearer ${THRUM_API_TOKEN}" \
  "${THRUM_API_URL}/api/v1/tasks/${TASK_ID}/diff" | jq .
