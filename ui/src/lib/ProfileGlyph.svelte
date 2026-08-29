<script lang="ts">
  /**
   * ProfileGlyph — the DOM half of the profile-icon module (#536,
   * see `profileIcon.ts` for the canvas half).  Renders a
   * profile's identity — emoji or named icon from the shared
   * `Icon.svelte` registry — at a given pixel size.  Every surface
   * showing a profile icon (rail bubble, switcher popover rows,
   * the transition screen) goes through this component, so the
   * emoji-vs-named-icon branch exists exactly once.
   *
   * Colour comes from the surrounding text colour (named icons
   * stroke `currentColor`); emoji bring their own.
   */
  import Icon, { type IconName } from './Icon.svelte'
  import type { ProfileIcon } from './api'

  interface Props {
    icon: ProfileIcon
    /** Glyph size in px — the emoji font-size / the SVG box. */
    size: number
  }
  let { icon, size }: Props = $props()
</script>

{#if icon.kind === 'emoji'}
  <span class="leading-none" style="font-size: {size}px">{icon.value}</span>
{:else}
  <Icon name={icon.value as IconName} {size} />
{/if}
