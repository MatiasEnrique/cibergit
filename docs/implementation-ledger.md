# Implementation ledger and acceptance map

Baseline: `23db8a1ff11ccb7dead7b4c2ca9d280e7945cdb3`, recorded 2026-09-12. Sources are [product requirements](product-requirements.md), [technical design](technical-design.md), [coordinator brief](implementation-coordinator.md), and [README](../README.md). This is an initial execution ledger, not evidence that application features work. Later accepted decisions must retain their source and replace the affected open record explicitly.

PLAN-1 owns this file only. Its assigned thread is `thr_germtczfdt`, branch `worker/requirements`, worktree `/Users/matias/Development/cibergit-requirements`, and base is the baseline above. Parent `thr_w6kmysabvr` owns integration, source, manifests, lockfiles, and shared contracts. Integration branch is `integration/v1` in `/Users/matias/Development/cibergit`, observed at the baseline. The PLAN-1 commit and verified artifact/version IDs belong in its durable handoff and the parent's state checkpoint; this initial file does not predict those IDs or its own commit SHA.

## Execution checkpoint — 2026-09-13

Integration branch: `integration/v1`. Real PR review and persistent local editing are runnable. M0 is proven; G1–G5 remain open. Every acceptance below is scoped. Historical source/report chains remain in coordinator state artifact `art_4093b33c-94aa-410e-88b3-3ff83175ee7e`, revision 28 and successors.

- Foundation: pinned GPUI Base 0.6.1 / GPUI 0.3.1 runtime shaders, MIT/notices, macOS 15 arm64, IBM Plex Sans. Native foundation and dependency audits are verified in `art_38b8ddbc-509d-449c-bab5-85e11a1c88e4` revision 2 and `art_6e2ffc7c-6d8e-48e4-9c32-a21d024287ee`.
- Provider transport/account/review foundations at `56662a3` are independently accepted (`art_486e91ad-c526-4f46-b562-c7bc4e31da92` / `av_83d59727-e2f5-4faf-8057-8d85a92edb7c`). Native sidebar/views/file tree/diff clipping and inline review are integrated. Unknown-ID mutation outcomes remain unresolved; no automatic replay or unsafe branch deletion.
- Native review reconciliation is independently accepted at `c3bb8bd1ec909473d7ae9f850ee92a768e7a4ad0`. Production pending comments with no side join exact IDs to unique complete current detail threads; uncertain new IDs are never matched by body. Parent 281/0/8 ignored, default/all-feature strict Clippy and dark restart passed. Worker `art_a732905c-fe1a-49e9-87a1-c883c6c7b160` / `av_76b87d5d-025d-4190-9553-e3daf912846d`; parent `art_a629f545-7fd7-4618-83df-82089c2ef76d` / `av_1480670e-eec2-4409-b182-dbfd144b04fd`; review `art_fbf76bd0-798c-41fa-8fd2-a8cfcd0ec669` / `av_174fdaac-d17a-4e71-bf8d-59d997d4e0b3`.
- Comparison backend `eea5e9c` → `f80986a` / export `ef0a8d1` is independently accepted (`art_677071aa-be13-4de7-a2a1-2e6117912aba` / `av_b5b27b1f-9eab-4c71-8fb7-e3177dd77f9a`). Native selector `4d13be84429034e3afc61045a2c45128951eb718` → `4dee33589135b8399a58d50bd5da9799f24e3c4f` adds canonical/selected separation, Full/commit/range/Since controls, independent inventory generations, safe canonical comment re-anchor and bounded atomic per-request progress. Parent 310/0/9 ignored, strict all-feature Clippy, fresh public light and dark restart passed. Verified worker evidence `art_f860f247-ec80-46b5-ac16-12742129acc4` / `av_7e1fa0e1-9723-4cc9-8e50-905375e67528`, 12 files / 7,893,864 bytes. Independent native ACCEPT at `4dee335` is banked in `art_b9a00542-5c63-4857-b0c0-d07076d2c431` / `av_dab2cd6c-728d-45b9-b12d-9bc97b3fdab2`. Parent `3519041` adds compact revision disclosure without changing comparison semantics. Accepted local-completion baseline persistence remains open.
- PR lifecycle backend `578e604e7122a82a9c6f2404a48f9660c07d3626` → `fa981e6` adds metadata/choices, one-action lifecycle and top-level discussion, separate PR creation and held durable admission interfaces. Verified report `art_6684ac11-069c-4160-b919-b3f562c2068f` / `av_069877a5-6114-4961-af5b-73474fc5f3bd`. Worker 26 focused pass plus separately passing public snapshot read; mutation evidence is fixture-only. First parent parallel suite hit an unclassified terminal-save fixture failure; unchanged isolated rerun passed in 0.43s. Diagnostic rerun confirmed fake credential subprocess startup timed out before dispatch; `c911a6d` changes only the ordinary fixture budget from 3s to 30s and adds failure diagnostics. Dedicated deadline tests remain unchanged. Combined 342/0/10 passed at the previous checkpoint. Independent review found three production-shape defects (comment acknowledgement capabilities, assignee Issue URLs, and unused null update fields). Sol correction `bba2d9bb1f74e337e21734135adc5ef76190b9e4` is integrated as `e53b0b941ece061b60f1c36762356df7431b0ee1`; focused 30/0/1, worker 343/0/10 and mutation probes passed. Report `art_2dde3fc9-32b8-4191-8b7e-6dd639e9a9b9` / `av_8f29f090-15be-46c7-861c-2fe59a92689a`; independent ACCEPT is banked in `art_1074b344-41bd-44ef-bf45-0f340dfcc3fe` / `av_67e84061-346c-4640-a534-6764dd850e81`. Native lifecycle and concrete admission are integrated below; PR creation UI and the broader PR inventory remain required.
- Document/Git `11a3a8e` / `d138c7c` and worktree provenance `85af97c` remain independently accepted in `art_3e0be915-12bf-4e65-83e6-d0f9d7497cca` / `av_1df8c8ce-f167-4c14-b330-62ff319dde65` and `art_f3fd1bca-80c8-490e-8af6-64325190b1a7` / `av_fd1b8438-8041-4ee7-831e-2ea6af642900`.
- LocalWorkspace hardening `ced6f0a` → `e193ccc` is independently accepted (`art_dba381fa-731c-42c1-ba6d-86da28dd4fab` / `av_a704fcc6-e83a-4c16-83b2-836064ff6807`). Parent LocalCheckout integration/provisioning/source routing is runnable with real fresh light creation and dark reuse while Review stays pinned (`art_0a89eee9-f27c-4ced-b637-79d9a1c13895` / `av_604db5a1-9a4c-4dc7-838b-916de425c7a4`). Full attach/cleanup/publish acceptance remains open.
- Worktree store cross-process authority is now independently accepted at `8991f51e33894fa47ddd995c4d83ca124e06be46`: persistent private descriptor-held flock around existing operations, five-second bounded admission, crash release, store-before-Git ordering. Parent 20/0/1 helper ignored and strict Clippy passed; removing flock fails the regression. Source proof `art_19102915-d42c-414f-9de5-89c36cc7cd08` / `av_2410ea5e-12c4-429a-ab35-29498c2b2635`; independent review `art_eb71a293-e6a9-4ab3-b878-993d3e820412` / `av_6fc50f8b-62fd-4779-9c69-e19d868e7cf4`.
- Rebase backend is independently accepted at `651037f5faadab3f9597f0a9c34c8ab17c8af3bc` (`art_b0e7ca6b-153f-4d77-a24d-66439823e8b7` / `av_a6c2a0a7-3e50-4854-a115-4e6dcdc0bb89`). Native source through `0593f76` renders plan/edit/conflict/stash/result with document/action interlocks. Worker final 24-file evidence `art_eff4ed34-c444-4a63-849f-5ce8ce7fbba3` / `av_7c153c61-3afa-4331-8f25-c7b2067d0088`; parent focused 27/0/1, strict Clippy and fresh dark native proof `art_5da93e78-86b4-4e58-8a33-f4edbd601560` / `av_451cc9e2-8e83-4cd3-9454-853d3a0a5328`. Independent HOLD N1: plan inputs can change after confirmation freezes an older payload. Correction `e4c94262ce6ebfcfa24338cde0405c1e53996e55` → `565b61f` freezes visible inputs and independently rechecks them at Confirm; verified worker artifact `art_41ac19e8-6e89-46af-a912-a73ee4679baa` / `av_3de1c397-dee2-47b8-97db-9d09013e21ee` (19 files, 6,825,169 bytes). Parent 342/0/10 and fresh light/dark native scenes passed. Independent N1 ACCEPT at `565b61f` is banked in `art_77741be7-48e4-4dc3-8556-20969faca6fa` / `av_e6c31e5f-0551-491f-aae2-87ab1d63f8ff`. Parent panel width `b8bb5f5` and default-test feature gate `5494a25` also pass both Clippy configurations. Earlier same-current-branch failure remains unclassified with retained journal; later passing instrumented probes do not classify it. The three-way editor is integrated below; PR-source publishing remains required.
- Parent LocalGit `c3a0bc51ba094da84c25c7d3d945a6bb6d064883` adds explicit local-branch → remote-branch lease publishing, pinning the displayed immutable local OID. Seventeen temp-repository tests and strict scoped Clippy pass (`art_f923153e-303b-4883-b4a6-9a24d8f7dbf9` / `av_e54c00bb-ade7-4b47-b05e-cae5f7f90270`). Native verified target routing and independent acceptance remain open; no live push.
- Optimized packaging profile `a401860` builds without application debug assertions or smoke, with arm64/macOS 15 metadata, complete extracted manifest and strict ad-hoc signature (`art_5d45f3c0-34d0-45fb-950b-91159d7abc51` / `av_7ca15477-7651-4262-ad5c-804d46cd2799`). This historical source is not a release candidate. Original icon `500ac49` is bundled and verified (`art_317f0cb4-f297-4028-a530-a12c02a25ae1` / `av_2791c8ea-e9f1-4352-8f1b-30a427965c5e`). Final optimized package, Finder/oldest-OS, Developer ID and notarization remain open.

