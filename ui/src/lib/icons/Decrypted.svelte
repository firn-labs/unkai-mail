<script lang="ts">
  export let size: number | string = 20;
  let className = '';
  export { className as class };
</script>

<!--
  "Decrypted" icon — same shield silhouette + lock body as
  Encrypted.  The shackle ("Bügel") is drawn as one continuous
  stroke whose right leg stops short of the lock body, leaving a
  visible gap so the lock reads as *open* at a glance.  No keyhole
  / extra glyph in the body — the open-vs-closed shackle is the
  whole semantic difference, anything else inside the body just
  adds noise at chip-row sizes (14–20 px).
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
  <!-- Shackle, one stroke: up the left, over the top, down a short
       way on the right.  The right leg ends at y=10 while the
       lock body's top edge is y=11, so a 1-unit gap reads as a
       clearly-popped-open padlock. -->
  <path d="M10.5 11V9.5a1.5 1.5 0 0 1 3 0V10"/>
</svg>
