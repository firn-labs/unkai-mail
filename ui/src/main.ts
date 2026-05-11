import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'
import StandaloneMail from './lib/StandaloneMail.svelte'
import StandaloneMailFile from './lib/StandaloneMailFile.svelte'
import StandaloneCompose from './lib/StandaloneCompose.svelte'
import StandaloneReminder from './lib/StandaloneReminder.svelte'

// Same Vite bundle, five entry routes selected via the URL query:
//
//   ?view=mail&account=…&folder=…&uid=…  → standalone mail reader (#104)
//   ?view=mailfile&path=…                → view-only .eml from disk (#254)
//   ?view=compose&key=…                  → standalone compose window (#110)
//   ?view=reminder&key=…                 → calendar reminder popup (#203)
//   anything else                        → the full 3-pane app
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
} else {
  app = mount(App, { target })
}

export default app