- Parent `3d51bb3` persists horizontal positions per raw-safe file inside each comparison session, removing cross-mode leakage from the old tab-wide map. Focused file/legacy-record, same-path A→B→A and actual Store round-trip tests plus strict Clippy pass. The combined suite at `e53b0b9` exposed a LocalWorkspace journal lock lifetime failure. A deterministic duplicated-descriptor fixture reproduced it. Separate correction `431375753b269390baec4ba84e763731f9de51fc` → `73368805b1967e7c9ecb6056a592654d190813e2` explicitly unlocks on normal and post-acquisition error paths, without timeout or unlink changes. The corrected combined suite passes 350 tests, 0 failures, 10 ignored. Prior failure and isolated pass remain in `art_6f857d8f-2905-4bf1-b7ea-8a236f494ec5` / `av_50e58892-8bc1-4549-b3a1-087b9de87200`, alongside passing light/dark comparison evidence and focused horizontal tests.

Prior stack/three-way integration checkpoint was `4b5c43e627f9823abf28c4e7306008c11f1dd9dc`. The stack backend at `3aa339bce2d8abbdb4e5bcd04cd4b47058785830` now proves every remaining frozen layer head occurs in order on the boundary-to-tip history. The advanced-lower-branch regression failed before the correction; local and remote fixtures now pass. Independent ACCEPT is banked in `art_ab0287b8-6799-4a13-86dc-0a53cf668a43` / `av_5bea43ce-bb2a-46f4-8f50-8be1f569a28b`. Native stack controls and personal-correction persistence remain open.

Three-way sources are integrated from `a8153eb7e24ee597abd3c2961a766d46bc877e7f`. Parent `b932fc7` replaces eager per-line elements with virtual lists and sparse indexes; `4b5c43e` fixes measured-row font binding so the horizontal end remains readable. Independent ACCEPT is `art_78e2be8a-0480-4081-853d-25783beba41e` / `av_5b437274-e225-4ef5-aedf-73cc4ef0ca38`. Parent light/dark actual Git workflows pass; 5,003 source rows render at most nine at once, with END tokens visually inspected. Full unchanged rerun passes 378/0/10 and both strict Clippy configurations pass. Two earlier worktree hook-handshake failures remain unclassified; isolated and full unchanged reruns passed, with no timeout/source change. All logs and final captures are banked in `art_c9137e85-a725-4f4a-961f-2ccb5c42844b` / `av_46233e7a-167b-4402-8f83-b30946af40f6`.

