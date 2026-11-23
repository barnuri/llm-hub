import repoPackage from "../../package.json";
import { bugReportUrl, type RepoMeta } from "../lib/bug-report";
import { Icon } from "./Icon";

/** Opens a new GitHub issue for this repo in a new tab, or the repo homepage when no issue URL resolves. Renders nothing when no URL resolves. */
export function BugReportLink({ meta = repoPackage }: { meta?: RepoMeta }) {
  const href = bugReportUrl(meta);
  if (href === null) return null;
  return (
    <a
      className="btn small bug-report"
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      aria-label="Report a bug in a new tab"
      title="Report a bug"
    >
      <Icon name="bug" />
    </a>
  );
}
