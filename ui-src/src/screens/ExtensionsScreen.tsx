import { useCallback, useEffect, useState } from "react";

import { Checkbox } from "../components/Checkbox";
import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { api } from "../lib/api";
import type { ExtensionDisabledBy, ExtensionsResponse, ExtensionStatus } from "../lib/types";
import "../styles/screens/extensions.css";

interface ExtensionsScreenProps {
  readonly onToast: (message: string, isError?: boolean) => void;
}

const ENV_VAR = "LLM_HUB_EXTENSIONS_DISABLED";

function DisabledNote({ by }: { readonly by: ExtensionDisabledBy }) {
  if (by === "env") {
    return (
      <Pill tone="warn" title={`Set by ${ENV_VAR}`}>
        off by env
      </Pill>
    );
  }
  return by === "ui" ? <Pill>off</Pill> : null;
}

export function ExtensionsScreen({ onToast }: ExtensionsScreenProps) {
  const [data, setData] = useState<ExtensionsResponse | null>(null);
  const [saving, setSaving] = useState<string | null>(null);

  const fail = useCallback((err: unknown) => onToast(err instanceof Error ? err.message : String(err), true), [onToast]);

  useEffect(() => {
    api<ExtensionsResponse>("/api/extensions").then(setData).catch(fail);
  }, [fail]);

  const toggle = async (path: string, key: string, enabled: boolean) => {
    setSaving(key);
    try {
      setData(await api<ExtensionsResponse>(path, { method: "PUT", body: JSON.stringify({ enabled }) }));
    } catch (err) {
      fail(err);
    } finally {
      setSaving(null);
    }
  };

  const extensions = data?.extensions ?? [];
  const off = extensions.filter((extension) => !extension.enabled).length;

  const row = (extension: ExtensionStatus) => {
    const key = `ext:${extension.name}`;
    return (
      <tr key={key}>
        <td>
          <Checkbox
            checked={extension.enabled}
            disabled={extension.disabled_by === "env" || saving !== null}
            onChange={(enabled) =>
              void toggle(`/api/extensions/${encodeURIComponent(extension.name)}`, key, enabled)
            }
            label={extension.name}
          />
        </td>
        <td>
          <Pill tone="info">{extension.kind}</Pill>
        </td>
        <td className="mono dim">{extension.phases.length > 0 ? extension.phases.join(", ") : "unknown"}</td>
        <td>
          <DisabledNote by={extension.disabled_by} />
        </td>
      </tr>
    );
  };

  const ruleRows = (extension: ExtensionStatus) =>
    (extension.rules ?? []).map((rule) => {
      const key = `rule:${rule.name}`;
      return (
        <tr key={key} className="extensions-rule">
          <td>
            <Checkbox
              checked={rule.enabled}
              disabled={rule.disabled_by === "env" || saving !== null}
              onChange={(enabled) =>
                void toggle(`/api/extensions/rules/${encodeURIComponent(rule.name)}`, key, enabled)
              }
              label={rule.name}
            />
          </td>
          <td>{rule.mode === "shadow" ? <Pill>shadow</Pill> : <span className="dim">rule</span>}</td>
          <td className="mono dim">{rule.phase}</td>
          <td>
            <DisabledNote by={rule.disabled_by} />
          </td>
        </tr>
      );
    });

  return (
    <section className="tab active extensions">
      <Panel
        title="Extensions"
        sub={data ? `${extensions.length} extensions, ${off} off` : undefined}
      >
        <p className="note extensions-note">
          An extension that is off is left out when the hub builds its extension set, so it costs a request nothing.
          Entries in <code>{ENV_VAR}</code> show as off by env and change only there.
        </p>
        {data === null ? (
          <div className="skeleton extensions-skeleton" aria-label="Loading extensions" />
        ) : (
          <div className="table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Kind</th>
                  <th>Phases</th>
                  <th>
                    <span className="sr-only">State</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {extensions.flatMap((extension) => [row(extension), ...ruleRows(extension)])}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </section>
  );
}
