export function formatTime(tsMs: number): string {
  return new Date(tsMs).toLocaleTimeString();
}

export function formatDate(tsMs: number): string {
  return new Date(tsMs).toLocaleDateString();
}
