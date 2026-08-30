"use strict";

const KEY_STORAGE = "llm-hub-key";
const state = { models: [], profiles: [], meta: {}, authNeeded: false };

// --- api helper: attaches saved key, prompts through the modal on 401 ---
async function api(path, options = {}) {
  const headers = Object.assign({ "content-type": "application/json" }, options.headers || {});
  const saved = localStorage.getItem(KEY_STORAGE);
  if (saved) headers["authorization"] = `Bearer ${saved}`;
  const response = await fetch(path, Object.assign({}, options, { headers }));
  if (response.status === 401) {
    const key = await promptForKey();
    if (key === null) throw new Error("unauthorized");
    localStorage.setItem(KEY_STORAGE, key);
    return api(path, options);
  }
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error?.message || `${response.status} ${response.statusText}`);
  }
  return response.json();
}

// --- toast + modal (no native popups) ---
function toast(message, isError = false) {
  const el = document.createElement("div");
  el.className = "toast" + (isError ? " error" : "");
  el.textContent = message;
  document.getElementById("toasts").appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

function openModal({ title, bodyHtml, confirmLabel = "Confirm", danger = false }) {
  return new Promise((resolve) => {
    const backdrop = document.getElementById("modal-backdrop");
    document.getElementById("modal-title").textContent = title;
    document.getElementById("modal-body").innerHTML = bodyHtml;
    const confirm = document.getElementById("modal-confirm");
    const cancel = document.getElementById("modal-cancel");
    confirm.textContent = confirmLabel;
    confirm.className = "btn " + (danger ? "danger" : "primary");
    backdrop.hidden = false;
    const close = (value) => { backdrop.hidden = true; confirm.onclick = cancel.onclick = null; resolve(value); };
    confirm.onclick = () => close(true);
    cancel.onclick = () => close(false);
    backdrop.querySelector(".input")?.focus();
  });
}

async function promptForKey() {
  const ok = await openModal({
    title: "API key required",
    bodyHtml: `<div class="field"><label for="f-key">Master key or hub API key</label>
      <input id="f-key" class="input" type="password" autocomplete="off"></div>`,
    confirmLabel: "Save key",
  });
  return ok ? document.getElementById("f-key").value.trim() : null;
}

// --- tabs ---
document.querySelectorAll(".nav-tab").forEach((button) => {
  button.addEventListener("click", () => {
    document.querySelectorAll(".nav-tab").forEach((b) => b.classList.remove("active"));
    document.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
    button.classList.add("active");
    document.getElementById(`tab-${button.dataset.tab}`).classList.add("active");
    refreshTab(button.dataset.tab);
  });
});

function activeTab() {
  return document.querySelector(".nav-tab.active").dataset.tab;
}

function refreshTab(tab) {
  const loaders = { models: loadModels, stats: loadStats, usage: loadUsage, profiles: loadProfiles, keys: loadKeys, setup: loadSetup };
  (loaders[tab] || (() => {}))().catch((e) => toast(e.message, true));
}

// --- models ---
async function loadModels() {
  const body = await api("/v1/models");
  state.models = (body.data || []).map((m) => m.id).sort();
  renderModels();
}

function renderModels() {
  const query = document.getElementById("model-search").value.toLowerCase();
  const rows = state.models.filter((id) => id.toLowerCase().includes(query));
  document.getElementById("model-count").textContent = `${rows.length} models`;
  document.getElementById("model-rows").innerHTML = rows
    .map((id) => {
      const [profile] = id.split("/");
      const rest = id.slice(profile.length + 1);
      return `<tr>
        <td><span class="model-id"><span class="model-profile-part">${esc(profile)}/</span>${esc(rest)}</span></td>
        <td class="mono">${esc(profile)}</td>
        <td><button class="btn small" data-copy="${esc(id)}">Copy</button></td>
      </tr>`;
    })
    .join("");
}

document.getElementById("model-search").addEventListener("input", renderModels);

document.addEventListener("click", (event) => {
  const id = event.target.dataset?.copy;
  if (!id) return;
  navigator.clipboard.writeText(id).then(() => toast(`Copied ${id}`));
});

// --- ports sidebar ---
async function loadPorts() {
  const body = await api("/api/profiles");
  state.profiles = body.profiles;
  state.meta = body;
  document.getElementById("version").textContent = `v${body.version}`;
  const authBadge = document.getElementById("auth-badge");
  authBadge.hidden = false;
  authBadge.textContent = body.auth_enabled ? "auth on" : "auth off";
  authBadge.classList.toggle("on", body.auth_enabled);
  const persistBadge = document.getElementById("persist-badge");
  persistBadge.hidden = false;
  persistBadge.textContent = body.persistent ? "persistent" : "in-memory";
  persistBadge.classList.toggle("on", body.persistent);
  document.getElementById("port-list").innerHTML = body.profiles
    .map((p) => {
      const led = p.healthy === true ? "up" : p.healthy === false ? "down" : "off";
      return `<li><span class="led ${led}"></span>${esc(p.name)}</li>`;
    })
    .join("");
}

// --- stats ---
async function loadStats() {
  const body = await api("/api/stats");
  const row = (entry) => `<tr>
    <td class="mono">${esc(entry.key)}</td>
    <td class="num">${entry.requests}</td><td class="num">${entry.errors}</td>
    <td class="num">${entry.p50_ms}</td><td class="num">${entry.p95_ms}</td>
    <td class="num">${entry.tokens_in}</td><td class="num">${entry.tokens_out}</td>
  </tr>`;
  document.getElementById("stats-profile-rows").innerHTML = body.profiles.map(row).join("");
  document.getElementById("stats-model-rows").innerHTML = body.models.map(row).join("");
}

// --- usage ---
async function loadUsage() {
  const note = document.getElementById("usage-note");
  try {
    const body = await api("/api/usage");
    note.hidden = true;
    document.getElementById("usage-totals").innerHTML = [
      ["Requests", body.total_requests],
      ["Errors", body.total_errors],
      ["Tokens in", body.total_tokens_in],
      ["Tokens out", body.total_tokens_out],
    ].map(([label, value]) => `<div class="stat-tile"><div class="value">${value}</div><div class="label">${label}</div></div>`).join("");
    document.getElementById("usage-rows").innerHTML = body.recent
      .map((r) => `<tr>
        <td class="mono">${new Date(r.ts_ms).toLocaleTimeString()}</td>
        <td class="mono">${esc(r.model)}</td>
        <td class="num">${r.status}</td><td class="num">${r.latency_ms}</td>
        <td class="num">${r.tokens_in}</td><td class="num">${r.tokens_out}</td>
      </tr>`)
      .join("");
  } catch (e) {
    document.getElementById("usage-totals").innerHTML = "";
    document.getElementById("usage-rows").innerHTML = "";
    note.hidden = false;
  }
}

// --- profiles ---
async function loadProfiles() {
  await loadPorts();
  document.getElementById("readonly-note").hidden = !state.meta.readonly;
  document.getElementById("profile-add").disabled = !!state.meta.readonly;
  document.getElementById("profile-rows").innerHTML = state.profiles
    .map((p) => `<tr>
      <td class="mono">${esc(p.name)}</td>
      <td class="mono">${esc(p.base_url)}</td>
      <td class="mono">${esc(p.api_key_masked || "—")}</td>
      <td>${p.enabled ? "yes" : "no"}</td>
      <td>
        <button class="btn small" data-test="${esc(p.name)}">Test</button>
        <button class="btn small" data-edit="${esc(p.name)}" ${state.meta.readonly ? "disabled" : ""}>Edit</button>
        <button class="btn small danger" data-del="${esc(p.name)}" ${state.meta.readonly ? "disabled" : ""}>Delete</button>
      </td>
    </tr>`)
    .join("");
}

document.getElementById("profile-add").addEventListener("click", () => profileForm());

document.addEventListener("click", async (event) => {
  const dataset = event.target.dataset || {};
  try {
    if (dataset.test) {
      const result = await api(`/api/profiles/${encodeURIComponent(dataset.test)}/test`, { method: "POST" });
      toast(result.ok ? `${dataset.test}: reachable (${result.latency_ms} ms)` : `${dataset.test}: ${result.error || "status " + result.status}`, !result.ok);
      loadPorts();
    }
    if (dataset.edit) profileForm(state.profiles.find((p) => p.name === dataset.edit));
    if (dataset.del) {
      const ok = await openModal({
        title: `Delete profile ${dataset.del}`,
        bodyHtml: `<p>Removes <b class="mono">${esc(dataset.del)}</b> and its env vars from .env. Models under <span class="mono">${esc(dataset.del)}/</span> stop resolving immediately.</p>`,
        confirmLabel: "Delete profile",
        danger: true,
      });
      if (ok) { await api(`/api/profiles/${encodeURIComponent(dataset.del)}`, { method: "DELETE" }); toast("Profile deleted"); loadProfiles(); }
    }
    if (dataset.delkey) {
      const ok = await openModal({
        title: `Revoke key ${dataset.delkey}`,
        bodyHtml: `<p>Clients using <b class="mono">${esc(dataset.delkey)}</b> get 401 immediately.</p>`,
        confirmLabel: "Revoke key",
        danger: true,
      });
      if (ok) { await api(`/api/keys/${encodeURIComponent(dataset.delkey)}`, { method: "DELETE" }); toast("Key revoked"); loadKeys(); }
    }
  } catch (e) { toast(e.message, true); }
});

async function profileForm(existing) {
  const p = existing || { name: "", base_url: "", headers: {}, models: [], enabled: true };
  const ok = await openModal({
    title: existing ? `Edit profile ${p.name}` : "Add profile",
    bodyHtml: `
      <div class="field"><label for="f-name">Name (becomes the model prefix)</label>
        <input id="f-name" class="input" value="${esc(p.name)}" ${existing ? "readonly" : ""}></div>
      <div class="field"><label for="f-url">Base URL (OpenAI-compatible, ends with /v1)</label>
        <input id="f-url" class="input" value="${esc(p.base_url)}" placeholder="https://api.groq.com/openai/v1"></div>
      <div class="field"><label for="f-apikey">API key ${existing ? "(leave empty to keep current)" : ""}</label>
        <input id="f-apikey" class="input" type="password" autocomplete="off"></div>
      <div class="field"><label for="f-models">Static models (comma list, only if upstream has no /v1/models)</label>
        <input id="f-models" class="input" value="${esc((p.models || []).join(","))}"></div>
      <div class="field"><label><input id="f-enabled" type="checkbox" ${p.enabled ? "checked" : ""}> Enabled</label></div>`,
    confirmLabel: existing ? "Save changes" : "Add profile",
  });
  if (!ok) return;
  const payload = {
    name: document.getElementById("f-name").value.trim(),
    base_url: document.getElementById("f-url").value.trim(),
    api_key: document.getElementById("f-apikey").value || undefined,
    models: document.getElementById("f-models").value.split(",").map((s) => s.trim()).filter(Boolean),
    enabled: document.getElementById("f-enabled").checked,
  };
  try {
    await api("/api/profiles", { method: "POST", body: JSON.stringify(payload) });
    toast(existing ? "Profile saved" : "Profile added");
    loadProfiles();
  } catch (e) { toast(e.message, true); }
}

// --- api keys ---
async function loadKeys() {
  const body = await api("/api/keys");
  document.getElementById("keys-note").hidden = body.persistent;
  document.getElementById("key-add").disabled = !body.persistent;
  document.getElementById("key-rows").innerHTML = (body.keys || [])
    .map((k) => `<tr>
      <td class="mono">${esc(k.name)}</td>
      <td class="mono">${esc(k.masked)}</td>
      <td class="mono">${new Date(k.created_ms).toLocaleDateString()}</td>
      <td><button class="btn small danger" data-delkey="${esc(k.name)}">Revoke</button></td>
    </tr>`)
    .join("");
}

document.getElementById("key-add").addEventListener("click", async () => {
  const ok = await openModal({
    title: "Create hub API key",
    bodyHtml: `<div class="field"><label for="f-keyname">Key name</label>
      <input id="f-keyname" class="input" placeholder="my-laptop"></div>`,
    confirmLabel: "Create key",
  });
  if (!ok) return;
  try {
    const result = await api("/api/keys", { method: "POST", body: JSON.stringify({ name: document.getElementById("f-keyname").value.trim() }) });
    await openModal({
      title: "Key created — copy it now",
      bodyHtml: `<p>This key is shown once. Only its hash is stored.</p><div class="reveal">${esc(result.key)}</div>`,
      confirmLabel: "Done",
    });
    loadKeys();
  } catch (e) { toast(e.message, true); }
});

// --- setup snippets ---
const SETUP_TARGETS = [
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

function snippetFor(target, base, model, key) {
  const snippets = {
    "claude-code": `# Claude Code — route through llm-hub via env:\nexport ANTHROPIC_BASE_URL=${base}\nexport ANTHROPIC_AUTH_TOKEN=${key}\nexport ANTHROPIC_MODEL=${model}\nclaude\n# note: works when the selected upstream is Anthropic-compatible;\n# for OpenAI-only upstreams use an adapter profile.`,
    "codex": `# ~/.codex/config.toml\nmodel = "${model}"\nmodel_provider = "llm-hub"\n\n[model_providers.llm-hub]\nname = "llm-hub"\nbase_url = "${base}/v1"\nenv_key = "LLM_HUB_KEY"   # export LLM_HUB_KEY=${key}`,
    "cursor": `Cursor -> Settings -> Models -> OpenAI API Key:\n  API key:  ${key}\n  Override base URL: ${base}/v1\n  Model: ${model}`,
    "continue": `# ~/.continue/config.yaml\nmodels:\n  - name: llm-hub\n    provider: openai\n    model: ${model}\n    apiBase: ${base}/v1\n    apiKey: ${key}`,
    "aider": `aider --openai-api-base ${base}/v1 \\\n      --openai-api-key ${key} \\\n      --model openai/${model}`,
    "openai-python": `from openai import OpenAI\n\nclient = OpenAI(base_url="${base}/v1", api_key="${key}")\nresponse = client.chat.completions.create(\n    model="${model}",\n    messages=[{"role": "user", "content": "hello"}],\n)\nprint(response.choices[0].message.content)`,
    "openai-js": `import OpenAI from "openai";\n\nconst client = new OpenAI({ baseURL: "${base}/v1", apiKey: "${key}" });\nconst response = await client.chat.completions.create({\n  model: "${model}",\n  messages: [{ role: "user", content: "hello" }],\n});\nconsole.log(response.choices[0].message.content);`,
    "claude-agent-sdk": `# Claude Agent SDK routes through the Anthropic API surface.\n# Point it at llm-hub with:\nexport ANTHROPIC_BASE_URL=${base}\nexport ANTHROPIC_AUTH_TOKEN=${key}\n\n# python\nfrom claude_agent_sdk import query\nasync for message in query(prompt="hello"):\n    print(message)`,
    "langchain": `from langchain_openai import ChatOpenAI\n\nllm = ChatOpenAI(\n    base_url="${base}/v1",\n    api_key="${key}",\n    model="${model}",\n)\nprint(llm.invoke("hello").content)`,
    "litellm": `import litellm\n\nresponse = litellm.completion(\n    model="openai/${model}",\n    api_base="${base}/v1",\n    api_key="${key}",\n    messages=[{"role": "user", "content": "hello"}],\n)\nprint(response.choices[0].message.content)`,
  };
  return snippets[target] || "";
}

async function loadSetup() {
  const targetSelect = document.getElementById("setup-target");
  if (!targetSelect.options.length) {
    targetSelect.innerHTML = SETUP_TARGETS.map(([value, label]) => `<option value="${value}">${label}</option>`).join("");
    targetSelect.addEventListener("change", renderSetup);
    document.getElementById("setup-model").addEventListener("change", renderSetup);
    document.getElementById("setup-copy").addEventListener("click", () => {
      navigator.clipboard.writeText(document.getElementById("setup-snippet").textContent).then(() => toast("Snippet copied"));
    });
  }
  if (!state.models.length) await loadModels().catch(() => {});
  const modelSelect = document.getElementById("setup-model");
  modelSelect.innerHTML = state.models.map((id) => `<option>${esc(id)}</option>`).join("");
  renderSetup();
}

function renderSetup() {
  const authOn = !!state.meta.auth_enabled;
  const key = authOn ? (localStorage.getItem(KEY_STORAGE) || "<your-hub-key>") : "dummy";
  document.getElementById("setup-auth-note").innerHTML = authOn
    ? `Auth is on — use your master key or a hub API key from the <b>API keys</b> tab.`
    : `Auth is off — any placeholder key works (SDKs require one, so <code>dummy</code> is used).`;
  const model = document.getElementById("setup-model").value || "<profile>/<model>";
  const snippet = snippetFor(document.getElementById("setup-target").value, window.location.origin, model, key);
  document.getElementById("setup-snippet").textContent = snippet;
}

// --- shared ---
function esc(text) {
  return String(text).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

// boot
loadPorts().catch((e) => toast(e.message, true));
loadModels().catch((e) => toast(e.message, true));
setInterval(() => { if (activeTab() === "stats") loadStats().catch(() => {}); }, 5000);