Native lifecycle source `09de22e` → `f1a994f` integrates Overview metadata/state/reviewer/label/assignee actions, Activity discussions and one shared target mutation authority across DraftStore and ActionJournal. Worker evidence `art_57f766c5-08aa-4f11-be0c-3b7da7a482f9` / `av_a97df595-9ed4-4fe9-86f8-4ee0c6911ff8` is fully verified. Independent review held dirty-form discard and in-flight confirmation replacement; authority was not a HOLD. Parent `ce36e23` clears only matching sent-form witnesses, preserves newer/unrelated drafts and gates begin/prepare/cancel while a request runs. The regression failed before the fix and passed afterward. Choices page at 18 (`a7645b4`), comments at 20. Corrected full suite: 392 passed, 0 failed, 11 ignored; both strict Clippy configurations passed. Final light/dark scenes at `ce36e23` use a real public read and synthetic confirmations with zero mutation transport. Independent recheck closes those two HOLD items: `art_2565eaa0-41b8-46e2-b940-3b2f8252f6ee` / `av_0e3c6009-79ef-4452-a483-8dcb1f93f1be`; original report `art_ee983d2a-8b65-4a87-860b-e3dbbd9930d4` / `av_06b57ab6-d46b-4508-91f2-10166cc3510c`. Parent `c933f3e` corrects two Cancel status handlers during active writes; strict all-feature Clippy/fmt/diff passed, without another full-suite or independent-review claim. Integrated report, logs and scenes: `art_9a627804-18d6-4f30-888c-c42b61397ef2` / `av_9eed8ae2-d81b-46e1-8ecb-cec3ecc06f07` (18 files, 5,376,527 bytes), full readback/download hashes verified. Mixed-version processes sharing one data directory still require migration consideration; the original preview is isolated.

The current PR feature inventory draft is banked in `art_2adccd06-cc44-4998-8c01-e5795eae331e` / `av_199744c2-0efe-4b7a-b413-5ed44415122c`, revision 2, pinned to `0b7e2f6`. Show, full readback and file hash are verified. Native creation, Stack presentation, explicit source publishing and bounded native notifications now have the scoped acceptance recorded below. Existing-pending file comments and submitted-review summary editing/persistence now have the bounded acceptance below. No-existing-pending file flow, other published review controls, reactions and broader CI/job/log actions remain open. Activity still displays at most 20 reviews and 30 threads; Checks displays at most 40 checks. These display bounds are separate from provider completeness. This inventory records remaining work without resolving Q65 or reducing scope.

Latest source integration: publish `e1453135` → `6e6d5a9`, notifications `a60a1ae9` → `104cc9b`, notifications export `f634dda`. Combined locked all-target/all-feature suite passed 426/0/12 across 21 outer executables; strict Clippy passed. Root `b00c547` adds unique tab-lifetime admission for metadata/details/lifecycle/review/reconciliation read callbacks; a close/reopen regression and a failing mutation probe pin the delayed-reply fix. Presentation `3b1a92d` keeps Publish to PR compact, exact identities behind details, and confirmation reachable through Local Changes scrolling. Actual 1440×900 and 1040×820 light/dark viewport captures now differ, expanded detail scroll is 86px, and exact local-bare published OID readback passes. Strict Clippy and native checks passed; the full suite is attributed to `f634dda`, not a later presentation rerun. Verified parent report, logs and captures: `art_5447ad54-d5c1-4f93-82bb-f356b6cc1d4b` / `av_80538836-dd78-4763-85ee-fad692712365` (25 files, 3,550,969 bytes).

Publish recovery correction `1fda5b1efac6963713264042e918cb5afbdc109b` is integrated as `ea28933`, preserving the parent presentation. Recovery observes the exact journaled target, source branches, checkout identities, configured destination and remote ref. A missing ref or changed identity retains the record. A matching nonmissing ref allows explicit acknowledgement of observed current state; an old or different OID does not prove that the attempt had no effect and never causes replay. Handoff `art_4f889839-ae5d-42e5-9e23-c444b96be79f` / `av_23cfc1aa-62d9-45ee-960f-24c5c1cd19f4` is verified. Independent recheck is active; the original HOLD report remains `art_2fab39e5-a07d-456b-a523-bd0e73076bfb` / `av_c8abb32c-110c-43da-949d-3d00b2933a08`.

Native Stack `f7c689c55be7ad46c83e09d8ebb56fd41feaad90` is integrated as `e63b4a8`. It owns a separate immutable net comparison, controller lifetime and generation, background CAS personal corrections, virtualized rows and narrow Layers disclosure. Parent constructor conflict resolution preserves both the Stack controller and PR-tab lifetime. Full worker evidence `art_e5fd6c23-763b-4fb3-8689-7a6b1a39c82f` / `av_3d5eea5c-7e37-4a7d-b2bc-7d81c07c5cdd` (11 files, 3,395,759 bytes) is verified. Parent inspected corrected wide, narrow, horizontal-end and final disclosed-layers scenes. Independent source review is active. Q60 multiple tips remain unavailable; Q61–Q64 actions are absent.

Parent read/save callbacks at `052cd39` require the current tab lifetime and latest per-key save sequence. A real DraftStore/controller regression covers reordered completion and reopen; removing either guard fails the test. Verified proof: `art_d4a7c97a-a5ee-4bee-ae8f-160745faac83` / `av_76720f9e-286a-4832-8700-5b0d11dba7bb`. At `0733fc2`, a failed pending-review read preserves successful fresh PR details and previous pending data, disables pending completeness, and does not reconcile or save review linkage. Focused application suite passed 104/0/2. The integrated `e63b4a8` all-target/all-feature suite passed 447/0/12 across 21 executables; strict Clippy and native build passed. Integrated native scenes are in progress; no physical-input or live-write claim.

