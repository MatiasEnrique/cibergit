# Implementation ledger and acceptance map

Baseline: `23db8a1ff11ccb7dead7b4c2ca9d280e7945cdb3`, recorded 2026-09-12. Sources are [product requirements](product-requirements.md), [technical design](technical-design.md), [coordinator brief](implementation-coordinator.md), and [README](../README.md). This is an initial execution ledger, not evidence that application features work. Later accepted decisions must retain their source and replace the affected open record explicitly.

PLAN-1 owns this file only. Its assigned thread is `thr_germtczfdt`, branch `worker/requirements`, worktree `/Users/matias/Development/cibergit-requirements`, and base is the baseline above. Parent `thr_w6kmysabvr` owns integration, source, manifests, lockfiles, and shared contracts. Integration branch is `integration/v1` in `/Users/matias/Development/cibergit`, observed at the baseline. The PLAN-1 commit and verified artifact/version IDs belong in its durable handoff and the parent's state checkpoint; this initial file does not predict those IDs or its own commit SHA.

## Execution checkpoint — 2026-09-12

The initial ledger was integrated at `165b1ed` from worker commit `b4fb5df3dffaf06edf062930a0aa78562d5bb54f`. Its source coverage artifact is `art_c809ecdc-d937-4a8b-86e0-771ddffcd5b6`, version `av_ec810d11-a8a5-4f74-b223-374812676e61`.

M0 native foundation is integrated at `5abd59c` and shared contracts at `2734890`. Build, native Metal-rendered text, GPUI Kit buffer edit and disk save round-trip passed. macOS 15 is the minimum deployment target; runtime validated on macOS 26.5.2 Apple Silicon. Full physical input/AX and oldest-supported-OS validation remain M5 evidence. The disposable-file editor preview does not claim M3 race-safe saving or syntax highlighting yet. Audit: `art_6e2ffc7c-6d8e-48e4-9c32-a21d024287ee`; native evidence: `art_38b8ddbc-509d-449c-bab5-85e11a1c88e4` (use latest verified revision in coordinator state).

M1 work is underway. Account/provider transport: `thr_8bjg7f39hz`, branch `worker/provider`, worktree `/Users/matias/Development/cibergit-provider`, base `27348909fe5e7f8042d8501e62d73d27f169a775`. Immutable/local diff core: `thr_hdkztpg3pr`, branch `worker/review`, worktree `/Users/matias/Development/cibergit-review`, same base. These are prerequisite implementation slices of the M1 tasks; no end-to-end M1 task is accepted yet.

Initial personal workspace/filter/cache source and three behavior tests are integrated at `165c3f8`. The M1-STATE backend is under independent Grok review by `thr_3sn2q2yqcx` on `worker/workspace-review`, worktree `/Users/matias/Development/cibergit-workspace-review`, base `165c3f88e654e82168a007892a59805bf4c51c56`. Per-repository account keys and separate draft files are implemented; complete offline/pending-review/session recovery awaits UI and M2 integration. JSON with atomic private-file replacement is an accepted reversible engineering choice for this early milestone, not an interview decision.

The user now requests most implementation delegation to `gpt-5.6-sol` and use of Grok agents. The permanent coordinator retains integration/verification. Every original agreed behavior below remains in scope; all unanswered decisions remain open.

## Reading and maintaining the ledger

Every acceptance row links an agreed requirement or a documented engineering check to an executable task. Task records supply milestone, dependencies, assignment, status, integration, and artifact fields. Evidence in the acceptance map is required future evidence, not a reported passing result. `P` means product requirements, `T` technical design, and `C` coordinator brief. Q references preserve the origins actually present in those sources.

States are `ready`, `dependent`, `running`, `awaiting integration`, `verified`, and `blocked`. `ready` means no listed prerequisite is outstanding, not that a worker has been assigned. `dependent` means the named prerequisite, milestone gate, or decision remains outstanding. Only M0-ENV starts `running`, as assigned to the coordinator. No implementation task starts verified. On integration, replace `pending` with the exact integration SHA and attach evidence tied to that SHA. Failed or partial checks keep the task unverified.

Assignment bindings supply the associated owner/branch/worktree/base records without inventing worker allocations:

| Binding | Owner | Branch | Worktree | Base commit |
| --- | --- | --- | --- | --- |
| C0 | Coordinator `thr_w6kmysabvr` | `integration/v1` | `/Users/matias/Development/cibergit` | `23db8a1ff11ccb7dead7b4c2ca9d280e7945cdb3` |
| U | Unassigned; coordinator accountable | Unassigned | Unassigned; isolate before dispatch | Unassigned; record integrated prerequisite SHA before dispatch |

C0 keeps shared-contract, build, and integration ownership. For each U task, replace its binding with an explicit owner/thread, branch, absolute worktree, and exact base before starting. Never reuse this baseline blindly after prerequisites land. The coordinator may split a task with disjoint ownership while preserving its requirement links. Logical names below describe work, not mandatory crate boundaries or new interfaces. Engineering recommendations require validation and a recorded choice; SQLite, watcher libraries, directory layouts, and trait signatures remain unselected.

Dependencies name tasks or `G0` through `G5`, the runnable milestone gates below. `D60` through `D66` and `D68` name open decisions. They block only the listed dependent behavior. Independent agreed work continues. Future tasks remain `dependent` initially even when their prerequisite is an unanswered decision; mark a concrete blocker when it is reached. For acceptance rows spanning milestones, each task delivers its stated portion; the complete row remains pending until all mapped contributions pass. For example, M1 provides stack grouping structure and draft storage, while M4 supplies stack relationships and M2 supplies pending-review behavior.

## Implementation tasks

