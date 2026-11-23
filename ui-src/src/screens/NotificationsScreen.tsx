import { useEffect, useState } from "react";

import { InlineConfirm } from "../components/InlineConfirm";
import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { Select, type SelectOption } from "../components/Select";
import { api } from "../lib/api";
import { errorText, formatDateTime, formatNumber } from "../lib/format";
import { useLive } from "../lib/live";
import type {
  DesktopStatus,
  HubNotification,
  NotificationMatcher,
  NotificationMute,
  NotificationMuteScope,
  NotificationMutesResponse,
  NotificationSeverity,
  NotificationsResponse,
  NotificationStatus,
} from "../lib/types";
import "../styles/screens/notifications.css";

const HOUR_MS = 3_600_000;
const ALL = "all";
const MATCHER_FIELDS = ["source", "kind", "rule", "profile", "model", "title"] as const;
type MatcherField = (typeof MATCHER_FIELDS)[number];
type MatcherDraft = Record<MatcherField, string>;

type MuteDuration = "1h" | "1d" | "1w" | "forever";
const DURATIONS: readonly SegmentedOption<MuteDuration>[] = [
  { value: "1h", label: "1 hour" },
  { value: "1d", label: "1 day" },
  { value: "1w", label: "1 week" },
  { value: "forever", label: "Forever" },
];
const DURATION_MS: Readonly<Record<MuteDuration, number | null>> = {
  "1h": HOUR_MS,
  "1d": 24 * HOUR_MS,
  "1w": 7 * 24 * HOUR_MS,
  forever: null,
};

type MuteTarget = "exact" | "like";
const TARGETS: readonly SegmentedOption<MuteTarget>[] = [
  { value: "exact", label: "This exact one" },
  { value: "like", label: "All like this" },
];

const SCOPES: readonly SegmentedOption<NotificationMuteScope>[] = [
  { value: "all", label: "Hide and silence" },
  { value: "sinks", label: "Silence popups only" },
];

const SEVERITY_OPTIONS: readonly SelectOption[] = [
  { value: ALL, label: "All severities" },
  { value: "info", label: "Info" },
  { value: "warning", label: "Warning" },
  { value: "critical", label: "Critical" },
];

const STATUS_OPTIONS: readonly SelectOption[] = [
  { value: ALL, label: "All statuses" },
  { value: "pending", label: "Pending" },
  { value: "approved", label: "Approved" },
  { value: "applied", label: "Applied" },
  { value: "failed", label: "Failed" },
  { value: "dismissed", label: "Dismissed" },
];

const SEVERITY_TONE: Readonly<Record<NotificationSeverity, PillTone>> = {
  info: "info",
  warning: "warn",
  critical: "danger",
};

const STATUS_TONE: Readonly<Record<NotificationStatus, PillTone>> = {
  pending: "info",
  approved: "info",
  applied: "ok",
  failed: "danger",
  dismissed: "neutral",
};

const OS_LABEL: Readonly<Record<DesktopStatus["os"], string>> = {
  macos: "macOS",
  linux: "Linux",
  windows: "Windows",
  other: "This OS",
};

interface NotificationsScreenProps {
  readonly onCopy: (text: string) => void;
  readonly onToast: (message: string, isError?: boolean) => void;
  readonly onPending: (pending: number) => void;
}

/** The exact matcher pins source, rule, model and title; a field the card lacks stays unset. */
function exactMatcher(item: HubNotification): NotificationMatcher {
  return compactMatcher({
    source: item.source,
    kind: "",
    rule: item.rule ?? "",
    profile: "",
    model: item.model ?? "",
    title: item.title,
  });
}

function likeDraft(item: HubNotification): MatcherDraft {
  return { source: item.source, kind: item.kind, rule: item.rule ?? "", profile: "", model: "", title: "" };
}

function compactMatcher(draft: MatcherDraft): NotificationMatcher {
  const matcher: Partial<Record<MatcherField, string>> = {};
  for (const field of MATCHER_FIELDS) {
    const glob = draft[field].trim();
    if (glob !== "") {
      matcher[field] = glob;
    }
  }
  return matcher;
}

