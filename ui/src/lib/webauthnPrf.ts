// WebAuthn PRF helper for the FIDO unlock feature (#164).
//
// We use the PRF extension — WebAuthn's standardised wrapping of
// CTAP2's hmac-secret — to derive a stable 32-byte key from a FIDO
// authenticator (USB hardware key, Touch ID, Windows Hello, ...).
// The OS handles the auth UX entirely; we hand the PRF output to
// the Rust side which uses it as an AES-256-GCM key to wrap /
// unwrap the SQLCipher master key.
//
// Browser support landscape (as of 2026):
// - Safari 17+ on macOS / iOS — supports PRF.
// - Edge / Chrome on Windows / macOS — supports PRF for hmac-secret.
// - WebKitGTK on Linux — depends on the libwebkit2gtk-4.1 build
//   shipped by the distro; users may need a recent enough version.
// All fall back to "PRF extension not supported" with a clean
// error if the engine doesn't expose it; the Settings UI surfaces
// that as a tooltip on the disabled button.

const RP_NAME = 'Unkai Mail'

// The Relying Party ID must be a registrable suffix of (or equal to)
// the current page's effective domain, otherwise the browser refuses
// the call with "The relying party ID is not a registrable domain
// suffix of, nor equal to the current domain."  Tauri's webview
// origin differs by platform — `tauri://localhost` on macOS,
// `https://tauri.localhost` on Windows, `http://localhost:1420` in
// Linux dev — so we read the actual hostname at call time instead
// of hardcoding one.  WebAuthn treats `localhost` and `*.localhost`
// as a special case (no certificate or HTTPS required), which is
// exactly what we get inside the Tauri webview.  Using the same
// value at enroll and unlock time is what matters: a credential
// registered against host X can only be asserted against host X.
function rpId(): string {
  return window.location.hostname
}

function bufToB64(buf: ArrayBuffer | Uint8Array): string {
  const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
  let bin = ''
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i])
  return btoa(bin)
}

