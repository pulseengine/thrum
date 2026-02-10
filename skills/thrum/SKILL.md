---
name: thrum
description: Manage Thrum pipeline tasks — check status, approve/reject tasks awaiting human review, view diffs, and set up automated approval polling.
metadata: {"openclaw":{"emoji":"🔧","requires":{"env":["THRUM_API_URL","THRUM_API_TOKEN"],"bins":["curl","jq"]}}}
---

# Thrum Task Management

## Overview

Use this skill to interact with a running Thrum orchestration engine via its REST API.
Thrum drives autonomous AI development pipelines: tasks move through gated stages
(quality, proof, integration) and pause at **AwaitingApproval** for human review.

This skill lets you list tasks, inspect diffs, approve or reject work, and set up
cron-based polling so approvals happen without manual checking.

## Environment

Two environment variables are required:

| Variable | Purpose | Example |
| --- | --- | --- |
| `THRUM_API_URL` | Base URL of the Thrum API server | `http://localhost:3000` |
| `THRUM_API_TOKEN` | Bearer token for authenticated requests | `thrum_tok_abc123` |

All helper scripts live in `{baseDir}/scripts/` and source these variables automatically.

## Operations

### 1. Check task status

List all tasks, optionally filtered by status or repository:

```bash
# All tasks
{baseDir}/scripts/check-status.sh

# Only tasks awaiting approval
{baseDir}/scripts/check-status.sh --status awaitingapproval

# Tasks for a specific repo
{baseDir}/scripts/check-status.sh --repo loom
```

Raw curl equivalent:

```bash
curl -s -H "Authorization: Bearer $THRUM_API_TOKEN" \
  "$THRUM_API_URL/api/v1/tasks?status=awaitingapproval" | jq .
```

### 2. Approve a task

Move a task from **AwaitingApproval** to **Approved** so the pipeline continues:

```bash
{baseDir}/scripts/approve-task.sh <task-id>
```

Raw curl equivalent:

```bash
curl -s -X POST \
  -H "Authorization: Bearer $THRUM_API_TOKEN" \
  "$THRUM_API_URL/api/v1/tasks/<task-id>/approve" | jq .
```

Only tasks in the `awaitingapproval` state can be approved. The API returns an error
for tasks in any other state.

### 3. Reject a task

Send a task back to **Implementing** with human feedback:

```bash
{baseDir}/scripts/reject-task.sh <task-id> "Feedback explaining what needs to change"
```

Raw curl equivalent:

```bash
curl -s -X POST \
  -H "Authorization: Bearer $THRUM_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"feedback":"Feedback explaining what needs to change"}' \
  "$THRUM_API_URL/api/v1/tasks/<task-id>/reject" | jq .
```

The feedback string is stored on the task and provided to the implementing agent
on the next retry.

### 4. View diffs

Inspect the code changes produced by a task:

```bash
{baseDir}/scripts/view-diff.sh <task-id>
```

Raw curl equivalent:

```bash
curl -s -H "Authorization: Bearer $THRUM_API_TOKEN" \
  "$THRUM_API_URL/api/v1/tasks/<task-id>/diff" | jq .
```

Use this before approving or rejecting to review what the agent actually changed.

### 5. Automated approval polling

Set up a cron job that periodically checks for tasks awaiting approval and
notifies or auto-processes them:

```bash
# Install the cron job (checks every 5 minutes)
{baseDir}/scripts/poll-approvals.sh --install

# Run a single poll cycle manually
{baseDir}/scripts/poll-approvals.sh

# Uninstall the cron job
{baseDir}/scripts/poll-approvals.sh --uninstall
```

The polling script lists tasks with `status=awaitingapproval` and prints a summary.
It does **not** auto-approve by default — it only reports. Pass `--auto-approve` to
automatically approve all pending tasks (use with caution).

## Task States Reference

```
Pending -> Implementing -> Gate1(Quality) -> Reviewing -> Gate2(Proof)
  -> AwaitingApproval -> Approved -> Integrating -> Gate3(Integration) -> Merged
```

- **AwaitingApproval**: The only state where `approve` and `reject` actions apply.
- **Rejected**: Returns to Implementing with your feedback for the agent.
- **Gate failures**: Automatically retry up to 3 times before stopping.

## Typical Workflow

1. Run `check-status.sh --status awaitingapproval` to see what needs review.
2. For each task, run `view-diff.sh <id>` to inspect the changes.
3. Run `approve-task.sh <id>` or `reject-task.sh <id> "reason"` based on review.
4. Optionally install `poll-approvals.sh --install` for hands-free monitoring.
