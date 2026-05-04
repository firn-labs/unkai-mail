// Attachment-mention detector (#250).
//
// Compose runs the user's body text through `mentionsAttachment`
// to decide whether to show the "you mentioned an attachment but
// didn't attach a file" banner.  Multi-language by design: we
// match against keywords from every supported locale at once
// rather than only the active UI locale, because users routinely
// write a German email while the UI runs in English (or vice
// versa).
//
// Detection rules:
//
//   - Word-boundary matches only.  "detached" / "anhängig" /
//     "attaché" must NOT trigger.  We use Unicode-aware regex
//     `\b` semantics (the standard `/\b/u` flag).
//   - Quoted-reply text is excluded.  Lines starting with `>`
//     (the standard plain-text reply quote) and HTML
//     `<blockquote>` blocks (Tiptap-formatted replies) are
//     stripped before scanning, so a forwarded thread with
//     "see attached" in the original message doesn't fire the
//     warning when the user just wrote "Hi, FYI."
//   - Threshold: any single match.  If the user really wants to
//     write the word "attachment" without attaching anything,
//     the banner has a Dismiss button.
//
// Future-proofing: when a new locale is added to
// `messages/<code>.json`, append its keywords here in the same
// shape.  The file pattern keeps adding new locales a localised
// change rather than a sweep across detection logic.

/**
 * Word-stems we recognise as "the user is mentioning an
 * attachment", grouped per locale.  Stems use the regex
 * source-string convention; the consumer wraps each stem in
 * `\b…\b` at match time.
 *
 * Conservative on purpose: we'd rather miss an oblique
 * reference than fire the warning on a false positive (e.g.
 * "detached" or "anhängig").  The keys here are the obvious
 * core verbs / nouns that mean "I'm referring to the file I
 * attached":
 *
 *   - English: attached, attachment(s), enclosed, "see/find
 *     attached" (caught via the bare "attached" stem).
 *   - German: Anhang/Anhänge, anbei, beigefügt.  Pure
 *     `anhäng-` is too greedy ("anhängig" = pending) so we
 *     anchor the noun forms specifically.
 */
const MENTION_STEMS_BY_LOCALE: Record<string, string[]> = {
  en: [
    'attached',
    'attachment',
    'attachments',
    'enclosed',
  ],
  de: [
    // Noun forms — both the singular "Anhang" and the
    // plural / dative "Anhänge"/"Anhängen".  Word boundaries
    // around the regex stop the match from picking up
    // unrelated -hang words ("Vorhang", "anhängig").
    'Anhang',
    'Anhänge',
    'Anhängen',
    'anbei',
    'beigefügt',
    'beigefügte',
    'beigefügten',
  ],
}

/**
 * Strip quoted-reply blocks from `text` so a forwarded thread
 * with "see attached" in the original doesn't trigger the
 * warning.  Recognises:
 *   - lines starting with `>` (RFC-2822 plain-text quote)
 *   - HTML <blockquote>...</blockquote> wrappers (Tiptap
 *     reply formatting)
 *
 * The plain-text strip happens line-by-line, so a `>` later in
 * a line (e.g. inside running text) is preserved.
 */
function stripQuotedReplies(text: string): string {
  // Drop blockquote element contents.  We pass the body
 // through `htmlToText`-style normalisation upstream, but a
  // belt-and-braces pass against `<blockquote>...</blockquote>`
  // catches cases where the Tiptap output wasn't fully
  // serialised before this function runs.
  let cleaned = text.replace(/<blockquote[\s\S]*?<\/blockquote>/gi, '\n')
  cleaned = cleaned
    .split('\n')
    .filter((line) => !/^\s*>/.test(line))
    .join('\n')
  return cleaned
}

/**
 * Return `true` when `text` (a plain-text version of the
 * Compose body) contains an attachment mention in any
 * supported locale.  Intended to be called on the full
 * post-quoted-strip body — the function does that strip
 * itself so callers don't have to remember.
 */
export function mentionsAttachment(text: string): boolean {
  const cleaned = stripQuotedReplies(text)
  if (!cleaned.trim()) return false
  for (const stems of Object.values(MENTION_STEMS_BY_LOCALE)) {
    for (const stem of stems) {
      // `i` is fine: keyword lists are ASCII-or-Latin-1, and
      // German nouns (capitalised) should still trigger when
      // the user writes them lowercase mid-sentence.
      const re = new RegExp(`\\b${escapeRegex(stem)}\\b`, 'iu')
      if (re.test(cleaned)) return true
    }
  }
  return false
}

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}
