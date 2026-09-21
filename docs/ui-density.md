# Dense desktop UI guidelines

Cibergit's sizing and spacing follow this scale. Reuse `cibergit::ui` and the shared component helpers when adding or updating UI; do not introduce independent Tailwind text sizes or control geometry. IBM Plex Sans is the heading face and DM Sans the reading face; Menlo remains the code face. Roles name their own family, so a heading is Plex wherever it appears and the reading roles inherit — which is what lets a diff surface set Menlo once and keep it. Values are logical pixels, independent of display scale.

| Text role | Size | Line height | Weight | Face |
| --- | ---: | ---: | --- | --- |
| Display | 24 | 28 | Bold | Plex |
| Title | 15 | 20 | Bold | Plex |
| Subtitle | 13 | 18 | Bold | Plex |
| Body | 12 | 18 | Medium | DM Sans |
| Label / table header | 12 | 16 | Bold | DM Sans |
| Caption / help | 11 | 16 | Medium | DM Sans |
| Kicker | 11 | 16 | Bold; uppercase; 0.08em tracking | Plex |

The ladder runs above the stock weights, because 11-13px text over translucent
chrome goes thin and grey at Regular. Running text stays at Medium — paragraphs
of Semibold read as one grey slab — and the step up goes to the roles meant to
carry it. Take weights from
`ui::WEIGHT_TEXT` / `WEIGHT_EMPHASIS` / `WEIGHT_STRONG` rather than naming a
`FontWeight` at the call site. Bold is the ceiling: IBM Plex Sans ships no
heavier face, so emphasis and strong share a weight and the kicker's tracking
is what separates them.

`Density::ui_text` sets all three metrics together. Use `ui::kicker` for tracked uppercase text; GPUI lacks a native letter-spacing style, so it renders individually spaced glyphs under a single accessible label. User-authored Markdown retains heading hierarchy using the display/title/subtitle scale; source code keeps its monospace face.

| Spacing relationship | Pixels |
| --- | ---: |
| Icon to label / toolbar neighbors | 4 |
| Field label to control | 6 |
| Related groups | 8 |
| Above and below action rows | 8 each |
| Footer buttons | 8 |
| Related field columns | 12 |
| Panel / window horizontal gutter | 20 |
| First content under title bar | 8 |
| Page sections | 24 |

## Reaching the scale

`src/ui.rs` holds the numbers and the palette-free builders; `src/app/layout.rs`
wraps the ones that need the review window's `Palette`. The split is not
cosmetic: the standalone `LocalWorkspace` harness mounts its module outside the
application crate root, so shared shapes have to be reachable from the library.
Screens name a relationship and take the spacing that comes with it rather than
choosing a token per element — which is what let the same relationship come out
4px in one place and 8px two lines below.

| Builder | Gap | What it separates |
| --- | ---: | --- |
| `ui::page` | 24 | sections on a page; also owns the 20px gutter around one |
| `layout::section(title, colors)` | 8 | a titled region's own blocks and lines |
| `ui::block` | 8 | items belonging to one thought |
| `ui::lines` | 4 | consecutive caption lines that are one paragraph |
| `ui::row` | 4 | items on a single line |
| `layout::field(label, colors)` | 6 | a label from the control it names |
| `ui::notice(text, tint)` | — | a full-bleed strip across a pane |
| `ui::rule(color)` | — | a hairline, for structure inside content only |

Chips — page tabs, the Compare bar, any segmented choice — are one component:

| Builder | Geometry |
| --- | --- |
| `ui::chip_row()` | 36 high; `GAP_ICON` pitch; hangs `CHIP_BLEED` (9) left of its container's content edge |
| `layout::chip(id, label, count, selected, colors)` | a 24px painted chip inside a full-row Button; count in the faint colour |

A chip is a `Button`, not a styled `div`, which is what makes it a real tab stop
with a focus ring that answers Enter and Space. The Compare bar's four modes
were plain divs and had none of that while the page tabs directly above them
did; they also sat at a different inset, so their labels landed 8px right of the
tab labels. `CHIP_BLEED` is the one negative margin in the app and it is an
optical correction, not spacing: a row hangs back by the painted chip's inset
plus its ring border so a *resting label* starts on the page gutter, which is
the edge the eye actually reads.
The Compare bar is chrome under the tab row, not page content, on every tab
that shows it. It used to reach the Commits tab through the inspector page
instead, which put that page's 20px top padding between it and the tabs — so
the identical bar sat 20px lower on Commits than on Files changed.
`every_pr_tab_starts_its_content_on_the_same_gutter` pins the two rows to one
gutter, one pitch and one height, and pins the step below the tab row by
measuring Commits and Files changed against **each other**. An earlier version
of that assertion compared one tab against a constant, which looked deliberate
and let this defect through: a geometry test that only ever measures one side
cannot see a disagreement.

