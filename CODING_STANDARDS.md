# Coding standards

Judgement calls a reviewer checks by reading. Anything a tool can check belongs in `cargo fmt`, clippy or a test, not here.

## Error messages

- Lower case, with no full stop at the end.
- A failed step reads "cannot <verb> …": `cannot read {path}`, `cannot start the Sandbox for {path}`.
  A rejected config reads "invalid {path} …". Anything else states the fault: `` the Sandbox for {path} has no `claude` … ``.
- Where there is a Workspace, name it by its path, as "the Sandbox for {path}" (see `failed` in `src/sandbox.rs`).
- Say how to fix it when boxq knows how.

## The recreate hint

- Telling the user to `boxq rm` then `boxq run` always goes through `spec::recreate_hint`, never a copy of its wording.

## Doc comments

- Where a decision came from an issue, cite it as `jonaswre-labs/boxlite-quick#N` (not `#N`, nor the repo's old name `jonaswre/boxlite`).

## Tests

- A test's name is a sentence saying what behaviour it checks: `a_group_readable_credentials_file_is_refused`, not `test_credentials_mode`.

## Terms

- User-facing text (the README, errors, status lines) uses the terms in `GLOSSARY.md`: Sandbox, Workspace, Global Config, and so on.
- "Box" and "VM" are fine only where they mean BoxLite's VM, quote BoxLite's own messages, or mean something that isn't a Sandbox, such as a cloud VM.
- A word-match lint for these was tried and rejected: every hit left in the README was a legitimate use.
