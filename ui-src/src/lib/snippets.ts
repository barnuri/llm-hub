export const SETUP_TARGETS: ReadonlyArray<readonly [string, string]> = [
  ["claude-code", "Claude Code"],
  ["codex", "Codex CLI"],
  ["cursor", "Cursor"],
  ["continue", "Continue"],
  ["aider", "aider"],
  ["openai-python", "OpenAI SDK (Python)"],
  ["openai-js", "OpenAI SDK (JS/TS)"],
  ["claude-agent-sdk", "Claude Agent SDK"],
  ["langchain", "LangChain (Python)"],
  ["litellm", "LiteLLM"],
];

const FALLBACKS_HEADER = "X-LLM-Hub-Fallbacks";

export function snippetFor(
  target: string,
  base: string,
  model: string,
  key: string,
  fallbacks: readonly string[] = [],
): string {
  const chain = fallbacks.join(",");
  const fb = chain.length > 0;
  const snippets: Record<string, string> = {
    "claude-code": `# Claude Code — route through llm-hub via env:
export ANTHROPIC_BASE_URL=${base}
export ANTHROPIC_AUTH_TOKEN=${key}
export ANTHROPIC_MODEL=${model}${
      fb
        ? `
export ANTHROPIC_CUSTOM_HEADERS="${FALLBACKS_HEADER}: ${chain}"`
        : ""
    }
claude
# note: works when the selected upstream is Anthropic-compatible;
# for OpenAI-only upstreams use an adapter profile.`,
    codex: `# ~/.codex/config.toml
model = "${model}"
model_provider = "llm-hub"

[model_providers.llm-hub]
name = "llm-hub"
base_url = "${base}/v1"
env_key = "LLM_HUB_KEY"   # export LLM_HUB_KEY=${key}${
      fb
        ? `
http_headers = { "${FALLBACKS_HEADER}" = "${chain}" }`
        : ""
    }`,
    cursor: `Cursor -> Settings -> Models -> OpenAI API Key:
  API key:  ${key}
  Override base URL: ${base}/v1
  Model: ${model}${
      fb
        ? `
# Cursor cannot send custom headers — set the hub-wide default instead
# (Profiles tab -> Default fallbacks, or LLM_HUB_DEFAULT_FALLBACKS=${chain})`
        : ""
    }`,
    continue: `# ~/.continue/config.yaml
models:
  - name: llm-hub
    provider: openai
    model: ${model}
    apiBase: ${base}/v1
    apiKey: ${key}${
      fb
        ? `
    requestOptions:
      headers:
        ${FALLBACKS_HEADER}: "${chain}"`
        : ""
    }`,
    aider: `aider --openai-api-base ${base}/v1 \\
      --openai-api-key ${key} \\
      --model openai/${model}${
      fb
        ? `
# aider cannot send custom headers — set the hub-wide default instead
# (Profiles tab -> Default fallbacks, or LLM_HUB_DEFAULT_FALLBACKS=${chain})`
        : ""
    }`,
    "openai-python": `from openai import OpenAI

client = OpenAI(
    base_url="${base}/v1",
    api_key="${key}",${
      fb
        ? `
    default_headers={"${FALLBACKS_HEADER}": "${chain}"},`
        : ""
    }
)
response = client.chat.completions.create(
    model="${model}",
    messages=[{"role": "user", "content": "hello"}],
)
print(response.choices[0].message.content)`,
    "openai-js": `import OpenAI from "openai";

const client = new OpenAI({
  baseURL: "${base}/v1",
  apiKey: "${key}",${
      fb
        ? `
  defaultHeaders: { "${FALLBACKS_HEADER}": "${chain}" },`
        : ""
    }
});
const response = await client.chat.completions.create({
  model: "${model}",
  messages: [{ role: "user", content: "hello" }],
});
console.log(response.choices[0].message.content);`,
    "claude-agent-sdk": `# Claude Agent SDK routes through the Anthropic API surface.
# Point it at llm-hub with:
export ANTHROPIC_BASE_URL=${base}
export ANTHROPIC_AUTH_TOKEN=${key}${
      fb
        ? `
export ANTHROPIC_CUSTOM_HEADERS="${FALLBACKS_HEADER}: ${chain}"`
        : ""
    }

# python
from claude_agent_sdk import query
async for message in query(prompt="hello"):
    print(message)`,
    langchain: `from langchain_openai import ChatOpenAI

llm = ChatOpenAI(
    base_url="${base}/v1",
    api_key="${key}",
    model="${model}",${
      fb
        ? `
    default_headers={"${FALLBACKS_HEADER}": "${chain}"},`
        : ""
    }
)
print(llm.invoke("hello").content)`,
    litellm: `import litellm

response = litellm.completion(
    model="openai/${model}",
    api_base="${base}/v1",
    api_key="${key}",${
      fb
        ? `
    extra_headers={"${FALLBACKS_HEADER}": "${chain}"},`
        : ""
    }
    messages=[{"role": "user", "content": "hello"}],
)
print(response.choices[0].message.content)`,
  };
  return snippets[target] ?? "";
}
