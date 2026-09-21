# Interface direction

User instruction, 2026-09-12: match the Codex app’s aesthetics, use an acrylic sidebar, and use IBM Plex Sans.

Use native macOS visual effects for the sidebar, restrained neutral surfaces, a quiet divider, compact navigation rows and softly rounded selection states. Keep the main review canvas opaque and readable. Preserve native titlebar/traffic-light spacing, support both appearances and avoid oversized headings, decorative gradients or dashboard-style cards.

Pair two bundled faces: IBM Plex Sans (Regular/Medium/Semibold/Bold) for headings — display, title, subtitle and kicker — and DM Sans (Regular/Medium/Bold) for everything read at 11-13px: body, labels, controls, captions and the sidebar. Plex gives the app its voice at display sizes and turns cold and cramped below them; DM Sans stays open there. Keep code aligned in a monospace font. Font files and both SIL Open Font Licenses are in `assets/fonts`; `manifest.json` records the exact upstream revisions, sizes and SHA-256 checksums. DM Sans is published upstream only as a variable font, which the font-kit matcher cannot instance, so its static weights are cut from the pinned variable file by `scripts/build-text-font.py` and the manifest records that derivation. Load the fonts through GPUI’s text system, without installing them globally. Include the font license in app packaging.

The sidebar's translucency is a material view the application installs itself (`src/glass.rs`), placed beneath GPUI's Metal layer and resized with the panel. On macOS 26 that view is an `NSGlassEffectView`, looked up by class name because the pinned AppKit bindings predate it. Which material is installed is a saved personal preference, offered on the Settings page as Clear glass, Tinted glass, Frosted or Solid. Clear is the default: Tinted is the same `NSGlassEffectView` with the Regular style, whose legibility scrim is most of what a sidebar over a quiet desktop ends up showing, while Clear keeps the refraction and takes its colour from whatever is actually behind the window. The corner radius is zero, since the window's own corners already clip the column. Frosted, and either glass choice on a system older than 26, is an `NSVisualEffectView` using the Sidebar material with `BehindWindow` blending. Solid installs nothing and the sidebar paints its opaque panel fill. The window is `WindowBackgroundAppearance::Transparent` and the sidebar column paints no fill of its own, so the material shows through; the opaque canvas still covers everything from the splitter rightwards. GPUI's own `Blurred` appearance is not used, because it spreads one Selection-material view across the whole window and then strips that view's desktop tint and saturation, which blurs without reading as glass. Do not emulate the material with a flat gray rectangle either. The window server composites the material behind the Metal layer, so `render_to_image` can never contain it and no scene capture can say which view was installed: the smoke harness keeps the opaque fallback fill so its evidence stays readable, `CIBERGIT_GLASS_REPORT=1` names the installed material on stderr, and the glass itself is confirmed on the actual window.

User instruction, 2026-09-14: follow Linear's layout and tabs, and remove the chrome separator rules.

The open-PR strip, the PR title block, the PR tab row, the sidebar account footer and the status bar are separated by their own fill and by spacing, not by rules; the panel splitter is a plain drag band. Page tabs, and the open-PR chips above them, are small rounded chips: the selected one carries a quiet fill, the rest are muted text on the surface, and a count beside a tab label is rendered in the faint colour rather than repeating the label's weight. Keep rules for structure inside content, such as table headers and the diff's own columns.

The chrome above a diff is three rows: title with its actions, branches with the local-edit and diff-mode controls, and the tab row, with the Compare bar as a fourth only where it applies. Information that a neighbouring row already carries does not get a row of its own: the published-revision label, the comparison line where the Compare bar is on screen, and a settled commit count are all dropped, and exact SHAs sit behind the revision summary. A warning about the comparison is carried by the revision summary in the Compare bar, which already prints the viewed head beside the published one: when they differ the row takes the warning tint and its accessibility label carries the full sentence. A paragraph under the bar restating that cost a line of the diff on every truncated comparison. Instructional prompts while picking a commit or a range are gone entirely — the chips say which mode is active and the commit list is open beside them. This supersedes the earlier "a warning about the comparison keeps its own line, always" under the 2026-09-16 instruction to remove those lines.

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