| Task | Deliverable and acceptance rows | Milestone | Dependencies | Assignment | Initial status | Integration commit | Artifact IDs / versions |
| --- | --- | --- | --- | --- | --- | --- | --- |
| M0-ENV | Validate toolchain, compatible pins and native editor proof; F01, F02, E01 | M0 | None | C0 | verified | 5abd59c | art_6e2ffc7c-6d8e-48e4-9c32-a21d024287ee; art_38b8ddbc-509d-449c-bab5-85e11a1c88e4 |
| M0-BUILD | Reproducible build/run instructions, license inventory; F03, E02 | M0 | M0-ENV | C0 | verified | 5abd59c | art_38b8ddbc-509d-449c-bab5-85e11a1c88e4 |
| M0-CONTRACT | Establish only needed shared state/provider/operation contracts; E03, E04 | M0 | M0-ENV | C0 | verified | 2734890 | art_4093b33c-94aa-410e-88b3-3ff83175ee7e |
| M1-ACCOUNT | Existing gh accounts, provider transport and isolation; F04, F05, W02, E05, E06 | M1 | G0 | U | dependent | pending | pending |
| M1-REPOS | Explicit local/remote repository setup and personal persistence; F06, W01, W03 | M1 | M1-ACCOUNT, M1-STATE | U | dependent | pending | pending |
| M1-STATE | Durable session, cache and drafts; E04, E07 | M1 | G0 | U | dependent | pending | pending |
| M1-SIDEBAR | All-open rows, ordered groups, saved filters; W04-W08 | M1 | M1-REPOS | U | dependent | pending | pending |
| M1-TABS | Restored PR tabs, file navigation, header/panel frame; V01-V03 | M1 | M1-REPOS | U | dependent | pending | pending |
| M1-DIFF | Immutable local/remote comparisons, adaptive text diffs; V04-V08, E08 | M1 | M1-TABS | U | dependent | pending | pending |
| M1-SYNC | Polling, explicit revision advancement and offline read; S01-S04, R04, E09 | M1 | M1-DIFF, M1-SIDEBAR | U | dependent | pending | pending |
| M2-DISCUSS | Inline threads and Overview/Activity content; V03, R01 | M2 | G1 | U | dependent | pending | pending |
| M2-REVIEW | Pending/immediate comments, draft recovery, explicit submission; R02, R03, R05, E10 | M2 | M2-DISCUSS, M1-STATE | U | dependent | pending | pending |
| M2-PROGRESS | Full/commit/range/since-review selection and viewed progress; V08, R06 | M2 | G1, M2-REVIEW | U | dependent | pending | pending |
| M2-PR | PR lifecycle, metadata, reviewers and checks; F07, R07 | M2 | G1 | U | dependent | pending | pending |
| M2-MERGE | Current-head requirement and explicit merge controls; R08, R09, E10 | M2 | M2-REVIEW, M2-PR | U | dependent | pending | pending |
| M2-NOTIFY | Personal unread state and opt-in system notifications; S05 | M2 | M2-DISCUSS, M2-PR | U | dependent | pending | pending |
| M3-WORKTREE | Create/attach/reuse persistent PR checkouts; L01, L02 | M3 | G2 | U | dependent | pending | pending |
| M3-EDITOR | Focused editor, Edit locally, file browser and quick-open; L03-L05, F08 | M3 | M3-WORKTREE | U | dependent | pending | pending |
| M3-EXTERNAL | Disk/Git observation, safe saves and reconciliation; L06-L08, E11, E12 | M3 | M3-EDITOR | U | dependent | pending | pending |
| M3-GIT | Stage/unstage, commit, fetch/pull/push and branches; L09, S04, E06, E12 | M3 | M3-EXTERNAL | U | dependent | pending | pending |
| M3-CREATE | Secondary normal/draft PR creation; L10 | M3 | M3-GIT, M2-PR | U | dependent | pending | pending |
| M3-CLEAN | Safe persistent-worktree cleanup; L11 | M3 | M3-GIT | U | dependent | pending | pending |
| M4-STACK | Native/inferred relationships, corrections and aggregate diff; K01-K03, E13 | M4 | G3, M1-SIDEBAR, M2-PROGRESS | U | dependent | pending | pending |
| M4-REBASE | Linear graphical plan, edit/split and dirty preparation; B01-B03, E14 | M4 | G3 | U | dependent | pending | pending |
| M4-CONFLICT | Editable three-way result, external resolution and recovery; B04, B05, E14 | M4 | M4-REBASE, M3-EXTERNAL | U | dependent | pending | pending |
| M4-PUBLISH | Inspect rewritten commits, explicit lease-protected push; B06, E15 | M4 | M4-CONFLICT, M3-GIT | U | dependent | pending | pending |
| M4-TIPS | Implement accepted multiple-tip behavior and evidence | M4 | M4-STACK, D60 | U | dependent | pending | pending |
| M4-COMMENT | Implement accepted aggregate comment routing and evidence | M4 | M4-STACK, M2-REVIEW, D61 | U | dependent | pending | pending |
| M4-GROUP | Implement accepted grouped review behavior and evidence | M4 | M4-STACK, M2-REVIEW, D62 | U | dependent | pending | pending |
| M4-MERGE | Implement accepted stack integration behavior and evidence | M4 | M4-STACK, M2-MERGE, D63 | U | dependent | pending | pending |
| M4-DESC | Implement accepted descendant-repair behavior and evidence | M4 | M4-STACK, M4-PUBLISH, D64 | U | dependent | pending | pending |
| M5-PARITY | Reconcile full PR scope, capability evidence and accepted inventory; F07, E16 | M5 | G2, D65 | U | dependent | pending | pending |
| M5-UI | Running-app visual/input and local performance validation; V09, V10, F06, F09, E17 | M5 | G3, M4-STACK, M4-PUBLISH | U | dependent | pending | pending |
| M5-NUMERIC | Apply accepted numeric performance gates | M5 | M5-UI, D66 | U | dependent | pending | pending |
| M5-PACK | Apple Silicon packaging, minimum OS and install/launch checks; F02, F10 | M5 | G3, M4-STACK, M4-PUBLISH, M0-BUILD | U | dependent | pending | pending |
| M5-SIGN | Signed/notarized public release readiness and evidence; F10 | M5 | M5-PACK, M5-PARITY, M5-UI | C0 | dependent | pending | pending |
| M5-UPDATE | Implement accepted update delivery and evidence | M5 | M5-PACK, D68 | U | dependent | pending | pending |
| C-TRACK | Maintain traceability, decisions and durable checkpoints; O01, O04 | All | None | C0 | ready | pending | pending |
| C-INTEGRATE | Dispatch ownership, sequential integration and review; O02, O03, O05 | All | None | C0 | ready | pending | pending |
| C-RELEASE | Final integrated scope/evidence/application handoff; O06 | M5 | G5 | C0 | dependent | pending | pending |

