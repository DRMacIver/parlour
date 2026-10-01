# UI mockups

Exported from the design canvas (https://claude.ai/artifact/PPCuLhSznBYVmHTLsyTVH3).
Each `.dc.html` file is one artboard: HTML with inline styles inside an
`<x-dc>` element, and a `renderVals()` script that supplies the sample data.

- **Chosen visual direction:** `ClassicBroadsheet.dc.html` ("3b · Classic × Broadsheet"); see DESIGN.md §5.
  The real stylesheet is `server/static/parlour.css`, and all styling goes through its CSS variables.
- **Screen structure** (what each page contains and how features are laid out) comes from the earlier
  screen mockups: `Main`, `Category`, `Thread`, `Fork`, `Messages`, `Revisions`, `InviteTree`, `ModQueue`.
  These use an older visual style. Follow their structure, not their look.
- `Look*.dc.html`, `ClassicRefreshed.dc.html` and `ClassicSwiss.dc.html` are alternative directions kept for later theme variants.
