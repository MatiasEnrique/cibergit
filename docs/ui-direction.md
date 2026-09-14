# Interface direction

User instruction, 2026-09-12: match the Codex app’s aesthetics, use an acrylic sidebar, and use IBM Plex Sans.

Use native macOS visual effects for the sidebar, restrained neutral surfaces, a quiet divider, compact navigation rows and softly rounded selection states. Keep the main review canvas opaque and readable. Preserve native titlebar/traffic-light spacing, support both appearances and avoid oversized headings, decorative gradients or dashboard-style cards.

Use bundled IBM Plex Sans Regular/Medium/Semibold for interface text, controls, sidebar and PR titles. Keep code aligned in a monospace font. Font files and the SIL Open Font License are in `assets/fonts`; `manifest.json` records the exact IBM upstream revision, sizes and SHA-256 checksums. Load the fonts through GPUI’s text system, without installing them globally. Include the font license in app packaging.

On the pinned GPUI macOS implementation, `WindowBackgroundAppearance::Blurred` creates a native `NSVisualEffectView` using its Selection material. Leave the root/sidebar transparent or lightly tinted so this effect is visible; apply an opaque background only to the content region. Do not emulate the requested acrylic effect with a flat gray rectangle. In-process scene captures cannot prove desktop backdrop blending; record this visual verification limit and validate the actual window when native capture/manual evidence is available.

User instruction, 2026-09-14: follow Linear's layout and tabs, and remove the chrome separator rules.

The open-PR strip, the PR title block, the PR tab row, the sidebar account footer and the status bar are separated by their own fill and by spacing, not by rules; the panel splitter is a plain drag band. Page tabs, and the open-PR chips above them, are small rounded chips: the selected one carries a quiet fill, the rest are muted text on the surface, and a count beside a tab label is rendered in the faint colour rather than repeating the label's weight. Keep rules for structure inside content, such as table headers and the diff's own columns.

The chrome above a diff is three rows: title with its actions, branches with the local-edit and diff-mode controls, and the tab row, with the Compare bar as a fourth only where it applies. Information that a neighbouring row already carries does not get a row of its own: the published-revision label, the comparison line where the Compare bar is on screen, and a settled commit count are all dropped, and exact SHAs sit behind the revision summary. A warning about the comparison keeps its own line, always.

These choices refine the existing agreed Codex-inspired/native-macOS requirement. They do not narrow any functional V1 requirement.
