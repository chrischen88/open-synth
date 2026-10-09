import sys,os,re,struct,subprocess,cbor2,json,collections
root=sys.argv[1]
params=collections.defaultdict(lambda: collections.defaultdict(set))
def norm(k): return re.sub(r'\d+$','#',k)
def walk(path,o):
    if isinstance(o,dict):
        for k,v in o.items():
            if k=='plainParams' and isinstance(v,dict):
                for pk,pv in v.items():
                    s=params[path][pk]
                    if isinstance(pv,str): s.add(pv)
                    elif len(s)<1: s.add('<num>')
            elif k in('curveVals','lfo','curveData','clip','notes','data','samples','wavetable','tableData'): params[path]['['+k+']'].add('<blob>')
            else: walk(path+'/'+norm(k),v)
    elif isinstance(o,list):
        for x in o: walk(path+'[]',x)
n=0
for dp,_,fs in os.walk(root):
    for f in fs:
        if not f.endswith('.SerumPreset'): continue
        d=open(os.path.join(dp,f),'rb').read()
        try:
            i=d.index(b'\x28\xb5\x2f\xfd')
            raw=subprocess.run(['zstd','-dqc'],input=d[i:],capture_output=True).stdout
            walk('',cbor2.loads(raw)); n+=1
        except Exception as e: pass
print('presets parsed',n)
for p in sorted(params):
    print(p)
    for k in sorted(params[p]):
        v=params[p][k]; vs=sorted(v-{'<num>'})
        print('   ',k, ('= '+', '.join(vs[:60])) if vs else '')