Notification enumeration remains under correction for valid non-PR subjects and endpoint page size (`art_57a400a6-7bbb-40c0-9bb9-01d065c02515` / `av_6c84e85f-5f56-4909-8f77-a10be238df76`). A diagnostic rerun of its ordinary fixture proved the three-second fake credential subprocess timed out before repository API dispatch. Only that fixture budget may increase to ten seconds; production and dedicated deadline tests are unchanged. Original failed and diagnostic output are retained.

All three bounded reviews have completed. Publish recovery ACCEPT is `art_9f077aef-da03-4f2b-bc40-31a00f8bde6a` / `av_5f881dd8-32d7-4aa5-b258-f7c50b9476f7` at worker `1fda5b1` / parent `ea28933`. Notification enumeration ACCEPT is `art_5c65907c-381d-4cde-8b34-8dbce7877ab1` / `av_b4578d44-e00c-492e-8779-dbda7ec728d8` at worker `d1276a60` / parent `ed6aabc`. Stack and separately scoped parent callback ACCEPT are `art_2e41f233-6641-4b2e-ac75-e1183367a13c` / `av_2fc1369b-a472-4d1a-a617-c6c0dc287e7c` at `f7c689c` / `e63b4a8`. Parent verified every report manifest and full readback hash. The integrated `ed6aabc` full suite passed 454/0/12; both strict Clippy configurations passed. Integrated light/dark Stack runs exited successfully and the full END marker was visually inspected. Report and logs: `art_07911a39-efc7-4c85-8c01-e5dda80cea2f` / `av_fb8a229d-7b66-4aa3-aeca-ef7de3c6bda3` revision2, 22 files / 2,714,337 bytes, full download hashes verified.

Native BB assignments dispatched from exact `ed6aabc70cc81b9963cfb0c4034c80db7d9ea46a`:

| Worker | Deliverable | Workspace / verified brief |
| --- | --- | --- |
| Sol `thr_rtfg32b8ce`, completed and archived | Native PR creation, frozen form and durable admission | `/Users/matias/Development/cibergit-native-pr-creation`; `art_8aef5166-d8b6-4b89-81b0-d0d24648be80` / `av_6c0b50a1-fcf3-4626-a252-0ba2ac895d81` |
| Sol `thr_k57rh8p6my`, completed and archived | In-app unread, exact mark-read and explicit macOS opt-in | `/Users/matias/Development/cibergit-native-notifications`; `art_6d2a4d76-2f06-49f4-b3a3-5706504b4313` / `av_1a003a15-39aa-4414-bc70-4cd7d08e95a2` |
| Sol `thr_hs8k2a8rgv`, stopped before final proof | Attachment verification and conservative managed cleanup | `/Users/matias/Development/cibergit-native-checkout-cleanup`; `art_be9f7d23-a611-42b9-88de-112673d40312` / `av_940306be-d8ea-47a1-8831-eb7f5efa70c9` |

Parent `3b00b0426d0325f79ed2108c1d74588b974c80b8` bounds native PR metadata/details/lifecycle and sidebar reads to one active request per lane. Polls no longer supersede slow reads; explicit refreshes coalesce one follow-up and discard the earlier observation. Mutation-invalidated details callbacks release the read slot without installing stale evidence. Application tests passed 117/0/2, focused tests 12/0, both strict Clippy configurations passed, and overlap/lost-explicit mutation probes failed as expected. Verified report, logs and exact patch: `art_f1ba5449-0493-452a-bfb1-5b80304fd0a6` / `av_d8212b4b-75b3-4d32-8876-bc875739764f` (8 files, 37,403 bytes). Independent review and live slow-network timing remain unclaimed; the last complete integrated suite remains `ed6aabc` 454/0/12.

Cleanup brief revision 2 is authorized at `art_be9f7d23-a611-42b9-88de-112673d40312` / `av_940306be-d8ea-47a1-8831-eb7f5efa70c9`. Scope includes the narrow rebase-panel lease hook. A shared lease on the exact checkout-directory inode covers each workspace lifetime and independently owned background tasks. Cleanup may acquire exclusive authority only after fencing and releasing its own idle workspace; other workspaces/tasks refuse it. Cancel restores a validated shared lease before editing resumes. If an uncertain removal has already removed the path, the entity remains fenced but the accepted manager's read-only exact-operation reconciliation remains available. The Darwin directory-inode flock fixture is worker evidence. The worker stopped before final native proof; its workspace remains untouched and no completed handoff or native acceptance is inferred.

Native PR creation `7ca8506` is integrated as `cbb026d`, preserving parent read admission. Parent corrections `e603f2c` save the latest form before recovery navigation and open the exact clicked historical PR; `2ee4e9c` separates account drafts, imports only matching legacy text without replacing it, and allows unavailable saved repositories to close without overwrite. Independent review `art_2eb21033-37ac-4a09-845f-305a64969c5f` / `av_fed405b8-1714-47b7-a83c-ff6ea928282c` accepts the creation authority and recovery navigation. Addendum `art_d2424dd3-6533-47ec-b59a-713e08280dc7` / `av_d9379aea-62a3-48cb-94fe-ce14f51be45a` accepts the account correction at exact `2ee4e9cbb7af091ba026492cd8a83d4380fe3716`. Earlier parent `3b00b042` remains outside that independent review.

Final creation integration validation at `2ee4e9c` passed 491/0/13 across 21 outer Cargo executables, both strict Clippy configurations, formatting, and native build. Four parent mutation probes caught the intended guard failures. Fresh light/dark native preparation and synthetic moved-head acknowledgement passed with focus disabled and zero creation transport. Report, logs, source patches and captures are banked in `art_9b44a108-c54a-478c-99fd-4c507b99dcc1` / `av_7f06a84f-3e9f-4b48-88c5-9eb6f3902f03`, revision 2, 44 files / 2,146,609 bytes; manifest, full report readback and every downloaded file hash were verified. Validation used a clean detached checkout and excluded existing coordinator notification edits. Earlier quick totals counted six nested helper-process results; correct `cbb026d` and `e603f2c` totals are 487/0/13 and 489/0/13.

