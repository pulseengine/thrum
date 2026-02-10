#!/usr/bin/env bash
# reject-task.sh -- Reject a Thrum task with feedback for the implementing agent.
#
# Usage:
#   reject-task.sh <task-id> "Reason for rejection"
set -euo pipefail

: "${THRUM_API_URL:?THRUM_API_URL must be set}"
: "${THRUM_API_TOKEN:?THRUM_API_TOKEN must be set}"

if [[ $# -lt 2 ]]; then
  echo "Usage: $0 <task-id> \"feedback message\"" >&2
  exit 1
fi

TASK_ID="$1"
FEEDBACK="$2"

curl -sf -X POST \
  -H "Authorization: Bearer ${THRUM_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d "$(jq -n --arg fb "$FEEDBACK" '{feedback: $fb}')" \
  "${THRUM_API_URL}/api/v1/tasks/${TASK_ID}/reject" | jq .
