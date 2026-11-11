import "./ModelName.css";

interface ModelNameProps {
  /** Qualified model id, `profile/model`. */
  readonly id: string | null | undefined;
  /** Profile to show when `id` carries none. */
  readonly profile?: string | null;
}

/** The full model name with its profile prefix, so every screen shows which profile served it. */
export function ModelName({ id, profile }: ModelNameProps) {
  if (!id) {
    return <span className="dim">—</span>;
  }
  const slash = id.indexOf("/");
  const prefix = slash > 0 ? id.slice(0, slash) : profile;
  const name = slash > 0 ? id.slice(slash + 1) : id;
  return (
    <span className="model-name" title={prefix ? `${prefix}/${name}` : name}>
      {prefix ? <span className="model-name-profile">{prefix}/</span> : null}
      {name}
    </span>
  );
}
