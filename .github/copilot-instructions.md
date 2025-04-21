# Linkerd2 Proxy Copilot Instructions

## Code Generation

- Code MUST pass `cargo fmt`.
- Code MUST pass `cargo clippy --all-targets --all-features -- -D warnings`.
- Markdown MUST pass `markdownlint-cli2`.
- Prefer `?` for error propagation.
- Avoid `unwrap()` and `expect()` outside tests.
- Use `tracing` crate macros (`tracing::info!`, etc.) for structured logging.

### Comments

Comments should explain **why**, not **what**. Focus on high-level rationale and
design intent at the function or block level, rather than line-by-line
descriptions.

- Use comments to capture:
  - System-facing or interface-level concerns
  - Key invariants, preconditions, and postconditions
  - Design decisions and trade-offs
  - Cross-references to architecture or design documentation
- Avoid:
  - Line-by-line commentary explaining obvious code
  - Restating what the code already clearly expresses
- For public APIs:
  - Use `///` doc comments to describe the contract, behavior, parameters, and
    usage examples
- For internal rationale:
  - Use `//` comments sparingly to note non-obvious reasoning or edge-case
    handling
- Be neutral and factual.

### Rust File Organization

For Rust source files, enforce this layout:

1. **Non‑public imports**  
   - Declare all `use` statements for private/internal crates first.  
   - Group imports to avoid duplicates and do **not** add blank lines between
     `use` statements.

2. **Module declarations**  
   - List all `mod` declarations.

3. **Re‑exports**  
   - Follow with `pub use` statements.

4. **Type definitions**  
   - Define `struct`, `enum`, `type`, and `trait` declarations.  
   - Sort by visibility: `pub` first, then `pub(crate)`, then private.
   - Public types should be documented with `///` comments.

5. **Impl blocks**  
   - Implement methods in the same order as types above.  
   - Precede each type’s `impl` block with a header comment: `// === <TypeName> ===`

6. **Tests**  
   - End with a `tests` module guarded by `#[cfg(test)]`.
   - If the in‑file test module exceeds 100 lines, move it to
     `tests/<filename>.rs` as a child integration‑test module.

## Test Generation

- Async tests MUST use `tokio::test`.
- Synchronous tests use `#[test]`.
- Include at least one failing‑edge‑case test per public function.
- Use `tracing::info!` for logging in tests, usually in place of comments.

## Code Review

### Rust

- Point out any `unsafe` blocks and justify their safety.
- Flag functions >50 LOC for refactor suggestions.
- Highlight missing docs on public items.

### Markdown

- Use `markdownlint-cli2` to check for linting errors.
- Lines SHOULD be wrapped at 80 characters.
- Fenced code blocks MUST include a language identifier.

### Copilot Instructions

- Start each instruction with an imperative, present‑tense verb.
- Keep each instruction under 120 characters.
- Provide one directive per instruction; avoid combining multiple ideas.
- Use "MUST" and "SHOULD" sparingly to emphasize critical rules.
- Avoid semicolons and complex punctuation within bullets.
- Do not reference external links, documents, or specific coding standards.

## Commit Messages

Commits follow the Conventional Commits specification:

### Subject

Subjects are in the form: `<type>[optional scope]: <description>`

- **Type**: feat, fix, docs, refactor, test, chore, ci, build, perf, revert
  (others by agreement)
- **Scope**: optional, lowercase; may include `/` to denote sub‑modules (e.g.
  `http/detect`)
- **Description**: imperative mood, present tense, no trailing period
- MUST be less than 72 characters
- Omit needless words!

### Body

Non-trivial commits SHOULD include a body summarizing the change.

- Explain *why* the change was needed.  
- Describe *what* was done at a high level.
- Use present-tense narration.
- Use complete sentences, paragraphs, and punctuation.
- Preceded by a blank line.
- Wrapped at 80 characters.
- Omit needless words!

### Breaking changes

If the change introduces a backwards-incompatible change, it MUST be marked as
such.

- Indicated by `!` after the type/scope (e.g. `feat(inbound)!: …`)  
- Optionally including a `BREAKING CHANGE:` section in the footer explaining the
  change in behavior.

### Examples

```text
feat(auth): add JWT refresh endpoint

There is currently no way to refresh a JWT token.

This exposes a new `/refresh` route that returns a refreshed token.
```

```text
feat(api)!: remove deprecated v1 routes

The `/v1/*` endpoints have been deprecated for a long time and are no
longer called by clients.

This change removes the `/v1/*` endpoints and all associated code,
including integration tests and documentation.

BREAKING CHANGE: The previously-deprecated `/v1/*` endpoints were removed.
```

## Pull Requests

- The subject line MUST be in the conventional commit format.
- Auto‑generate a PR body summarizing the problem, solution, and verification steps.
- List breaking changes under a separate **Breaking Changes** heading.
