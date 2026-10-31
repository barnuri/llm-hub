import { useMemo, useState } from "react";

import { Panel } from "../components/Panel";
import { setQueryParam, useRoute } from "../lib/router";
import type { ProfileRow } from "../lib/types";
import "../styles/screens/models.css";

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

  const emptyText = models.length === 0 ? "No models yet. Add a profile to list its models." : "No models match this filter.";

  return (
    <section className="tab active models">
      <Panel
        title="Models"
        sub={filtered.length === models.length ? `${models.length} available` : `${filtered.length} of ${models.length}`}
        actions={
          <input
            className="input search"
            type="search"
            placeholder="Filter models…"
            aria-label="Filter models"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
          />
        }
      >
        {profileNames.length > 1 ? (
          <div className="toolbar chips" role="group" aria-label="Filter by profile">
            <button
              type="button"
              className={activeProfile === "" ? "chip active" : "chip"}
              aria-pressed={activeProfile === ""}
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
        {filtered.length === 0 ? (
          <p className="note models-empty">{emptyText}</p>
        ) : (
          <div className="table-scroll models-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Model id</th>
                  <th>Profile</th>
                  <th>
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {filtered.map((id) => {
                  const profile = id.split("/")[0] ?? "";
                  return (
                    <tr key={id}>
                      <td>
                        <span className="model-id">
                          <span className="model-profile-part">{profile}/</span>
                          {id.slice(profile.length + 1)}
                        </span>
                      </td>
                      <td className="dim">{labelFor(profile)}</td>
                      <td className="models-actions">
                        <button
                          type="button"
                          className="btn small ghost"
                          aria-label={`Copy ${id}`}
                          onClick={() => onCopy(id)}
                        >
                          Copy
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </section>
  );
}
