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
  <path d="M10.5 6.5 C11 10 13 12 16.5 12.5 C13 13 11 15 10.5 18.5 C10 15 8 13 4.5 12.5 C8 12 10 10 10.5 6.5 Z"/>
  <path d="M17.5 4 C17.7 5.7 18.8 6.8 20.5 7 C18.8 7.2 17.7 8.3 17.5 10 C17.3 8.3 16.2 7.2 14.5 7 C16.2 6.8 17.3 5.7 17.5 4 Z"/>
</svg>
