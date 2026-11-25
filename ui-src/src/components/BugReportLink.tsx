import repoPackage from "../../package.json";
import { bugReportUrl, repoUrl, type RepoMeta } from "../lib/bug-report";
import { Icon } from "./Icon";

/** Opens the repo's issues in a new tab, or the repo homepage when no issues URL resolves. Renders nothing when no URL resolves. */
export function BugReportLink({ meta = repoPackage }: { meta?: RepoMeta }) {
  const href = bugReportUrl(meta);
  if (href === null) return null;
  return (
    <a
      className="btn small bug-report"
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      aria-label="Open the issues in a new tab"
      title="Issues"
    >
      <Icon name="bug" />
    </a>
  );
}

/** Opens the GitHub repo in a new tab. Renders nothing when no repository URL resolves. */
export function RepoLink({ meta = repoPackage }: { meta?: RepoMeta }) {
  const href = repoUrl(meta);
  if (href === null) return null;
  return (
    <a
      className="btn small bug-report"
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      aria-label="Open the GitHub repo in a new tab"
      title="GitHub repo"
    >
      <Icon name="github" />
    </a>
  );
}
