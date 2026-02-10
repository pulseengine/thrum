#!/usr/bin/env bash
# check-status.sh -- List Thrum pipeline tasks, optionally filtered.
#
# Usage:
#   check-status.sh                          # all tasks
#   check-status.sh --status awaitingapproval  # filter by status
#   check-status.sh --repo loom              # filter by repo
#   check-status.sh --status pending --repo loom  # both filters
set -euo pipefail

: "${THRUM_API_URL:?THRUM_API_URL must be set}"
: "${THRUM_API_TOKEN:?THRUM_API_TOKEN must be set}"

STATUS=""
REPO=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --status) STATUS="$2"; shift 2 ;;
    --repo)   REPO="$2";   shift 2 ;;
    -h|--help)
      echo "Usage: $0 [--status <status>] [--repo <repo>]"
      echo ""
      echo "Statuses: pending, implementing, gate1, reviewing, gate2,"
      echo "          awaitingapproval, approved, integrating, gate3, merged, rejected"
      exit 0
      ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

QUERY=""
[[ -n "$STATUS" ]] && QUERY="status=${STATUS}"
[[ -n "$REPO" ]]   && QUERY="${QUERY:+${QUERY}&}repo=${REPO}"

URL="${THRUM_API_URL}/api/v1/tasks${QUERY:+?${QUERY}}"

curl -sf \
  -H "Authorization: Bearer ${THRUM_API_TOKEN}" \
  "$URL" | jq .
