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
</svg>
