/**
 * Turn a Tauri-side error into a human-readable string.
 *
 * UnkaiError is a Rust enum with `#[derive(Serialize)]`, which serde
 * serialises as an externally-tagged object: `{ "Network": "..." }`,
 * `{ "Auth": "..." }`, etc. The variant name tells us the category,
 * the inner string is the message.
 */
export function formatError(e: unknown): string {
  if (e == null) return ''
  if (typeof e === 'string') return e
  if (e instanceof Error) return e.message

  if (typeof e === 'object') {
    const obj = e as Record<string, unknown>
    if (typeof obj.message === 'string') return obj.message

    // Externally tagged UnkaiError: first key is the variant name,
    // its value is the message string.
    const entries = Object.entries(obj)
    if (entries.length === 1) {
      const [variant, value] = entries[0]
      if (typeof value === 'string') return `${variant}: ${value}`
    }

    try {
      return JSON.stringify(e)
    } catch {
      return String(e)
    }
  }
  return String(e)
}
