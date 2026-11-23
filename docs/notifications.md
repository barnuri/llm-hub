# Notifications

The hub collects events that need your attention in one inbox: proposals from the
[local model tuner](local-tuner.md), refused requests, new error groups, unhealthy profiles,
budget steps, and messages from your own hooks, rules and extensions. The **Notifications** tab
lists them, and the hub can also send each one to a desktop popup, a webhook or a command.

Notifications and the desktop popup are on by default. `LLM_HUB_NOTIFICATIONS=false` turns the
whole system off: the hub stores no new notification and sends nothing, and
a valid `POST /api/notifications` within its rate limit answers 202 with `{"disabled": true}`. The page still lists what the
inbox held before. [Configuration](configuration.md#notifications) lists every variable.

The hub reads `LLM_HUB_NOTIFICATIONS`, `LLM_HUB_NOTIFY_MIN_SEVERITY`, `LLM_HUB_NOTIFY_MUTE`, the
budget steps and the budgets again when `.env` changes. It builds the sinks once at startup, so a
change to `LLM_HUB_NOTIFY_DESKTOP`, `LLM_HUB_NOTIFY_WEBHOOK_URL`, `LLM_HUB_NOTIFY_COMMAND` or
`LLM_HUB_NOTIFY_TIMEOUT_MS` takes effect only after a restart.

## The notification

| Field | Meaning |
|---|---|
| `id` | A stable id. When a source sends a known id again, a pending notification takes the new body. The hub never adds it as a new entry or sends it again, and a notification you dismissed or applied stays as it is. |
| `source` | Who sent it: `local_tuner`, `budget`, `errors`, `health`, `ext:<name>`, or the free-text source of a `POST /api/notifications` call. |
| `kind` | `proposal` (a change you can approve), `guard` (the prefill guard refused a request), `advisory` (worth knowing, nothing to apply) or `custom` (a hook, rule or extension). |
| `severity` | `info`, `warning` or `critical`. |
| `title`, `body` | A one-line summary and the main text. |
| `profile`, `model`, `rule` | What it is about, when it is about a profile, a model or a tuner rule. A budget notification puts its quota name in `rule`. |
| `diff` | The flag change of a proposal: `add`, `remove`, and `ctx` with the old size in `old_ctx`. |
| `actions` | The buttons the page offers: `approve` and `dismiss` for a proposal, `dismiss` for every other kind. |
| `status` | `pending`, `approved`, `dismissed`, `applied` or `failed`. Without an approve action, a notification is only `pending` or `dismissed`. |
| `result` | The outcome of an approval: the registry edit summary or error, and the apply command's output tail. |
| `muted` | True when a mute rule with scope `all` matched when the notification arrived. The page and `GET /api/notifications` hide it by default. |
| `created_ms`, `updated_ms` | Unix times in milliseconds. |

The inbox holds at most 500 notifications. When it is full, the oldest one that is no longer
pending goes first, and the oldest pending one only when all are pending. With
`LLM_HUB_PERSISTENT=true`, the store keeps the notifications and the mute rules you add in the UI
across restarts. Without persistence they live in memory.

## Sources

| Source | What it sends |
|---|---|
| `local_tuner` | Proposals, advisories and guard events. See [Local model tuner](local-tuner.md#rules). |
| `budget` | A step of a provider quota or of a monthly spend budget. See [Budget notifications](#budget-notifications). |
| `errors` | A new error group. See [Errors and health](#errors-and-health). |
| `health` | A profile that turned unhealthy, or recovered. See [Errors and health](#errors-and-health). |
| `ext:<name>` | A notification from a rule's `notify` action (`ext:rules`) or a Rust extension. See [Extensions](extensions.md#notifications). |
| Any other text | A notification posted to `POST /api/notifications`, with the source the caller chose. The hub reserves no source name, so a caller can also post as `budget` or `errors`. |

The desktop test popup goes straight to the desktop and never reaches the inbox.

### Errors and health

The `errors` source sends one `warning` advisory for the first failure of each error group: a
request that answered 4xx or 5xx, keep-warm pings excluded. The group is the normalized signature
the Errors page shows, and the id `errors:<hash>` keeps one notification per group. At startup the
hub reads the groups already in the store and marks them seen, so a restart does not send them
again. Without persistence, the hub starts with no seen groups. The hub remembers at most 4,096
groups. Past that, new groups stop notifying until the next restart.

The `health` source sends an advisory when a profile's health flips: `critical` when it turns
unhealthy, and `info` when it recovers. The models list refresh and the profile test on the
Profiles page set the health. The first unhealthy report notifies, and the profile notifies again
only after it recovered. At the default minimum severity (`warning`), a recovery reaches the inbox
but no sink.

## Sinks

The hub hands each new notification to a bounded queue, and one background task delivers it to
every sink in turn. Nothing waits on a sink: when the queue is full (256 notifications), the hub
drops the delivery and logs it, and the inbox still holds the notification. A sink failure logs a
warning and never blocks the inbox.

A notification reaches the sinks only when all of these hold:

- It is new. An update to a known id is not delivered again.
- No mute rule matches it.
- Its severity is at least `LLM_HUB_NOTIFY_MIN_SEVERITY` (default `warning`).

| Sink | Turn on | What it does |
|---|---|---|
| Desktop | On by default. `LLM_HUB_NOTIFY_DESKTOP=false` turns it off. | Shows a native popup with the title and body (capped at 120 and 400 characters). macOS runs `osascript -e 'display notification …'`. Linux runs `notify-send --app-name=llm-hub`. Windows runs `powershell` with a toast script. Other systems have no popup. |
| Webhook | `LLM_HUB_NOTIFY_WEBHOOK_URL` (`http://` or `https://`) | POSTs the notification as JSON. An answer other than 2xx counts as a failure. |
| Command | `LLM_HUB_NOTIFY_COMMAND` | Runs the command with the notification as JSON on stdin. The hub splits the value like a shell (quotes work, `~/` is expanded) and runs the program directly, without a shell. A non-zero exit counts as a failure. |

The desktop popup, the webhook call and the command each time out after
`LLM_HUB_NOTIFY_TIMEOUT_MS` (default 10 seconds). The hub kills a popup tool or a command that
runs longer.

A command sink that forwards to another tool:

```sh
#!/bin/sh
# Reads one notification as JSON on stdin.
jq -r '"[\(.severity)] \(.title): \(.body)"' >> "$HOME/llm-hub-notifications.log"
```

## Desktop setup

A desktop popup can fail without an error: the OS may block the app that shows it, or Focus mode
may hide it. The Notifications page shows a setup banner until the desktop sink works and you
confirm that you saw the test popup. With `LLM_HUB_PERSISTENT=true` the hub saves that
confirmation in the store, so it survives a restart. With `LLM_HUB_NOTIFY_DESKTOP=false` the page
shows a one-line note that popups are off instead of the banner. The banner shows:

- the OS, and whether the hub finds the popup tool on `PATH`. On Linux, it also shows whether the
  hub sees a desktop session (`DBUS_SESSION_BUS_ADDRESS`, `WAYLAND_DISPLAY` or `DISPLAY`);
- the time of the last popup that worked, and the error of the last one that failed;
- numbered fix steps for the OS.

The status check reads the environment and the file system only. It starts no process.

| OS | Fix steps |
|---|---|
| macOS | 1. Open System Settings > Notifications, select "Script Editor" and allow notifications (**Run**: `open "x-apple.systempreferences:com.apple.Notifications-Settings.extension"`). 2. Turn off Focus (Do Not Disturb) in Control Center. 3. Send a test popup (**Run**), then press "I saw it". |
| Linux | 1. Install libnotify: `sudo apt install libnotify-bin` (Debian, Ubuntu) or `sudo dnf install libnotify` (Fedora, RHEL). 2. Run the hub in your desktop session. For a systemd user service: `systemctl --user import-environment DBUS_SESSION_BUS_ADDRESS DISPLAY WAYLAND_DISPLAY`. 3. Send a test popup (**Run**), then press "I saw it". |
| Windows | 1. Turn on notifications in Settings > System > Notifications, and allow Windows PowerShell (**Run**: `start ms-settings:notifications`). 2. Turn off Do not disturb. 3. Send a test popup (**Run**), then press "I saw it". |
| Other | Desktop popups are not supported. Use `LLM_HUB_NOTIFY_WEBHOOK_URL` or `LLM_HUB_NOTIFY_COMMAND`. |

Every step with a command offers **Copy**. Only a fixed allowlist also offers **Run**: opening the
OS notification settings and sending the test popup. Those take no input and need no `sudo`. A
step that installs a package or needs `sudo` is Copy only, and you run it yourself.

## Mute and snooze

A mute rule matches notifications by glob on any of the fields `source`, `kind`, `rule`,
`profile`, `model` and `title`. A rule matches when every field it sets matches. A field the
notification does not have never matches. In a glob, `*` matches any run of characters, `/`
included. Every other character matches itself, and case counts.

The `model` field holds the bare model name (`gemma-3-27b`) in notifications from `local_tuner`
and `errors`, and `<profile>/<model>` in notifications from a rule's `notify` action in the
`attempt` or `response` phase. A glob such as `*gemma-*` matches both forms.

The hub checks the mute rules when a notification arrives. A rule you add later does not hide or
mark the notifications already in the inbox.

A rule has a scope:

- `all`: the hub stores the notification with `muted: true`, hides it on the page by default,
  leaves it out of the pending count, and sends it to no sink.
- `sinks`: the page shows the notification, and no sink gets it.

Set rules for good in `LLM_HUB_NOTIFY_MUTE`: rules separated by `;`, each a list of
`field=glob` separated by `,`. These rules have scope `all`, never expire, and the page shows them
read-only.

```sh
# Mute every error group, and every tuner notification about a gemma model.
LLM_HUB_NOTIFY_MUTE=source=errors;source=local_tuner,model=gemma-*

# Mute budget steps of one profile, and every advisory.
LLM_HUB_NOTIFY_MUTE=source=budget,profile=copilot;kind=advisory
```

On the page, each card has **Mute** for 1 hour, 1 day, 1 week or forever, for this exact
notification or for all like it, with scope `all` or `sinks`. The "all like this" glob comes
prefilled, and you can edit it. The **Mutes** section lists every rule, and Delete removes a rule
added in the UI. A snooze stops matching when it ends, and a background task deletes it within a
minute.

Limits:

| Limit | Value |
|---|---|
| Mute rules, env and UI together | 200 |
| Characters per glob | 256 |
| Notifications in the inbox | 500 |
| Notifications per source, from `POST /api/notifications` and `ctx.notify(..)` together | 60 per minute: a burst of 60, then one more per second |
| Characters of a posted `source` | 64 |
| Title plus body of a posted notification | 4,096 bytes |

One limiter counts per source. `POST /api/notifications` counts the posted `source` and answers
429 past the limit. An extension's `ctx.notify(..)` counts as source `ext:<name>`, and every rule's
`notify` action together as `ext:rules`; the hub drops what passes the limit, with one debug log
per minute.

A config load fails when `LLM_HUB_NOTIFY_MUTE` holds an unknown or repeated field, a part without
`=`, an empty or too long glob, or more than 200 rules.

## Budget notifications

The hub sends a `budget` notification when usage crosses a step of a budget. It checks one minute
after start, then every 5 minutes, and never calls an upstream for it. It watches:

- the provider quotas in the hub's in-memory usage cache, as used against the entitlement, for
  example Copilot premium requests. The 15-minute quota poll (SQLite store only) and each usage read
  through the API, such as `GET /api/profiles/{name}/usage`, fill that cache. A profile whose usage
  the hub has not read since it started sends no quota step.
- a monthly spend budget against the hub's own measured cost for the UTC calendar month:
  `LLM_HUB_BUDGET_MONTHLY_USD` for all profiles together, and `LLM_HUB_<NAME>_BUDGET_MONTHLY_USD`
  for one profile. The hub prices the stored request rows the same way the usage pages do,
  keep-warm pings included. Spend budgets need persistence (`LLM_HUB_PERSISTENT=true`). Without a
  store the hub logs a warning and checks no spend budget. The `json` store keeps only a recent
  window of rows, so it can count less than the month's real spend.

`LLM_HUB_NOTIFY_BUDGET_STEPS` sets the steps in percent, default `25,50,75,80,85,95,100`. Each
step is 1 to 1000, and you can set at most 20.

For each budget and period, the hub sends one notification for the highest step crossed. When
usage jumps past several steps between two checks, only the highest one notifies. Its id is
`budget:<profile>:<quota>:<period>:<step>` (`global` for the hub-wide budget, `spend_usd` as the
quota of a spend budget), so the inbox never repeats a step while it holds that notification,
after a restart too when persistence is on. The period is the month of the quota's reset date when
the provider reports one, else the calendar month. A step below 100 has severity `warning`, so the
desktop popup shows it at the default minimum severity. A step of 100 or more is `critical`.

## HTTP API

These routes follow the `/api/*` auth rule: with hub auth on, send the key as
`Authorization: Bearer <key>` or `x-api-key: <key>`. See
[Hub authentication](http-api.md#hub-authentication).

| Route | Purpose |
|---|---|
| `GET /api/notifications` | `notifications` (newest first) and `pending`, the count of pending notifications that are not muted. Muted ones are left out unless the query has `include_muted=1`. |
| `POST /api/notifications` | Post a notification from a hook or a script. Answers 200 with the stored notification. |
| `POST /api/notifications/{id}/approve` | Approve a pending proposal and apply it. See [Approval](local-tuner.md#approval). 404 for an unknown id. 409 when it is not a pending proposal, it has no diff or model, its profile is gone or has no registry, or another approval for the profile runs. |
| `POST /api/notifications/{id}/dismiss` | Dismiss a pending notification. 404 for an unknown id, 409 when it is not pending. |
| `GET /api/notifications/mutes` | Every mute rule, env rules first. |
| `POST /api/notifications/mutes` | Add a UI mute rule. Body: `matcher` (the fields and globs), `scope` (`all` or `sinks`), optional `until_ms` (a future Unix time in ms; absent mutes forever) and optional `note` (at most 500 characters). The hub sets a new id each time. 400 for an empty matcher, a glob too long, a full rule set, a past `until_ms` or a note too long. |
| `DELETE /api/notifications/mutes/{id}` | Delete a UI mute rule. 204 on success, 404 for an unknown id, 409 for an env rule. |
| `GET /api/notifications/desktop` | The desktop sink status and the fix steps for this OS. |
| `POST /api/notifications/desktop/test` | Send a test popup now. A failed popup still answers 200, with the reason in `last_error`. |
| `POST /api/notifications/desktop/confirm` | Record that you saw the test popup. |
| `POST /api/notifications/desktop/fix/{step_id}` | Run a runnable fix step. 400 for a Copy-only or unknown step, 500 when the step's command fails. |

The body of `POST /api/notifications`:

| Field | Required | Meaning |
|---|---|---|
| `source` | yes | Who sends it, 1 to 64 characters after trimming. The rate limit counts per source. |
| `severity` | yes | `info`, `warning` or `critical`. |
| `title` | yes | A one-line summary. It must not be blank. |
| `body` | yes | The main text. Title and body hold at most 4,096 bytes together. |
| `kind` | no | `custom` (default), `advisory` or `guard`. The hub refuses `proposal`, because a proposal needs a flag change. |
| `profile`, `model` | no | What the notification is about, for mute rules and the page. |

The hub gives each posted notification a new id, so a repeated post adds a second entry. It answers
400 for a field that breaks these rules, and 429 when the source sent more than its share in the
last minute. Errors have the body `{"error": {"message", "type"}}` of the other `/api/*` routes;
a 429 has the type `rate_limit_error`. A body that is not JSON, or misses a
required field, or names an unknown `severity` or `kind`, fails before these checks with the
framework's plain-text rejection (400, 415 or 422).

A hook that posts its own notification:

```sh
curl -sS -X POST http://127.0.0.1:8888/api/notifications \
  -H "Authorization: Bearer $LLM_HUB_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"source": "nightly-backup", "severity": "warning", "title": "Backup failed", "body": "The nightly backup exited with code 2.", "kind": "custom"}'
```

The generated [API docs](../README.md#api-docs) hold the exact request and response shapes.
