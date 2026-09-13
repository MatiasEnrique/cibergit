# Interface direction

User instruction, 2026-09-12: match the Codex app’s aesthetics, use an acrylic sidebar, and use IBM Plex Sans.

Use native macOS visual effects for the sidebar, restrained neutral surfaces, a quiet divider, compact navigation rows and softly rounded selection states. Keep the main review canvas opaque and readable. Preserve native titlebar/traffic-light spacing, support both appearances and avoid oversized headings, decorative gradients or dashboard-style cards.

Use bundled IBM Plex Sans Regular/Medium/Semibold for interface text, controls, sidebar and PR titles. Keep code aligned in a monospace font. Font files and the SIL Open Font License are in `assets/fonts`; `manifest.json` records the exact IBM upstream revision, sizes and SHA-256 checksums. Load the fonts through GPUI’s text system, without installing them globally. Include the font license in app packaging.

On the pinned GPUI macOS implementation, `WindowBackgroundAppearance::Blurred` creates a native `NSVisualEffectView` using Sidebar material. Leave the root/sidebar transparent or lightly tinted so this effect is visible; apply an opaque background only to the content region. Do not emulate the requested acrylic effect with a flat gray rectangle. In-process scene captures cannot prove desktop backdrop blending; record this visual verification limit and validate the actual window when native capture/manual evidence is available.

These choices refine the existing agreed Codex-inspired/native-macOS requirement. They do not narrow any functional V1 requirement.