M5-PARITY discovery may start after G2 even while D65 is open, but final parity acceptance depends on D65. M5-UI and M5-PACK can validate the implemented core while stack decisions remain open; include later accepted interactions in final G5 checks. M5-SIGN can prepare release materials without signing access; lack of credentials does not block unsigned M5-PACK or implementation. M5-NUMERIC and M5-UPDATE do not assert that the proposals have been approved. The relevant decision may approve a different behavior or explicitly defer it. Record that outcome before closing the task. C-TRACK and C-INTEGRATE recur at each task/milestone and do not imply gates have passed.

## Runnable milestone gates

Every gate requires an exact integrated SHA, build/run commands appropriate to that revision, and a native application that launches. Record formatting, compile, lint and relevant behavioral checks; use `cargo fmt --check`, `cargo check`, `cargo clippy` and `cargo test` with the actual package/target selections once M0 establishes them. Record actual UI checks, screenshots when tooling permits, and any checks unavailable. A fixture-only UI, successful compile, or worker report cannot pass the real-provider gates. All gates start pending.

| Gate | Prerequisite tasks | Runnable acceptance and evidence |
| --- | --- | --- |
| G0 | M0-ENV, M0-BUILD, M0-CONTRACT | Build pinned dependencies on Apple Silicon; launch native GPUI window, render text, edit and save a file with GPUI Kit. Publish actual tool versions, minimum supported macOS choice, reproducible commands and dependency notices. Shared contracts have one owner before dependents start. |
| G1 | G0 and all M1 tasks | Add a real repository and account, open a real PR and navigate files both with and without local objects. Show saved groups/filters and restored tabs. Demonstrate narrow/wide diff modes, no media loading, polling/backoff, cached offline reading, and a remote head update that leaves the reviewed revision unchanged. |
| G2 | G1 and all M2 tasks | Run revision-correct discussions/reviews, pending draft recovery across restart/offline, online browser continuation, reviewed-file invalidation, PR metadata/checks, notifications and merge confirmation. Show account isolation, old-revision review with warning, current-head merge requirement and uncertain-outcome handling. Use fixtures and explicitly authorized test repositories for writes. |
| G3 | G2 and all M3 tasks | Edit a PR in a reused dedicated worktree and an explicitly attached checkout. Show full-file navigation, local changes/unpushed commits, local Git actions and PR creation. Race an external edit against save, switch/rebase externally, restart, and attempt unsafe cleanup; prove unsaved/uncommitted/unpushed work survives. |
| G4 | G3 and M4-STACK, M4-REBASE, M4-CONFLICT, M4-PUBLISH; disposition of M4-TIPS, M4-COMMENT, M4-GROUP, M4-MERGE, M4-DESC | Launch combined net-unmerged stack review and single-branch rebase. Exercise merged lower layers, ambiguous relationships, every plan action including split, dirty preparation, stash restoration conflicts, external continuation/abort, restart recovery, rejected lease and explicit publish. D60-D64 remain visible until decided; demonstrate each accepted interaction or record an explicit accepted deferral. An independent core demo is progress while these decisions remain open, not a completed G4. |
| G5 | G4, M5-PARITY, M5-UI, M5-PACK, M5-SIGN; disposition of M5-NUMERIC and M5-UPDATE | Reconcile every agreed acceptance row with evidence; complete the accepted PR inventory. Run actual native UI and representative performance checks, install/launch the packaged Apple Silicon app, verify signature/notarization and record publication status. Resolve D65, D66 and D68 or record explicit accepted deferrals. Unsigned previews cannot establish public release completion. |

No new numeric thresholds are imposed here. The broad fast-local-operation requirement remains testable through representative measurements and observed usability while D66 is open. Decisions and missing release credentials must not hold up independent implementation or preview builds.

## Agreed requirement acceptance map

### Product and release scope

| ID | Requirement and source/origin | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| F01 | Native Rust/GPUI application with GPUI Kit focused editor; P scope/local editing, T technology, initial request, Q23, Q31, Q54 | M0-ENV | Native Apple Silicon window renders text; selected editor edits/saves using a validated compatible pinned family. |
| F02 | macOS Apple Silicon only; P scope, Q8, final clarification/Q67 | M0-ENV, M5-PACK | Build/package target is `aarch64-apple-darwin`; supported minimum macOS is documented from validation. No Intel/universal/Linux/Windows release requirement. |
| F03 | Open source, MIT original code, retained dependency terms, no copied GPL Zed editor; P scope, T build, Q7, Q30 | M0-BUILD | MIT file and dependency/license inventory reviewed against pinned sources; original editor integration uses GPUI Kit. |
| F04 | User-installed Git and gh; GitHub.com V1, enterprise/self-hosted outside V1, future GitLab/provider separation; P scope, Q5, Q9, Q29, Q36 | M1-ACCOUNT | Missing-prerequisite guidance and structured real GitHub.com read work. Review provider-aware IDs/capabilities and separation from views; no new provider delivery commitment. |
| F05 | Local desktop, polling, no cibergit-operated service; P scope, initial request, Q23 | M1-ACCOUNT, M1-SYNC | Run with local Git/gh and GitHub endpoints, with no cibergit backend required. |
| F06 | Developers/independent reviewers including agent-generated PRs; normally 1-5 explicitly added app repositories; usable large-change navigation; P scope, Q1, Q4, Q10, Q59 | M1-REPOS, M5-UI | Real multi-repository review walkthrough and representative large application PR navigation; personal configuration does not change collaborators' workspace. |
| F07 | Everything on a PR including lifecycle, metadata, discussions, reviewers, checks and integration; repository administration outside scope; P scope, C fixed requirements; Q65 exact inventory open | M2-PR, M5-PARITY | Capability inventory maps each agreed PR capability to working behavior/evidence and explicitly records API gaps. Exact checklist/fallback acceptance waits for D65; broad scope is not reduced to comments and merge. |
| F08 | AI V2; no V1 language servers, debugging, extensions or integrated terminal; P scope/local editing, Q6, Q31, Q54 | M3-EDITOR | Inspect delivered editor and scope inventory for focused features and recorded exclusions. |
| F09 | All agreed V1 features retained through runnable milestones; fast local operation without accepted numeric budgets; P delivery, Q57, Q66 open | M5-UI, C-TRACK | G0-G5 evidence and full row reconciliation; representative performance report distinguishes observations from unresolved thresholds. |
| F10 | Runnable development previews then signed/notarized public macOS release; P scope/delivery, Q57, Q58 | M5-PACK, M5-SIGN | Runnable previews at gates, package install/launch evidence, signing/notarization results and separate publication status. Updates remain D68. |

