#!/usr/bin/env bash
# poll-approvals.sh -- Poll for Thrum tasks awaiting approval.
#
# Usage:
#   poll-approvals.sh                  # run a single poll cycle
#   poll-approvals.sh --auto-approve   # auto-approve all pending tasks
#   poll-approvals.sh --install        # install cron job (every 5 minutes)
#   poll-approvals.sh --uninstall      # remove cron job
set -euo pipefail

: "${THRUM_API_URL:?THRUM_API_URL must be set}"
: "${THRUM_API_TOKEN:?THRUM_API_TOKEN must be set}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRIPT_PATH="${SCRIPT_DIR}/$(basename "$0")"
CRON_TAG="thrum-approval-poll"

install_cron() {
  local cron_line="*/5 * * * * THRUM_API_URL=${THRUM_API_URL} THRUM_API_TOKEN=${THRUM_API_TOKEN} ${SCRIPT_PATH} >> /tmp/thrum-poll.log 2>&1 # ${CRON_TAG}"
  (crontab -l 2>/dev/null | grep -v "${CRON_TAG}"; echo "$cron_line") | crontab -
  echo "Installed cron job: polls every 5 minutes"
  echo "Logs written to /tmp/thrum-poll.log"
}

uninstall_cron() {
  crontab -l 2>/dev/null | grep -v "${CRON_TAG}" | crontab -
  echo "Removed cron job"
}

poll() {
  local auto_approve="${1:-false}"
  local tasks

  tasks=$(curl -sf \
    -H "Authorization: Bearer ${THRUM_API_TOKEN}" \
    "${THRUM_API_URL}/api/v1/tasks?status=awaitingapproval")

  local count
  count=$(echo "$tasks" | jq 'length')

  if [[ "$count" -eq 0 ]]; then
    echo "[$(date -Iseconds)] No tasks awaiting approval"
    return 0
  fi

  echo "[$(date -Iseconds)] ${count} task(s) awaiting approval:"
  echo "$tasks" | jq -r '.[] | "  Task \(.id): \(.title) [\(.repo)]"'

  if [[ "$auto_approve" == "true" ]]; then
    echo "$tasks" | jq -r '.[].id' | while read -r task_id; do
      echo "  Auto-approving task ${task_id}..."
      curl -sf -X POST \
        -H "Authorization: Bearer ${THRUM_API_TOKEN}" \
        "${THRUM_API_URL}/api/v1/tasks/${task_id}/approve" | jq -r '"  -> \(.status)"'
    done
  fi
}

case "${1:-}" in
  --install)   install_cron ;;
  --uninstall) uninstall_cron ;;
  --auto-approve) poll true ;;
  -h|--help)
    echo "Usage: $0 [--install | --uninstall | --auto-approve]"
    echo ""
    echo "  (no args)        Run a single poll cycle and report"
    echo "  --auto-approve   Approve all tasks awaiting approval"
    echo "  --install        Install cron job (every 5 minutes)"
    echo "  --uninstall      Remove cron job"
    exit 0
    ;;
  "") poll false ;;
  *) echo "Unknown option: $1" >&2; exit 1 ;;
esac
