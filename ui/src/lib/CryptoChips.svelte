<!--
  Renders the encryption + signature status of a fetched email (#57).

  Two surfaces in one component:
    * Inline chips next to the subject / sender — "🔓 Decrypted",
      "✓ Signed by 9F2A…AAAA", "⚠ Signature invalid", etc.
    * A full-width banner when the receive path couldn't unwrap an
      encrypted message (JMAP no-raw-blob fallback) — tells the
      user to open elsewhere instead of just showing an empty body.

  All status values are kebab-case strings emitted by the Rust
  receive path (see `unkai_core::crypto::DecryptedPayload` +
  `unkai_imap::parse_eml_bytes_with_crypto`).  Keep the matchers in
  sync with the strings; there's a small finite set.
-->
<script lang="ts">
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  let { protection, signatureStatus, signerFingerprint } = $props<{
    protection?: string | null
    signatureStatus?: string | null
    signerFingerprint?: string | null
  }>()

  // Group the 4-char hex into pairs for readability ("9F2A AAAA")
  // and clip to the last 16 nibbles — long fingerprints clutter
  // the header without adding identification value.
  function shortFingerprint(fp: string): string {
    const tail = fp.length > 16 ? fp.slice(-16) : fp
    return tail.match(/.{1,4}/g)?.join(' ') ?? tail
  }
</script>

{#if protection === 'encrypted-cannot-decrypt'}
  <!-- Banner: JMAP fetched an encrypted message we can't unwrap.  The
       receive path swapped the body for a marker string; we replace
       the marker with a more actionable banner here. -->
  <div
    class="rounded-md border border-warning-300 bg-warning-50 dark:border-warning-700 dark:bg-warning-900/30 p-3 text-sm flex items-center gap-2"
    data-test="crypto-cannot-decrypt-banner"
  >
    <Icon name="encrypted" size={20} />
    <span class="flex-1">{m.mail_view_cannot_decrypt_banner()}</span>
  </div>
{:else if protection}
  <!-- Inline chip strip.  Render one chip per dimension (protection
       + signature) so the user can tell at a glance whether the
       message was encrypted, signed, or both. -->
  <div class="flex flex-wrap gap-2 mt-1" data-test="crypto-chips">
    {#if protection === 'encrypted' || protection === 'signed-and-encrypted'}
      <span
        class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-primary-100 text-primary-800 dark:bg-primary-900/40 dark:text-primary-200"
      >
        <!-- Open-padlock variant of the shield: "we received this
             encrypted and decrypted it locally for you to read".
             Pairs with the closed-padlock chip MailList shows on
             rows whose `protection === "encrypted"` — the same
             shield silhouette, the lock state tells you whether
             you're looking at the wire (closed) or the
             decrypted view (open). -->
        <Icon name="decrypted" size={14} />
        {m.mail_view_chip_decrypted()}
      </span>
    {/if}
    {#if protection === 'signed' || protection === 'signed-and-encrypted'}
      {#if signatureStatus === 'valid'}
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-success-100 text-success-800 dark:bg-success-900/40 dark:text-success-200"
          title={signerFingerprint ?? ''}
        >
          <Icon name="verified" size={14} />
          {m.mail_view_chip_signed_valid({
            fp: signerFingerprint ? shortFingerprint(signerFingerprint) : '',
          })}
        </span>
      {:else if signatureStatus === 'invalid'}
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-error-100 text-error-800 dark:bg-error-900/40 dark:text-error-200"
        >
          <Icon name="warning" size={14} />
          {m.mail_view_chip_signed_invalid()}
        </span>
      {:else}
        <!-- unknown-signer or any other non-valid status -->
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-surface-200 text-surface-800 dark:bg-surface-700 dark:text-surface-200"
        >
          <Icon name="signed" size={14} />
          {m.mail_view_chip_signed_unknown()}
        </span>
      {/if}
    {/if}
  </div>
{/if}
