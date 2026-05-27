<script lang="ts">
  export let size: number | string = 20;
  let className = '';
  export { className as class };
</script>

<!--
  NotDone — Success (circle + checkmark) with a diagonal slash
  cutting across it.  Same 24×24 / 1.6-stroke vocabulary as the
  rest of the icon family so it lines up next to Success in
  toolbars without visual drift.  Used by the TasksView "Mark
  as not done" button: clicking the slashed check toggles a
  completed task back to NEEDS-ACTION.
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
  <circle cx="12" cy="12" r="9"/>
  <path d="M7.5 12.5l3 3 6-6.5"/>
  <line x1="19" y1="19" x2="5" y2="5"/>
</svg>
