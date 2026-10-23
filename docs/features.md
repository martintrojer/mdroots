# What mdroots does

Everything mdroots offers, in one list. Each entry names the feature
briefly and links to the spec that defines its exact behaviour.

mdroots reads notes and proposes edits. The editor applies them: text
changes, new files and moves alike. mdroots writes nothing under a notebook
([D10](DECISIONS.md#d10-mdroots-proposes-edits-the-editor-applies-them)).

## Notebooks and links

- **Zero-config roots.** The notebook a file belongs to is found from the
  file itself, by markers, the VCS or a bounded walk, never by walking a
  remote, virtual or oversized tree ([roots spec](specs/roots.md)).
- **Every link style in one notebook.** [zk](https://github.com/zk-org/zk),
  [Obsidian](https://obsidian.md),
  [marksman](https://github.com/artempyanykh/marksman),
  [Foam](https://foambubble.github.io/foam/),
  [Dendron](https://www.dendron.so), [Logseq](https://logseq.com),
  [org-mode](https://orgmode.org) and plain relative paths:
  - wiki links with anchors, block refs, labels and embeds;
  - Markdown, reference and HTML links;
  - org links, including `file:`, `id:` and `#+LINK` abbreviations;
  - bare paths, frontmatter values, tags, footnotes and templating
    shortcodes.

  All resolve through one ladder ([index spec §2](specs/index.md#2-liberal-link-model-d7)).
- **Code-aware.** A link in code or a comment is a mention: you can jump
  to it, but it is never diagnosed or rewritten. `path:line` mentions jump
  to the line.
- **Tool settings honoured.** zk and Obsidian configs decide link style
  and broken-link severity; otherwise the notes vote
  ([index spec §3](specs/index.md#3-dialects-without-configuration-d8)).
- **Warm cache shared across editors** in your user cache dir; only
  changed notes are re-read ([index spec §1](specs/index.md#1-index-under-churn)).

## Language server (`mdroots lsp`)

Behaviour in detail: [library spec §3.6](specs/library.md#36-embedding-the-server).

**Navigation**
- Go to definition on any link form: notes, headings, block anchors,
  `path:line` code mentions.
- References: backlinks of a link's target or of the current note; on a
  `#tag`, every note carrying it.
- Hover: the target's title and first lines, or "broken link"; on a tag,
  its note count and titles.
- Document highlight: every link in the note that goes where the one
  under the cursor goes.
- Document symbols (headings nested by level) and workspace symbols
  (fuzzy search over titles, file names and aliases).
- Document links for every resolved link; folding by heading section and
  frontmatter.

**Writing**
- Completion of notes after `[[`, headings after `[[note#`, paths after
  `](` and tags after `#`, inserting text that resolves to the chosen note.
- Inlay hints: the resolved title after a bare `[[stem]]`.
- Code lenses: "N backlinks" on the title, "N links" on headings that
  other notes link to.

**Diagnostics and quick fixes**
- Broken links, missing anchors, ambiguous links, frontmatter that does
  not parse, and a frontmatter key that loses to another one
  (`Title:` next to `title:`). Severity follows the notebook
  ([index spec §3.3](specs/index.md#33-diagnostics)).
- Quick fixes for each:
  - `Change to <note>` (up to three close matches; the link keeps its
    syntax, anchor and label);
  - `Create note <path>` (an empty note with a title line, never over an
    existing file);
  - `Change to #<heading>`;
  - `Pick <path>` for an ambiguous link.

  A fix that is the only candidate is marked preferred.

**Refactoring**
- Rename a note from a link or its title: every link to it is updated.
- Moves made by your file tree or file manager (`workspace/willRenameFiles`):
  links to and from moved notes and folders are fixed, with every move in
  the request planned together.
- Convert a link between wiki and Markdown syntax.
- Turn a selection into a link to the note it names.
- Extract a selection to a new note, leaving a link behind.

**Notebook commands** (`workspace/executeCommand`; results keep the
server's order)
- `mdroots.orphans`, `mdroots.missingBacklinks`, `mdroots.related`: the
  link graph.
- `mdroots.search`: full-text search with snippets.
- `mdroots.query`: every `mdroots notes` filter (tags, dates, links,
  sort) as a JSON object.
- `mdroots.check`: diagnostics of every note.
- `mdroots.tags`, `mdroots.backlinks`, `mdroots.anchorLinks`,
  `mdroots.linkTo`, `mdroots.renameFile`.
- `mdroots.reindex`: rescan from disk; `mdroots.info`: the root, why it
  was chosen, and every effective setting with its source.

## Neovim

[`editors/nvim`](../editors/nvim) works with the built-in 0.12 client, no
plugins needed. It adds mappings for backlinks, search, the notebook
commands (into the quickfix list, in server order), tags, rename and
extract, plus `:MdrootsInfo`, `:MdrootsCheck`, `:MdrootsQuery` and
`:MdrootsReindex` ([library spec](specs/library.md#neovim-012-example)).

## Command line (`mdroots`)

Details: [library spec §6](specs/library.md#6-cli).

- `mdroots check`: diagnostics for CI; exits 1 on errors or warnings.
- `mdroots notes`: `zk list`'s filters (tags, full text, dates, links,
  orphans, related, sort, formats).
- `mdroots tags`, `mdroots search`, `mdroots backlinks`.
- `mdroots resolve FROM LINK`: where a link goes, and by which rule.
- `mdroots roots NOTE`: the root, why it was chosen, and the effective
  settings with their sources.
- `mdroots lsp`: the language server.

## Library (`mdroots` crate)

The server and the CLI are thin layers over one Rust API. Everything
above is a library call: resolution, backlinks, graph queries, search,
diagnostics, quick fixes, renames and moves as edits, link writing and
conversion ([library spec §3](specs/library.md#3-public-api-sketch)).

## Not built

See [ROADMAP](ROADMAP.md): semantic tokens, fix-all, call hierarchy and
the rest of the deferred list.
