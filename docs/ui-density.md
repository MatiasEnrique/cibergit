# Dense desktop UI guidelines

Cibergit's sizing and spacing follow this scale. Reuse `cibergit::ui` and the shared component helpers when adding or updating UI; do not introduce independent Tailwind text sizes or control geometry. IBM Plex Sans remains the interface face; Menlo remains the code face. Values are logical pixels, independent of display scale.

| Text role | Size | Line height | Weight |
| --- | ---: | ---: | --- |
| Display | 24 | 28 | Medium |
| Title | 15 | 20 | Medium |
| Subtitle | 13 | 18 | Medium |
| Body | 12 | 18 | Regular |
| Label / table header | 12 | 16 | Medium |
| Caption / help | 11 | 16 | Regular |
| Kicker | 11 | 16 | Semibold; uppercase; 0.08em tracking |

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

Dense PR and file rows use a contiguous 28px pitch with a full-width hit region and keyboard navigation. Overlapping 40px targets on that pitch would make adjacent rows ambiguous. Standalone sidebar icon buttons reserve a real 40px layout target around a 28px painted control; PR page tabs use 40px targets. Touch sizing is a token for future touch surfaces, not a claim that this macOS app supports touch input. Splitter drag handles retain their dedicated geometry and keyboard alternatives.

Native macOS title-bar height/traffic-light clearance is platform-owned. Source editor text, scrollbars, tree indentation, split-pane widths, and multiline content are not single-line control heights. Expanded check/job cards grow to fit exact metadata instead of clipping to 28 or 44px. No minimum height should truncate a wrapped label or user content.

The scale is shared by review/navigation, repository setup, PR creation, notifications, comparison/Checks controls, and local editing/rebase/conflict surfaces. `Density::control` owns control font, line height, inset, radius and height; `Density::badge` owns passive badge geometry. Keep caption styling on help and metadata rather than on control labels.

Verification includes actual GPUI bounds for labels, fields, buttons, badges, menu/navigation rows, and the 40px icon hit area. A pointer click outside the painted icon but inside its target must activate it. Existing pointer tab/splitter and large-inventory virtual-list regressions protect behavior when density changes. Visual fixtures use isolated synthetic data with provider reads disabled.