Native notifications are integrated through source `54736934e9fe6c4da37241e33319cc82e896423a`. Worker `134198a` → `fd38062` adds the Alerts panel, local exact-page mark-read and explicit per-account macOS opt-in. Parent corrections keep one read lane per account across repository changes, preserve individual backoff on partial results, reject stale schedule/mark callbacks, and filter snapshots without deleting removed repositories' unread events. Consent is captured when a poll starts and checked at admission; a background-acquired registry lease spans the platform request to serialize cross-process disable. Both live and persisted response routes are bounded to 512. UI/consent independent ACCEPT is pinned to `73fa888` in `art_eb654342-587e-4b38-b9ad-ec6c9a34b5c6` / `av_d723e13d-e474-4d2a-aa8b-ba72bdb4edf1`.

The retained body-mention edits are now committed as `f20181355fabb0bf982f7dc3e0211169c7586485`, independently ACCEPT in `art_2b227b56-97bc-4de5-9b93-7deb168124fb` / `av_65ed719b-081c-4f46-86f3-7e28236bb72d`. Classification uses native timeline recipient/ID/repository URL/timestamp evidence, with no author attribution. Comment/team mentions and sticky-reason evidence remain incomplete. The provider blob is unchanged at final integration.

Notification validation: serial default **460/0/13** across 20 outer executables and all-features **514/0/14** across 21 at `73fa888`; nine guard mutations fail as expected and are restored. Final `5473693` changes only an unused test helper's cfg after Clippy found it exposed in a non-test ui-smoke build; both strict Clippies, focused **20/0/1**, fmt and native build pass on that final source. Fresh focus-false light/dark synthetic notification captures pass with zero OS calls/prompts and zero remote writes. Parent evidence `art_dd6a27c8-7cda-46ac-91be-2503ecb0c020` / `av_6b071eab-e000-497b-921b-80e52dbf4aa3` contains 50 files / 1,372,647 bytes; manifest, full report and every downloaded size/hash were verified. The earlier parallel `965cfc1` failure (508/2/13) is preserved: one fake-credential timeout before API dispatch is diagnosed, the other unexpected incomplete result lacked its reason. Both pass serially; no timeout was changed. Full M2/V1, conditional caching and physical OS delivery remain open. Long UI scheduling delays can exhaust another account's roughly 500 ms delivery-lock budget; that event remains in-app unread without an OS retry.

Consumed workers are archived/stopped. Preserve the original preview state; the formerly recorded PID43840 was already absent before creation integration validation. No live remote writes or auth/global changes. Remaining notification coverage/sync, cleanup and full PR inventory remain required. Q60–Q66/Q68 and signing, physical-input and live-write gates remain explicit. Historical unclassified worktree-hook and same-current-branch incidents remain disclosed. G1–G5 and full V1 remain open.

Offline collaboration cache is integrated at `d9e1cec4e7455476ebbfabaa5ff26cc20ef7a118`, with owned source identical to worker `66d4b96`. Separate cached details render Overview, Activity and Checks without granting fresh permissions, pending linkage, Since-review baselines or inline mutation controls. Durable reservations and predecessor checks fence older writes; bounded retention preserves invalid and foreign entries. Independent source review and addendum found no HOLD at their exact pins, with product acceptance explicitly left to integration proof.

Parent validation on `d9e1cec` passed default **475/0/13** and all-features **529/0/14**, both strict Clippies, formatting and native build. Four worker mutation probes failed at the intended retention and fresh-data assertions, then restored exact source. Integrated explicit-PR populate and light/dark restarts passed; all six scenes were inspected. Evidence `art_3d24796c-a510-478e-b7c3-3291b714912d` / `av_6a3af1a7-871e-4edf-b4c9-2e13cb29a779`, revision 2, contains 49 files / 10,523,805 bytes. Parent verified the manifest, full report and every downloaded size/hash. This accepts the bounded cache path only.

The stronger parent restart using only --data-dir failed at d9e1cec before a PR tab became ready; its failed logs remain in the cache packet. The correction bacedb1c1f31d14f3833bee05f70dbdea6c12ec1 is integrated and accepted at dcd2e94bbd7875c81c3fc3d9936f4dae87b0abfc. Saved tabs now reopen in order with exact account/repository admission and their canonical pin, including closed PRs absent from the open list. Missing siblings stay on disk; saved metadata grants no write authority. Parent locked serial suites passed **485/0/13 default** and **539/0/14 all features**, both strict Clippies, fmt and native build. Five integrated native phases passed: populate, ordinary mixed-freshness light/dark, and ordinary all-gh-refused light/dark (seven refused calls each). All eight captures were inspected. This proves provider refusal, not physical disconnection. Parent packet `art_390149a0-6f08-4364-9412-a15907496fd8` / `av_31d63147-a661-447a-abdd-c3bd264f97d3`, 43 files / 7,186,027 bytes, was shown, fully read and freshly downloaded with every size/hash verified. Worker packet `art_009a5ba2-eecc-41ca-9571-701fe783609a` / `av_1da3dac2-0073-4fa8-a7ac-a9f41e26d7e6` and independent no-HOLD addendum `art_1d3f92c3-74f0-4a29-8a1f-66f18fa87ca3` / `av_480aff9d-242d-4392-a8a8-93f1ea83b43e` are verified. Conditional sync, broader offline workflows and G1 remain open. The duplicate offline diagnostic row is corrected and visually verified at a73d7f9; verbose diagnostics remain M5 presentation work.

Author-owned submitted-review summary editing is accepted as a bounded action at `a73d7f9f369caebb14f792f572af99965be96211`, from worker `609784c3385bcefc5449c3ad95f12a53deb593ee`. Fresh capability and exact preflight/acknowledgement guard the mutation; confirmation binds the full old/new tuple and a unique generation. Recovery success/error application checks the originating workspace and tab before touching UI state. Independent B1/R1 findings are closed. Full parent suites at production twin `5c1d28f` passed **496/0/13 default** and **550/0/14 all features**, both strict Clippies, fmt and native build. Later changes affect only the ui-smoke function; final a73d7f9 check/Clippy/fmt/build passed. Final integrated light/dark real-read plus synthetic-owned-review handler checks passed for cli/cli#14130 and #14259, with complete top/continuation disclosure inspected and zero mutation transport. All five final startup/cache regression phases passed, including seven refused gh calls in each refused theme.