### Workspace and sidebar

| ID | Requirement and source/origin | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| W01 | Explicit local-folder or remote-repository addition, persisted selection; P workspace, Q10, Q11, Q12, Q35 | M1-REPOS | Add each source type, restart and recover exactly the chosen repositories. |
| W02 | Default GitHub account per repository, simultaneous identities; P workspace, Q10, Q11, Q12, Q35 | M1-ACCOUNT | Two accounts with distinct private access operate concurrently; changing one repository account does not change another or global gh active login. |
| W03 | GitHub shared state; personal sidebar/preferences; P workspace, Q10, Q11, Q12, Q35 | M1-REPOS, M1-STATE | Personal grouping/preferences restore locally without writing shared PR metadata. |
| W04 | All open PRs by default; title, number and source branch in every row; required filtering; P workspace, Q12, Q13, Q18, Q19, Q43 | M1-SIDEBAR | Real paginated repository shows all open rows with all three fields, then correctly filters them. |
| W05 | Ordered composable groups by repository, target branch, source branch and PR stack; P workspace, Q13, Q18, Q19, Q43 | M1-SIDEBAR, M4-STACK | Save/reorder combined grouping levels, confirm membership and stack groups against native/inferred relationships when M4 lands. |
| W06 | Exact source names and configurable source prefixes; named grouping/filter combinations; P workspace, Q13, Q18, Q19, Q43 | M1-SIDEBAR | Exact/prefix fixtures and real view show intended members; saved named combinations survive restart. |
| W07 | Search title/number/branch; filter author/reviewer/assignee/label/draft/review/check/target/source branch; P workspace, Q12, Q43 | M1-SIDEBAR | Filter matrix exercises every listed criterion individually and in saved combinations with matching/nonmatching PRs. |
| W08 | Quick filters for needs-my-review, own PRs and participated PRs; closed/merged PR search; P workspace, Q12, Q43 | M1-SIDEBAR | Account-specific quick-filter cases and closed/merged search work while initial view remains all-open. |

### Review workspace

| ID | Requirement and source/origin | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| V01 | PR tabs preserve file, scroll, reviewed revision and unfinished comments; P review workspace, Q16, Q26 | M1-TABS, M1-STATE, M2-REVIEW | Navigate/switch tabs and restart; selected file/position/revision and unsent text recover. |
| V02 | File tree, one selected diff, next/previous-file navigation; P review workspace, Q16, Q26 | M1-TABS | Mouse and keyboard traverse files in a real PR with one selected file diff. |
| V03 | Header title/branches/review/merge actions; collapsible right Overview/Activity/Checks panel; inline threads; P review workspace, Q27 | M1-TABS, M2-DISCUSS, M2-PR, M2-MERGE | Running UI shows all named content and actions, panel collapse and inline thread navigation. M1 can supply the frame; M2 supplies functional content. |
| V04 | Wide defaults side-by-side, narrow defaults unified; explicit mode remembered; P review workspace, Q16, Q26 | M1-DIFF | Resize fresh tab across modes, select override and restore it across navigation/restart. |
| V05 | Every changed file listed, including media/binary; do not load images or videos; P review workspace, Q41, Q44, Q59 | M1-DIFF | Mixed-file PR lists all paths; request/file-read evidence proves no media payload loading, including selection. No earlier image-preview recommendation survives. |
| V06 | Other binary files show metadata instead of text diff; selected text loads on demand; P review workspace, Q41, Q44, Q59 | M1-DIFF | Binary selection shows metadata; instrumentation demonstrates text content loads on selection. |
| V07 | Large-file lazy loading/virtualization and usable navigation; P review workspace, Q41, Q44, Q59 | M1-DIFF, M5-UI | Representative large file/PR remains scrollable/navigable; observe bounded rendering/loading and record measurements. |
| V08 | Full PR, individual commit, commit range and changes since last review; active comparison above diff; P review workspace, Q37, Q46 | M1-DIFF, M2-PROGRESS | Same PR exercised in all four modes with correct immutable endpoints and visible comparison label. |
| V09 | Codex-app visual direction, native macOS conventions, resizable panels, restrained colors, light/dark themes; P review workspace, Q17, Q42 | M1-TABS, M5-UI | Inspect actual native windows at narrow/wide sizes and both themes; resize panels and record screenshots when possible. |
| V10 | Equal keyboard/mouse support, keyboard navigation and command palette; P review workspace, Q17, Q42 | M1-TABS, M5-UI | Complete representative repository/tab/file/review/editor flows by each input method; command palette opens and invokes available actions. |

### Reviews and incoming activity

| ID | Requirement and source/origin | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| R01 | PR discussions and inline review threads; P scope/review workspace, Q27 | M2-DISCUSS | Real discussion/thread content maps to correct PR/file/revision; updates appear inline and in Activity. |
| R02 | Inline comments pending by default, separate immediate-post action, locally preserved unfinished text; P reviews, Q21, Q47 | M2-REVIEW | Draft/restart/offline tests retain text; default action adds to pending review; immediate post is a distinct explicit action. |
| R03 | Online pending-review synchronization supports browser continuation; submission explicit; P reviews, Q21, Q47 | M2-REVIEW | Authorized test review visible pending in browser; submission requires user action and is not triggered by draft sync/reconnect. |
| R04 | Visible new-commit feedback, manual advance only; comments/checks refresh independently; P reviews, Q22, Q34 | M1-SYNC | Remote head changes during reading; current code/scroll/comparison stay stable while indicator, comments and checks refresh. |
| R05 | Submit against older actually reviewed revision with newer-commit indication; P reviews, Q48 | M2-REVIEW | Head advances before submission; posted review uses reviewed commit with a clear newer-head warning, including race at submit. |
| R06 | Viewed files tied to reviewed revision; changed files need another look; missing old revision explains full-diff fallback; P review workspace, Q37, Q46 | M2-PROGRESS | Change one reviewed file then advance; only affected progress invalidates as appropriate. Remove old object and show explained full comparison, not a silent substitute. |
| R07 | PR lifecycle, metadata, reviewers, checks and panel content; P scope/review workspace, Q27; exact inventory D65 | M2-PR, M5-PARITY | Exercise documented PR capabilities against structured provider responses and real reads; inventory lists supported actions, settings/capability restrictions and unresolved gaps. |
| R08 | Update to current revision before initiating merge; compact confirmation of method/message/branch deletion/blockers; P reviews, Q48, Q49 | M2-MERGE | Stale review prevents merge initiation; advancing enables confirmation with each field, blockers and explicit confirmation. Recheck changing remote state. |
| R09 | Remember merge method per repository; available merge/auto-merge/queue actions follow repository settings; P reviews, Q49 | M2-MERGE | Settings/capability matrix shows only available actions; method preference survives restart and stays per repository. Stack merge remains D63. |

