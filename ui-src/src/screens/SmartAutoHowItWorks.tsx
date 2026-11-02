import type { ReactNode } from "react";

import { Panel } from "../components/Panel";
import "../styles/screens/smart-auto.css";

interface Step {
  readonly title: string;
  readonly detail: ReactNode;
}

/**
 * The request path, top to bottom. Each box is HTML so its text wraps inside the box at phone width;
 * only the arrows between the boxes are inline SVG.
 */
const STEPS: readonly Step[] = [
  {
    title: "Request arrives",
    detail: (
      <>
        The client calls <code>hub/smart-auto-&lt;tier&gt;</code>.
      </>
    ),
  },
  {
    title: "Read the selector and tier",
    detail: (
      <>
        The model name sets the tier. Headers (<code>x-llm-hub-smart-auto-*</code>) add more filters, minimums, a role
        and a rules mode.
      </>
    ),
  },
  {
    title: "Rules layer",
    detail: (
      <>
        <code>LLM_HUB_SMART_AUTO_RULES</code>, if set, replaces everything else. Otherwise the saved rules file applies:
        the global lists and each profile's lists.
      </>
    ),
  },
  {
    title: "Merge",
    detail:
      "Rules, selector and headers combine. They only narrow the pool. A header can widen it only when no env rules are set.",
  },
  {
    title: "Eligible pool",
    detail:
      "Chat models only, with no Auto aliases. Then profile, include and exclude filters, minimums and prompt size. Unknown capacity fails any minimum.",
  },
  {
    title: "Classify the role",
    detail:
      "Each turn gets a role: planning, investigation, implementation, validation or conversation. It is also marked plan-backed or not.",
  },
  {
    title: "Keep the same model",
    detail:
      "The session keeps its model, so the prompt cache stays warm. Tool follow-ups use that model. It switches only when the work really changes.",
  },
  {
    title: "Pick the model",
    detail: "Picks the best model for the role and tier. Usage breaks ties.",
  },
  {
    title: "Effort",
    detail:
      "The tier and role set the effort. The client's effort is ignored. The value snaps to a level the model supports.",
  },
  {
    title: "A second model checks",
    detail:
      "Balance checks planning and implementation turns. Intelligence checks with up to two models. The checker uses another profile and is never the answering model.",
  },
  {
    title: "Stream or buffer",
    detail: "Checked turns are held until the check ends. All other turns stream.",
  },
  {
    title: "Fallbacks and history",
    detail: (
      <>
        Fallbacks use the same filters. Each choice is written to history. The status line shows the current model.
      </>
    ),
  },
];

interface TierRow {
  readonly tier: string;
  readonly optimises: string;
  readonly review: string;
  readonly buffering: string;
}

const TIERS: readonly TierRow[] = [
  { tier: "Efficiency", optimises: "Cost", review: "None", buffering: "Streams" },
  {
    tier: "Balance (default)",
    optimises: "Cost and quality",
    review: "Planning and implementation",
    buffering: "Checked turns buffered, others stream",
  },
  { tier: "Intelligence", optimises: "Quality", review: "Up to two models", buffering: "Buffered" },
  { tier: "Fast", optimises: "Speed", review: "None", buffering: "Streams" },
];

function FlowArrow() {
  return (
    <svg className="smart-auto-how-arrow" viewBox="0 0 24 22" aria-hidden="true" focusable="false">
      <path d="M12 2 V16" />
      <path d="M7 12 L12 18 L17 12" />
    </svg>
  );
}

export function SmartAutoHowItWorks() {
  return (
    <Panel title="How Smart Auto works" className="smart-auto-how">
      <h3 className="smart-auto-rules-heading">Request path</h3>
      <ol className="smart-auto-how-flow" aria-label="Smart Auto request path">
        {STEPS.map((step, index) => (
          <li key={step.title} className="smart-auto-how-item">
            <div className="smart-auto-how-step">
              <span className="smart-auto-how-num" aria-hidden="true">
                {index + 1}
              </span>
              <div className="smart-auto-how-text">
                <p className="smart-auto-how-title">{step.title}</p>
                <p className="smart-auto-how-detail">{step.detail}</p>
              </div>
            </div>
            {index < STEPS.length - 1 ? <FlowArrow /> : null}
          </li>
        ))}
      </ol>

      <h3 className="smart-auto-rules-heading">Tiers</h3>
      <div className="table-scroll smart-auto-table">
        <table className="table">
          <thead>
            <tr>
              <th>Tier</th>
              <th>Optimises for</th>
              <th>Review</th>
              <th>Buffering</th>
            </tr>
          </thead>
          <tbody>
            {TIERS.map((row) => (
              <tr key={row.tier}>
                <td className="mono" data-label="Tier">
                  {row.tier}
                </td>
                <td data-label="Optimises for">{row.optimises}</td>
                <td data-label="Review">{row.review}</td>
                <td data-label="Buffering">{row.buffering}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <h3 className="smart-auto-rules-heading smart-auto-how-where-heading">Where to see it</h3>
      <ul className="smart-auto-how-where">
        <li>
          This tab: the current selection panel shows one session's or worker's model, the rules panel shows the saved
          rules, and the timeline shows history.
        </li>
        <li>The status line shows the current model.</li>
        <li>
          Hub endpoints: <code>/api/smart-auto/current</code>, <code>/api/smart-auto/history</code> and{" "}
          <code>/api/smart-auto/rules</code>.
        </li>
      </ul>
    </Panel>
  );
}
