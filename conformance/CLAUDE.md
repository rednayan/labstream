# Project instructions

## The project

This project studies the Lab Streaming Layer core library and plans a Rust
implementation. The reference C++ source is in `liblsl/`, pinned at commit
`e651023c`. Read `CONFORMANCE-PLAN.md` for the current direction.

## Writing standard

Write every deliverable with the `simple-english` skill. This skill applies the
rules of ASD-STE100 Simplified Technical English.

Use pragmatic mode. Domain words stay. Do not use strict mode unless the user
asks for it.

Load the skill before you write or edit any file in the list below. Do not write
from memory of the rules.

### Where the standard applies

| Applies | Does not apply |
|---|---|
| Documents and READMEs | Chat replies and analysis |
| Architecture and design pages | Trade-off discussion |
| Published artifacts | Recommendations to the user |
| Code comments | |
| Commit messages | |
| Error strings and log messages | |

The standard does not apply to conversation. STE removes the connecting words
that make an argument readable. Analysis and recommendations stay in normal prose.

### Fixed vocabulary for this project

Rule 1.11 gives one name to one item. These choices are already used in
`liblsl-architecture.html` and `CONFORMANCE-PLAN.md`. Keep them.

| Concept | Use this word | Do not use |
|---|---|---|
| Settings and tuning values | configuration | config, settings, options |
| A fault in code or data | error | issue, problem |
| An operation that did not complete | failure | breakage |
| To confirm a condition | make sure that | check, verify, ensure, validate |
| To start a program | run | execute, invoke, launch |
| To put on screen | show | display, render, present |
| A connected inlet queue | consumer | subscriber, client |
| An inlet-side component | receiver | listener, handler |

Technical nouns from the codebase stay exact: outlet, inlet, stream, sample,
timestamp, chunk, channel, offset, wave, query, resolver.

### Rules that matter most here

1. Use only these modal verbs: can, will, must.
2. Do not use should, may, might, could, or would.
3. Write descriptive sentences of 25 words or fewer.
4. Write procedural sentences of 20 words or fewer.
5. Write one instruction per sentence.
6. If a sentence has a condition, put the condition first.
7. Use active voice.
8. Do not use semicolons. Write two sentences.
9. Do not use present perfect tense or `-ing` verbs.
10. Delete filler words. Examples: simply, just, robust, comprehensive, seamlessly.

### Untouchables

Never rewrite these, even when they break a rule:

- Code blocks, inline code, and identifiers
- File paths, CLI commands, and flags
- Quoted error messages and log lines
- Protocol strings such as `LSL:streamfeed` and `LSL:shortinfo`
- Configuration keys and header names such as `Max-Chunk-Length`

### Before you deliver

Run the self-check in the skill. Count the words in the three longest sentences.
Search the draft for `should`, `may`, `might`, contractions, and semicolons.
Correct what you find.

## Source claims

State a fact about liblsl only after you read it in `liblsl/`. Cite the source as
`file:line`. Do not repeat claims from project documentation or from other
implementations without a check against the source.