Text fields have one definition each, and both are built from `Density::control`
so a caret starts on the same vertical line in either:

| Builder | Geometry |
| --- | --- |
| `layout::text_field(colors)` | one line; control height, inset, radius, **vertically centred** |
| `layout::text_area(lines, colors)` | `lines` Body lines plus the cell inset and hairline; top-aligned |

A multi-line field is sized by how many lines of body text it should show, not by
a pixel height chosen at the call site. Before this there were six hand-built
single-line boxes and eleven hand-built multi-line ones: every single-line box
fixed a 28px height and then centred nothing, so the text hung from the top edge
with ten pixels of dead space under it, and two of them had no horizontal inset
and ran their text into the border. The eleven textareas had picked five
different heights between them and none had any inset at all.
`density_controls_keep_visual_metrics_and_real_hit_targets` pins the centring and
the inset.

Three rules keep the result from drifting again:

1. **Margins are not a layout tool.** A container declares one `gap`; its children
   carry none. A margin hung on each child restates the same relationship once per
   child, so it drifts the moment anyone edits the list, and adding a child
   silently changes the rhythm of the whole block. Tailwind's `.mt_2()`/`.p_3()`
   spacing helpers are not used anywhere in this app.
2. **Both axes come from the same source.** `.px(px(CELL_INSET)).py_1()` is two
   scales in one element, and they cannot move together.
3. **Content blocks carry no inset.** The page gutter is the only horizontal
   padding a block gets. Insets belong to things with an edge — controls
   (`CONTROL_INSET`), table cells (`CELL_INSET`), menu rows (`MENU_INSET`),
   badges (`BADGE_INSET`). A block of prose has no edge.

## No card containers

A fill plus a radius plus a box inset around content is a container the reader
parses before reaching what is inside it, and a page of them reads as a
dashboard. Content blocks separate by space and announce themselves with a
kicker. This holds for the pending-review and reconciliation blocks, every
confirmation, the PR creation dialog's confirmation and outcome, the rebase
panel's notices and summaries, notification candidates, and the Conversation
timeline's comments.

Fills, borders and radii remain for things you can point at — controls, chips,
badges, selection and hover states, input fields, avatar pucks — and for the
surfaces that genuinely float: the window frame, dialogs, popovers, hover cards
and menus. Bands that are structure inside content, such as the split diff's
OLD/NEW column headers, keep theirs too.

The four PR tabs are one page reached four ways, and
`every_pr_tab_starts_its_content_on_the_same_gutter` pins that: Conversation,
Commits and Checks lay out the same page box, and the title block above the tabs
starts on the same gutter. Files changed closes the inspector and shows the diff,
which is not a fourth page.

| Element | Geometry |
| --- | --- |
| Controls / single-line data rows | 28 high |
| Control radius / horizontal inset | 10 / 10 |
| Control icons | 14 |
| Table cell inset | 10 |
| Menu row inset | 6 |
| Badges | 20 high; radius 6; 11 medium; inset 8 |
| Windows / dialog cards | Radius 12 |
| Popovers | Radius 10 |
| Two-line rows | 44 minimum; grow for wrapped content |
| Button xs / default / lg | 24 / 28 / 36 high |
| Standalone pointer target | 40 desktop; 44 touch |

Dense PR and file rows use a contiguous 28px pitch with a full-width hit region and keyboard navigation. Overlapping 40px targets on that pitch would make adjacent rows ambiguous. Standalone sidebar icon buttons reserve a real 40px layout target around a 28px painted control; PR page tabs paint a 24px chip inside a 36px row that is the whole pointer target. Touch sizing is a token for future touch surfaces, not a claim that this macOS app supports touch input. Splitter drag handles retain their dedicated 8px geometry and keyboard alternatives, but they take no width in layout: the band is positioned over the seam rather than holding a column open between the panes. An 8px column there cut every diff line in half at the file tree's edge. The file tree carries its own right hairline, so the panes meet on that.

Native macOS title-bar height/traffic-light clearance is platform-owned. Source editor text, scrollbars, tree indentation, split-pane widths, and multiline content are not single-line control heights. Expanded check/job cards grow to fit exact metadata instead of clipping to 28 or 44px. No minimum height should truncate a wrapped label or user content.

The scale is shared by review/navigation, repository setup, PR creation, notifications, comparison/Checks controls, and local editing/rebase/conflict surfaces. `Density::control` owns control font, line height, inset, radius and height; `Density::badge` owns passive badge geometry. Keep caption styling on help and metadata rather than on control labels.

Verification includes actual GPUI bounds for labels, fields, buttons, badges, menu/navigation rows, and the 40px icon hit area. A pointer click outside the painted icon but inside its target must activate it. Existing pointer tab/splitter and large-inventory virtual-list regressions protect behavior when density changes. Visual fixtures use isolated synthetic data with provider reads disabled.
