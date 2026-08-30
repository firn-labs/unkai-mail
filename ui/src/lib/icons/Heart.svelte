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
  <path d="M12 20.3C7 16.5 3.5 13.3 3.5 9.7 3.5 7.2 5.5 5.3 7.9 5.3c1.6 0 3.1.8 4.1 2.2 1-1.4 2.5-2.2 4.1-2.2 2.4 0 4.4 1.9 4.4 4.4 0 3.6-3.5 6.8-8.5 10.6z"/>
</svg>
