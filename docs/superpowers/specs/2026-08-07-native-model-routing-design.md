# Native model routing precedence

## Goal

Prevent configured external providers from hijacking bare native OpenAI model
slugs such as `gpt-5.5`.

## Rule

- A qualified `provider/model` slug always routes to that enabled provider.
- A bare slug routes to an external provider only when `native_slug_mode` is
  enabled.
- Otherwise, a bare slug is forwarded unchanged to ChatGPT/OpenAI.

`native_slug_mode` remains the explicit opt-in for installations that replace
the native catalog with external bare slugs. In the normal catalog mode,
external entries already use qualified slugs, so the rule has no UI migration.

## Implementation

Change the shared proxy resolver so its bare-model lookup is conditional on
`AppConfig.native_slug_mode`. Keep qualified-provider resolution and side-call
fallback behavior unchanged.

## Tests

- An external provider declaring `gpt-5.5` does not capture bare `gpt-5.5` in
  normal mode.
- The qualified `provider/gpt-5.5` still routes in normal mode.
- Bare external slugs still route in native-slug mode.
