# Lune Project Instructions

## Development Scope

- Define required capabilities and acceptance criteria before implementation, then implement and harden the foundation in small, coherent steps.
- Measure progress by correctness, robustness, and clear boundaries of each foundational component.
- Keep changes focused on the current request.
- Treat performance as a core requirement. Measure hot paths with representative workloads, validate optimizations against correctness invariants, and account for regressions across workloads.

## Language

- Write all code comments, agent instructions, and skills in English.

## Documentation

- Treat documentation as development material by default and state each document's purpose directly.
- Describe the current design, behavior, and usage accurately. Keep provisional decisions identifiable as provisional.
- Make stability and compatibility commitments only when explicitly requested by the user.

## Compatibility

- Preserve compatibility with development-stage code, APIs, data formats, configuration, and earlier designs only when the user explicitly requests it.
- Update affected callers and development artifacts directly when changing an interface.
- Add compatibility layers, legacy aliases, migration paths, or fallback behavior for earlier development implementations only when explicitly requested.

## Communication

- State decisions, changes, evidence, and limitations directly and concisely.
- Omit filler, redundant explanations, unsolicited negative statements, and contrasts such as "this is not X" or "we do not use X."
- Describe the current implementation without narrating removed content or abandoned approaches unless that history is necessary to answer the user.

## Tests

- Add tests only for meaningful behavior, important invariants, or concrete regression risks.
- Keep the test suite small and purposeful. Avoid trivial assertions, redundant cases, and tests that merely mirror the implementation.
- Choose verification appropriate to the change. Documentation-only edits need content review rather than new automated tests.

## Environment and Privacy

- Keep machine-specific environment details and personal information out of project files, documentation, examples, fixtures, snapshots, and tests.
- This includes real usernames, personal filesystem paths, hostnames, device identifiers, account details, credentials, and private service addresses.
- Use portable path resolution, runtime configuration, temporary directories, and synthetic data where needed.
- Review generated files, copied logs, and tool output before including them in the project; remove identifying and environment-specific content.
