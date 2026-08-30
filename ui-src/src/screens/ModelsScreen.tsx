import { useMemo, useState } from "react";

import { setQueryParam, useRoute } from "../lib/router";
import type { ProfileRow } from "../lib/types";

interface ModelsScreenProps {
  readonly models: readonly string[];
  readonly profiles: readonly ProfileRow[];
  readonly onCopy: (id: string) => void;
}

export function ModelsScreen({ models, profiles, onCopy }: ModelsScreenProps) {
  const [query, setQuery] = useState("");
  const route = useRoute();
  const activeProfile = route.query.get("profile") ?? "";

  const profileNames = useMemo(() => {
    const prefixes = new Set(models.map((id) => id.split("/")[0] ?? ""));
    prefixes.delete("");
    return [...prefixes].sort();
  }, [models]);

  const labelFor = (name: string) => profiles.find((p) => p.name === name)?.label ?? name;

  const filtered = useMemo(
    () =>
      models
        .filter((id) => activeProfile === "" || id.startsWith(`${activeProfile}/`))
        .filter((id) => id.toLowerCase().includes(query.toLowerCase())),
    [models, query, activeProfile],
  );

  return (
    <section className="tab active">
      <div className="toolbar">
        <input
          className="input search"
          type="search"
          placeholder="Filter models…"
          aria-label="Filter models"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
        />
        <span className="dim">{filtered.length} models</span>
      </div>
      {profileNames.length > 1 ? (
        <div className="toolbar chips" role="group" aria-label="Filter by profile">
          <button
            type="button"
            className={activeProfile === "" ? "chip active" : "chip"}
            onClick={() => setQueryParam("profile", null)}
          >
            All
          </button>
          {profileNames.map((name) => (
            <button
              key={name}
              type="button"
              className={activeProfile === name ? "chip active" : "chip"}
              aria-pressed={activeProfile === name}
              onClick={() => setQueryParam("profile", activeProfile === name ? null : name)}
            >
              {labelFor(name)}
            </button>
          ))}
        </div>
      ) : null}
      <table className="table">
        <thead>
          <tr>
            <th>Model id</th>
            <th>Profile</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {filtered.map((id) => {
            const profile = id.split("/")[0];
            return (
              <tr key={id}>
                <td>
                  <span className="model-id">
                    <span className="model-profile-part">{profile}/</span>
                    {id.slice((profile ?? "").length + 1)}
                  </span>
                </td>
                <td className="mono">{profile}</td>
                <td>
                  <button type="button" className="btn small" onClick={() => onCopy(id)}>
                    Copy
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}
