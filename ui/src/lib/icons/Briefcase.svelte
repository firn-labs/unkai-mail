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
  <rect x="3.5" y="7.5" width="17" height="13" rx="2"/>
  <path d="M9 7.5V6a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v1.5"/>
  <path d="M3.5 12.5h17"/>
</svg>