### Stacks

| ID | Requirement and source/origin | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| K01 | Native relationships first, otherwise infer source/target dependencies; P stacks, Q32 | M4-STACK | Native/inferred test graphs and real provider reads preserve relationship origin and choose native data when available. |
| K02 | Flag ambiguity, personal corrections, no silent remote target changes; P stacks, Q32 | M4-STACK | Ambiguous graph is visibly flagged; personal correction persists locally with no remote retarget request. Multiple-tip behavior remains D60. |
| K03 | Combined stack diff is net remaining unmerged changes from effective base to selected tip; neither whole original feature after lower merges nor concatenated patches; P stacks, Q13, Q33 | M4-STACK | Layered commits with overlapping edits and a merged lower PR match the effective-base/tip tree diff. Prove cancelled intermediate changes do not remain as concatenated patches. |

### Local Git and source editing

| ID | Requirement and source/origin | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| L01 | Clone-free review; create/attach checkout only for local work; prefer dedicated PR worktree; explicit existing-checkout attachment; P local Git, Q3, Q20 | M1-DIFF, M3-WORKTREE | Open PR without a clone; Edit locally creates or attaches on request; subsequent visits reuse the same PR worktree. |
| L02 | Review is selected published revision; Local Changes shows uncommitted and unpushed work with local/remote differences; P local Git, Q45 | M3-WORKTREE, M3-EXTERNAL | Index/worktree/local commit changes alter Local Changes while published Review remains unchanged; external agent changes are labeled against remote state. |
| L03 | Published diffs read-only; Edit locally opens corresponding file in same PR tab; P local Git, Q55, Q56 | M3-EDITOR | Typing cannot edit Review; Edit locally navigates to editable worktree file without replacing the PR tab. |
| L04 | Full-worktree file browser and quick-open; changed files stay default review navigation; P local Git, Q55, Q56 | M3-EDITOR | Open/save an unchanged file through browser and quick-open; review tree still defaults to changed paths. |
| L05 | Syntax highlighting, undo/redo, find/replace, indentation, file navigation and save using GPUI Kit; custom diff/conflict interfaces; P local Git, Q31, Q54 | M3-EDITOR, M4-CONFLICT | Exercise every focused editor feature on real files, save/read-back contents, and inspect cibergit diff/conflict integration with pinned editor. |
| L06 | External edits reload clean buffers; preserve dirty buffers and compare concurrent disk changes; P local Git, Q52, Q53 | M3-EXTERNAL | Clean buffer reloads; concurrent local unsaved text and external edit both remain recoverable in reconciliation. |
| L07 | Observe external commits, branch switches and rebases; show actual Git state; P local Git, Q52, Q53 | M3-EXTERNAL | External tool performs each operation; UI reflects actual HEAD/index/operation state without requiring a cibergit action. |
| L08 | Pause incompatible operations, preserve unsaved text, offer reconciliation/reload; P local Git, Q52, Q53 | M3-EXTERNAL | External operation invalidates an action; action pauses with current state and preserves dirty buffer until explicit reconciliation. |
| L09 | Stage/unstage, commit, fetch, pull, push, branch switch and branch creation; P local Git, Q14, Q25 | M3-GIT | Temporary-repository scenario exercises each action and relevant dirty/lock/error paths; remote writes use designated authorized targets only. |
| L10 | Create normal and draft PRs as a secondary workflow; P local Git, Q14, Q25 | M3-CREATE | Authorized test or provider fixture creates each form from local work; main navigation remains review-focused. |
| L11 | Worktrees persist across sessions; cleanup offered for merged/closed PRs; preserve uncommitted/unpushed work; tab close is not disposal; P local Git, Q40 | M3-CLEAN | Restart/tab-close keeps association; cleanup rejects unsafe deletion in dirty/unpushed cases, and offers eligible closed/merged cleanup. |

### Rebase and conflicts

| ID | Requirement and source/origin | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| B01 | One-branch interactive plan: reorder, squash, fixup, drop, reword, edit stops for modify/split; P rebase, Q15, Q50 | M4-REBASE | Temporary linear-history scenario for every action, including a split into multiple commits, matches intended final commits/tree. |
| B02 | V1 graphical plan handles linear histories; merge histories get explained external workflow without silent flattening; P rebase, Q15, Q50 | M4-REBASE | Merge-containing history is detected before execution; explanation and external path appear and no flattening occurs. |
| B03 | Dirty worktree offers commit/stash/cancel; stash restore explicit with conflict handling; P rebase, Q51 | M4-REBASE, M4-CONFLICT | Exercise all three choices, identify exact created stash, finish rebase without auto-restore, explicitly restore and resolve a resulting conflict. |
| B04 | Built-in three-way conflict view with editable result and explicit continue/abort; external editor allowed; P rebase, Q38 | M4-CONFLICT | Resolve a conflict internally and externally, verify saved result/staging, continue and abort in separate scenarios. |
| B05 | Actual rebase state survives interruptions and external operation changes; T rebase, supports P Q52, Q53 | M4-CONFLICT | Restart at edit/conflict stops; external continue/abort is detected; UI reconciles preparation/running/paused/conflicted/completed/aborted/failed state. |
| B06 | Show resulting commits after rebase; offer explicit rewritten-branch push using lease; never auto-push; P rebase, Q39 | M4-PUBLISH | Completion displays result without publishing. Explicit push succeeds against observed head and rejects competing remote update. Descendant behavior remains D64. |

