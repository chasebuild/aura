# aura-notify-contracts

This crate defines the notification interfaces used by AURA orchestration.

## Scope

This crate intentionally does not include concrete transports (Telegram, Slack, email, webhook).
External projects should implement `NotificationSink` and plug into AURA.

## Required AURA Use Cases

1. PR ready for review
- Event: `pr_ready_for_review`
- Trigger: strict gates pass for an orchestrated task
- Payload minimum: task id, task title, PR metadata

2. Retry exhausted / human attention needed
- Event: `retry_exhausted`
- Trigger: adaptive retry limit reached with failing gates
- Payload minimum: failure summary, latest gate results

3. Daemon unhealthy
- Event: `daemon_unhealthy`
- Trigger: supervisor loop failure, lock starvation, persistent provider errors
- Payload minimum: error type, timestamp, recovery hint
