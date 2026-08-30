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
  <path d="M2.5 9.3L12 4.8l9 4.5-9 4.5-9.5-4.5z"/>
  <path d="M6.5 11.4v4.4c0 1.5 2.4 2.7 5.5 2.7s5.5-1.2 5.5-2.7v-4.4"/>
  <path d="M21 9.3v5.2"/>
</svg>
