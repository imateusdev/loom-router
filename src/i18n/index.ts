// Tiny i18n entry point. Today it always returns English; later this can
// load a locale file by user preference without touching components.

import en, { type Strings } from './en'

export function useStrings(): Strings {
  return en
}

export function t(strings: Strings) {
  return strings
}
