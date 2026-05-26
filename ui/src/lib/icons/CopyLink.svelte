<script lang="ts">
  export let size: number | string = 20;
  let className = '';
  export { className as class };
</script>

<svg
  xmlns="http://www.w3.org/2000/svg"
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="1.6"
  stroke-linecap="round"
  stroke-linejoin="round"
  class={className}
  aria-hidden="true"
>
  <!-- Back page (the "duplicate" / copy target): just the L-corner
       so it reads as a second sheet behind the front one without
       making the icon noisy at 14 px. -->
  <path d="M9 4h9a2 2 0 0 1 2 2v9"/>
  <!-- Front page (the source). -->
  <rect x="4" y="8" width="12" height="12" rx="2"/>
  <!-- A short chain-link inside the front page, drawn as two
       opposing half-loops on a 45° diagonal so the "link" reading
       comes through at small sizes. -->
  <path d="M10.5 15.5l3-3"/>
  <path d="M13 13.5a1.6 1.6 0 0 0 0-2.3 1.6 1.6 0 0 0-2.3 0L10 12"/>
  <path d="M11 14l-0.7 0.7a1.6 1.6 0 0 0 0 2.3 1.6 1.6 0 0 0 2.3 0"/>
</svg>
