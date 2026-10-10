#!/usr/bin/env python3
"""Deterministic synthetic notebook for the LSP benchmark (run.py).

usage: gen.py N OUTDIR

The same N always gives byte-identical files.

Mirrors crates/mdroots/examples/bench_root.rs: same LCG (seed 42), same word
list, frontmatter (title, tags), `# Note i`, four 60-word paragraphs, five
`[[note-NNNNN]]` wiki links (1 in 50 broken -> `[[missing-k]]`). Additions:
two `## Section` headings, two inline #tags per note, a .zk/config.toml
for zk (https://github.com/zk-org/zk) and an empty .marksman.toml (the
workspace marker of marksman, https://github.com/artempyanykh/marksman).

The probe page note-00000.md is fixed: exactly one broken link
([[missing-probe]]) and four valid ones, first link on a known line.
"""
import os, sys

WORDS = ["alpha", "branch", "cedar", "delta", "ember", "fjord", "garnet", "harbor", "indigo", "juniper",
         "kelp", "lumen", "meadow", "nectar", "orbit", "pebble", "quartz", "river", "summit", "thicket"]
M = (1 << 64) - 1

class Lcg:
    def __init__(self, s): self.s = s
    def next(self):
        self.s = (self.s * 6364136223846793005 + 1442695040888963407) & M
        return self.s >> 33
    def below(self, n): return self.next() % n

def note(i, n, rng):
    out = [f"---\ntitle: Note {i}\ntags: [t{i % 17}, t{i % 5}]\n---\n", f"# Note {i}\n"]
    paras = []
    for p in range(4):
        paras.append(' '.join(WORDS[rng.below(len(WORDS))] for _ in range(60)) + f" #topic{i % 23}" * (p in (1, 3)))
    out.append(paras[0] + "\n")
    out.append("## Background\n")
    out.append(paras[1] + "\n\n" + paras[2] + "\n")
    out.append("## Links\n")
    out.append(paras[3] + "\n")
    links = []
    for _ in range(5):
        if rng.below(50) == 0: links.append(f"See [[missing-{rng.below(n)}]].")
        else: links.append(f"See [[note-{rng.below(n):05}]].")
    if i == 0:  # probe page: four valid, one known broken
        links = [f"See [[note-{k:05}]]." for k in (1, 2, 3, 4)] + ["See [[missing-probe]].", "", "Completion probe: [["]
    out.append('\n'.join(links) + "\n")
    return '\n'.join(out)

def main():
    if len(sys.argv) != 3: sys.exit(__doc__.strip())
    n, d = int(sys.argv[1]), sys.argv[2]
    if n < 5: sys.exit('N must be at least 5 (the probe page links to note-00001..4)')
    os.makedirs(os.path.join(d, ".zk"), exist_ok=True)
    with open(os.path.join(d, ".zk", "config.toml"), "w") as f:
        f.write('[note]\nfilename = "{{id}}"\n\n[format.markdown]\nlink-format = "wiki"\n\n'
                '[lsp.diagnostics]\ndead-link = "error"\nwiki-title = "none"\n')
    open(os.path.join(d, ".marksman.toml"), "w").close()  # marksman treats the dir as a workspace only with a marker
    rng = Lcg(42)
    broken = 0
    for i in range(n):
        s = note(i, n, rng)
        broken += s.count("[[missing-")
        with open(os.path.join(d, f"note-{i:05}.md"), "w") as f: f.write(s)
    print(f"{d}: {n} notes, {broken} broken links")

main()
