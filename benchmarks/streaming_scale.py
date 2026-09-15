"""Scale benchmark for LHR streaming writer/reader.
Example: python benchmarks/streaming_scale.py --rows 10000000 --batch 100000
"""
import argparse,tempfile,shutil,time,resource,sys
from pathlib import Path
import numpy as np
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from python.lhr.streaming import build_streaming,StreamingDataset
from python.lhr.planner import choose_hierarchies

def main():
 p=argparse.ArgumentParser();p.add_argument('--rows',type=int,default=1_000_000);p.add_argument('--batch',type=int,default=100_000);p.add_argument('--queries',type=int,default=100);p.add_argument('--page',type=int,default=1024);a=p.parse_args()
 cards=[8,12,6,16,10,20,8,14];rng=np.random.default_rng(71); root=Path(tempfile.mkdtemp(prefix='lhr-scale-'))
 def batches():
  left=a.rows
  while left:
   n=min(left,a.batch);yield np.column_stack([rng.integers(0,k,n,dtype=np.uint8) for k in cards]);left-=n
 try:
  hs=choose_hierarchies(cards,max_hierarchies=16)
  t=time.perf_counter();build_streaming(batches(),root,cards,hs,page_rows=a.page);build=time.perf_counter()-t
  ds=StreamingDataset(root);lat=[];touch=[]
  for _ in range(a.queries):
   q={int(c):int(rng.integers(0,cards[c])) for c in rng.choice(len(cards),3,replace=False)}
   t=time.perf_counter();hits,checked=ds.query_count(q);lat.append((time.perf_counter()-t)*1000);touch.append(checked)
  disk=sum(f.stat().st_size for f in root.rglob('*') if f.is_file())
  print({'rows':a.rows,'hierarchies':len(hs),'build_s':round(build,3),'disk_MB':round(disk/1e6,2),'peak_rss_MB':round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss/1024,2),'median_query_ms':round(float(np.median(lat)),3),'p95_query_ms':round(float(np.quantile(lat,.95)),3),'median_pct_touched':round(100*np.median(touch)/a.rows,4)})
 finally: shutil.rmtree(root,ignore_errors=True)
if __name__=='__main__':main()