### Synchronization, offline work and notifications

| ID | Requirement and source/origin | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| S01 | Active PR approximately 15 seconds; sidebar 60 seconds; refresh on focus/actions/manual request; P sync, Q23, Q34 | M1-SYNC | Scheduler evidence demonstrates both cadences and each trigger without advancing selected revision. |
| S02 | Slow down while inactive/rate-limited; local filesystem changes appear promptly; P sync, Q23, Q34 | M1-SYNC, M3-EXTERNAL | Inactivity/rate-limit scenarios back off; external file changes promptly invalidate/reload current state. Record timing without inventing an accepted SLA. |
| S03 | Offline cached PR reading, available local diffs, editing, local Git and review drafting; P sync, Q24 | M1-SYNC, M2-REVIEW, M3-EDITOR, M3-GIT | Disconnect and exercise each available workflow; unavailable remote data is explained and drafts persist across restart. |
| S04 | Failed/offline review publication, push and merge need explicit retry; no automatic replay on reconnect; P sync, Q24 | M1-SYNC, M2-REVIEW, M2-MERGE, M3-GIT | Lose connectivity during each mutation; reconnect triggers reads only, preserves uncertainty/text and waits for explicit retry after checking outcome. |
| S05 | Default in-app unread; opt-in macOS notifications for review request, mention, reply to own thread and failed check on own PR; P sync, Q28 | M2-NOTIFY | Default produces in-app indicators only; opt-in cases cover all four events, own-PR filtering and relevant account identity. |

## Technical-design validation map

These rows preserve design recommendations and upstream constraints as engineering work, not new product decisions. Validate current upstream facts during the assigned implementation task. This ledger makes no fresh claim that a dependency version or preview API has been tested. Record selected choices and rationale in coordinator-owned design/build records.

| ID | Documented check / source | Task | Required acceptance evidence |
| --- | --- | --- | --- |
| E01 | T dependency/build: Rust, Git, gh, Xcode/Metal; compatible GPUI/Kit family, minimum OS/tools | M0-ENV | Actual environment versions, current manifests and native edit/save proof. Do not treat documented Kit 0.6.1 / gpui-pre 0.3.1 as a tested lockfile or mix arbitrary packages. Record exact failure/remedy if unavailable; no framework/license substitution. |
| E02 | T dependency/build; C environment: reproducibility and prerequisite distinction | M0-BUILD | Pinned lockfile/family and commands reproduce build; docs distinguish end-user Git/gh from developer Xcode/Metal. Preserve existing changes; no incidental global settings change, remote creation or credential purchase. |
| E03 | T proposed boundaries/technology: provider-aware domain, capability-oriented adapter, parsing/CLI away from views; modules may be one package | M0-CONTRACT, M1-ACCOUNT | Reviewed shared contracts carry provider/host/opaque IDs and explicit capability differences. Views avoid GitHub response types and CLI parsing. Module count, crate layout and signatures remain engineering choices. |
| E04 | T state ownership: GitHub collaboration, immutable comparison, actual Git/worktree state, unsaved buffer, local draft and personal state remain distinct | M0-CONTRACT, M1-STATE | State-transition tests preserve all six authorities. Keys distinguish accounts/repos/PRs and immutable comparisons, never branch names alone. Worktree metadata records actual Git/common directories as needed. |
| E05 | T transport: gh structured commands plus REST/GraphQL gh api; argument arrays/stdin, explicit host/repo/account; pagination and loading/empty/stale/unavailable/failed states; cancellable bounded subprocesses off UI | M1-ACCOUNT | Quoted/special branch/comment input stays data; complete pagination and failure-state fixtures; cancel/large-output behavior does not stall native UI. Real provider read confirms integration beyond fixtures. |
| E06 | T transport: account-specific authentication proposal, private resource partitioning; API identity separate from Git credentials/authorship | M1-ACCOUNT, M3-GIT | Validate account selection without global login changes; credentials absent from logs/args/cache. Private cached data/drafts do not cross accounts. Fetch/push failures and commit identity are shown without changing global Git configuration. Token-selection mechanism remains subject to integration proof. |
| E07 | T persistence: durable draft text separate from disposable cache; SQLite/versioned config only proposed | M1-STATE | Select/document storage/binding/config strategy; restart and cache refresh preserve drafts and personal state. Record recovery behavior and account partitions. |
| E08 | T revision model: immutable endpoints, prefer available local objects, clone-free remote route; incomplete patch disclosure and stable coordinates | M1-DIFF | Same comparison across local/remote source switch retains SHAs and inline coordinates. Missing/incomplete remote patch is labeled, not shown as complete. |
| E09 | T sync: conditional requests where supported, server-aware backoff, no refresh storms; separate head availability from active comparison | M1-SYNC | Conditional/rate-limit response fixtures and concurrent-trigger traces show bounded refreshes; returned retry headers respected. |
| E10 | T persistence/validation: uncertain mutations require outcome reconciliation before explicit retry; pending sync is not submission | M2-REVIEW, M2-MERGE, M3-GIT | Simulate server success followed by client timeout; reconcile remote state and avoid duplicate review/merge/push. Offline reconnect never submits accumulated work implicitly. |
| E11 | T sync: actual worktree/Git metadata watching, debounce/focus reread, event invalidation; save-time disk recheck | M3-EXTERNAL | Linked worktree with Git dir outside checkout observes refs/index/operations. Delayed/dropped watcher and save race preserve external edits and dirty buffers. Watcher choice is recorded after validation. |
| E12 | T local Git: installed executable proposed, structured/NUL path handling; serialize incompatible writes, shared refs, external locks and preflight revalidation | M3-GIT, M3-EXTERNAL | Temporary repos with unusual paths, concurrent external operation, linked worktrees and Git lock failures produce correct status and no destructive overwrite. Chosen backend/managed paths documented without asserting app-exclusive ownership. |
| E13 | T stacks: native API facts require revalidation; relationship provenance; aggregate coordinates distinct from destination PR/commit/path/line | M4-STACK, M4-COMMENT, M4-MERGE | Current provider capability evidence and graph fixtures; keep coordinates separate. If D63 accepts async native merging, verify final status and background blockers rather than initial-request success. No stack UI proposal is implicitly approved. |
| E14 | T rebase: Git sequence editor and lifecycle, actual-state restart/external recovery, correct stash entry | M4-REBASE, M4-CONFLICT | Exercise interrupted/failed/edit/conflicted/completed/aborted paths, actual continuation/skip/abort state where applicable, and correct stash restoration even after other stash entries are added. |
| E15 | T rebase: force-with-lease uses explicit expected remote head | M4-PUBLISH | Background tracking-ref refresh after another writer pushes does not weaken lease; explicit stale expected head rejects publish. |
| E16 | T milestones/unresolved: concrete GitHub capability inventory; broad PR scope already agreed, detailed parity/API-gap policy open | M5-PARITY | Inventory ties available APIs, restrictions and implemented behaviors to acceptance evidence; D65 resolution precedes treating checklist/browser fallback as accepted. |
| E17 | T milestones/validation: prioritize corruption/data loss cases and representative local performance | M5-UI, C-INTEGRATE | Evidence covers save races, head/submission races, account isolation, missing historical objects, dirty worktrees, uncertain actions and interrupted rebases; native UI checks supplement automated tests. |

