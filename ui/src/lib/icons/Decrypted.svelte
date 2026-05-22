<script lang="ts">
  export let size: number | string = 20;
  let className = '';
  export { className as class };
</script>

<!--
  "Decrypted" icon — same shield silhouette + padlock body as the
  Encrypted icon, with the shackle drawn in the *open* position
  (raised, tilted right) instead of seated into the lock body.
  Visually pairs with `Encrypted.svelte` so the closed↔open
  affordance reads at a glance in the MailView chip strip and the
  MailList row icons.
-->
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
  <!-- Shield outline — identical to Encrypted. -->
  <path d="M12 3l8 3v5c0 5-3.5 9-8 10-4.5-1-8-5-8-10V6l8-3z"/>
  <!-- Lock body — identical to Encrypted. -->
  <rect x="9" y="11" width="6" height="5" rx="1"/>
  <!-- Open shackle: starts at the left edge of the lock body and
       arcs up + tilts to the right, ending mid-air above the body.
       The unanchored end communicates "unlocked" — the canonical
       open-padlock convention used by every major icon set. -->
  <path d="M10.5 11V9.5a1.5 1.5 0 0 1 3 0"/>
  <path d="M13.5 9.5a1.5 1.5 0 0 1 2.6-1"/>
</svg>
