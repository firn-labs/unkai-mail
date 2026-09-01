<script lang="ts" generics="T extends string | number | null">
  /**
   * Select — custom popover dropdown (#128).
   *
   * Replaces the native `<select>` element for fields that
   * need a more modern look.  Native selects render the
   * platform's system dropdown which clashes badly with the
   * rest of the editor (especially on Linux Chromium).  This
   * component matches the styling vocabulary of `DateField`
   * and `TimeField`: a button face, a popover anchored below,
   * hover / selected highlights with the primary colour, and
   * keyboard navigation (Arrow Up/Down, Enter, Escape).
   *
   * Generic over the option `value` so the same component can
   * back string-keyed selects (calendar id, RSVP partstat,
   * show-as) and number-keyed ones (reminder minutes-before)
   * without coercing through `string`.
   */

  import { onMount } from 'svelte'
  import Icon, { type IconName } from './Icon.svelte'

  /** Optional icon adornment for an option.  Renders before the
   *  label both in the trigger face and in the dropdown rows.
   *  `colorClass` is a Tailwind color token (e.g.
   *  `text-success-500`) applied to the icon wrapper so callers
   *  can colour-code semantic options like RSVP states. */
  type Option<V> = {
    value: V
    label: string
    iconName?: IconName
    iconColorClass?: string
  }

  let {
    value = $bindable(),
    options,
    placeholder = '',
    id,
    ariaLabel,
    disabled = false,
    onchange,
  }: {
    value: T
    options: Option<T>[]
    placeholder?: string
    id?: string
    ariaLabel?: string
    disabled?: boolean
    /** Optional callback fired when the user picks an option.
     *  Useful when the parent doesn't want to use `bind:value`
     *  (e.g. the value is a derived view of some other state
     *  and needs custom write-back logic). */
    onchange?: (v: T) => void
  } = $props()

  let open = $state(false)
  let anchor: HTMLDivElement | undefined = $state()
  let listEl: HTMLUListElement | undefined = $state()
  /** Index of the option highlighted by keyboard nav.  Resets
   *  to the currently-selected value on open so the cursor
   *  starts on something familiar. */
  let activeIndex = $state(0)

  let selectedOption = $derived(options.find((o) => o.value === value))
  let displayLabel = $derived(selectedOption?.label ?? placeholder)

  function pick(opt: Option<T>) {
    value = opt.value
    open = false
    onchange?.(opt.value)
  }

  function toggle() {
    if (disabled) return
    if (!open) {
      activeIndex = Math.max(
        0,
        options.findIndex((o) => o.value === value),
      )
    }
    open = !open
  }

  function onKey(e: KeyboardEvent) {
    if (disabled) return
    if (!open) {
      if (e.key === 'ArrowDown' || e.key === 'Enter' || e.key === ' ') {
        e.preventDefault()
        toggle()
      }
      return
    }
    if (e.key === 'Escape') {
      e.preventDefault()
      open = false
    } else if (e.key === 'ArrowDown') {
      e.preventDefault()
      activeIndex = (activeIndex + 1) % options.length
      ensureVisible()
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      activeIndex = (activeIndex - 1 + options.length) % options.length
      ensureVisible()
    } else if (e.key === 'Enter') {
      e.preventDefault()
      const opt = options[activeIndex]
      if (opt) pick(opt)
    } else if (e.key === 'Home') {
      e.preventDefault()
      activeIndex = 0
      ensureVisible()
    } else if (e.key === 'End') {
      e.preventDefault()
      activeIndex = options.length - 1
      ensureVisible()
    }
  }

  /** Scroll the active option into the popover viewport so the
   *  keyboard cursor doesn't disappear off the bottom on long
   *  lists (calendar pickers, reminder presets). */
  function ensureVisible() {
    queueMicrotask(() => {
      const li = listEl?.querySelector(`[data-idx="${activeIndex}"]`)
      if (li instanceof HTMLElement) li.scrollIntoView({ block: 'nearest' })
    })
  }

  onMount(() => {
    function onClick(e: MouseEvent) {
      if (!open || !anchor) return
      if (!anchor.contains(e.target as Node)) open = false
    }
    document.addEventListener('mousedown', onClick)
    return () => document.removeEventListener('mousedown', onClick)
  })
</script>

<div class="relative" bind:this={anchor}>
  <button
    type="button"
    {id}
    aria-label={ariaLabel}
    aria-haspopup="listbox"
    aria-expanded={open}
    {disabled}
    class="input w-full px-3 py-2 text-sm rounded-lg text-left flex items-center justify-between gap-2 disabled:opacity-50 disabled:cursor-not-allowed"
    onclick={toggle}
    onkeydown={onKey}
  >
    <span class="truncate flex items-center gap-1.5 {selectedOption ? '' : 'text-surface-400'}">
      {#if selectedOption?.iconName}
        <span class={selectedOption.iconColorClass ?? ''}>
          <Icon name={selectedOption.iconName} size={14} />
        </span>
      {/if}
      <span class="truncate">{displayLabel}</span>
    </span>
    <svg
      xmlns="http://www.w3.org/2000/svg"
      class="w-4 h-4 text-surface-500 shrink-0 transition-transform {open
        ? 'rotate-180'
        : ''}"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <polyline points="6 9 12 15 18 9" />
    </svg>
  </button>

  {#if open}
    <ul
      bind:this={listEl}
      class="absolute z-50 mt-1 w-full max-h-72 overflow-y-auto popover-opaque rounded-xl"
      role="listbox"
    >
      {#each options as opt, i (`${i}-${String(opt.value)}`)}
        {@const isSelected = opt.value === value}
        {@const isActive = i === activeIndex}
        <li
          role="option"
          aria-selected={isSelected}
          data-idx={i}
          class="px-3 py-2 text-sm cursor-pointer flex items-center justify-between gap-2 {isSelected
            ? 'bg-primary-500 text-white'
            : isActive
              ? 'bg-primary-500/15'
              : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
          onmousedown={(e) => {
            e.preventDefault()
            pick(opt)
          }}
          onmouseenter={() => (activeIndex = i)}
        >
          <span class="truncate flex items-center gap-1.5">
            {#if opt.iconName}
              <span class={isSelected ? '' : (opt.iconColorClass ?? '')}>
                <Icon name={opt.iconName} size={14} />
              </span>
            {/if}
            <span class="truncate">{opt.label}</span>
          </span>
          {#if isSelected}
            <svg
              xmlns="http://www.w3.org/2000/svg"
              class="w-4 h-4 shrink-0"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2.5"
              stroke-linecap="round"
              stroke-linejoin="round"
              aria-hidden="true"
            >
              <polyline points="20 6 9 17 4 12" />
            </svg>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>
