---
name: rfc
description: Workflow for creating, updating, or superseding RFCs in Luma. Use when the task involves opening a new RFC, updating RFC status, marking an RFC as superseded or implemented, or deciding whether a change needs an RFC at all.
---

# RFC Workflow in Luma

## Source of truth

`docs/rfcs/README.md` — contains the full RFC index with current status.
`docs/rfcs/0000-template.md` — canonical template for new RFCs.

Always read these two files before creating or updating any RFC. If they do not exist, create `docs/rfcs/` and bootstrap both files before proceeding.

## When to open an RFC

RFC is required for:
- cross-cutting architecture changes (provider, core, config, wire format);
- changes to prompt assembly layers or instruction loading model;
- breaking changes to user-facing CLI or config;
- changes to memory taxonomy or source-of-truth hierarchy.

RFC is NOT required for:
- prompt wording patches;
- tool description tweaks;
- heuristic tuning;
- bugfix or local refactor;
- adding tests or docs.

Only open an RFC when there is local evidence or session evidence. Do not open an RFC for brainstorming without evidence.

## Creating a new RFC

1. Read `docs/rfcs/README.md` to find the next available number.
2. Copy `docs/rfcs/0000-template.md` to `docs/rfcs/NNNN-short-name.md`.
3. Fill in the metadata header. Set `Status: Draft`.
4. Fill in Summary, Motivation, and Reference-level explanation.
5. Add the new RFC to the table in `docs/rfcs/README.md`.

## Updating an RFC

- Update the `Updated` date in the metadata header.
- Update `Status` when the state changes.
- Update `Implementation status` section when code ships.

## Valid status values

- `Draft` — open for discussion, not yet accepted.
- `Accepted` — approved, implementation in progress.
- `Implemented` — fully shipped.
- `Superseded` — replaced by another RFC or by a direct code/prompt patch. Set `Superseded by` field.
- `Withdrawn` — no longer pursued. Keep the file, record the reason.

## When to supersede

Supersede an RFC when:
- a newer RFC covers the same ground more accurately;
- the core intent was implemented directly in code or prompt without needing the full RFC scope.

When superseding, set `Status: Superseded` and fill in `Superseded by` with the RFC number or a short description of what replaced it.

## Patch nearest layer first

Before opening an RFC, check whether the change can be made as a direct patch:
- tool description → edit the tool's description string in `src/tool/`;
- behavior guidance → edit `src/config/prompt/smart.md` or the relevant mode file;
- tool usage policy → edit `src/config/prompt/tools_native.md` or `tools_patch.md`;
- project conventions → edit `AGENTS.md`.

Only escalate to RFC if the change is cross-cutting and cannot be contained in one layer.