Source text in a diff keeps the default foreground on both sides of a change, exactly as GitHub renders it; only the marker and the line and gutter tints carry the colour. Recolouring the code itself green or red reads as a different language, which is what a diff is not. Branch names are monospace chips in the accent tint, and pull request state is a filled pill. A name longer than the chip can show is cut by the chip rather than by the layout, and hovering it shows the whole name: the pair sits beside the PR title in a row that does not shrink, so an uncut long name ate the title — the branch is the predictable part, the title is the part being read. Menlo is monospace, so the chip's character count is its width and it knows exactly when it dropped something; the affordance appears only where it did.

Editor syntax highlighting in local editing remains this app's own theme; Primer's PrettyLights syntax tokens are not adopted here.

These choices refine the existing agreed Codex-inspired/native-macOS requirement. They do not narrow any functional V1 requirement.

User instruction, 2026-09-14: give the Conversation tab's comments GitHub's display — the missing icon, the unrendered prose and the raw `Z` on the date.

A comment is the author's puck beside a byline naming who acted, in what way — "commented", "approved these changes" — and how long ago, with an "Author" chip where GitHub shows one, over the rendered prose. The puck is drawn locally from the login: its first letter on one of eight fills the login itself picks, so a participant keeps one colour across the workspace.

User instruction, 2026-09-16: make paddings and spacings consistent across the app, standardize the components, and avoid card containers as much as possible.

This supersedes the bordered card and tinted header strip this paragraph originally specified. A conversation is a column of twenty comments, and twenty boxes stacked down a page read as a list of containers rather than a thread: the reader parses an edge before every remark. The puck already marks where a comment starts and the page gap already separates them, so the box carried nothing the layout did not carry first.

The same rule now holds everywhere. A fill plus a radius plus a box inset around content is not used to group content; blocks separate by space and announce themselves with a kicker. Fills, borders and radii stay on things you can point at — controls, chips, badges, selection and hover states, fields, pucks — and on the surfaces that genuinely float: the window frame, dialogs, popovers, hover cards and menus. Bands that are structure inside content, such as the split diff's OLD/NEW column headers, keep theirs.

Spacing reaches the screen through a named vocabulary rather than a token chosen per element, and Tailwind's spacing helpers are gone from the source. The full rules and the builder table are in [dense desktop UI guidelines](ui-density.md). The four PR tabs are one page reached four ways and lay out on one gutter; a regression test pins that.

User instruction, 2026-09-16: show each commenter's real GitHub picture on that puck.

The drawn puck is now the fallback rather than the whole of it. The details read carries an avatar URL per participant — taken from the payload, because GitHub spells a bot two ways, `coderabbitai` in GraphQL and `coderabbitai[bot]` in REST, and neither spelling addresses the picture — and the fetched image is laid over the letter, which stays behind it while the bytes arrive and for good if they never do. This is the one remote media this app resolves, and it is narrow by construction: only GitHub's own avatar host, only a URL the provider itself returned, never one typed into a comment body. An `<img>` in prose still leaves its omission note. Fetching is anonymous — an avatar is public and has no business carrying the selected account's token — once per login per run, and saved beside the rest of the workspace state so the next launch paints from disk.

Times are read as an age. "5 minutes ago" up to a month, then the date; the exact UTC instant stays on the card's own metadata line and in the header's accessibility label, so nothing is lost by not printing `2026-09-14T17:34:41Z`.

Prose is sanitized, not escaped flat. A leading `>` keeps its blockquote, so quoted prose and the alert callouts bots write with it (`> [!WARNING]`, rendered as its bold label) read as GitHub writes them. HTML that the rich text renders as structure passes through — `<details>`, `<summary>` and the ordinary inline and table tags — and a `<summary>` is bolded, because the pinned rich text draws no disclosure control and the summary still has to read as the section title it is. Tags whose purpose is to fetch media are dropped before the renderer ever sees them, an `<img>` leaving the same omission note a Markdown image leaves. Anything unrecognised stays escaped and renders as the literal text it is.

Known gap: `<details>` sections render expanded. GitHub collapses them, and a long bot review is a wall of prose until the rich text can carry a real disclosure control.