function matcherLabel(matcher: NotificationMatcher): string {
  return MATCHER_FIELDS.flatMap((field) => {
    const glob = matcher[field];
    return glob ? [`${field}=${glob}`] : [];
  }).join(", ");
}

function needsDesktopSetup(status: DesktopStatus): boolean {
  return (
    status.tool_found === false ||
    status.session_ok === false ||
    status.last_error !== null ||
    !status.confirmed
  );
}

function diffLines(item: HubNotification): readonly string[] {
  if (!item.diff) {
    return [];
  }
  return [
    ...item.diff.remove.map((flag) => `- ${flag}`),
    ...item.diff.add.map((flag) => `+ ${flag}`),
    ...(item.diff.ctx !== null ? [`ctx: ${item.diff.old_ctx ?? "?"} → ${item.diff.ctx}`] : []),
  ];
}

export function NotificationsScreen({ onCopy, onToast, onPending }: NotificationsScreenProps) {
  const [items, setItems] = useState<readonly HubNotification[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [mutes, setMutes] = useState<readonly NotificationMute[]>([]);
  const [desktop, setDesktop] = useState<DesktopStatus | null>(null);
  const [showMuted, setShowMuted] = useState(false);
  const [source, setSource] = useState(ALL);
  const [severity, setSeverity] = useState(ALL);
  const [status, setStatus] = useState(ALL);
  const [confirmApprove, setConfirmApprove] = useState<string | null>(null);
  const [muteFor, setMuteFor] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const { tick } = useLive();
  const reload = () => setReloadKey((key) => key + 1);

  useEffect(() => {
    let cancelled = false;
    api<NotificationsResponse>(`/api/notifications?include_muted=${showMuted ? 1 : 0}`)
      .then((body) => {
        if (!cancelled) {
          setItems(body.notifications);
          setLoadError(null);
          onPending(body.pending);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setLoadError(errorText(err));
        }
      });
    api<NotificationMutesResponse>("/api/notifications/mutes")
      .then((body) => {
        if (!cancelled) {
          setMutes(body.mutes);
        }
      })
      .catch((err: unknown) => console.warn("Mute rules unavailable", err));
    api<DesktopStatus>("/api/notifications/desktop")
      .then((body) => {
        if (!cancelled) {
          setDesktop(body);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          console.warn("Desktop status unavailable", err);
          setDesktop(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [showMuted, tick, reloadKey, onPending]);

  const act = (id: string, action: "approve" | "dismiss") => {
    setBusy(true);
    setConfirmApprove(null);
    api<HubNotification>(`/api/notifications/${encodeURIComponent(id)}/${action}`, { method: "POST" })
      .then((updated) => {
        onToast(action === "approve" ? `Proposal ${updated.status}` : "Dismissed");
        reload();
      })
      .catch((err: unknown) => onToast(errorText(err), true))
      .finally(() => setBusy(false));
  };

  const addMute = (matcher: NotificationMatcher, untilMs: number | null, scope: NotificationMuteScope) => {
    setBusy(true);
    api<NotificationMute>("/api/notifications/mutes", {
      method: "POST",
      body: JSON.stringify({ matcher, until_ms: untilMs, scope }),
    })
      .then(() => {
        setMuteFor(null);
        onToast(`Muted ${matcherLabel(matcher)}`);
        reload();
      })
      .catch((err: unknown) => onToast(errorText(err), true))
      .finally(() => setBusy(false));
  };

  const deleteMute = (id: string) => {
    setBusy(true);
    api<void>(`/api/notifications/mutes/${encodeURIComponent(id)}`, { method: "DELETE" })
      .then(() => {
        onToast("Mute rule deleted");
        reload();
      })
      .catch((err: unknown) => onToast(errorText(err), true))
      .finally(() => setBusy(false));
  };

  const desktopAction = (path: string, done: string) => {
    setBusy(true);
    api<DesktopStatus>(path, { method: "POST" })
      .then((body) => {
        setDesktop(body);
        onToast(done);
      })
      .catch((err: unknown) => onToast(errorText(err), true))
      .finally(() => setBusy(false));
  };

  if (loadError !== null && items === null) {
    return (
      <section className="tab active notifications">
        <Panel title="Notifications unavailable">
          <p className="note" role="status">
            {loadError}. <code>LLM_HUB_NOTIFICATIONS=false</code> turns the inbox off.
          </p>
        </Panel>
      </section>
    );
  }

  const all = items ?? [];
  const sources = [...new Set(all.map((item) => item.source))].sort();
  const sourceOptions: readonly SelectOption[] = [
    { value: ALL, label: "All sources" },
    ...sources.map((name) => ({ value: name, label: name })),
  ];
  const shown = all.filter(
    (item) =>
      (source === ALL || item.source === source) &&
      (severity === ALL || item.severity === severity) &&
      (status === ALL || item.status === status),
  );

  return (
    <section className="tab active notifications" aria-busy={items === null}>
      {desktop && !desktop.enabled ? (
        <p className="note" role="status">
          Desktop popups are off (LLM_HUB_NOTIFY_DESKTOP=false).
        </p>
      ) : desktop && needsDesktopSetup(desktop) ? (
        <DesktopBanner
          status={desktop}
          busy={busy}
          onCopy={onCopy}
          onRun={(stepId) => desktopAction(`/api/notifications/desktop/fix/${encodeURIComponent(stepId)}`, "Step ran")}
          onTest={() => desktopAction("/api/notifications/desktop/test", "Test popup sent")}
          onConfirm={() => desktopAction("/api/notifications/desktop/confirm", "Desktop popups confirmed")}
        />
      ) : null}

      <div className="toolbar notifications-toolbar">
        <Select ariaLabel="Source" value={source} options={sourceOptions} onChange={setSource} />
        <Select ariaLabel="Severity" value={severity} options={SEVERITY_OPTIONS} onChange={setSeverity} />
        <Select ariaLabel="Status" value={status} options={STATUS_OPTIONS} onChange={setStatus} />
        <button
          type="button"
          className="btn small"
          aria-pressed={showMuted}
          onClick={() => setShowMuted((current) => !current)}
        >
          {showMuted ? "Hide muted" : "Show muted"}
        </button>
        <span className="dim">
          {items === null ? "Loading…" : `${formatNumber(shown.length)} of ${formatNumber(all.length)} shown`}
        </span>
      </div>

      <div className="stack">
        {items !== null && shown.length === 0 ? (
          <Panel>
            <p className="note">No notifications match these filters.</p>
          </Panel>
        ) : null}
        {shown.map((item) => (
          <NotificationCard
            key={item.id}
            item={item}
            busy={busy}
            confirming={confirmApprove === item.id}
            muting={muteFor === item.id}
            onApprove={() => setConfirmApprove(item.id)}
            onCancelApprove={() => setConfirmApprove(null)}
            onConfirmApprove={() => act(item.id, "approve")}
            onDismiss={() => act(item.id, "dismiss")}
            onToggleMute={() => setMuteFor((current) => (current === item.id ? null : item.id))}
            onMute={addMute}
          />
        ))}

        <Panel title="Mutes" sub="Rules that hide notifications or keep them from desktop, webhook and command sinks">
          {mutes.length === 0 ? (
            <p className="note">No mute rules.</p>
          ) : (
            <div className="table-scroll">
              <table className="table notifications-mutes">
                <thead>
                  <tr>
                    <th>Match</th>
                    <th>Scope</th>
                    <th>Until</th>
                    <th>Note</th>
                    <th>
                      <span className="sr-only">Action</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {mutes.map((mute) => (
                    <tr key={mute.id}>
                      <td className="mono">{matcherLabel(mute.matcher)}</td>
                      <td className="nowrap">{mute.scope === "all" ? "Hide and silence" : "Silence popups"}</td>
                      <td className="nowrap">{mute.until_ms === null ? "Forever" : formatDateTime(mute.until_ms)}</td>
                      <td className="dim">{mute.note ?? ""}</td>
                      <td className="nowrap">
                        {mute.origin === "env" ? (
                          <button
                            type="button"
                            className="btn small"
                            disabled
                            title="Set in LLM_HUB_NOTIFY_MUTE; edit the env to remove it"
                          >
                            from env
                          </button>
                        ) : (
                          <button
                            type="button"
                            className="btn small"
                            disabled={busy}
                            onClick={() => deleteMute(mute.id)}
                          >
                            Delete
                          </button>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Panel>
      </div>
    </section>
  );
}

interface NotificationCardProps {
  readonly item: HubNotification;
  readonly busy: boolean;
  readonly confirming: boolean;
  readonly muting: boolean;
  readonly onApprove: () => void;
  readonly onCancelApprove: () => void;
  readonly onConfirmApprove: () => void;
  readonly onDismiss: () => void;
  readonly onToggleMute: () => void;
  readonly onMute: (matcher: NotificationMatcher, untilMs: number | null, scope: NotificationMuteScope) => void;
}

function NotificationCard({
  item,
  busy,
  confirming,
  muting,
  onApprove,
  onCancelApprove,
  onConfirmApprove,
  onDismiss,
  onToggleMute,
  onMute,
}: NotificationCardProps) {
  const pending = item.status === "pending";
  const canApprove = pending && item.kind === "proposal" && item.actions.includes("approve");
  const canDismiss = pending && item.actions.includes("dismiss");
  const lines = diffLines(item);
  const showResult = item.result !== null && (item.status === "applied" || item.status === "failed");

  return (
    <Panel className={item.muted ? "notification-card muted" : "notification-card"}>
      <div className="notification-head">
        <Pill tone={SEVERITY_TONE[item.severity]}>{item.severity}</Pill>
        <Pill tone={STATUS_TONE[item.status]}>{item.status}</Pill>
        {item.muted ? <Pill title="A mute rule with scope all matched">muted</Pill> : null}
        <span className="mono dim">{item.source}</span>
        <span className="dim notification-time" title={`Updated ${formatDateTime(item.updated_ms)}`}>
          {formatDateTime(item.created_ms)}
        </span>
      </div>
      <h3 className="notification-title">{item.title}</h3>
      {item.profile !== null || item.model !== null || item.rule !== null ? (
        <div className="notification-meta mono dim">
          {item.profile !== null ? <span>profile {item.profile}</span> : null}
          {item.model !== null ? <span>model {item.model}</span> : null}
          {item.rule !== null ? <span>rule {item.rule}</span> : null}
        </div>
      ) : null}
      {item.body ? <p className="notification-body">{item.body}</p> : null}
      {lines.length > 0 ? (
        <pre className="snippet notification-diff">
          {lines.map((line, index) => (
            <span
              key={`${index}:${line}`}
              className={line.startsWith("+") ? "diff-add" : line.startsWith("-") ? "diff-remove" : "diff-ctx"}
            >
              {line}
              {"\n"}
            </span>
          ))}
        </pre>
      ) : null}
      {showResult ? <pre className="snippet notification-result">{item.result}</pre> : null}

      {confirming ? (
        <InlineConfirm
          title="Apply this proposal?"
          confirmLabel="Approve"
          onConfirm={onConfirmApprove}
          onCancel={onCancelApprove}
        >
          The hub edits the model registry and runs the apply command.
        </InlineConfirm>
      ) : (
        <div className="notification-actions">
          {canApprove ? (
            <button type="button" className="btn small primary" disabled={busy} onClick={onApprove}>
              Approve
            </button>
          ) : null}
          {canDismiss ? (
            <button type="button" className="btn small" disabled={busy} onClick={onDismiss}>
              Dismiss
            </button>
          ) : null}
          <button type="button" className="btn small" aria-expanded={muting} onClick={onToggleMute}>
            Mute…
          </button>
        </div>
      )}
      {muting ? <MuteForm item={item} busy={busy} onCancel={onToggleMute} onMute={onMute} /> : null}
    </Panel>
  );
}

interface MuteFormProps {
  readonly item: HubNotification;
  readonly busy: boolean;
  readonly onCancel: () => void;
  readonly onMute: (matcher: NotificationMatcher, untilMs: number | null, scope: NotificationMuteScope) => void;
}

function MuteForm({ item, busy, onCancel, onMute }: MuteFormProps) {
  const [duration, setDuration] = useState<MuteDuration>("1d");
  const [target, setTarget] = useState<MuteTarget>("exact");
  const [scope, setScope] = useState<NotificationMuteScope>("all");
  const [draft, setDraft] = useState<MatcherDraft>(() => likeDraft(item));
  const matcher = target === "exact" ? exactMatcher(item) : compactMatcher(draft);
  const empty = Object.keys(matcher).length === 0;

  const save = () => {
    const span = DURATION_MS[duration];
    onMute(matcher, span === null ? null : Date.now() + span, scope);
  };

  return (
    <div className="notification-mute">
      <div className="toolbar">
        <Segmented label="Mute duration" options={DURATIONS} value={duration} onChange={setDuration} />
        <Segmented label="Mute target" options={TARGETS} value={target} onChange={setTarget} />
        <Segmented label="Mute scope" options={SCOPES} value={scope} onChange={setScope} />
      </div>
      {target === "like" ? (
        <div className="notification-mute-fields">
          {MATCHER_FIELDS.map((field) => (
            <div key={field} className="field">
              <label htmlFor={`mute-${item.id}-${field}`}>{field}</label>
              <input
                id={`mute-${item.id}-${field}`}
                className="input"
                value={draft[field]}
                placeholder="any"
                onChange={(event) => setDraft((current) => ({ ...current, [field]: event.target.value }))}
              />
            </div>
          ))}
        </div>
      ) : null}
      <p className="dim">
        Matches <span className="mono">{empty ? "nothing set" : matcherLabel(matcher)}</span>. <code>*</code> matches
        any run of characters.
      </p>
      <div className="notification-actions">
        <button type="button" className="btn small" onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className="btn small primary" disabled={busy || empty} onClick={save}>
          Mute
        </button>
      </div>
    </div>
  );
}

interface DesktopBannerProps {
  readonly status: DesktopStatus;
  readonly busy: boolean;
  readonly onCopy: (text: string) => void;
  readonly onRun: (stepId: string) => void;
  readonly onTest: () => void;
  readonly onConfirm: () => void;
}

function desktopProblem(status: DesktopStatus): string {
  if (status.tool_found === false) {
    return "The hub cannot find the popup tool on PATH.";
  }
  if (status.session_ok === false) {
    return "The hub sees no desktop session.";
  }
  if (status.last_error !== null) {
    return `The last popup failed: ${status.last_error}`;
  }
  return "Send a test popup and confirm that you saw it.";
}

function DesktopBanner({ status, busy, onCopy, onRun, onTest, onConfirm }: DesktopBannerProps) {
  return (
    <Panel
      className="notifications-desktop"
      title={`Set up desktop popups · ${OS_LABEL[status.os]}`}
      sub={status.last_ok_ms !== null ? `Last popup worked ${formatDateTime(status.last_ok_ms)}` : undefined}
      actions={
        <>
          <button type="button" className="btn small" disabled={busy} onClick={onTest}>
            Send test
          </button>
          <button type="button" className="btn small primary" disabled={busy} onClick={onConfirm}>
            I saw it
          </button>
        </>
      }
    >
      <p className={status.last_error !== null ? "note notifications-desktop-error" : "note"} role="status">
        {desktopProblem(status)}
      </p>
      {status.steps.length > 0 ? (
        <ol className="notifications-steps">
          {status.steps.map((step) => (
            <li key={step.id}>
              <span>{step.text}</span>
              {step.command !== null ? (
                <div className="notifications-step-command">
                  <code className="mono">{step.command}</code>
                  <button type="button" className="btn small" onClick={() => onCopy(step.command ?? "")}>
                    Copy
                  </button>
                  {step.runnable ? (
                    <button type="button" className="btn small" disabled={busy} onClick={() => onRun(step.id)}>
                      Run
                    </button>
                  ) : null}
                </div>
              ) : step.runnable ? (
                <div className="notifications-step-command">
                  <button type="button" className="btn small" disabled={busy} onClick={() => onRun(step.id)}>
                    Run
                  </button>
                </div>
              ) : null}
            </li>
          ))}
        </ol>
      ) : null}
    </Panel>
  );
}
