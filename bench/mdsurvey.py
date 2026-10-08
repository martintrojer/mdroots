import subprocess,re,sys,collections,os
root=sys.argv[1]; os.chdir(root)
fs=subprocess.run(['rg','--files','-g','*.md','-g','*.org'],capture_output=True,text=True).stdout.split()
C=collections.Counter(); keys=collections.Counter(); ex=collections.defaultdict(list)
pats={
 'wiki':r'(?<!!)\[\[[^\]\[]+\]\](?!\[)', 'wiki_piped':r'\[\[[^\]|\[]+\|[^\]]+\]\]', 'wiki_anchor':r'\[\[[^\]\[]*#[^\]]*\]\]',
 'embed':r'!\[\[[^\]]+\]\]', 'md_local':r'(?<!!)\[[^\]]*\]\((?!https?:|mailto:|#)[^)\s]+\)', 'md_anchor_only':r'\]\(#[^)]+\)',
 'md_http':r'\]\(https?:', 'img_local':r'!\[[^\]]*\]\((?!https?:)[^)]+\)', 'org_link':r'\[\[[^\]\[]+\]\[[^\]]+\]\]',
 'org_file':r'\[\[file:[^\]]+\]', 'refdef':r'^\s{0,3}\[[^\]]+\]:\s+\S', 'bare_path':r'(?<![\[(\w/`])(?:\.{1,2}/)?[\w.-]+/[\w./-]+\.(?:md|org|png|pdf|jpg)\b',
 'angle_url':r'<https?://[^>]+>', 'hashtag':r'(?:^|\s)#[A-Za-z][\w/-]*', 'zk_id_link':r'\[\[\d{4,}[\w-]*\]\]', 'block_ref':r'\^[\w-]{4,}\b\]\]|#\^[\w-]+',
 'inline_code_link':r'`[^`]*(\[\[|\]\()[^`]*`',
}
fm=0;org=0;fence_hits=0
for f in fs:
    t=open(f,errors='ignore').read()
    if f.endswith('.org'): org+=1
    if t.startswith('---\n'):
        m=re.match(r'---\n(.*?)\n(---|\.\.\.)\n',t,re.S)
        if m:
            fm+=1
            for k in re.findall(r'^([A-Za-z_][\w-]*):',m.group(1),re.M): keys[k]+=1
    inf=False
    for l in t.splitlines():
        if re.match(r'\s*(```|~~~)',l): inf=not inf; continue
        if inf:
            if re.search(r'\[\[|\]\(',l): fence_hits+=1
            continue
        for k,p in pats.items():
            for m in re.finditer(p,l):
                C[k]+=1
                if len(ex[k])<3: ex[k].append(f"{f}: {m.group(0)[:70]}")
print(f"files={len(fs)} org={org} frontmatter={fm} link-like-lines-in-fences={fence_hits}")
for k in pats: print(f"  {k:16} {C[k]:6}   e.g. {ex[k][:2]}")
print("  fm keys:",keys.most_common(20))