Parent packet `art_846da1d7-dcbc-4a9d-89f7-46228780ecd3` / `av_6d4e7743-99c7-41c2-bcc3-632c5422c8db`, revision 2, contains 94 files / 24,237,190 bytes. Worker packet `art_d2cf5118-8bbf-4304-af56-0ac67595a2e3` / `av_5db29ec1-1336-4a94-9bc6-cf415b6a79f4`, revision 2, contains 68 files / 31,816,609 bytes. Both were shown, fully read and freshly downloaded with every size/hash verified. Five worker mutation probes caught their intended guard breaks; source was restored. Independent actual apply-path source acceptance is `art_f119e075-d370-4645-9158-6e5f3dff4d98` / `av_7fd70656-4cee-43c2-82ba-2d6328bf1ef1`. The feature is not a live-write or whole-M2 acceptance.

Submitted-review draft persistence and existing-pending FILE comments are accepted together at `664e30fc8800a5dbcd728703376cdd42ae0b977c`. The earlier memory-only submitted-draft gap is closed: exact unsent text survives ordinary restart and safe tab close, with per-account/repository/PR/review CAS, tombstones, conflict recovery and historical-only restored source metadata. FILE comments retain a real whole-file target and fresh pending witness; Unknown subjects are read-only at both UI and provider boundaries. No pending review is created implicitly and no immediate-post fallback is used.

The shared-input integration restores line/file and submitted bodies through the same exact tab ownership. Close requires line/file durability before the submitted save barrier, freezes both inputs and blocks later PR operations until that barrier finishes. Failed saves preserve text and reopen editing. All independent source HOLDs in these slices are closed. Parent full suites at `7c819bb9abeb8972cb6f12cb5bacc1bd62fe7205` passed **542/0/13 default** and **597/0/14 all features**. Later adjacent guards and mechanical lint fixes separately passed final-tip all-target checks, both strict Clippies, formatting, native build, Root 1/0, submitted 17/0, FILE provider 3/0 and participation 14/0. Four parent guard mutations failed the intended Root assertions and were restored.

Eleven final-tip native phases passed: light/dark draft populate and restart, light/dark FILE confirmation handlers, and five startup/cache regression phases including seven refused gh calls per refused theme. Full confirmation disclosure was inspected; the restored-draft screenshot is a viewport, with exact full text established by the runtime witness. All reports declare zero mutation transport. This is not physical-input, live-write, whole-M2 or V1 acceptance. Parent packet `art_84fce191-e344-4c02-89ac-d2d5164f8e5a` / `av_b1ed806e-f15f-47fa-9abf-27584fafcc8c`, 133 files / 29,067,304 bytes, passed manifest show, full report readback and fresh-download size/SHA verification with no extras. It includes exact source, checks, mutation logs, native reports/captures, historical failures and independent reviews.

M1-SYNC-NOTIFICATION-CONDITIONAL is accepted at `2cf6fd80baf0cee9d21294c6f0c4a0be65a3d907`, integrated from worker `b1effafb96ef6659b2f0ca2ed74e5fdf37478552`. Exact page identity and bounded conditional caching still require fresh event hydration. All refresh triggers respect account-specific server floors; rate responses stop later requests using the same credential. Independent source review is CLEAR in `art_dee841c2-7f45-4891-97aa-755a1abe8179` / `av_9510b49d-f4d8-405f-aac0-c4c362dd8bba`. Parent full gate at prior `d352956` passed default 514/0/13 and all-feature 568/0/14, both locked checks and strict Clippies, formatting and native build. Final one-file parser correction separately passed parser 5/0, both checks/Clippies, formatting and build. Final integrated light/dark native notices passed and were visually inspected. The first parent launcher used the wrong output variable and timed out before smoke entry; its failed evidence is preserved. No live304, OS delivery, physical input or provider mutation is claimed.

Parent packet `art_7b5f0f92-193d-490b-80aa-5a0584f3e0c6` / `av_bf6eba8f-5187-4e71-a620-7db2651b9f28` contains 53 files and 1,606,195 bytes. Manifest, full report and fresh-download size/SHA verification passed. Worker report `art_325fb2da-a377-453d-982d-f77503ccc9cd` / `av_653ed92e-acd3-4694-9952-995ff46eef88` links the separately verified source, focused/mutation logs and native packets. The worker and reviewer are consumed and archived. General conditional sync is recorded separately below; comment/team mention evidence and physical OS delivery remain open.

The draft and FILE workers and their consumed reviewers are archived. Their separately parent-verified packets are `art_cf0cf538-7ab8-48d5-8bad-c1e44c5cadd5` / `av_319a3b4f-ed18-44ce-9b0e-829c8771d72d` and `art_a20c6467-e55e-40e9-8469-1958644b8f02` / `av_ffc8a248-0079-4498-8cab-8615ea5fbaea`. Final independent integration/lint acceptance is `art_9a65ade9-8312-40ef-839f-a442c81301c1` / `av_c22af5c2-ff61-4a4c-9e9a-c7e912529057`. Historical ef669/7bd/43ff failures remain disclosed in the combined packet; they are not current source HOLDs.

M1-GENERAL-SYNC is accepted at `b5ff71dafdbb0b0226ee0858b883cce7165981b4`. Worker production `9099b2e3a8e64a2de99fb4ef0fe67b8ced9a36f3` and parent twin `583a9d74ae68a4fe24fa43b4adcca7f487e39540` passed independent source review. Conditional REST sidebar/PR metadata, account scheduling, server-floor preservation, fair follow-ups, and replacement-workspace callback rejection are integrated without cached GraphQL or mutation authority. Parent full suites at 583 passed 564/0/13 default and 619/0/14 all features, both locked all-target checks, both strict Clippies without allowances, and formatting. Later harness changes and the final smoke-only notice correction separately passed focused checks, strict Clippy and native build.

