# mdroots

A zero-config library for markdown and org notes. Give it any file;
it finds the notes root around it (without ever walking a huge, remote
or virtual tree), indexes the root, and answers link, backlink, tag,
outline, search, rename and diagnostics queries. It understands the link
styles of [zk](https://github.com/zk-org/zk),
[Obsidian](https://obsidian.md),
[marksman](https://github.com/artempyanykh/marksman), wiki links,
org-mode and plain relative paths.

```rust
use mdroots::{Cancel, Options, Workspace};
use std::path::Path;

let note = Path::new("notes/garden.md");
let ws = Workspace::open_for(note, Options::default())?;
println!("root {} ({:?}): {}", ws.root().path.display(), ws.root().mode, ws.root().reason);
for d in ws.diagnostics(note, &Cancel::new())? {
    println!("{:?}: {}", d.severity, d.message);
}
for b in ws.backlinks(note)? {
    println!("linked from {} ({})", b.from.display(), b.from_title);
}
# Ok::<(), mdroots::Error>(())
```

Notes are cached per root in the user cache dir (`IndexMode::Memory`
turns that off), so a later process starts without re-reading unchanged
files. The library never writes your notes: renames come back as a
`WorkspaceEdit`.

The command line and language server are in
[`mdroots-cli`](https://crates.io/crates/mdroots-cli). Design and specs:
<https://github.com/martintrojer/mdroots>.

`mdroots` is the stable entry point; the `mdroots-*` crates it depends on
are internal and pinned to exact versions.
