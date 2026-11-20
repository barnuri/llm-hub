import { Icon } from "./Icon";
import "./ModelName.css";

interface ModelNameProps {
  /** Qualified model id, `profile/model`. */
  readonly id: string | null | undefined;
  /** Profile to show when `id` carries none. */
  readonly profile?: string | null;
  /** When set, shows a copy button that copies the full id. */
  readonly onCopy?: (text: string) => void;
}

/** The full model name with its profile prefix, so every screen shows which profile served it. */
export function ModelName({ id, profile, onCopy }: ModelNameProps) {
  if (!id) {
    return <span className="dim">—</span>;
  }
  const slash = id.indexOf("/");
  const prefix = slash > 0 ? id.slice(0, slash) : profile;
  const name = slash > 0 ? id.slice(slash + 1) : id;
  const full = prefix ? `${prefix}/${name}` : name;
  return (
    <span className="model-name-wrap">
      <span className="model-name" title={full}>
        {prefix ? <span className="model-name-profile">{prefix}/</span> : null}
        {name}
      </span>
      {onCopy ? (
        <button
          type="button"
          className="model-copy"
          aria-label={`Copy ${full}`}
          title={`Copy ${full}`}
          onClick={() => onCopy(full)}
        >
          <Icon name="copy" />
        </button>
      ) : null}
    </span>
  );
}