Seven parent native phases passed across explicitly attributed pins: general-sync light/dark at `9656210934b765d321f4e688b9c52280ae73cd78`, then five startup/cache phases at b5ff, including seven refused gh calls per refused theme. Pinned comparison, line/FILE/submitted drafts and journal state survived deferral; saved closed PR and cached collaboration restored without fresh write authority. Fourteen images were visually inspected. The first mixed restart failed an obsolete notice literal; the corrected assertion still requires the exact failed-read message and a recorded read attempt. Worker native scene failures and seven discriminating mutations are preserved, separately attributed. No live 304, mutation, OS delivery, physical-input or whole-app mutation-throttle claim is made.

Parent packet `art_76dc4294-5711-485f-92e0-c4e1b145e400` / `av_1c64d060-b56f-4ac7-88b2-03bdad3eb6ab`, manifest `47e3e381c8eef559c03b588ae7d93f188ecabf50ab071dc4c9ed6c15402d35d4`, contains 115 files and 23,995,281 bytes. Full manifest, untruncated 9,729-byte REPORT readback and every fresh-downloaded file size/SHA matched with no extras. It includes independent report `art_f4bf751f-8649-4754-aa74-bba32f91619b` / `av_d192aab7-8c7f-49d0-8b72-9df1cf2cc5dc` and worker packet `art_8a512de6-26c5-4315-912f-8503c9854b75` / `av_e7ec7c3d-a968-4e96-b320-6a3f72f14ab1`. Worker `thr_9t36atauss` and reviewer `thr_nu2fei65is` are consumed, archived and stopped. This closes the bounded general-sync slice, not all M1/V1 acceptance.

Remaining PR control discovery is complete in `art_df341191-3f50-4465-9654-a605949bb79a` / `av_3e58c927-2172-4154-b756-0d5229368aee` at source `664e30f`; the parent verified the full manifest and report read-back hash. M2-REACTIONS is assigned to `thr_vrkqm5ghjw`, branch `worker/pr-reactions`, worktree `/Users/matias/Development/cibergit-pr-reactions`, base `664e30fc8800a5dbcd728703376cdd42ae0b977c`. It covers one GraphQL add/remove action for PR, review, discussion-comment, and review-comment subjects, with fresh selected-viewer authority and durable uncertainty handling. Source `364e331bf17cdc100c81fce361beb753f4ab21be` and cached-thread correction `978463de4a10ef00559aa660b8d38873396c8d81` are under independent review by `thr_fhxd37nrdz`; focused worker validation is in progress. No implementation acceptance is claimed. Removal has no server-side expected-reaction-ID condition; the final preflight and exact acknowledgement cannot eliminate the intervening remove/re-add race. No-existing-pending workflow and dismissal authority remain unresolved; these assignments do not approve Q65 or any other unanswered policy.

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

