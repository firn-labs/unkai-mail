import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'
import StandaloneMail from './lib/StandaloneMail.svelte'
import StandaloneMailFile from './lib/StandaloneMailFile.svelte'
import StandaloneCompose from './lib/StandaloneCompose.svelte'
import StandaloneReminder from './lib/StandaloneReminder.svelte'
import StandaloneEventEditor from './lib/StandaloneEventEditor.svelte'
import StandaloneSignatureEditor from './lib/StandaloneSignatureEditor.svelte'

// Same Vite bundle, seven entry routes selected via the URL query:
//
//   ?view=mail&account=…&folder=…&uid=…  → standalone mail reader (#104)
//   ?view=mailfile&path=…                → view-only .eml from disk (#254)
//   ?view=compose&key=…                  → standalone compose window (#110)
//   ?view=reminder&key=…                 → calendar reminder popup (#203)
//   ?view=event-editor&key=…             → standalone event editor (#304)
//   ?view=signature-editor&key=…         → standalone signature editor (#314)
//   anything else                        → the full 3-pane app
//
// Two cross-cutting params ride alongside every route (#535, both
// parsed in `lib/windowContext.ts`): `profile=<id>` on `profile-*`
// windows (the static main window has none — App asks the backend
// for its resolved startup profile), and `parent=<label>` on
// popouts (where their handoff events are targeted).
//
// Reusing one bundle keeps the build simple and gives every route
// access to the full component library (MailView, Compose) without
// duplication.
const params = new URLSearchParams(window.location.search)
const target = document.getElementById('app')!
const view = params.get('view')

let app
if (view === 'mail') {
  const accountId = params.get('account') ?? ''
  const folder = params.get('folder') ?? 'INBOX'
  const uid = Number.parseInt(params.get('uid') ?? '0', 10)
  app = mount(StandaloneMail, {
    target,
    props: { accountId, folder, uid },
  })
} else if (view === 'mailfile') {
  app = mount(StandaloneMailFile, {
    target,
    props: { path: params.get('path') ?? '' },
  })
} else if (view === 'compose') {
  app = mount(StandaloneCompose, {
    target,
    props: { popoutKey: params.get('key') ?? '' },
  })
} else if (view === 'reminder') {
  app = mount(StandaloneReminder, {
    target,
    props: { popoutKey: params.get('key') ?? '' },
  })
} else if (view === 'event-editor') {
  app = mount(StandaloneEventEditor, {
    target,
    props: { popoutKey: params.get('key') ?? '' },
  })
} else if (view === 'signature-editor') {
  app = mount(StandaloneSignatureEditor, {
    target,
    props: { popoutKey: params.get('key') ?? '' },
  })
} else {
  app = mount(App, {
    target,
    props: { initialProfileId: params.get('profile') },
  })
}

export default app
