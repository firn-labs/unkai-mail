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
  <path d="M13.5 9L21 13.5v2L13.5 13v4.5l2 1.5v1.5L12 19.5l-3.5 1V19l2-1.5V13L3 15.5v-2L10.5 9V4.5a1.5 1.5 0 0 1 3 0V9z"/>
</svg>
