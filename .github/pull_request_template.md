<!--
The title of this pull request becomes the subject line on main, because merges
are squashes. It has to be a Conventional Commit, for example

  fix(daemon): stop on the internal shutdown channel as well

release-please reads that line to decide the next version. A title outside the
scheme produces a wrong version or a missing changelog entry.

A breaking change needs a `!` after the type here and a `BREAKING CHANGE:`
paragraph in the body below, which also becomes the squash body.
-->

## What this changes

<!-- One paragraph. What behaviour is different afterwards, not which files moved. -->

## Why

<!--
The reason, and the alternative you rejected if there was one. Link the issue
with `Closes #123` if there is one.
-->

## How it was verified

<!--
What you actually ran, not what should pass. If you added or changed a check,
say what the counter-probe was: a deliberately invalid input has to fail for the
expected reason, otherwise the check has only ever seen good input.
-->

## Checklist

- [ ] `just check` passes
- [ ] The title is a Conventional Commit
- [ ] No AI attribution trailer in any commit, in the title or in this body
- [ ] Documentation matches the new behaviour, README included where it makes a claim
- [ ] A new embedded asset carries its licence in `THIRD-PARTY-NOTICES.md`, `licenses/` and every packaging channel
