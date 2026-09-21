# Commit history

Choose **Commit history** in the sidebar's Repositories header, press `Shift Command H`, or pick it from the command palette. The page opens on the repository of the pull request you are reading, or the first one you have configured, and keeps a chip in the tab strip beside Settings. It is read-only: there is no checkout, cherry-pick, revert, tag or branch action anywhere on it.

The left column lists commits newest first, and is resizable and collapsible on the same terms as a review's file tree: drag its seam, or use the control in its header. Collapsed, it keeps its job and loses its names — one dot per commit in its lane's colour, at the same pitch the full rows use, so where you are in the history is still on screen and still answers a click. The column exists to let you choose a commit, and once you have chosen one it is the diff that you are reading; at its old fixed width the diff was left about a third of the window, which is too narrow for side by side to be usable at all.

Beside each commit, the gutter draws the lane it sits on and the lanes crossing it. A lane keeps its colour from the moment it opens until it is merged or ends, so a branch is one colour for its whole length. An ordinary commit is a filled dot; a merge is a ring, with a line arriving from each parent. Lane colours come from Primer's data-visualization scale — GitHub's own palette for telling series in a graph apart — and are deliberately not the semantic colours used elsewhere, so a lane drawn in red means "the seventh branch" and never "something failed".

## Choosing what the graph covers

**All branches** is the default and is what makes the graph a graph: it reads every branch and tag, so divergence and merges are visible. The chips beside it narrow to one branch's ancestry, which draws a straight line and is useful when the graph is busier than it is informative.

Narrowing keeps your selected commit if that exact commit is also in the narrower scope, and clears it if not. The selection is held as a commit, never as a row, so a refresh that reorders or shortens the history cannot quietly move it onto a different commit.

A read is capped at 500 commits. A history longer than that is cut to the most recent 500 and says so; it is never silently shortened. The graph is capped at 16 lanes. A repository with more branches live at once folds the rightmost ones into a shared column and tells you, rather than drawing a picture that is missing edges.

## Reading one commit

Select a commit and the right-hand side shows what it says and what it changed: the full headline, the author, both dates, the complete object ID, every branch and tag pointing at it, and its parents.

The diff is the comparison between the commit and its **first parent**, which is what GitHub's own commit page shows. For a merge, that means you see what the merge brought onto the branch it landed on; the other parents are named in the metadata so you can go to them. A root commit has no parent at all, so it is compared against an empty tree and reads as the addition of every file in it.

Changed files are listed without their patches, and the patch for a file loads when you select it. Binary and media content is never loaded. Unified and side-by-side views work as they do in a review, and long lines scroll horizontally.

The diff reads by keyboard here exactly as it does in a review: `⌘↓` and `⌘↑` move the cursor a row at a time, `⌥↓` and `⌥↑` jump between hunks, and `⌘Home` and `⌘End` go to the ends of the patch. `←`, `→`, `Home` and `End` scroll a long line sideways — those four are bound to every diff pane and, until this page took the shared one, History's diff was the one that answered none of them. The comment-thread jumps exist in the same keymap but have nothing to find here, because a commit's diff carries no threads.

## Repositories with no local clone

A repository you added by name rather than by folder has no checkout to read, so cibergit reads its history from GitHub instead. GraphQL has no equivalent of `git log --all`: it can only walk one ref's ancestry. A remote history is therefore the default branch's, and the notice on the page says so rather than implying you are looking at the whole repository. Other branches still appear as labels on the commits they point at.

Two things a remote read cannot do. It cannot show a root commit's diff, because GitHub's compare API has no way to express a comparison against a missing parent — clone the repository locally to read its first commit. And it refuses a commit with more parents than its bounded parent page, rather than drawing a merge with an edge missing.

## Refresh and isolation

History owns its own comparison, file selection, diff mode and scroll positions. Opening or refreshing it never touches a pull request tab's pinned comparison, canonical review revision, review draft or local checkout, and it has no write action of its own.

Choose **Refresh** to read again. Replies from an earlier read, another repository, a superseded scope or a commit you have since moved off are discarded rather than mixed into what is on screen. Closing the page releases the commits and the diff with them, so reopening reads afresh instead of showing a history that has since moved.