## Open product decisions

All eight records below are **open, unanswered proposals**. They are not approved defaults. Q67 is resolved by the Apple Silicon clarification and maps to F02. Prepare a concrete recommendation only when its dependent behavior needs the answer; continue independent work rather than restarting the interview. A decision's closure evidence must include the accepted answer, affected scope and updated acceptance criteria. If a proposal is changed or deferred, preserve that decision rather than marking the original proposal implemented.

| Decision / origin | Unaccepted proposal | Dependent task / milestone | Acceptance after a decision |
| --- | --- | --- | --- |
| D60 / Q60, open | Select one tip and show its dependency path for a multiple-tip stack | M4-TIPS / M4 | Accepted branched-graph selection rule plus comparison fixtures for multiple tips; no silent synthesized merge/path choice. |
| D61 / Q61, open | Direct aggregate comment only for unambiguous mapping, otherwise individual PR | M4-COMMENT / M4 | Accepted routing rule, overlapping-layer/ambiguous line cases and revision-correct destination evidence. |
| D62 / Q62, open | One grouped panel submits distinct per-PR decisions with individual results | M4-GROUP / M4 | Accepted submission semantics, per-PR outcomes and partial-failure/retry evidence if grouping is selected. |
| D63 / Q63, open | Native asynchronous stack merge with affected PRs; sequential guidance for inferred stacks | M4-MERGE / M4 | Accepted native/inferred integration behavior, authorized execution evidence and final failure/success reconciliation. |
| D64 / Q64, open | Show affected descendants and guide individual repair, without automatic cascade | M4-DESC / M4 | Accepted post-rewrite descendant handling and affected-branch scenarios; no inferred cascade authority. |
| D65 / Q65, open | Formal PR feature checklist; browser fallback only for verified API gaps | M5-PARITY / M5 | Accepted inventory and API-gap treatment, all agreed capabilities accounted for. Browser fallback is not an accepted substitute yet. |
| D66 / Q66, open | Cached PR switch within 100 ms and first useful local diff within 500 ms | M5-NUMERIC / M5 | Accepted metrics, thresholds and representative measurement method. Fast local interaction and measurement remain required independently. |
| D68 / Q68, open | Check GitHub Releases and offer explicit installation | M5-UPDATE / M5 | Accepted update mechanism or explicit disposition, install/update evidence for that decision. Signed/notarized packaging remains agreed independently. |

## Coordinator acceptance and fixed-brief cross-check

| ID | Coordinator obligation | Task | Evidence |
| --- | --- | --- | --- |
| O01 | Preserve agreed/proposed distinction and full requirement/dependency ledger; continue runnable delivery | C-TRACK | Every accepted row has task/evidence, statuses reflect actual work, and open decision effects are visible. No milestone narrows V1 scope. |
| O02 | Environment verification, settled shared interfaces, bounded isolated workers with recorded ownership/base; coordinator owns manifests/contracts/integration | M0-ENV, C-INTEGRATE | Actual BB/Git context, scoped briefs with all required fields and acceptance checks; no overlapping writers. Respect up-to-three worker limit and inherited provider/model. PLAN-1 spawns no workers. |
| O03 | Sequential integration, affected checks at integrated SHA, independent review for substantial/risky changes, authorized test targets | C-INTEGRATE | Reviewed scoped diff, integration SHA and relevant format/build/lint/behavior/native-UI evidence. Independent review for account/revision/mutation/write/cleanup/rebase risks; resolve material findings. No live user-repository writes inferred from read access. |
| O04 | Durable state/artifact checkpoint after integration/milestone and before compaction/handoff | C-TRACK | Artifact includes milestone/DoD, integration HEAD, dirty files, ledger/frontier, active worker IDs/bases/worktrees, pending/integrated SHAs, checks/failures, accepted/provisional decisions, blockers, full artifact/version IDs and next actions. Verify show manifest/sizes and important content read-back; short note indexes it. |
| O05 | Supervise actual liveness, preserve worker handoffs, stop/archive consumed workers; reconcile after restart | C-INTEGRATE | Explicit completion/blocker messages and Git/artifact inspection, no intention-only idle worker counted done, no duplicate mutations after restart. Check uncommitted/unintegrated work before worktree removal; parent remains available. |
| O06 | Complete integrated V1 handoff with build/run instructions, app artifact, scope/evidence, limitations, signing/publication status | C-RELEASE | G5 inventory and exact integrated commit, verified artifact IDs/versions and honest unresolved limitation report. Credential/publication blockers are named without withholding independent unsigned validation. |

The 17 fixed bullets in C are covered in their source order:

