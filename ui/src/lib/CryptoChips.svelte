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

  let { protection, signatureStatus, signerFingerprint, decrypted } = $props<{
    protection?: string | null
    signatureStatus?: string | null
    signerFingerprint?: string | null
    /** `true` when we actually have the plaintext body in hand
     *  (either the message was plain to start with — in which case
     *  the encrypt-side chips don't render at all — or the
     *  decrypt-on-demand IPC has run and re-parsed the inner MIME).
     *  Flips the encrypt chip from a closed-lock "Encrypted" pill
     *  (still on the wire, awaiting passphrase) to an open-lock
     *  "Decrypted" pill (unlocked locally).  See MailView, which
     *  passes `!!(email.body_text || email.body_html)`. */
    decrypted?: boolean
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
      {#if decrypted}
        <!-- We have the plaintext — show the open-padlock variant
             of the shield ("decrypted on this device").  Pairs
             with the closed-padlock chip MailList shows on rows
             whose body hasn't been unlocked yet: the same shield
             silhouette, the lock state tells you which side of
             the decrypt you're on. -->
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-primary-100 text-primary-800 dark:bg-primary-900/40 dark:text-primary-200"
        >
          <Icon name="decrypted" size={14} />
          {m.mail_view_chip_decrypted()}
        </span>
      {:else}
        <!-- Encrypted but not yet unlocked — closed padlock,
             muted tone so it visually pairs with the inline
             Decrypt prompt rendered below the chip strip. -->
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-warning-100 text-warning-800 dark:bg-warning-900/40 dark:text-warning-200"
        >
          <Icon name="encrypted" size={14} />
          {m.mail_view_chip_encrypted()}
        </span>
      {/if}
    {/if}
    {#if protection === 'signed' || protection === 'signed-and-encrypted'}
      {#if signatureStatus === 'valid'}
        <!-- Green: math sound AND signer trusted (CA chain or TOFU). -->
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-success-100 text-success-800 dark:bg-success-900/40 dark:text-success-200"
          title={signerFingerprint ?? ''}
        >
          <Icon name="verified" size={14} />
          {#if signerFingerprint}
            {m.mail_view_chip_signed_valid({ fp: shortFingerprint(signerFingerprint) })}
          {:else}
            {m.mail_view_chip_signed_trusted()}
          {/if}
        </span>
      {:else if signatureStatus === 'invalid'}
        <!-- Red: signature does not verify (tampered / wrong key). -->
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-error-100 text-error-800 dark:bg-error-900/40 dark:text-error-200"
        >
          <Icon name="warning" size={14} />
          {m.mail_view_chip_signed_invalid()}
        </span>
      {:else if signatureStatus === 'valid-expired-cert'}
        <!-- Amber: math sound but the signing certificate has expired. -->
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-warning-100 text-warning-800 dark:bg-warning-900/40 dark:text-warning-200"
          title={m.mail_view_chip_signed_expired_tooltip()}
        >
          <Icon name="signed" size={14} />
          {m.mail_view_chip_signed_expired()}
        </span>
      {:else if signatureStatus === 'valid-untrusted-issuer'}
        <!-- Amber: math sound but the issuer isn't trusted / self-signed. -->
        <span
          class="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full bg-warning-100 text-warning-800 dark:bg-warning-900/40 dark:text-warning-200"
          title={m.mail_view_chip_signed_untrusted_tooltip()}
        >
          <Icon name="signed" size={14} />
          {m.mail_view_chip_signed_untrusted()}
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
