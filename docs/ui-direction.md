# Interface direction

User instruction, 2026-09-12: match the Codex app’s aesthetics, use an acrylic sidebar, and use IBM Plex Sans.

Use native macOS visual effects for the sidebar, restrained neutral surfaces, a quiet divider, compact navigation rows and softly rounded selection states. Keep the main review canvas opaque and readable. Preserve native titlebar/traffic-light spacing, support both appearances and avoid oversized headings, decorative gradients or dashboard-style cards.

Use bundled IBM Plex Sans Regular/Medium/Semibold for interface text, controls, sidebar and PR titles. Keep code aligned in a monospace font. Font files and the SIL Open Font License are in `assets/fonts`; `manifest.json` records the exact IBM upstream revision, sizes and SHA-256 checksums. Load the fonts through GPUI’s text system, without installing them globally. Include the font license in app packaging.

On the pinned GPUI macOS implementation, `WindowBackgroundAppearance::Blurred` creates a native `NSVisualEffectView` using its Selection material. Leave the root/sidebar transparent or lightly tinted so this effect is visible; apply an opaque background only to the content region. Do not emulate the requested acrylic effect with a flat gray rectangle. In-process scene captures cannot prove desktop backdrop blending; record this visual verification limit and validate the actual window when native capture/manual evidence is available.

User instruction, 2026-09-14: follow Linear's layout and tabs, and remove the chrome separator rules.

The open-PR strip, the PR title block, the PR tab row, the sidebar account footer and the status bar are separated by their own fill and by spacing, not by rules; the panel splitter is a plain drag band. Page tabs, and the open-PR chips above them, are small rounded chips: the selected one carries a quiet fill, the rest are muted text on the surface, and a count beside a tab label is rendered in the faint colour rather than repeating the label's weight. Keep rules for structure inside content, such as table headers and the diff's own columns.

The chrome above a diff is three rows: title with its actions, branches with the local-edit and diff-mode controls, and the tab row, with the Compare bar as a fourth only where it applies. Information that a neighbouring row already carries does not get a row of its own: the published-revision label, the comparison line where the Compare bar is on screen, and a settled commit count are all dropped, and exact SHAs sit behind the revision summary. A warning about the comparison keeps its own line, always.

Freshness and completeness notices are the exception: a stale or partial read reports itself through
one warning icon at the right of the tab row, whose hover card holds the full text and whose
accessibility label repeats it. Those notices were three stacked bands of amber prose above the
content, and they cost a reader two lines of diff on every screen that carried them.

User instruction, 2026-09-14: match GitHub's colours and diff highlighting so the workspace reads the way the pull request page it came from does.

Surfaces keep this app's own neutrals; every colour that carries meaning comes from GitHub's Primer functional tokens, light and dark. The mapping is:

| Use | Primer token | Light | Dark |
| --- | --- | --- | --- |
| Links, actions, selection accent | `fgColor-accent` | `#0969da` | `#4493f8` |
| Branch chips, hunk header fill | `bgColor-accent-muted` | `#ddf4ff` | `#388bfd1a` |
| Additions, `+n` | `fgColor-success` | `#1a7f37` | `#3fb950` |
| Deletions, `-n` | `fgColor-danger` | `#d1242f` | `#f85149` |
| Warnings and notices | `fgColor-attention` / `bgColor-attention-muted` | `#9a6700` / `#fff8c5` | `#d29922` / `#bb800926` |
| Added line / its gutter | `diffBlob-additionLine-bgColor` / `-additionNum-bgColor` | `#dafbe1` / `#aceebb` | `#2ea04326` / `#3fb9504d` |
| Deleted line / its gutter | `diffBlob-deletionLine-bgColor` / `-deletionNum-bgColor` | `#ffebe9` / `#ffcecb` | `#f851491a` / `#f851494d` |
| A side with no line | `diffBlob-emptyLine-bgColor` | `#f6f8fa` | `#151b23` |
| State pill fills: open, closed, merged, draft | `bgColor-{success,danger,done,neutral}-emphasis` | `#1f883d`, `#cf222e`, `#8250df`, `#59636e` | `#238636`, `#da3633`, `#8957e5`, `#656c76` |

Source text in a diff keeps the default foreground on both sides of a change, exactly as GitHub renders it; only the marker and the line and gutter tints carry the colour. Recolouring the code itself green or red reads as a different language, which is what a diff is not. Branch names are monospace chips in the accent tint, and pull request state is a filled pill.

Editor syntax highlighting in local editing remains this app's own theme; Primer's PrettyLights syntax tokens are not adopted here.

These choices refine the existing agreed Codex-inspired/native-macOS requirement. They do not narrow any functional V1 requirement.