| Fixed bullet | Acceptance rows |
| --- | --- |
| 1. Rust/GPUI/Kit, pin before dependent application work | F01, E01, E03 |
| 2. Apple Silicon macOS only | F02 |
| 3. MIT and dependency notices, no Zed GPL editor copy | F03 |
| 4. Git/gh, GitHub.com and future provider separation | F04, E03, E05 |
| 5. No service, existing gh auth and simultaneous accounts | F05, W02, E06 |
| 6. Developers, 1-5 explicit repos, full PR scope, secondary creation | F06, F07, W01, L10 |
| 7. Sidebar grouping/filtering/row fields | W04-W08 |
| 8. Tabs/diffs/discussions/panels/visual direction/input | V01-V04, V09, V10, R01 |
| 9. Every file, no image/video loading | V05, V06 |
| 10. Stable revision/manual advance/independent refresh | R04, E08, E09 |
| 11. Pending/local drafts, revision progress and explicit actions | R02, R03, R05, R06, R08, R09 |
| 12. Clone-free review and dedicated/attached worktrees | L01 |
| 13. Review/local editing split, external Git/files and unsaved work | L02-L08, E11, E12 |
| 14. Linear single-branch rebase/edit-split/conflicts/stash/lease | B01-B06, E14, E15 |
| 15. Net-unmerged stack diff, open stack proposals distinguished | K01-K03, D60-D64 |
| 16. Polling/focus/backoff/offline/no automatic replay | S01-S04, E09, E10 |
| 17. Deferred AI/LSP/debugger/extensions/terminal | F08 |

## Source-section and origin audit

| Product section | Acceptance coverage |
| --- | --- |
| Product and release scope | F01-F10, W02, E01-E06, D65 |
| Workspace and sidebar | W01-W08 |
| Review workspace | V01-V10, R06 |
| Reviews and incoming activity | R01-R09, E10 |
| Stacks | K01-K03, D60-D64 |
| Local Git and source editing | L01-L11, F08, E11, E12 |
| Rebase and conflicts | B01-B06, E14, E15, D64 |
| Synchronization, offline work, and notifications | S01-S05, E07, E09, E10 |
| Delivery | F09, F10, G0-G5, D66, D68 |

| Technical section | Acceptance coverage |
| --- | --- |
| Technology decisions | F01-F05, F10, W02, E03, E08 |
| Dependency and build constraints | E01, E02, F01-F03 |
| Design tree | Product-section coverage above and D60-D66, D68 |
| Proposed application boundaries | E03, E04; recommendations retain their provisional status |
| GitHub transport and accounts | E05, E06, W02 |
| State ownership and revision model | E04, E08, V08, L02 |
| Synchronization and persistence | E07, E09-E11, S01-S04 |
| Local Git and worktrees | L01-L11, E11, E12 |
| Rebase execution | B01-B06, E14, E15 |
| Stack representation and upstream constraints | K01-K03, E13, D60-D64 |
| Implementation milestones | G0-G5, E17, O03, O06 |
| Unresolved details | D60-D66, D68; Q67 resolved in F02; engineering validation E01-E16 |

The source documents retain Q origins, not the complete interview transcript. Do not invent a requirement for an unreferenced question. Q2 is not attributed in either requirements or technical design. All other Q1-Q59 origins that appear in those documents are indexed below. Ranges expand inclusively.

| Origin | Acceptance rows |
| --- | --- |
| Initial request | F01, F05 |
| Q1 | F06 |
| Q3 | L01 |
| Q4 | F06 |
| Q5 | F04 |
| Q6 | F08 |
| Q7 | F03 |
| Q8 | F02 |
| Q9 | F04 |
| Q10 | F06, W01-W03 |
| Q11 | W01-W03 |
| Q12 | W01-W04, W07, W08 |
| Q13 | W04-W06, K03 |
| Q14 | L09, L10 |
| Q15 | B01, B02 |
| Q16 | V01, V02, V04 |
| Q17 | V09, V10 |
| Q18 | W04-W06 |
| Q19 | W04-W06 |
| Q20 | L01 |
| Q21 | R02, R03 |
| Q22 | R04 |
| Q23 | F01, F05, S01, S02 |
| Q24 | S03, S04 |
| Q25 | L09, L10 |
| Q26 | V01, V02, V04 |
| Q27 | V03, R01, R07 |
| Q28 | S05 |
| Q29 | F04 |
| Q30 | F03 |
| Q31 | F01, F08, L05 |
| Q32 | K01, K02 |
| Q33 | K03 |
| Q34 | R04, S01, S02 |
| Q35 | W01-W03 |
| Q36 | F04 |
| Q37 | V08, R06 |
| Q38 | B04 |
| Q39 | B06 |
| Q40 | L11 |
| Q41 | V05-V07 |
| Q42 | V09, V10 |
| Q43 | W04-W08 |
| Q44 | V05-V07 |
| Q45 | L02 |
| Q46 | V08, R06 |
| Q47 | R02, R03 |
| Q48 | R05, R08 |
| Q49 | R08, R09 |
| Q50 | B01, B02 |
| Q51 | B03 |
| Q52 | L06-L08, B05 |
| Q53 | L06-L08, B05 |
| Q54 | F01, F08, L05 |
| Q55 | L03, L04 |
| Q56 | L03, L04 |
| Q57 | F09, F10, G0-G5 |
| Q58 | F10 |
| Q59 | F06, V05-V07 |
| Q60-Q66 | D60-D66, explicitly open |
| Q67 / final clarification | F02, resolved Apple Silicon only |
| Q68 | D68, explicitly open |

PLAN-1 verification is a source-by-source requirements and fixed-brief cross-check plus scoped diff review. It does not build the app or verify implementation acceptance. The parent must update task assignments, exact integration commits and verified evidence as work proceeds.

## PLAN-1 verification report

The source review covered all nine product sections, all twelve technical sections and all 17 fixed coordinator constraints. The ledger contains 40 task records and 85 acceptance rows: 62 behavioral/scope rows, 17 technical validation rows and six coordinator rows. All 67 Q numbers referenced by the sources map through the origin index, including the eight open proposals and resolved Q67. Q2 has no source attribution and no invented requirement.

A structural check resolved every acceptance-row task and dependency, checked the six-gate graph for cycles, and confirmed all assignment/status/integration/artifact fields. Initial states are one running coordinator task, 37 dependent tasks and two ready coordinator obligations. Scoped diff review and `git diff --cached --check` pass. Application acceptance remains pending; PLAN-1 makes no native build, runtime, provider-write or release claim.
