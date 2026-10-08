import subprocess,re,sys,collections,os,urllib.parse
root=sys.argv[1]; os.chdir(root)
fs=subprocess.run(['rg','--files','-g','*.md'],capture_output=True,text=True).stdout.split()
allf=set(subprocess.run(['rg','--files'],capture_output=True,text=True).stdout.split())
def slug(s): return re.sub(r'[^\w]+','-',s.lower()).strip('-')
paths={f[:-3]:f for f in fs}; stems=collections.defaultdict(list); titles=collections.defaultdict(list); ids=collections.defaultdict(list); aliases=collections.defaultdict(list); h1=collections.defaultdict(list)
for f in fs:
    stems[os.path.basename(f)[:-3].lower()].append(f)
    t=open(f,errors='ignore').read()
    m=re.match(r'---\n(.*?)\n---\n',t,re.S)
    if m:
        y=m.group(1)
        for k,d in (('title',titles),('id',ids)):
            mm=re.search(rf'^{k}:\s*["\']?(.+?)["\']?\s*$',y,re.M)
            if mm: d[slug(mm.group(1))].append(f)
        mm=re.search(r'^aliases:\s*\[(.*)\]',y,re.M)
        if mm:
            for a in mm.group(1).split(','): aliases[slug(a.strip(' "\''))].append(f)
    mh=re.search(r'^# (.+)$',t,re.M)
    if mh: h1[slug(mh.group(1))].append(f)
R=collections.Counter(); miss=[]; amb=0
for f in fs:
    inf=False
    for l in open(f,errors='ignore').read().splitlines():
        if re.match(r'\s*(```|~~~)',l): inf=not inf; continue
        if inf: continue
        l2=re.sub(r'`[^`]*`','',l)
        for m in re.finditer(r'(?<!!)\[\[([^\]\[|#]+)(?:#[^\]|]*)?(?:\|[^\]]*)?\]\]',l2):
            tgt=m.group(1).strip(); k=None
            rel=os.path.normpath(os.path.join(os.path.dirname(f),tgt))
            if tgt.lstrip('/') in paths or tgt.lstrip('/').removesuffix('.md') in paths: k='root-rel path'
            elif rel in paths or rel.removesuffix('.md') in paths: k='file-rel path'
            elif tgt.lower().removesuffix('.md') in stems: k='stem'+(' (ambiguous)' if len(stems[tgt.lower().removesuffix('.md')])>1 else '')
            elif slug(tgt) in ids: k='fm id'
            elif slug(tgt) in titles: k='fm title'
            elif slug(tgt) in h1: k='h1 title'
            elif slug(tgt) in aliases: k='fm alias'
            elif slug(tgt) in stems: k='stem-slug'
            else: k='UNRESOLVED'; len(miss)<8 and miss.append(f"{f}: [[{tgt}]]")
            R['wiki:'+k]+=1
        for m in re.finditer(r'(?<!!)\[[^\]]*\]\(([^)\s#]+)(?:#[^)]*)?\)',l2):
            u=urllib.parse.unquote(m.group(1))
            if re.match(r'\w+:',u): continue
            p=os.path.normpath(os.path.join(os.path.dirname(f),u)) if not u.startswith('/') else u.lstrip('/')
            R['mdlink:'+('ok' if p in allf or p+'.md' in allf or os.path.isdir(p) else ('ok-rootrel' if u in allf else 'UNRESOLVED'))]+=1
        for m in re.finditer(r'(?<![\[(\w/`<:.])((?:\.{1,2}/)?[\w-][\w.-]*/[\w./-]*\w)',l2):
            p=m.group(1); full=os.path.normpath(os.path.join(os.path.dirname(f),p))
            ex_= p in allf or full in allf or os.path.isdir(full) or os.path.isdir(p) or (p+'.md') in allf
            R['bare:'+('exists' if ex_ else 'no')]+=1
for k,v in sorted(R.items()): print(f"  {k:28}{v}")
print("  unresolved e.g.:"); [print('   ',x) for x in miss]
