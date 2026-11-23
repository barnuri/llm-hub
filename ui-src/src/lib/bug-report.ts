/** Repository metadata the bug link reads. Matches the `repository` and `homepage` fields of package.json. */
export interface RepoMeta {
  repository?: string | { type?: string; url?: string } | null;
  homepage?: string | null;
}

export interface RepoRef {
  host: string;
  owner: string;
  name: string;
}

/** Reads a git remote (`git+https://`, `https://`, `git@host:owner/repo.git`) or a `github:owner/repo` shorthand. Returns null when no owner and repo are present. */
export function parseRepoRef(raw: string): RepoRef | null {
  const value = raw.trim().replace(/^git\+/, "");
  let host: string;
  let path: string;
  const shorthand = /^github:(.+)$/.exec(value);
  const scp = /^[\w.-]+@([^:/]+):(.+)$/.exec(value);
  if (shorthand) {
    host = "github.com";
    path = shorthand[1] ?? "";
  } else if (scp) {
    host = scp[1] ?? "";
    path = scp[2] ?? "";
  } else {
    let url: URL;
    try {
      url = new URL(value);
    } catch {
      return null;
    }
    host = url.hostname;
    path = url.pathname;
  }
  const [owner, name] = path.replace(/^\/+/, "").replace(/\/+$/, "").replace(/\.git$/, "").split("/");
  if (!host || !owner || !name) return null;
  return { host, owner, name };
}

function httpUrl(raw: string): string | null {
  try {
    const url = new URL(raw.trim());
    return url.protocol === "https:" || url.protocol === "http:" ? url.href : null;
  } catch {
    return null;
  }
}

/**
 * The bug link target. A GitHub repository gives `<repo>/issues/new`. Any other resolvable
 * repository, or a valid homepage without a new-issue URL, gives the repository homepage.
 * Returns null when no metadata resolves to a URL.
 */
export function bugReportUrl(meta: RepoMeta): string | null {
  const raw = typeof meta.repository === "string" ? meta.repository : (meta.repository?.url ?? null);
  const ref = raw ? parseRepoRef(raw) : null;
  const repoUrl = ref ? `https://${ref.host}/${ref.owner}/${ref.name}` : null;
  if (ref && repoUrl && ref.host === "github.com") return `${repoUrl}/issues/new`;
  const homepage = meta.homepage ? httpUrl(meta.homepage) : null;
  return homepage ?? repoUrl;
}
