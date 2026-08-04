// English (source locale). All UI strings live here so adding a new
// locale later is just creating `pt-BR.ts` with the same keys.

const en = {
  app: {
    name: 'LoomRouter',
    tagline: "Weave any model into your coding agent's picker.",
  },
  nav: {
    providers: 'Providers',
    server: 'Server',
    codex: 'Codex Integration',
  },
  providers: {
    title: 'Providers',
    subtitle: 'Connect API providers and pick which models appear in your agent.',
    add: 'Add provider',
    addCustom: 'Custom endpoint',
    name: 'Name',
    baseUrl: 'Base URL',
    apiKey: 'API key',
    apiKeySet: 'Key stored locally',
    save: 'Save',
    cancel: 'Cancel',
    delete: 'Delete',
    edit: 'Edit',
    discover: 'Fetch models',
    discovering: 'Fetching…',
    validating: 'Validating key…',
    validationFailed: 'Key validation failed',
    saveAnyway: 'Save anyway',
    discoverFailed: 'Could not fetch models',
    noProviders: 'No providers yet. Add one to get started.',
    noModels: 'No models yet. Fetch the live catalog to pick models.',
    enabledModels: '{{count}} models enabled',
    keyRequired: 'Enter an API key first.',
    protocol: 'Protocol',
  },
  server: {
    title: 'Server',
    subtitle: 'The local proxy your agent talks to.',
    start: 'Start server',
    stop: 'Stop server',
    running: 'Running',
    stopped: 'Stopped',
    listeningOn: 'Listening on',
    port: 'Port',
  },
  codex: {
    title: 'Codex Integration',
    subtitle: 'Make external models appear in the Codex model picker, next to native GPT models.',
    apply: 'Apply integration',
    remove: 'Remove integration',
    applied: 'Integration active',
    notApplied: 'Integration not applied',
    mergedCatalog: 'Merged catalog',
    modelsInPicker: '{{count}} external models in the picker',
    codexHome: 'Codex home',
    cliAvailable: 'Codex CLI detected',
    nativeCatalog: 'Native catalog captured',
    restartHint: 'Fully quit and reopen Codex after applying — Codex only loads the catalog at startup.',
  },
  common: {
    loading: 'Loading…',
    error: 'Something went wrong',
    on: 'On',
    off: 'Off',
  },
} as const

export type Strings = typeof en
export default en
