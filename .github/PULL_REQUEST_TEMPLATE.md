## What changed, and why

<!--
Lead with the behaviour a user would notice, then the reason. If this fixes
something broken, describe the shape of the failure — that context is what
stops the bug from being reintroduced, and it belongs in the commit and at the
fix site too, not only here. See CONTRIBUTING.md, "Record the bug in the fix".
-->

Fixes #

## How it was verified

<!--
Name the check that would have failed before this change. "Tests pass" is not
that — CI already says so. Something like "added
`normalize_usage_reads_kimi_per_choice_placement`, which fails on main" is.
For provider-facing changes, say which provider and which endpoint you
actually ran against.
-->

## Checklist

- [ ] The full gate passes locally, not just the part I touched:
      `bun run lint && bun run test && bun run build`, and
      `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test` (all three with `--manifest-path src-tauri/Cargo.toml`)
- [ ] Tests live beside the code and are named as the sentence they assert
- [ ] No request or response body is logged — they carry user prompts and
      source code. Metadata only: path, status, byte length
- [ ] No provider field name is read outside `translate.rs`
- [ ] If this changes a Tauri command's backend state, the browser mock in
      `src/lib/api.ts` changed with it and is complete against its TS type
- [ ] If this is a version bump: `package.json`, `src-tauri/tauri.conf.json`
      and `src-tauri/Cargo.toml` all moved together, and `CHANGELOG.md` has
      the matching `## x.y.z` section in this same commit

<!--
First PR here? The gate is strict on purpose and every rule in CONTRIBUTING.md
exists because breaking it already cost this project a bug. If a check fails
for a reason you can't place, say so in a comment rather than forcing it —
`rustfmt` output differs between toolchain releases, and a local toolchain
older than CI's is the usual culprit.
-->
