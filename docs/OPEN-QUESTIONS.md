# Open questions

Each one ends as a spec change or a decision in [DECISIONS.md](DECISIONS.md).

1. **Unvalidated thresholds.** Diagnostic severity at 98% / 80% resolved links
   (a ~730-note [zk](https://github.com/zk-org/zk) vault resolves ~94.7%, so without reading its zk config it
   would get warnings); the 7-day hysteresis on lazy verdicts; the 95% sibling
   threshold for missing-frontmatter-key hints (7 hints in a ~210-note research
   vault, 3 in the zk vault; exclude generated folders such as a `cache/` of
   fetched papers). Measure each in the offline differential before 0.1.
   Full list: [specs/index.md §5.2](specs/index.md).
2. **Small repos on a virtual filesystem.** A small notes repo on
   [EdenFS](https://github.com/facebook/sapling) (the virtual filesystem from
   the Sapling project) is enumerated with `sl files`
   ([Sapling](https://sapling-scm.com/)) within 500 ms or stays lazy
   ([specs/roots.md](specs/roots.md) §3). Open: is 500 ms the right budget,
   the EdenFS glob API instead of a child process, refreshing the list
   without a watcher, and whether `git ls-files` should do the same for
   other lazy roots.
3. **FSEvents replay cost.** After a day offline, replay from `sinceWhen` may
   report directories only (`MustScanSubDirs`, dropped events,
   `EventIdsWrapped`) and fall back to the `dir_state` diff. How long does it
   take to reach `HistoryDone` on a busy volume, and how often does it degrade?
4. **Bare paths in prose at all?** In the research vault, 192 of 193 bare
   path-like tokens that exist on disk are frontmatter values; in the zk vault
   2 of 2,423 exist. Resolve bare paths only in frontmatter values, or also in
   prose (gated on existence)?
5. **Org-mode depth.** In scope: links, headings, `:ID:`, `#+TITLE`, `#+LINK`.
   Open: agenda, `id:` links across roots, properties beyond `:ID:`.
6. **Short binary alias.** `mw` (Wikimedia `mwcli`, crate `mw`) and `mdr`
   (crates.io) are taken. Find a candidate and run the D1 namespace checks.
7. **Name checks still missing for `mdroots`:** npm and trademark
   (USPTO/EUIPO, classes 9 and 42), before 0.1 (D1).
8. **`.mdrootsignore` vs `.ignore` + a key in `.mdroots`.** The dedicated
   file exists because `.gitignore` answers a VCS question, folders without
   VCS lack one, and an empty file means never index here / stop loose
   climbing. Folding it into `.ignore` (shared with
   [ripgrep](https://github.com/BurntSushi/ripgrep) and
   [fd](https://github.com/sharkdp/fd)) plus a `.mdroots` key would drop one
   magic file name. Revisit when used ([specs/roots.md](specs/roots.md)).
