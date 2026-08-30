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
  <path d="M3.5 11.5L12 4l8.5 7.5"/>
  <path d="M5.5 9.8v10.7h13V9.8"/>
  <path d="M10 20.5V15h4v5.5"/>
</svg>
