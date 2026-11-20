import type { ReactNode } from "react";

import "./MarkdownText.css";

/** Splits `**bold**` and `` `code` `` spans into elements. Everything else stays plain text. */
function inline(text: string): ReactNode[] {
  return text
    .split(/(\*\*[^*]+\*\*|`[^`]+`)/g)
    .filter((part) => part !== "")
    .map((part, index) => {
      if (part.startsWith("**") && part.endsWith("**")) {
        return <strong key={index}>{part.slice(2, -2)}</strong>;
      }
      if (part.startsWith("`") && part.endsWith("`")) {
        return <code key={index}>{part.slice(1, -1)}</code>;
      }
      return part;
    });
}

interface Block {
  readonly kind: "heading" | "bullets" | "numbers" | "paragraph";
  readonly lines: string[];
}

function blocks(markdown: string): Block[] {
  const out: Block[] = [];
  for (const raw of markdown.split("\n")) {
    const line = raw.trim();
    if (line === "") {
      out.push({ kind: "paragraph", lines: [] });
      continue;
    }
    const heading = /^#{1,6}\s+(.*)$/.exec(line);
    const bullet = /^[-*]\s+(.*)$/.exec(line);
    const number = /^\d+[.)]\s+(.*)$/.exec(line);
    const kind = heading ? "heading" : bullet ? "bullets" : number ? "numbers" : "paragraph";
    const text = heading?.[1] ?? bullet?.[1] ?? number?.[1] ?? line;
    const last = out[out.length - 1];
    if (kind !== "heading" && last && last.kind === kind && last.lines.length > 0) {
      last.lines.push(text);
    } else {
      out.push({ kind, lines: [text] });
    }
  }
  return out.filter((block) => block.lines.length > 0);
}

/** Renders the small Markdown subset model answers use, without raw HTML. */
export function MarkdownText({ markdown }: { readonly markdown: string }) {
  return (
    <div className="markdown-text">
      {blocks(markdown).map((block, index) => {
        if (block.kind === "heading") {
          return <h4 key={index}>{inline(block.lines[0] ?? "")}</h4>;
        }
        if (block.kind === "bullets" || block.kind === "numbers") {
          const items = block.lines.map((line, item) => <li key={item}>{inline(line)}</li>);
          return block.kind === "bullets" ? <ul key={index}>{items}</ul> : <ol key={index}>{items}</ol>;
        }
        return <p key={index}>{inline(block.lines.join(" "))}</p>;
      })}
    </div>
  );
}
