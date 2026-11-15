# Acceptance from a cloud session

A cloud session can't reach Telegram, so the `acceptance` workflow runs `cargo accept` on a GitHub runner, with the Test account's logins from the GitHub environment `acceptance`. Push your branch first: the workflow runs what is on GitHub.

1. **Trigger** it on your branch: `actions_run_trigger` with `method: run_workflow`, `workflow_id: acceptance.yml`, `ref: <your branch>`, `inputs: { scenarios: "<id> <id>" }` (empty: every scenario but `smoke`); for release acceptance add `image: "ghcr.io/qelphybox/tg-mcp:<full sha>"`. On a PR, adding the label `accept` triggers it too (remove and add it to run again).
2. **Find the run**: `actions_list` with `method: list_workflow_runs`, `resource_id: acceptance.yml`, filtered to your branch and `workflow_dispatch`; the newest is yours. Runs queue one at a time (concurrency group `acceptance`); the job takes several minutes, mostly building.
3. **Wait** with `send_later` or a scheduled wake-up of about 10 minutes, then `actions_get` with `method: get_workflow_run` until its status is `completed`. End your turn while it runs rather than polling in a loop.
4. **Read the report**: `actions_list` `list_workflow_jobs` for the job id, then `get_job_logs` with `return_content: true`. The report is the end of the `cargo accept` step's log, the same text as the job summary: the outcome line, the acceptance table, the Human checks.

The conclusion maps to the outcome: success is PASS. Failure is FAIL, unless the report starts with `# BLOCKED`, which is BLOCKED (SKILL.md step 3). A configuration error (a `TG_ACCEPT_*` secret missing) fails before any report: tell the maintainer which one, from the log, and that `scripts/accept-setup.sh` sets them.
