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
  <path d="M7.5 7.5h9c2.8 0 5 2.2 5 5 0 2.2-1.8 4-4 4-1.2 0-2.3-.5-3-1.4l-.6-.8H10.1l-.6.8c-.7.9-1.8 1.4-3 1.4-2.2 0-4-1.8-4-4 0-2.8 2.2-5 5-5z"/>
  <path d="M7.5 10.5v4"/>
  <path d="M5.5 12.5h4"/>
  <circle cx="15.3" cy="11.3" r="1" fill="currentColor" stroke="none"/>
  <circle cx="17.3" cy="13.6" r="1" fill="currentColor" stroke="none"/>
</svg>
