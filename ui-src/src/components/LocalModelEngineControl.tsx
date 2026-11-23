import { useState } from "react";

import { api } from "../lib/api";
import { engineBadgeLabel, LOCAL_ENGINE_OPTIONS, localEngineRoute, nextEngineSwitch } from "../lib/localEngine";
import type { LocalEngine } from "../lib/types";
import { Pill } from "./Pill";
import { Select } from "./Select";

interface LocalModelEngineControlProps {
  readonly profile: string;
  readonly model: string;
  readonly engine: LocalEngine;
  readonly onToast: (message: string, isError?: boolean) => void;
}

interface EngineResponse {
  readonly engine: LocalEngine;
}

export function LocalModelEngineControl({ profile, model, engine, onToast }: LocalModelEngineControlProps) {
  const [current, setCurrent] = useState<LocalEngine>(engine);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const changeEngine = async (next: string) => {
    const selected = nextEngineSwitch(current, next);
    if (selected === null) {
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const response = await api<EngineResponse>(localEngineRoute(profile, model), {
        method: "PUT",
        body: JSON.stringify({ engine: selected }),
      });
      setCurrent(response.engine);
      onToast(`Engine switched to ${engineBadgeLabel(response.engine)}`);
    } catch (caught: unknown) {
      const message = caught instanceof Error ? caught.message : String(caught);
      setError(message);
      onToast(message, true);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="local-engine-control">
      <Pill tone="info" title={`Serving engine: ${engineBadgeLabel(current)}`}>
        {engineBadgeLabel(current)}
      </Pill>
      <Select
        ariaLabel={`Engine for ${profile}/${model}`}
        value={current}
        options={LOCAL_ENGINE_OPTIONS}
        onChange={changeEngine}
        disabled={saving}
        className="models-engine-select"
      />
      {error ? <p className="field-error models-engine-error" role="alert">{error}</p> : null}
    </div>
  );
}
