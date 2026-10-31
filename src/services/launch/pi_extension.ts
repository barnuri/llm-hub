import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const DEFAULT_CONTEXT_WINDOW = 131072;
const MAX_OUTPUT_TOKENS = 32768;

export default async function (pi: ExtensionAPI) {
  const base = process.env.LLM_HUB_URL ?? "http://localhost:8888";
  const profiles = process.env.LLM_HUB_MODEL_PROFILES;
  const query = profiles ? `?profiles=${encodeURIComponent(profiles)}` : "";
  const res = await fetch(`${base}/v1/models${query}`, {
    headers: { authorization: `Bearer ${process.env.LLM_HUB_API_KEY ?? "llm-hub"}` },
  });
  if (!res.ok) {
    throw new Error(`llm-hub returned ${res.status} for ${base}/v1/models`);
  }
  const { data } = (await res.json()) as { data: Array<{ id: string; context_window?: number }> };
  pi.registerProvider("llm-hub", {
    baseUrl: `${base}/v1`,
    apiKey: "${LLM_HUB_API_KEY}",
    api: "openai-completions",
    compat: { supportsDeveloperRole: false, supportsReasoningEffort: false, maxTokensField: "max_tokens" },
    models: data.map((m) => {
      const contextWindow = m.context_window ?? DEFAULT_CONTEXT_WINDOW;
      return {
        id: m.id,
        name: m.id,
        reasoning: false,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow,
        maxTokens: Math.min(MAX_OUTPUT_TOKENS, Math.floor(contextWindow / 4)),
      };
    }),
  });
}
