import { type FormEvent, type ReactNode, useCallback, useEffect, useRef, useState } from "react";

import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { api } from "../lib/api";
import { formatDateTime, formatMs, formatNumber } from "../lib/format";
import type {
  CurrentSelection,
  CurrentSelectionResponse,
  ProfileRow,
  SelectionEvent,
  SelectionHistoryResponse,
  SelectionState,
  SmartAutoEffectReason,
  SmartAutoEffectResponse,
  SmartAutoEffectRow,
  SmartAutoGlobs,
  SmartAutoRules,
  SmartAutoRulesResponse,
  SmartAutoRulesSource,
} from "../lib/types";
import "../styles/screens/smart-auto.css";
import { SmartAutoHowItWorks } from "./SmartAutoHowItWorks";

/** Events the hub serves per page; the hub clamps larger requests to the same limit. */
const PAGE_LIMIT = 1000;
const MAX_PAGES_PER_REFRESH = 10;
/** Newest events kept in the browser; older ones drop off the timeline. */
const MAX_EVENTS = 2000;
/** Rows drawn in the timeline, newest first. */
const MAX_ROWS = 300;
const REFRESH_MS = 3000;
const SHORT_KEY_CHARS = 8;
const ALL = "";
const UNSET = "—";

const STATE_TONE: Readonly<Record<SelectionState, PillTone>> = {
  selected: "info",
  started: "info",
  completed: "ok",
  rejected: "danger",
  cancelled: "warn",
};

const FAILED_STATES: ReadonlySet<SelectionState> = new Set<SelectionState>(["rejected", "cancelled"]);

interface Filters {
  readonly workflow: string;
  readonly worker: string;
  readonly stage: string;
  readonly model: string;
  readonly effort: string;
}

const NO_FILTERS: Filters = { workflow: ALL, worker: ALL, stage: ALL, model: ALL, effort: ALL };

function shortKey(key: string | null): string {
  return key ? key.slice(0, SHORT_KEY_CHARS) : UNSET;
}

/** The distinct non-empty values `pick` yields, sorted, for one filter's options. */
function distinct<T>(items: readonly T[], pick: (item: T) => string | null): string[] {
  const values = new Set<string>();
  for (const item of items) {
    const value = pick(item);
    if (value) {
      values.add(value);
    }
  }
  return [...values].sort();
}

function matches(event: SelectionEvent, filters: Filters): boolean {
  return (
    (filters.workflow === ALL || event.workflow === filters.workflow) &&
    (filters.worker === ALL || event.worker === filters.worker) &&
    (filters.stage === ALL || event.stage === filters.stage) &&
    (filters.model === ALL || event.model === filters.model) &&
    (filters.effort === ALL || effortText(event) === filters.effort)
  );
}

function effortText(event: SelectionEvent): string {
  return event.sent_effort ?? event.selected_effort ?? UNSET;
}

function effortTitle(event: SelectionEvent): string {
  return [
    `requested ${event.requested_effort ?? UNSET}`,
    `selected ${event.selected_effort ?? UNSET}`,
    `sent ${event.sent_effort ?? UNSET}`,
    `confirmed ${event.confirmed_effort ?? UNSET}`,
  ].join(" · ");
}

/** Prompt and completion tokens the upstream reported for one dispatch. */
function tokenText(event: SelectionEvent): string {
  const tokensIn = event.tokens_in === null ? UNSET : formatNumber(event.tokens_in);
  const tokensOut = event.tokens_out === null ? UNSET : formatNumber(event.tokens_out);
  return `${tokensIn} / ${tokensOut}`;
}

