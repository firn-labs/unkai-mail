<script lang="ts">
  /**
   * PasswordInput — the canonical password-field shape (#481).
   * Packages the Skeleton `.input` field with the reveal-toggle
   * eye button so every password surface (setup wizard, account
   * settings, key passphrases, …) speaks the same visual language
   * without each call site re-inlining the
   * `<div class="relative">…<input…>…<button>eye</button></div>`
   * structure.
   *
   * Like SearchInput, this is structural rather than cosmetic: the
   * eye button is absolutely positioned inside the wrapper, the
   * input reserves right padding to clear it, and the input's
   * `type` flips between `password` and `text` with the toggle.
   * The visibility state is component-local and always starts
   * hidden — revealing is a deliberate per-field, per-mount act.
   *
   * # Adornment slot
   *
   * Callers that need a leading icon (the wizard's lock glyph)
   * pass children — they render inside the `relative` wrapper, so
   * `absolute left-2.5 top-1/2` resolves against the input box.
   * Pair with a matching `pl-*` in `inputClass`.
   */

  import type { Snippet } from 'svelte'
  import type { HTMLInputAttributes } from 'svelte/elements'
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  interface Props {
    /** Bindable password string. */
    value: string
    id?: string
    placeholder?: string
    /** Accessible label for the input itself, when there is no
     *  associated `<label>` element. */
    ariaLabel?: string
    autocomplete?: HTMLInputAttributes['autocomplete']
    disabled?: boolean
    /** Tailwind class on the outer `relative` wrapper — layout
     *  concerns (width, margins, flex participation) go here. */
    class?: string
    /** Sizing/border classes on the `<input>` — the component
     *  supplies `input w-full` and the `pr-9` that clears the eye
     *  button; callers pass the panel's canonical text/padding/
     *  radius classes so each surface keeps its local vocabulary. */
    inputClass?: string
    onkeydown?: (e: KeyboardEvent) => void
    /** Optional bindable handle on the underlying `<input>` so
     *  callers can `focus()` / `select()` from outside. */
    inputEl?: HTMLInputElement | null
    /** Pull focus when the field mounts (and again whenever this
     *  flips back to `true`, e.g. after a busy frame re-enables
     *  the form).  Component-managed rather than the native
     *  `autofocus` attribute so modal prompts get the same
     *  deferred-microtask focus without per-caller `{@attach}`. */
    autofocus?: boolean
    /** Adornment slot rendered inside the relative wrapper. */
    children?: Snippet
  }

  let {
    value = $bindable(''),
    id,
    placeholder,
    ariaLabel,
    autocomplete = 'off',
    disabled = false,
    class: cls = '',
    inputClass = 'text-sm px-3 py-2 rounded-lg',
    onkeydown,
    inputEl = $bindable(null),
    autofocus = false,
    children,
  }: Props = $props()

  let show = $state(false)
</script>

<div class="relative {cls}">
  {@render children?.()}
  <input
    {id}
    type={show ? 'text' : 'password'}
    spellcheck="false"
    autocapitalize="off"
    class="input w-full pr-9 {inputClass}"
    {placeholder}
    aria-label={ariaLabel}
    {autocomplete}
    {disabled}
    {onkeydown}
    bind:value
    bind:this={inputEl}
    {@attach (node: HTMLInputElement) => {
      // Deferred so focus lands after the surrounding modal/panel
      // finishes mounting; re-runs when `autofocus` flips true.
      if (autofocus) queueMicrotask(() => node.focus())
    }}
  />
  <button
    type="button"
    class="absolute right-2.5 top-1/2 -translate-y-1/2 flex items-center text-surface-400 hover:text-surface-600 dark:hover:text-surface-300 disabled:opacity-50"
    onclick={() => (show = !show)}
    {disabled}
    title={show ? m.hide_password() : m.show_password()}
    aria-label={show ? m.hide_password() : m.show_password()}
    aria-pressed={show}
  >
    <Icon name={show ? 'eye-off' : 'eye'} size={14} />
  </button>
</div>
