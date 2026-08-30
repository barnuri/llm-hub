const KEY_STORAGE = "llm-hub-key";

export class UnauthorizedError extends Error {
  constructor() {
    super("unauthorized");
  }
}

export function savedKey(): string | null {
  return localStorage.getItem(KEY_STORAGE);
}

export function saveKey(key: string): void {
  localStorage.setItem(KEY_STORAGE, key);
}

export async function api<T>(path: string, options: RequestInit = {}): Promise<T> {
  const headers = new Headers(options.headers);
  headers.set("content-type", "application/json");
  const key = savedKey();
  if (key) {
    headers.set("authorization", `Bearer ${key}`);
  }
  const response = await fetch(path, { ...options, headers });
  if (response.status === 401) {
    throw new UnauthorizedError();
  }
  if (!response.ok) {
    const body = (await response.json().catch(() => ({}))) as { error?: { message?: string } };
    throw new Error(body.error?.message ?? `${response.status} ${response.statusText}`);
  }
  return response.json() as Promise<T>;
}