| Task | Deliverable and acceptance rows | Milestone | Dependencies | Assignment | Status | Integration commit | Artifact IDs / versions |
| --- | --- | --- | --- | --- | --- | --- | --- |
| M0-ENV | Validate toolchain, compatible pins and native editor proof; F01, F02, E01 | M0 | None | C0 | verified | 5abd59c | art_6e2ffc7c-6d8e-48e4-9c32-a21d024287ee; art_38b8ddbc-509d-449c-bab5-85e11a1c88e4 |
| M0-BUILD | Reproducible build/run instructions, license inventory; F03, E02 | M0 | M0-ENV | C0 | verified | 5abd59c | art_38b8ddbc-509d-449c-bab5-85e11a1c88e4 |
| M0-CONTRACT | Establish only needed shared state/provider/operation contracts; E03, E04 | M0 | M0-ENV | C0 | verified | 2734890 | art_4093b33c-94aa-410e-88b3-3ff83175ee7e |
| M1-ACCOUNT | Existing gh accounts, provider transport and isolation; F04, F05, W02, E05, E06 | M1 | G0 | Active assignments above | running | Backend and native tree/layout16442a8 above; comparison selectors pending | Reports above; G1 evidence pending |
| M1-REPOS | Explicit local/remote repository setup and personal persistence; F06, W01, W03 | M1 | M1-ACCOUNT, M1-STATE | Active assignments above | running | Backend commits above; UI pending | Reports above; G1 evidence pending |
| M1-STATE | Durable session, cache and drafts; E04, E07 | M1 | G0 | Active assignments above | running | Backend commits above; UI pending | Reports above; G1 evidence pending |
| M1-SIDEBAR | All-open rows, ordered groups, saved filters; W04-W08 | M1 | M1-REPOS | Active assignments above | running | Backend commits above; UI pending | Reports above; G1 evidence pending |
| M1-TABS | Restored PR tabs, file navigation, header/panel frame; V01-V03 | M1 | M1-REPOS | Active assignments above | running | Backend commits above; UI pending | Reports above; G1 evidence pending |
| M1-DIFF | Immutable local/remote comparisons, adaptive text diffs; V04-V08, E08 | M1 | M1-TABS | Active assignments above | running | Backend commits above; UI pending | Reports above; G1 evidence pending |
| M1-SYNC | Polling, explicit revision advancement and offline read; S01-S04, R04, E09 | M1 | M1-DIFF, M1-SIDEBAR | C0 | running | Cache and ordinary saved-tab startup accepted at dcd2e94, including all-gh-refused light/dark; conditional notification polling accepted at2cf6fd8; bounded general conditional/offline proof accepted atb5ff71d; whole-scope cadence/physical-input acceptance remains open | art_390149a0-6f08-4364-9412-a15907496fd8 / av_31d63147-a661-447a-abdd-c3bd264f97d3; G1 pending |
| M2-DISCUSS | Inline threads and Overview/Activity content; V03, R01 | M2 | G1 | C0 | running | Native31672ac/22a4d15 and reconciliationc3bb8bd accepted; wider discussion controls pending | art_25a4a81b-bb27-4e01-83ea-423c7ddc5ab5 |
| M2-REVIEW | Pending/immediate comments, draft recovery, explicit submission; R02, R03, R05, E10 | M2 | M2-DISCUSS, M1-STATE | C0 | running | Backend9e6e65a/56662a3 accepted; nativec3bb8bd reconciliation independently accepted; remaining native scope open | art_25a4a81b-bb27-4e01-83ea-423c7ddc5ab5 |
| M2-PROGRESS | Full/commit/range/since-review selection and viewed progress; V08, R06 | M2 | G1, M2-REVIEW | C0 | running | Native4dee335 independently accepted; horizontal3d51bb3; local completion pending | art_94e5138f-e6a3-47b0-88b9-a3be5c6d346d |
| M2-PR | PR lifecycle, metadata, reviewers and checks; F07, R07 | M2 | Accepted provider; G1 for final gate | C0 | running | Native lifecycle ce36e23, submitted edit a73d7f9 and durable drafts/known-pending FILE664e30f accepted; broader PR/CI inventory pending | Integrated lifecycle evidence above |
| M2-MERGE | Current-head requirement and explicit merge controls; R08, R09, E10 | M2 | M2-REVIEW, M2-PR | C0 | running | Backend56662a3; native confirmation22a4d15; reconciliationc3bb8bd accepted; final gate pending | art_25a4a81b-bb27-4e01-83ea-423c7ddc5ab5 |
| M2-NOTIFY | Personal unread state and opt-in system notifications; S05 | M2 | M2-DISCUSS, M2-PR | Coordinator | running | Store/enumeration and bounded native UI/consent/body-mention accepted; conditional notification caching accepted at2cf6fd8; comment/team and physical delivery open | Notification integration and independent reports above |
| M3-WORKTREE | Create/attach/reuse persistent PR checkouts; L01, L02 | M3 | G2 | C0 | running | Backend85af97c accepted; nativea69cc54 create/restart passed; attach/recovery/cleanup pending | Reports above |
| M3-EDITOR | Focused editor, Edit locally, file browser and quick-open; L03-L05, F08 | M3 | M3-WORKTREE | C0 | running | Componente193ccc accepted; PR embeddinga69cc54 real-read native passed; full integration acceptance pending | art_dba381fa-731c-42c1-ba6d-86da28dd4fab |
| M3-EXTERNAL | Disk/Git observation, safe saves and reconciliation; L06-L08, E11, E12 | M3 | M3-EDITOR | C0 | running | Document11a3a8e and componente193ccc accepted; full PR flow evidence pending | art_dba381fa-731c-42c1-ba6d-86da28dd4fab |
| M3-GIT | Stage/unstage, commit, fetch/pull/push and branches; L09, S04, E06, E12 | M3 | M3-EXTERNAL | C0 | running | Gitd138c7c/componente193ccc accepted; temp native stage passed; PR-target publish mapping pending | Reports above |
| M3-CREATE | Secondary normal/draft PR creation; L10 | M3 | M3-GIT, M2-PR | Coordinator | verified | Native form/admission and recovery independently accepted at2ee4e9c; fullG3/live-write gate open | Creation integration report above |
| M3-CLEAN | Safe persistent-worktree cleanup; L11 | M3 | M3-GIT | Coordinator | blocked | Accepted backend; worker stopped before native proof; unaccepted workspace preserved | Brief above; no final native handoff |
| M4-STACK | Native/inferred relationships, corrections and aggregate diff; K01-K03, E13 | M4 | G3, M1-SIDEBAR, M2-PROGRESS | C0 | running | Backend 3aa339b and native e63b4a8 independently accepted; sidebar grouping and G4 policy gates open | Stack reports above |
| M4-REBASE | Linear graphical plan, edit/split and dirty preparation; B01-B03, E14 | M4 | Accepted M3 backend; G3 for final native gate | C0 | running | Lifecycle651037f and native N1 correction565b61f independently accepted | art_63cc3dbd-330a-43ae-a827-c4c6fe20b43e |
| M4-CONFLICT | Editable three-way result, external resolution and recovery; B04, B05, E14 | M4 | M4-REBASE, accepted Document backend; native M3-EXTERNAL | Coordinator | running | Native three-way 4b5c43e independently accepted; final G4 input validation pending | Three-way reports above |
| M4-PUBLISH | Inspect rewritten commits, explicit lease-protected push; B06, E15 | M4 | M4-CONFLICT, M3-GIT | C0 | running | Native source/destination plus exact recovery ea28933 independently accepted; final G4/live-write gate open | Publish recovery reports above |
| M4-TIPS | Implement accepted multiple-tip behavior and evidence | M4 | M4-STACK, D60 | U | dependent | pending | pending |
| M4-COMMENT | Implement accepted aggregate comment routing and evidence | M4 | M4-STACK, M2-REVIEW, D61 | U | dependent | pending | pending |
| M4-GROUP | Implement accepted grouped review behavior and evidence | M4 | M4-STACK, M2-REVIEW, D62 | U | dependent | pending | pending |
| M4-MERGE | Implement accepted stack integration behavior and evidence | M4 | M4-STACK, M2-MERGE, D63 | U | dependent | pending | pending |
| M4-DESC | Implement accepted descendant-repair behavior and evidence | M4 | M4-STACK, M4-PUBLISH, D64 | U | dependent | pending | pending |
| M5-PARITY | Reconcile full PR scope, capability evidence and accepted inventory; F07, E16 | M5 | G2, D65 | U | dependent | pending | pending |
| M5-UI | Running-app visual/input and local performance validation; V09, V10, F06, F09, E17 | M5 | G3, M4-STACK, M4-PUBLISH | U | dependent | pending | pending |
| M5-NUMERIC | Apply accepted numeric performance gates | M5 | M5-UI, D66 | U | dependent | pending | pending |
| M5-PACK | Apple Silicon packaging, minimum OS and install/launch checks; F02, F10 | M5 | G3, M4-STACK, M4-PUBLISH, M0-BUILD | C0 | running | Core ad-hoc package8709527 verified; final release/oldest-OS/install gates pending | art_4ad60d61-6787-4357-8dd9-36a0affe941c |
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

All eight records below are **open, unanswered proposals**. D60 has a concise asynchronous user question pending; no answer is recorded yet. They are not approved defaults. Q67 is resolved by the Apple Silicon clarification and maps to F02. Prepare a concrete recommendation only when its dependent behavior needs the answer; continue independent work rather than restarting the interview. A decision's closure evidence must include the accepted answer, affected scope and updated acceptance criteria. If a proposal is changed or deferred, preserve that decision rather than marking the original proposal implemented.

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