/** Age of the latest selection event, in the largest whole unit. */
function formatAge(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) {
    return `${seconds} s ago`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours} h ago`;
  }
  return `${Math.floor(hours / 24)} d ago`;
}

function FilterSelect({
  label,
  value,
  options,
  format,
  onChange,
}: {
  readonly label: string;
  readonly value: string;
  readonly options: readonly string[];
  readonly format: (value: string) => string;
  readonly onChange: (value: string) => void;
}) {
  return (
    <label className="smart-auto-filter">
      <span>{label}</span>
      <select className="input" value={value} onChange={(event) => onChange(event.target.value)}>
        <option value={ALL}>All</option>
        {options.map((option) => (
          <option key={option} value={option}>
            {format(option)}
          </option>
        ))}
      </select>
    </label>
  );
}

function EventRow({ event, startsTurn }: { readonly event: SelectionEvent; readonly startsTurn: boolean }) {
  const failed = FAILED_STATES.has(event.state);
  const className = [startsTurn ? "turn-start" : "", failed ? "failed" : ""].filter(Boolean).join(" ");
  return (
    <tr className={className || undefined}>
      <td data-label="Time">{formatDateTime(event.ts_ms)}</td>
      <td className="mono" title={event.turn ?? undefined} data-label="Turn">
        {shortKey(event.turn)}
      </td>
      <td className="mono" title={event.worker ?? undefined} data-label="Worker">
        {shortKey(event.worker)}
      </td>
      <td className="mono" title={event.workflow ?? undefined} data-label="Workflow">
        {shortKey(event.workflow)}
      </td>
      <td data-label="Stage">{event.stage}</td>
      <td className="mono" data-label="Model">
        {event.model}
      </td>
      <td data-label="State">
        <Pill tone={STATE_TONE[event.state]} dot>
          {event.state}
        </Pill>
      </td>
      <td className="smart-auto-reason" data-label="Reason">
        {event.reason ?? UNSET}
      </td>
      <td data-label="Role">{event.role ?? UNSET}</td>
      <td className="mono" title={effortTitle(event)} data-label="Effort">
        {effortText(event)}
      </td>
      <td className="num" data-label="Status">
        {event.status ?? UNSET}
      </td>
      <td className="num" data-label="Latency">
        {event.latency_ms === null ? UNSET : formatMs(event.latency_ms)}
      </td>
      <td className="num" data-label="Tokens in / out">
        {tokenText(event)}
      </td>
    </tr>
  );
}

/**
 * The current selection of one session or worker, from `GET /api/smart-auto/current`.
 * The Worker column shows salted hashes the hub cannot match, so the id is typed here instead.
 */
function CurrentSelectionPanel() {
  const [draft, setDraft] = useState("");
  // The id being watched; null until the user submits one.
  const [watched, setWatched] = useState<string | null>(null);
  // undefined until the first answer for `watched` arrives; null when the hub has no selection yet.
  const [selection, setSelection] = useState<CurrentSelection | null | undefined>(undefined);
  const [unavailable, setUnavailable] = useState(false);

  useEffect(() => {
    if (watched === null) {
      return;
    }
    let cancelled = false;
    const refresh = async () => {
      try {
        const params = new URLSearchParams({ session: watched });
        const body = await api<CurrentSelectionResponse>(`/api/smart-auto/current?${params}`);
        if (!cancelled) {
          setUnavailable(false);
          setSelection(body.selection);
        }
      } catch (err: unknown) {
        if (!cancelled) {
          setUnavailable(true);
        }
      }
    };
    void refresh();
    const timer = setInterval(() => void refresh(), REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [watched]);

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const next = draft.trim();
    setWatched(next === "" ? null : next);
    setSelection(undefined);
    setUnavailable(false);
  };

  const main = selection?.main ?? null;
  const effort = main?.sent_effort ?? main?.selected_effort ?? null;

  let body: ReactNode;
  if (watched === null) {
    body = <p className="note">Enter a session or worker id to see its current selection.</p>;
  } else if (selection === undefined) {
    body = unavailable ? (
      <p className="note" role="status">
        Current selection is unavailable. The panel retries every {REFRESH_MS / 1000} s.
      </p>
    ) : (
      <p className="note">Loading…</p>
    );
  } else if (selection === null) {
    body = <p className="note">No selection yet for this id.</p>;
  } else {
    body = (
      <dl className="smart-auto-current-facts">
        <div>
          <dt>Model</dt>
          <dd className="mono">{main?.model ?? UNSET}</dd>
        </div>
        <div>
          <dt>Tier</dt>
          <dd>{main?.tier ?? UNSET}</dd>
        </div>
        <div>
          <dt>Role</dt>
          <dd>{main?.role ?? UNSET}</dd>
        </div>
        {effort === null ? null : (
          <div>
            <dt>Effort</dt>
            <dd className="mono">{effort}</dd>
          </div>
        )}
        <div>
          <dt>State</dt>
          <dd>
            <Pill tone={STATE_TONE[selection.state]} dot>
              {selection.state}
            </Pill>
            {selection.stale ? (
              <Pill tone="warn" dot>
                stale
              </Pill>
            ) : null}
          </dd>
        </div>
        <div>
          <dt>Age</dt>
          <dd>{formatAge(selection.age_ms)}</dd>
        </div>
      </dl>
    );
  }

  return (
    <Panel
      title="Current selection"
      sub={watched === null ? undefined : `refreshed every ${REFRESH_MS / 1000} s`}
      className="smart-auto-current"
    >
      <form className="smart-auto-current-form" onSubmit={submit}>
        <label className="smart-auto-filter smart-auto-current-input">
          <span>Session or worker id</span>
          <input
            className="input"
            value={draft}
            placeholder="raw session id"
            onChange={(event) => setDraft(event.target.value)}
          />
        </label>
        <button type="submit" className="btn small">
          Show selection
        </button>
      </form>
      {watched !== null && selection !== undefined && unavailable ? (
        <Pill tone="warn" dot>
          <span role="status">Refresh failed, showing the last result</span>
        </Pill>
      ) : null}
      {body}
    </Panel>
  );
}

const EMPTY_GLOBS: SmartAutoGlobs = { include: [], exclude: [] };

const SOURCE_TEXT: Readonly<Record<SmartAutoRulesSource, string>> = {
  file: "Saved in the hub's rules file",
  env: "Set by LLM_HUB_SMART_AUTO_RULES",
  none: "No rules set yet",
};

function messageOf(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/** Trims each glob, then drops empty entries and repeats. */
function cleanGlobList(list: readonly string[]): string[] {
  return [...new Set(list.map((glob) => glob.trim()).filter((glob) => glob !== ""))];
}

function cleanGlobs(globs: SmartAutoGlobs): SmartAutoGlobs {
  return { include: cleanGlobList(globs.include), exclude: cleanGlobList(globs.exclude) };
}

function cleanRules(rules: SmartAutoRules): SmartAutoRules {
  const profiles: Record<string, SmartAutoGlobs> = {};
  for (const [name, globs] of Object.entries(rules.profiles)) {
    profiles[name] = cleanGlobs(globs);
  }
  return { ...cleanGlobs(rules), allow: cleanGlobList(rules.allow), profiles };
}

/** Rules as the hub returned them; a missing `allow` from the server reads as an empty list. */
function fromServer(rules: SmartAutoRules): SmartAutoRules {
  return { ...rules, allow: rules.allow ?? [] };
}

/** A stable text form of the cleaned rules, so the draft can be compared with the saved copy. */
function serializeRules(rules: SmartAutoRules): string {
  const clean = cleanRules(rules);
  const profiles = Object.entries(clean.profiles).sort(([a], [b]) => a.localeCompare(b));
  return JSON.stringify([clean.include, clean.exclude, clean.allow, profiles]);
}

/** `globs` with one of its two lists replaced. */
function patchGlobs(globs: SmartAutoGlobs, key: keyof SmartAutoGlobs, value: readonly string[]): SmartAutoGlobs {
  return key === "include" ? { include: value, exclude: globs.exclude } : { include: globs.include, exclude: value };
}

/** The list with `value` appended, or the same list when `value` is empty or already present. */
function withGlob(list: readonly string[], value: string): readonly string[] {
  const glob = value.trim();
  return glob === "" || list.includes(glob) ? list : [...list, glob];
}

function withoutProfile(
  profiles: Readonly<Record<string, SmartAutoGlobs>>,
  name: string,
): Record<string, SmartAutoGlobs> {
  const next: Record<string, SmartAutoGlobs> = {};
  for (const [key, globs] of Object.entries(profiles)) {
    if (key !== name) {
      next[key] = globs;
    }
  }
  return next;
}

/** Shown when a Results click needs the draft saved first. */
const UNSAVED_RESULTS_NOTE = "You have unsaved changes above. Save them first, then click a model.";
/** Shown for a model that a global exclude edit cannot change. */
const LIST_FIXED_NOTE = "This model is excluded by its type or an include list. Change those lists in the rules above.";

/** Whether the saved rules let the model in: an eligible row, or one an explicit allow put in. */
function isAllowed(row: SmartAutoEffectRow): boolean {
  return row.eligible || row.reason === "allowed_override";
}

/**
 * `saved` with one Results row flipped, or null when no global list edit can flip it.
 * An allowed row moves from the global allow to the global exclude, and an excluded row moves back.
 */
/** True when a `*` glob matches the whole id, with `*` standing for any run of characters. */
function globMatches(glob: string, id: string): boolean {
  const pattern = glob.split("*").map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*");
  return new RegExp(`^${pattern}$`).test(id);
}

function rulesWithResultToggled(saved: SmartAutoRules, row: SmartAutoEffectRow): SmartAutoRules | null {
  const id = row.qualified_id;
  if (isAllowed(row)) {
    const allow = saved.allow.filter((entry) => entry !== id);
    // Only add the exact id when no remaining exclude glob already excludes it, so the rules keep their form.
    const excludedAnyway = saved.exclude.some((glob) => globMatches(glob, id));
    return { ...saved, allow, exclude: excludedAnyway ? saved.exclude : withGlob(saved.exclude, id) };
  }
  if (row.reason !== "excluded_by_global" && row.reason !== "excluded_by_profile") {
    return null;
  }
  return { ...saved, allow: withGlob(saved.allow, id), exclude: saved.exclude.filter((entry) => entry !== id) };
}

const REASON_TEXT: Readonly<Record<SmartAutoEffectReason, string>> = {
  eligible: "Allowed",
  allowed_override: "Allowed by an explicit allow",
  excluded_by_global: "Excluded by global exclude",
  excluded_by_profile: "Excluded by profile exclude",
  not_in_global_include: "Not in global include",
  not_in_profile_include: "Not in this profile's include",
  not_chat: "Not a chat model",
  auto_alias: "Auto alias",
  hub_namespace: "Hub namespace",
};

/** Reasons in the order the summary lists them. */
const REASON_ORDER = Object.keys(REASON_TEXT) as SmartAutoEffectReason[];

/** The profile prefix of a "profile/model" id, or the whole id when it has no slash. */
function profileOf(qualifiedId: string): string {
  const slash = qualifiedId.indexOf("/");
  return slash === -1 ? qualifiedId : qualifiedId.slice(0, slash);
}

type ResultFilter = "allowed" | "excluded" | "all";

const RESULT_FILTERS: readonly ResultFilter[] = ["allowed", "excluded", "all"];

const RESULT_FILTER_TEXT: Readonly<Record<ResultFilter, string>> = {
  allowed: "Allowed",
  excluded: "Excluded",
  all: "All",
};

/** Which models the saved rules let Smart Auto pick. A row click flips it; the profile and result filters narrow the view. */
function EffectResults({
  rows,
  interactive,
  locked,
  busyId,
  note,
  toggleError,
  onToggle,
}: {
  readonly rows: readonly SmartAutoEffectRow[];
  /** False for read-only rules, so rows are plain text. */
  readonly interactive: boolean;
  /** True while a save runs, so rows ignore clicks. */
  readonly locked: boolean;
  /** The qualified id whose save is running, if any. */
  readonly busyId: string | null;
  readonly note: string | null;
  readonly toggleError: string | null;
  readonly onToggle: (row: SmartAutoEffectRow) => void;
}) {
  const [profile, setProfile] = useState(ALL);
  const [result, setResult] = useState<ResultFilter>("allowed");
  const eligible = rows.filter(isAllowed).length;
  const counts = new Map<SmartAutoEffectReason, number>();
  for (const row of rows) {
    counts.set(row.reason, (counts.get(row.reason) ?? 0) + 1);
  }
  const reasons = REASON_ORDER.filter((reason) => counts.has(reason));
  const profiles = distinct(rows, (row) => profileOf(row.qualified_id));
  // A profile that left the rows after a reload shows every row again.
  const active = profiles.includes(profile) ? profile : ALL;
  const inProfile = active === ALL ? rows : rows.filter((row) => profileOf(row.qualified_id) === active);
  const shown = inProfile.filter((row) => result === "all" || isAllowed(row) === (result === "allowed"));
  let emptyText = "No models match these filters.";
  if (rows.length === 0) {
    emptyText = "No models yet";
  } else if (inProfile.length === 0) {
    emptyText = "No models match this profile.";
  }

  return (
    <>
      <p className="smart-auto-effect-summary">
        {formatNumber(eligible)} eligible, {formatNumber(rows.length - eligible)} excluded
      </p>
      <div className="smart-auto-effect-reasons">
        {reasons.map((reason) => (
          <Pill key={reason} tone={reason === "eligible" || reason === "allowed_override" ? "ok" : undefined}>
            {REASON_TEXT[reason]}: {counts.get(reason)}
          </Pill>
        ))}
      </div>
      <div className="smart-auto-toolbar">
        <FilterSelect
          label="Profile"
          value={active}
          options={profiles}
          format={(value) => value}
          onChange={setProfile}
        />
        <label className="smart-auto-filter">
          <span>Result</span>
          <select className="input" value={result} onChange={(event) => setResult(event.target.value as ResultFilter)}>
            {RESULT_FILTERS.map((option) => (
              <option key={option} value={option}>
                {RESULT_FILTER_TEXT[option]}
              </option>
            ))}
          </select>
        </label>
      </div>
      {note !== null ? (
        <p className="note" role="status">
          {note}
        </p>
      ) : null}
      {toggleError !== null ? (
        <p className="smart-auto-rules-error" role="alert">
          {toggleError}
        </p>
      ) : null}
      <div className="table-scroll smart-auto-table">
        <table className="table">
          <thead>
            <tr>
              <th>Model</th>
              <th>Result</th>
              <th>Reason</th>
              <th>Matched glob</th>
            </tr>
          </thead>
          <tbody>
            {shown.length === 0 ? (
              <tr>
                <td colSpan={4} className="table-empty">
                  {emptyText}
                </td>
              </tr>
            ) : (
              shown.map((row) => {
                const busy = busyId === row.qualified_id;
                return (
                  <tr
                    key={row.qualified_id}
                    role={interactive ? "button" : undefined}
                    tabIndex={interactive ? 0 : undefined}
                    aria-pressed={interactive ? isAllowed(row) : undefined}
                    aria-disabled={interactive && locked ? true : undefined}
                    aria-busy={busy || undefined}
                    title={interactive ? (isAllowed(row) ? "Exclude this model" : "Allow this model") : undefined}
                    onClick={interactive && !locked ? () => onToggle(row) : undefined}
                    onKeyDown={
                      interactive && !locked
                        ? (event) => {
                            if (event.key === "Enter" || event.key === " ") {
                              event.preventDefault();
                              onToggle(row);
                            }
                          }
                        : undefined
                    }
                  >
                    <td className="mono" data-label="Model">
                      {row.qualified_id}
                    </td>
                    <td data-label="Result">
                      {busy ? (
                        <span className="dim">Saving…</span>
                      ) : (
                        <Pill tone={isAllowed(row) ? "ok" : undefined} dot>
                          {isAllowed(row) ? "eligible" : "excluded"}
                        </Pill>
                      )}
                    </td>
                    <td className="smart-auto-reason" data-label="Reason">
                      {REASON_TEXT[row.reason]}
                    </td>
                    <td className="mono" data-label="Matched glob">
                      {row.matched ?? UNSET}
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>
    </>
  );
}

interface GlobListProps {
  readonly label: string;
  readonly globs: readonly string[];
  readonly disabled: boolean;
  readonly onChange: (globs: readonly string[]) => void;
}

/** Glob chips, each with its own remove button, and a field that adds one on Enter or Add. */
function GlobList({ label, globs, disabled, onChange }: GlobListProps) {
  const [draft, setDraft] = useState("");

  const add = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const next = withGlob(globs, draft);
    if (next !== globs) {
      onChange(next);
    }
    setDraft("");
  };

  return (
    <div className="smart-auto-globs">
      {globs.length === 0 ? (
        <span className="dim">None</span>
      ) : (
        <ul className="chips smart-auto-glob-list" aria-label={label}>
          {globs.map((glob) => (
            <li key={glob} className="chip smart-auto-glob">
              <span className="smart-auto-glob-text">{glob}</span>
              <button
                type="button"
                className="smart-auto-glob-remove"
                aria-label={`Remove ${glob} from ${label}`}
                disabled={disabled}
                onClick={() => onChange(globs.filter((entry) => entry !== glob))}
              >
                ×
              </button>
            </li>
          ))}
        </ul>
      )}
      <form className="smart-auto-glob-form" onSubmit={add}>
        <input
          className="input"
          aria-label={`Add to ${label}`}
          placeholder="copilot/claude-*"
          value={draft}
          disabled={disabled}
          onChange={(event) => setDraft(event.target.value)}
        />
        <button type="submit" className="btn small" disabled={disabled || draft.trim() === ""}>
          Add
        </button>
      </form>
    </div>
  );
}

/**
 * The Smart Auto rules from `GET /api/smart-auto/rules`: global include and exclude globs, and one
 * row per profile. Edits stay in the draft until Save PUTs the whole document.
 */
function SmartAutoRulesPanel({ profiles }: { readonly profiles: readonly ProfileRow[] }) {
  const [loaded, setLoaded] = useState<SmartAutoRulesResponse | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  // The rules the hub last returned; `draft` is what the form edits until Save.
  const [saved, setSaved] = useState<SmartAutoRules | null>(null);
  const [draft, setDraft] = useState<SmartAutoRules | null>(null);
  const [newProfile, setNewProfile] = useState("");
  const [profileError, setProfileError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [justSaved, setJustSaved] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  // The rows of `GET /api/smart-auto/rules/effect`; `effectSeq` drops an answer that a newer load overtook.
  const [effect, setEffect] = useState<readonly SmartAutoEffectRow[] | null>(null);
  const [effectError, setEffectError] = useState<string | null>(null);
  const effectSeq = useRef(0);
  // The Results row whose click is saving, and the inline Results messages from the last click.
  const [resultBusy, setResultBusy] = useState<string | null>(null);
  const [resultNote, setResultNote] = useState<string | null>(null);
  const [resultError, setResultError] = useState<string | null>(null);

  const loadEffect = useCallback(() => {
    effectSeq.current += 1;
    const seq = effectSeq.current;
    api<SmartAutoEffectResponse>("/api/smart-auto/rules/effect")
      .then((body) => {
        if (seq === effectSeq.current) {
          setEffect(body.rows);
          setEffectError(null);
        }
      })
      .catch((err: unknown) => {
        if (seq === effectSeq.current) {
          setEffectError(messageOf(err));
        }
      });
  }, []);

  useEffect(() => {
    loadEffect();
    return () => {
      effectSeq.current += 1;
    };
  }, [loadEffect]);

  useEffect(() => {
    let cancelled = false;
    api<SmartAutoRulesResponse>("/api/smart-auto/rules")
      .then((body) => {
        if (!cancelled) {
          setLoaded(body);
          setSaved(fromServer(body.rules));
          setDraft(fromServer(body.rules));
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setLoadError(messageOf(err));
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const isReadonly = loaded?.readonly === true;
  const locked = isReadonly || saving || resultBusy !== null || draft === null;
  const dirty = draft !== null && saved !== null && serializeRules(draft) !== serializeRules(saved);
  const known = new Set(profiles.map((profile) => profile.name));
  // Profiles from the hub first, then any name the rules mention or the user added by hand.
  const names = [...new Set([...profiles.map((profile) => profile.name), ...Object.keys(draft?.profiles ?? {})])];

  const edit = (next: SmartAutoRules) => {
    setDraft(next);
    setJustSaved(false);
    setSaveError(null);
  };

  const setGlobal = (key: "include" | "exclude" | "allow", value: readonly string[]) => {
    if (draft !== null) {
      edit({ ...draft, [key]: value });
    }
  };

  // Adds "<profile>/*" to the global exclude for every profile except copilot. Save still writes it.
  const excludeNonCopilot = () => {
    if (draft !== null) {
      const names = profiles.map((profile) => profile.name).filter((name) => name !== "copilot");
      const added = names.map((name) => `${name}/*`);
      setGlobal("exclude", [...new Set([...draft.exclude, ...added])]);
    }
  };

  const setProfileGlobs = (name: string, key: keyof SmartAutoGlobs, value: readonly string[]) => {
    if (draft !== null) {
      const current = draft.profiles[name] ?? EMPTY_GLOBS;
      edit({ ...draft, profiles: { ...draft.profiles, [name]: patchGlobs(current, key, value) } });
    }
  };

  const addProfile = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const name = newProfile.trim();
    if (name === "" || draft === null) {
      return;
    }
    if (name.includes("/")) {
      setProfileError('A profile name cannot contain "/", because the globs split on it.');
      return;
    }
    if (names.includes(name)) {
      setProfileError(`${name} is already in the table.`);
      return;
    }
    edit({ ...draft, profiles: { ...draft.profiles, [name]: EMPTY_GLOBS } });
    setNewProfile("");
    setProfileError(null);
  };

  const removeProfile = (name: string) => {
    if (draft !== null) {
      edit({ ...draft, profiles: withoutProfile(draft.profiles, name) });
    }
  };

  /** PUTs `rules` as the whole document. On success they become the saved and draft copy and the results reload. Returns the error message, or null. */
  const putRules = async (rules: SmartAutoRules): Promise<string | null> => {
    setSaveError(null);
    setJustSaved(false);
    try {
      const body = await api<SmartAutoRulesResponse>("/api/smart-auto/rules", {
        method: "PUT",
        body: JSON.stringify(cleanRules(rules)),
      });
      setSaved(fromServer(body.rules));
      setDraft(fromServer(body.rules));
      setJustSaved(true);
      // The saved rules change which models pass, so the results are reloaded.
      loadEffect();
      return null;
    } catch (err) {
      return messageOf(err);
    }
  };

  const save = async () => {
    if (draft === null) {
      return;
    }
    setSaving(true);
    setSaveError(await putRules(draft));
    setSaving(false);
  };

  /** Flips one Results row. The new rules are built from the saved copy, so unsaved draft edits are never written. */
  const toggleResult = async (row: SmartAutoEffectRow) => {
    if (saved === null || locked) {
      return;
    }
    setResultNote(null);
    setResultError(null);
    if (dirty) {
      setResultNote(UNSAVED_RESULTS_NOTE);
      return;
    }
    const next = rulesWithResultToggled(saved, row);
    if (next === null) {
      setResultNote(LIST_FIXED_NOTE);
      return;
    }
    setResultBusy(row.qualified_id);
    const error = await putRules(next);
    setResultBusy(null);
    setResultError(error);
  };

  let body: ReactNode;
  if (loadError !== null) {
    body = (
      <p className="note" role="status">
        Rules are unavailable: {loadError}
      </p>
    );
  } else if (draft === null) {
    body = <p className="note">Loading…</p>;
  } else {
    body = (
      <>
        {isReadonly ? (
          <p className="note smart-auto-rules-notice" role="status">
            These rules are read-only. The <code>LLM_HUB_SMART_AUTO_RULES</code> environment variable controls them, so
            editing is off here.
          </p>
        ) : null}

        <div className="smart-auto-rules-global">
          <div className="smart-auto-rules-group">
            <h3 className="smart-auto-rules-heading">Global include</h3>
            <GlobList
              label="global include"
              globs={draft.include}
              disabled={locked}
              onChange={(value) => setGlobal("include", value)}
            />
          </div>
          <div className="smart-auto-rules-group">
            <h3 className="smart-auto-rules-heading">Global exclude</h3>
            <button type="button" className="btn" disabled={locked} onClick={excludeNonCopilot}>
              Exclude all non-Copilot profiles
            </button>
            <GlobList
              label="global exclude"
              globs={draft.exclude}
              disabled={locked}
              onChange={(value) => setGlobal("exclude", value)}
            />
            <h3 className="smart-auto-rules-heading smart-auto-rules-subheading">Global allow</h3>
            <GlobList
              label="global allow"
              globs={draft.allow}
              disabled={locked}
              onChange={(value) => setGlobal("allow", value)}
            />
          </div>
        </div>

        <h3 className="smart-auto-rules-heading">Per profile</h3>
        <form className="smart-auto-rules-form" onSubmit={addProfile}>
          <label className="smart-auto-filter">
            <span>Profile name</span>
            <input
              className="input"
              value={newProfile}
              placeholder="profile"
              disabled={locked}
              onChange={(event) => {
                setNewProfile(event.target.value);
                setProfileError(null);
              }}
            />
          </label>
          <button type="submit" className="btn small" disabled={locked || newProfile.trim() === ""}>
            Add profile
          </button>
        </form>
        {profileError !== null ? (
          <p className="smart-auto-rules-error" role="alert">
            {profileError}
          </p>
        ) : null}

        <div className="table-scroll smart-auto-table">
          <table className="table">
            <thead>
              <tr>
                <th>Profile</th>
                <th>Include</th>
                <th>Exclude</th>
                <th>
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {names.length === 0 ? (
                <tr>
                  <td colSpan={4} className="table-empty">
                    No profiles yet. Add one above, or create one on the Profiles page.
                  </td>
                </tr>
              ) : (
                names.map((name) => {
                  const globs = draft.profiles[name] ?? EMPTY_GLOBS;
                  const manual = !known.has(name);
                  return (
                    <tr key={name}>
                      <td className="mono" data-label="Profile">
                        {name}
                        {manual ? <Pill>not in profiles</Pill> : null}
                      </td>
                      <td data-label="Include">
                        <GlobList
                          label={`${name} include`}
                          globs={globs.include}
                          disabled={locked}
                          onChange={(value) => setProfileGlobs(name, "include", value)}
                        />
                      </td>
                      <td data-label="Exclude">
                        <GlobList
                          label={`${name} exclude`}
                          globs={globs.exclude}
                          disabled={locked}
                          onChange={(value) => setProfileGlobs(name, "exclude", value)}
                        />
                      </td>
                      <td>
                        {manual ? (
                          <button
                            type="button"
                            className="btn small danger"
                            aria-label={`Remove ${name} from rules`}
                            disabled={locked}
                            onClick={() => removeProfile(name)}
                          >
                            Remove
                          </button>
                        ) : null}
                      </td>
                    </tr>
                  );
                })
              )}
            </tbody>
          </table>
        </div>

        <div className="smart-auto-rules-save">
          <button type="button" className="btn primary" disabled={locked || !dirty} onClick={() => void save()}>
            {saving ? "Saving…" : "Save rules"}
          </button>
          {saveError !== null ? (
            <span className="smart-auto-rules-error" role="alert">
              {saveError}
            </span>
          ) : dirty ? (
            <Pill>unsaved changes</Pill>
          ) : justSaved ? (
            <Pill tone="ok" dot>
              <span role="status">Saved</span>
            </Pill>
          ) : null}
        </div>

        <div className="smart-auto-effect">
          <h3 className="smart-auto-rules-heading">Results</h3>
          {effectError !== null ? (
            <p className="note" role="status">
              Results are unavailable: {effectError}
            </p>
          ) : effect === null ? (
            <p className="note">Loading…</p>
          ) : (
            <EffectResults
              rows={effect}
              interactive={!isReadonly}
              locked={locked}
              busyId={resultBusy}
              note={resultNote}
              toggleError={resultError}
              onToggle={(row) => void toggleResult(row)}
            />
          )}
        </div>
      </>
    );
  }

  return (
    <Panel
      title="Rules"
      sub={loaded ? SOURCE_TEXT[loaded.source] : undefined}
      actions={isReadonly ? <Pill tone="warn" dot>read-only</Pill> : undefined}
      className="smart-auto-rules"
    >
      {body}
    </Panel>
  );
}

