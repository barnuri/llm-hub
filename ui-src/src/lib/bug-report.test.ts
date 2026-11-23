import { describe, expect, test } from "bun:test";
import { bugReportUrl, parseRepoRef } from "./bug-report";

const ISSUES = "https://github.com/barnuri/llm-hub/issues/new";

describe("bugReportUrl derivation", () => {
  test("https repository URL with .git gives the new-issue URL", () => {
    expect(bugReportUrl({ repository: "https://github.com/barnuri/llm-hub.git" })).toBe(ISSUES);
  });

  test("git+https object form gives the new-issue URL", () => {
    expect(bugReportUrl({ repository: { type: "git", url: "git+https://github.com/barnuri/llm-hub.git" } })).toBe(ISSUES);
  });

  test("scp-style ssh remote gives the new-issue URL", () => {
    expect(bugReportUrl({ repository: "git@github.com:barnuri/llm-hub.git" })).toBe(ISSUES);
  });

  test("github: shorthand gives the new-issue URL", () => {
    expect(bugReportUrl({ repository: "github:barnuri/llm-hub" })).toBe(ISSUES);
  });

  test("GitHub repo ignores homepage and still targets new issue", () => {
    expect(bugReportUrl({ repository: "https://github.com/barnuri/llm-hub", homepage: "https://example.com/docs" })).toBe(ISSUES);
  });
});

describe("bugReportUrl fallback to the repository homepage", () => {
  test("non-GitHub repository with no homepage falls back to the repo URL", () => {
    expect(bugReportUrl({ repository: "https://gitlab.com/acme/tool.git" })).toBe("https://gitlab.com/acme/tool");
  });

  test("non-GitHub repository with a homepage uses the homepage", () => {
    expect(bugReportUrl({ repository: "https://gitlab.com/acme/tool.git", homepage: "https://acme.example/tool" })).toBe(
      "https://acme.example/tool",
    );
  });

  test("homepage alone (no repository) is used", () => {
    expect(bugReportUrl({ homepage: "https://acme.example/tool" })).toBe("https://acme.example/tool");
  });

  test("unparseable repository falls back to the homepage", () => {
    expect(bugReportUrl({ repository: "not a repo", homepage: "https://acme.example/tool" })).toBe("https://acme.example/tool");
  });

  test("a non-http homepage is rejected", () => {
    expect(bugReportUrl({ homepage: "javascript:alert(1)" })).toBeNull();
  });

  test("no metadata resolves to null, so the link is not rendered", () => {
    expect(bugReportUrl({})).toBeNull();
    expect(bugReportUrl({ repository: "", homepage: null })).toBeNull();
  });
});

describe("parseRepoRef", () => {
  test("returns host, owner and name, dropping the .git suffix and extra path segments", () => {
    expect(parseRepoRef("https://github.com/barnuri/llm-hub.git/")).toEqual({ host: "github.com", owner: "barnuri", name: "llm-hub" });
  });

  test("returns null without an owner and a name", () => {
    expect(parseRepoRef("https://github.com/barnuri")).toBeNull();
  });
});
