# Feishu conversation failure notifications

Requires Python 3 on the backend machine. Configure the backend's environment:

```sh
export AIONUI_ERROR_NOTIFY_SCRIPT=/absolute/path/to/scripts/notifications/feishu_error_notify.py
export AIONUI_FEISHU_ERROR_WEBHOOK='your Feishu bot webhook URL'
```

Restart the backend with these variables. The webhook is not stored in source.
For systemd deployments, install the executable outside the application release
directory (for example `/usr/local/libexec/aioneasiful/feishu_error_notify.py`),
store the two variables in a root-owned mode-600 environment file, and reference
it from an `EnvironmentFile` service drop-in. This keeps notification configuration
and the script intact when `/opt/aionui-web` is replaced during an upgrade.
The executable and its parent directories must allow the service user to execute
and traverse them (normally mode 755); check this as the service user, not root.
On desktop installations, configure the environment of the launched backend;
variables exported in an unrelated terminal do not configure an already running app.

The user-turn orchestrator triggers the executable once on final failure after
automatic recovery. Successful recovery and warning/info tips do not notify.
Task-build, workspace and required-runtime-mode failures carry their structured
error through the same final-failure hook. User cancellation does not notify.
The detached hook has a 15-second deadline and cannot block the conversation.
Delivery success/failure is logged at info/warn; delivery is best effort, without
persistent retries. Process shutdown can interrupt an in-flight notification.
At most four hooks run concurrently; excess notifications are skipped with a warning.

Messages include backend hostname, detected RFC1918 IPv4 addresses, timestamp,
conversation/turn IDs, code, ownership, retryability and a safe situation summary.
Raw errors, prompts, outputs and credentials are not exported. Known OAuth expiry,
workspace routing 401 and safety blocking messages get specific summaries; other
errors use a generic summary with the structured code. The address identifies the
backend host, which may differ from the browser client on remote deployments.

Check formatting locally or send a clearly marked test notification:

```sh
./scripts/notifications/feishu_error_notify.py --test --dry-run
./scripts/notifications/feishu_error_notify.py --test
```

Ensure bot keyword/IP/signature restrictions allow this payload. Signature-protected
bots are not supported by this script. Install the script on each monitored device;
this is not a centrally deployed fleet monitor.

通知还包含当前会话的智能体名称、backend 与智能体 ID（未配置 ID 时显示“未提供”）。Aion CLI 会话显示 `Aion CLI / aionrs`；自定义 ACP 智能体优先使用已配置的名称。
