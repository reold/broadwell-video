# How to work in this repository

These are not style preferences. Each one is here because ignoring it cost real
time in this project, and most of the cost was mine.

## Reproduce before you repair

Write the failing case first, as something runnable, and watch it fail.

The export walked the wrong clips for four attempts. Four theories, four fixes,
none of them the bug. What ended it was `HWA_TEST_CLIPS="10000-15000,0-5000"`
exporting 152 frames where the ordered version exported 301 — a two-line
reproducer that took minutes and made the cause obvious. Reasoning about a bug
you have not reproduced is guessing with extra steps.

`preview/src/bin/editor_export.rs` exists for this. It drives the real renderer,
takes a timeline specification on an environment variable, and measures. Every
bug of consequence in this project was found or confirmed through it.

## Prove the fix ran, do not report that it did

Checking is not optional and it is not a summary. Run it.

- `./scripts/verify.sh` — tests, type checks, builds. One command, one verdict.
- A test that passes is not a feature that works. `svelte-check` proves a file
  compiles, not that it does what was intended: a `return` in the middle of an
  `onMount` skipped every startup call, left the timeline empty and the output
  path blank, and passed every check in the repository.
- After editing a file, confirm the edit is *in* the file. An anchored
  replacement that asserts and then fails to write looks exactly like success
  when its output is skimmed. It has happened twice here, and one of those fixes
  was reported in a commit message while the code still had the bug.

Prefer the file-editing tool to chained shell heredocs for exactly this reason.

## Measure, and pair the number with a content check

A frame count that looks perfect hid 600 of 900 frames being blank for an entire
session. The count was never the problem; nobody had looked at the pixels.

Every performance number in the README is paired with a second, independent
check. Keep that up. `mean luma` catches a grade that is not applied; the frame
count does not.

## When a fix is a guess, say so

The scrub crash was a guess about a stale frame. The guard was correct and the
guess was wrong, and the difference was only visible because the reproducer was
still failing afterwards. Being wrong is cheap. Being wrong and *reported as
fixed* is what costs a user a session.

## What this repository will not do

- No emoji or symbol glyphs in the UI. Icons are 16x16 paths in `currentColor`,
  because a font glyph is an emoji on one system and a missing box on another.
- No browser furniture: no OS scrollbars, no focus ring on pointer focus, no
  text caret over things that are not text, no context menu.
- Nothing claims to work without a measurement or a test behind it.
