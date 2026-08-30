import { useMemo, useState } from "react";

interface ModelsScreenProps {
  readonly models: readonly string[];
  readonly onCopy: (id: string) => void;
}

export function ModelsScreen({ models, onCopy }: ModelsScreenProps) {
  const [query, setQuery] = useState("");
  const filtered = useMemo(
    () => models.filter((id) => id.toLowerCase().includes(query.toLowerCase())),
    [models, query],
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
                    {id.slice(profile.length + 1)}
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