interface SmartAutoScreenProps {
  /** The hub's profiles (`meta.profiles` from `GET /api/profiles`), the rules table's rows. */
  readonly profiles: readonly ProfileRow[];
}

export function SmartAutoScreen({ profiles }: SmartAutoScreenProps) {
  const [events, setEvents] = useState<readonly SelectionEvent[] | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [filters, setFilters] = useState<Filters>(NO_FILTERS);
  // The last seq the browser holds; each refresh asks only for events after it.
  const cursor = useRef(0);
  const refreshing = useRef(false);

  useEffect(() => {
    let cancelled = false;
    const refresh = async () => {
      if (refreshing.current) {
        return;
      }
      refreshing.current = true;
      const fresh: SelectionEvent[] = [];
      try {
        for (let page = 0; page < MAX_PAGES_PER_REFRESH; page += 1) {
          const params = new URLSearchParams({ since_seq: String(cursor.current), limit: String(PAGE_LIMIT) });
          const body = await api<SelectionHistoryResponse>(`/api/smart-auto/history?${params}`);
          const last = body.events[body.events.length - 1];
          if (last === undefined) {
            break;
          }
          fresh.push(...body.events);
          cursor.current = last.seq;
          if (body.events.length < PAGE_LIMIT) {
            break;
          }
        }
        if (!cancelled) {
          setUnavailable(false);
          setEvents((current) => [...(current ?? []), ...fresh].slice(-MAX_EVENTS));
        }
      } catch (err: unknown) {
        if (!cancelled) {
          setUnavailable(true);
        }
      } finally {
        refreshing.current = false;
      }
    };
    void refresh();
    const timer = setInterval(() => void refresh(), REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  if (unavailable && events === null) {
    return (
      <section className="tab active smart-auto">
        <CurrentSelectionPanel />
        <SmartAutoRulesPanel profiles={profiles} />
        <Panel title="Smart Auto history unavailable">
          <p className="note" role="status">
            Smart Auto history is unavailable. Send a request to <code>hub/smart-auto-balance</code> and reload this tab.
          </p>
        </Panel>
        <SmartAutoHowItWorks />
      </section>
    );
  }

  const all = events ?? [];
  const filtered = all.filter((event) => matches(event, filters));
  const rows = filtered.slice(-MAX_ROWS).reverse();
  const failures = filtered.filter((event) => FAILED_STATES.has(event.state)).length;
  const turns = new Set(filtered.flatMap((event) => (event.turn ? [event.turn] : []))).size;
  const tokensIn = filtered.reduce((sum, event) => sum + (event.tokens_in ?? 0), 0);
  const tokensOut = filtered.reduce((sum, event) => sum + (event.tokens_out ?? 0), 0);
  const setFilter = (key: keyof Filters) => (value: string) => setFilters((current) => ({ ...current, [key]: value }));

  return (
    <section className="tab active smart-auto" aria-busy={events === null}>
      <CurrentSelectionPanel />
      <SmartAutoRulesPanel profiles={profiles} />
      <div className="smart-auto-head">
        <span className="dim">Every stage and turn Smart Auto dispatched, newest first. Keys are salted hashes.</span>
        {unavailable ? (
          <Pill tone="warn" dot>
            <span role="status">Refresh failed, showing the last result</span>
          </Pill>
        ) : (
          <Pill tone="ok" dot>
            live · every {REFRESH_MS / 1000} s
          </Pill>
        )}
      </div>

      <div className="smart-auto-kpis">
        <KpiCard label="Events" value={formatNumber(filtered.length)} sub={`${formatNumber(all.length)} kept in this tab`} />
        <KpiCard
          label="Failures"
          value={formatNumber(failures)}
          sub="Rejected or cancelled dispatches"
          tone={failures > 0 ? "danger" : "default"}
        />
        <KpiCard label="Turns" value={formatNumber(turns)} sub="Distinct turns in view" />
        <KpiCard
          label="Tokens in / out"
          value={`${formatNumber(tokensIn)} / ${formatNumber(tokensOut)}`}
          sub="Reported by upstreams"
        />
      </div>

      <Panel title="Stage and turn timeline" sub={`${formatNumber(rows.length)} shown`} className="smart-auto-panel">
        <div className="smart-auto-toolbar">
          <FilterSelect
            label="Workflow"
            value={filters.workflow}
            options={distinct(all, (event) => event.workflow)}
            format={shortKey}
            onChange={setFilter("workflow")}
          />
          <FilterSelect
            label="Worker"
            value={filters.worker}
            options={distinct(all, (event) => event.worker)}
            format={shortKey}
            onChange={setFilter("worker")}
          />
          <FilterSelect
            label="Stage"
            value={filters.stage}
            options={distinct(all, (event) => event.stage)}
            format={(value) => value}
            onChange={setFilter("stage")}
          />
          <FilterSelect
            label="Model"
            value={filters.model}
            options={distinct(all, (event) => event.model)}
            format={(value) => value}
            onChange={setFilter("model")}
          />
          <FilterSelect
            label="Effort"
            value={filters.effort}
            options={distinct(all, effortText)}
            format={(value) => value}
            onChange={setFilter("effort")}
          />
        </div>

        <div className="table-scroll smart-auto-table">
          <table className="table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Turn</th>
                <th>Worker</th>
                <th>Workflow</th>
                <th>Stage</th>
                <th>Model</th>
                <th>State</th>
                <th>Reason</th>
                <th>Role</th>
                <th>Effort</th>
                <th className="num">Status</th>
                <th className="num">Latency</th>
                <th className="num">Tokens in / out</th>
              </tr>
            </thead>
            <tbody>
              {events === null ? (
                <tr>
                  <td colSpan={13} className="table-empty">
                    Loading…
                  </td>
                </tr>
              ) : rows.length === 0 ? (
                <tr>
                  <td colSpan={13} className="table-empty">
                    {all.length === 0
                      ? "No Smart Auto selections yet. Requests to hub/smart-auto-balance appear here as they run."
                      : "No events match these filters."}
                  </td>
                </tr>
              ) : (
                rows.map((event, index) => (
                  <EventRow
                    key={event.seq}
                    event={event}
                    startsTurn={index === 0 || rows[index - 1]?.turn !== event.turn}
                  />
                ))
              )}
            </tbody>
          </table>
        </div>
      </Panel>
      <SmartAutoHowItWorks />
    </section>
  );
}