function b64ToBuf(s: string): Uint8Array<ArrayBuffer> {
  const bin = atob(s)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/**
 * Coarse check that WebAuthn itself is wired up.  We deliberately
 * *don't* gate on `getClientCapabilities().extensionPrf` here —
 * older engines (Linux WebKitGTK < 2.46, in particular) advertise
 * `extensionPrf: false` even when an authenticator that supports
 * hmac-secret would otherwise work, and there are
 * authenticator+engine combinations that surprise the static
 * capability table either way.  The honest source of truth is
 * whether `credentials.create({ extensions: { prf: ... } })`
 * returns a PRF output; the UI just calls it and surfaces a
 * specific error if the result is missing.
 */
export function isWebAuthnAvailable(): boolean {
  return (
    typeof PublicKeyCredential !== 'undefined' &&
    typeof navigator !== 'undefined' &&
    !!navigator.credentials &&
    typeof navigator.credentials.create === 'function'
  )
}

export interface EnrolledCredential {
  /** Base64-encoded credential id from the authenticator. */
  credentialIdB64: string
  /** Base64-encoded PRF output (32 bytes). */
  prfOutputB64: string
  /** Base64-encoded salt the credential was registered with. */
  saltB64: string
}

/**
 * Run WebAuthn `credentials.create` with the PRF extension enabled
 * and the supplied salt as the eval input.  Returns the credential
 * id plus the matching PRF output, both base64-encoded for the
 * Rust IPC.
 *
 * The OS shows its own auth sheet — Touch ID prompt, Windows Hello
 * sign-in, "Tap your security key", whatever's appropriate.  We
 * never see the biometric or the per-credential secret; we only
 * receive the deterministic PRF output that's reproducible by
 * future `credentials.get` calls with the same salt.
 */
export async function enrollFidoCredential(
  saltB64: string,
  userHandle: string,
  label: string,
): Promise<EnrolledCredential> {
  const salt = b64ToBuf(saltB64)
  // Random per-call challenge.  PRF doesn't care about the
  // challenge contents (it's bound to the credential, not the
  // assertion), but WebAuthn requires one.
  const challenge = crypto.getRandomValues(new Uint8Array(32))
  const userIdBytes = new TextEncoder().encode(userHandle)
  const cred = (await navigator.credentials.create({
    publicKey: {
      rp: { id: rpId(), name: RP_NAME },
      user: {
        id: userIdBytes,
        name: userHandle,
        displayName: label,
      },
      challenge,
      pubKeyCredParams: [
        { type: 'public-key', alg: -7 }, // ES256
        { type: 'public-key', alg: -257 }, // RS256
      ],
      authenticatorSelection: {
        residentKey: 'preferred',
        userVerification: 'required',
      },
      timeout: 60_000,
      extensions: {
        prf: { eval: { first: salt } },
      } as AuthenticationExtensionsClientInputs,
    },
  })) as PublicKeyCredential | null
  if (!cred) throw new Error('WebAuthn returned no credential')
  const exts = cred.getClientExtensionResults() as AuthenticationExtensionsClientOutputs & {
    prf?: { enabled?: boolean; results?: { first?: ArrayBuffer } }
  }
  const prfFirst = exts.prf?.results?.first
  if (!prfFirst) {
    // Three reasons we'd land here:
    //   1. The webview doesn't implement the PRF extension.  On
    //      Linux this means WebKitGTK < 2.46.  Update libwebkit2gtk
    //      via the system package manager — Ubuntu 24.04, Fedora
    //      40, and Arch all carry 2.46+ on stable.
    //   2. The selected authenticator doesn't support hmac-secret.
    //      Most YubiKey 5 / SoloKey 2 / Touch ID / Windows Hello
    //      builds do; some older third-party USB keys do not.
    //   3. `prf.enabled` came back true at create-time but the
    //      engine elected not to evaluate the salt during this
    //      registration (a few WebKit builds defer first eval to
    //      the first credentials.get).  We could retry via
    //      credentials.get with the same salt, but in practice
    //      this surfaces the same root cause as #1.
    const enabled = exts.prf?.enabled === true
    if (enabled) {
      throw new Error(
        'Authenticator registered, but the PRF extension didn\'t evaluate at enroll time. ' +
          'Try a different authenticator, or wait for the next browser engine update.',
      )
    }
    throw new Error(
      'PRF extension unavailable from this WebAuthn implementation. ' +
        'On Linux this usually means WebKitGTK is below 2.46 — update libwebkit2gtk ' +
        '(Ubuntu 24.04+, Fedora 40+, Arch all ship 2.46+). On macOS / Windows the OS ' +
        'shipped engines support PRF on recent versions.',
    )
  }
  return {
    credentialIdB64: bufToB64(cred.rawId),
    prfOutputB64: bufToB64(prfFirst),
    saltB64,
  }
}

/**
 * Run `credentials.get` against a previously-enrolled credential
 * and return its PRF output.  Used by the unlock flow (Phase 1B —
 * not wired yet at boot) and by the "Remove this key" confirm step
 * to prove the user can still authenticate before we drop a wrap.
 */
export async function evaluateFidoPrf(
  credentialIdB64: string,
  saltB64: string,
): Promise<string> {
  // Guard before reaching for `navigator.credentials.get`: on
  // Linux WebKitGTK builds without WebAuthn the property is
  // missing, and the raw "undefined is not an object" stack trace
  // is unreadable for users.  Surface the same message we use at
  // enroll time so they understand why the action failed.
  if (
    typeof navigator === 'undefined' ||
    !navigator.credentials ||
    typeof navigator.credentials.get !== 'function'
  ) {
    throw new Error(
      "Your platform's WebView doesn't expose WebAuthn, so this hardware key can't be used here. " +
        'On Linux this usually means WebKitGTK is below 2.46 — update libwebkit2gtk ' +
        '(Ubuntu 24.04+, Fedora 40+, Arch all ship 2.46+). On macOS / Windows the OS-shipped ' +
        'engines support WebAuthn on recent versions.',
    )
  }
  const credentialId = b64ToBuf(credentialIdB64)
  const salt = b64ToBuf(saltB64)
  const challenge = crypto.getRandomValues(new Uint8Array(32))
  const assertion = (await navigator.credentials.get({
    publicKey: {
      challenge,
      rpId: rpId(),
      allowCredentials: [{ type: 'public-key', id: credentialId }],
      userVerification: 'required',
      timeout: 60_000,
      extensions: {
        prf: { eval: { first: salt } },
      } as AuthenticationExtensionsClientInputs,
    },
  })) as PublicKeyCredential | null
  if (!assertion) throw new Error('WebAuthn returned no assertion')
  const exts = assertion.getClientExtensionResults() as AuthenticationExtensionsClientOutputs & {
    prf?: { results?: { first?: ArrayBuffer } }
  }
  const prfFirst = exts.prf?.results?.first
  if (!prfFirst) {
    throw new Error('Authenticator did not return a PRF output')
  }
  return bufToB64(prfFirst)
}
