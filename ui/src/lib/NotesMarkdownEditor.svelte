<script lang="ts">
  /**
   * NotesMarkdownEditor — CodeMirror 6 markdown editor for the Notes
   * UI (#138).
   *
   * Plain markdown source with grammar-aware syntax highlighting +
   * an optional preview pane that renders via `marked`.  Real
   * markdown round-trip end-to-end: what the user types is what
   * goes to the Nextcloud Notes API, no Tiptap-style HTML
   * intermediate.
   *
   * # Why CodeMirror 6 and not Tiptap
   *
   * The user explicitly asked for "real markdown" in #138.  Tiptap
   * stores ProseMirror nodes and would need a serializer to round-
   * trip through markdown — fine for casual prose but mildly lossy
   * on tables, custom HTML, advanced lists.  CodeMirror is honest
   * to the on-disk format and handles long documents well.
   *
   * # Preview
   *
   * Toggleable side-by-side preview pane — clicking the preview
   * button flips a flag on the parent.  Rendering uses `marked`
   * with no sanitiser because the source is the user's own notes,
   * already trusted.  We don't run inbound third-party markdown
   * here; if that ever changes (e.g. shared notes from another
   * user) wrap `marked.parse()` in DOMPurify.
   */

  import { onDestroy, onMount } from 'svelte'
  import { EditorState, Compartment } from '@codemirror/state'
  import { EditorView, keymap, lineNumbers, highlightActiveLine } from '@codemirror/view'
  import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands'
  import {
    bracketMatching,
    foldGutter,
    indentOnInput,
    syntaxHighlighting,
    defaultHighlightStyle,
  } from '@codemirror/language'
  import { markdown } from '@codemirror/lang-markdown'
  import { marked } from 'marked'

  interface Props {
    /** Markdown source — `bind:value` from the parent.  Updates
     *  flow both directions: external programmatic changes
     *  (loading a different note) reset the editor's contents,
     *  and user typing flows back through the dispatch handler. */
    value: string
    /** Fires on every user-triggered edit.  The parent uses this
     *  to schedule the auto-save debounce; nothing else. */
    onchange?: (next: string) => void
    /** Toggle for the side-by-side rendered preview. */
    showPreview?: boolean
    /** Placeholder shown when the document is empty. */
    placeholder?: string
  }
  let {
    value = $bindable(''),
    onchange,
    showPreview = false,
    placeholder = 'Start writing — markdown is preserved on the server.',
  }: Props = $props()

  let host: HTMLDivElement | undefined = $state()
  let view: EditorView | undefined = $state()

  /** Compartments let us reconfigure individual extensions
   *  (theme, read-only, etc.) without rebuilding the whole
   *  editor state.  We don't use them yet but they're free. */
  const themeCompartment = new Compartment()

  /** Skeleton-flavoured CM6 theme.  Light + dark variants by
   *  flipping CSS custom properties so `@media (prefers-color-
   *  scheme)` and the app's `data-mode` switch both Just Work. */
  const editorTheme = EditorView.theme({
    '&': {
      height: '100%',
      fontSize: '0.875rem',
      backgroundColor: 'transparent',
    },
    '.cm-scroller': {
      fontFamily:
        'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
      lineHeight: '1.6',
      padding: '0.5rem 0',
    },
    '.cm-content': {
      padding: '0.25rem 1.25rem',
      caretColor: 'var(--color-primary-500)',
    },
    '.cm-gutters': {
      backgroundColor: 'transparent',
      color: 'var(--color-surface-400)',
      border: 'none',
    },
    '.cm-activeLine': {
      backgroundColor: 'rgba(127, 127, 127, 0.05)',
    },
    '.cm-activeLineGutter': {
      backgroundColor: 'transparent',
    },
    '&.cm-focused': {
      outline: 'none',
    },
    '&.cm-focused .cm-cursor': {
      borderLeftColor: 'var(--color-primary-500)',
    },
  })

  function buildState(initial: string): EditorState {
    return EditorState.create({
      doc: initial,
      extensions: [
        lineNumbers(),
        history(),
        foldGutter(),
        indentOnInput(),
        bracketMatching(),
        syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
        highlightActiveLine(),
        markdown(),
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        themeCompartment.of(editorTheme),
        // Dispatch every doc-changing transaction back up to the
        // parent's onchange.  Skipping selection-only changes keeps
        // the auto-save debounce from firing on cursor moves.
        EditorView.updateListener.of((u) => {
          if (!u.docChanged) return
          const next = u.state.doc.toString()
          if (next !== value) {
            value = next
            onchange?.(next)
          }
        }),
      ],
    })
  }

  onMount(() => {
    if (!host) return
    view = new EditorView({
      state: buildState(value),
      parent: host,
    })
  })

  onDestroy(() => {
    view?.destroy()
    view = undefined
  })

  // External `value` changes (loading a different note) — push
  // them into the editor only when they don't match what the
  // editor already has, otherwise we'd echo every keystroke.
  $effect(() => {
    if (!view) return
    const current = view.state.doc.toString()
    if (current !== value) {
      view.dispatch({
        changes: { from: 0, to: current.length, insert: value },
      })
    }
  })

  // Markdown → HTML for the preview pane.  Re-runs reactively on
  // value change.  `marked` returns Promise in some configs;
  // `marked.parse()` synchronously is what we want here.
  const previewHtml = $derived.by(() => {
    if (!showPreview) return ''
    try {
      return marked.parse(value || '', {
        breaks: true,
        gfm: true,
      }) as string
    } catch {
      return '<p><em>Could not render preview.</em></p>'
    }
  })
</script>

<div class="markdown-editor-shell">
  <div class="markdown-editor-source" class:has-preview={showPreview}>
    <div bind:this={host} class="markdown-editor-host" data-placeholder={placeholder}></div>
  </div>
  {#if showPreview}
    <div class="markdown-editor-preview prose prose-sm dark:prose-invert">
      {@html previewHtml}
    </div>
  {/if}
</div>

<style>
  :global(.markdown-editor-shell) {
    flex: 1;
    min-height: 0;
    display: flex;
    overflow: hidden;
  }
  :global(.markdown-editor-source) {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  :global(.markdown-editor-source.has-preview) {
    border-right: 1px solid var(--color-surface-200);
  }
  :global([data-mode='dark'] .markdown-editor-source.has-preview) {
    border-right-color: var(--color-surface-700);
  }
  :global(.markdown-editor-host) {
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }
  :global(.markdown-editor-host .cm-editor) {
    height: 100%;
  }
  :global(.markdown-editor-preview) {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 1rem 1.25rem;
    max-width: none;
  }
  :global(.markdown-editor-preview h1),
  :global(.markdown-editor-preview h2),
  :global(.markdown-editor-preview h3) {
    margin-top: 0.875rem;
    margin-bottom: 0.5rem;
  }
  :global(.markdown-editor-preview pre) {
    background: var(--color-surface-100);
    padding: 0.75rem;
    border-radius: 0.375rem;
    overflow-x: auto;
  }
  :global([data-mode='dark'] .markdown-editor-preview pre) {
    background: var(--color-surface-800);
  }
  :global(.markdown-editor-preview code) {
    font-family: ui-monospace, SFMono-Regular, monospace;
    font-size: 0.85em;
  }
</style>
