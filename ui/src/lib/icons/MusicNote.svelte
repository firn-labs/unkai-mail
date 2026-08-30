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
  <path d="M9.5 18.2V6l10.5-2.5v12.4"/>
  <circle cx="6.8" cy="18.2" r="2.7"/>
  <circle cx="17.3" cy="15.9" r="2.7"/>
</svg>
