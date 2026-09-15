"""Benchmark the streaming page-hierarchy prototype."""
import argparse,tempfile,shutil,time,sys
from pathlib import Path
import numpy as np
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from python.lhr.streaming import HierarchySpec,StreamingDataset,build_streaming

def main():
    ap=argparse.ArgumentParser(); ap.add_argument('--rows',type=int,default=1_000_000); ap.add_argument('--batch',type=int,default=50_000); ap.add_argument('--queries',type=int,default=100)
    a=ap.parse_args(); cards=[8,12,6,16,10,20,8,14]; rng=np.random.default_rng(91)
    root=Path(tempfile.mkdtemp(prefix='lhr-stream-'))
    def batches():
        left=a.rows
        while left:
            n=min(a.batch,left); latent=rng.integers(0,64,n,dtype=np.uint16); x=np.empty((n,len(cards)),np.uint8)
            for c,k in enumerate(cards):
                patterned=((latent*(c+3)+c*5)%k).astype(np.uint8); noise=rng.integers(0,k,n,dtype=np.uint8); x[:,c]=np.where(rng.random(n)<.68,patterned,noise)
            yield x; left-=n
    specs=[HierarchySpec(x) for x in ((0,1),(2,3),(4,5),(6,7),(0,2),(1,4),(3,6),(0,1,2),(3,4,5),(1,6,7))]
    try:
        t=time.perf_counter(); build_streaming(batches(),root,cards,specs,page_rows=256); build=time.perf_counter()-t
        ds=StreamingDataset(root); lat=[]; touched=[]
        for _ in range(a.queries):
            cols=rng.choice(len(cards),3,replace=False); q={int(c):int(rng.integers(0,cards[c])) for c in cols}
            t=time.perf_counter(); _,checked=ds.query_count(q); lat.append((time.perf_counter()-t)*1000); touched.append(checked)
        disk=sum(p.stat().st_size for p in root.rglob('*') if p.is_file())
        print(f'rows={a.rows:,} build_s={build:.2f} disk_mb={disk/1e6:.2f} median_ms={np.median(lat):.3f} p95_ms={np.quantile(lat,.95):.3f} median_touched_pct={100*np.median(touched)/a.rows:.4f}')
    finally: shutil.rmtree(root,ignore_errors=True)
if __name__=='__main__': main()
