import numpy as np
from python.lhr.streaming import build_streaming,StreamingDataset,HierarchySpec

def test_streaming_exactness_across_batches(tmp_path):
 rng=np.random.default_rng(9);cards=[5,7,9,4]
 rows=np.column_stack([rng.integers(0,k,12003,dtype=np.uint8) for k in cards])
 batches=[rows[:3001],rows[3001:7777],rows[7777:]]
 hs=[HierarchySpec((0,1)),HierarchySpec((1,2)),HierarchySpec((0,2,3))]
 build_streaming(batches,tmp_path,cards,hs,page_rows=127,max_sort_records=41)
 ds=StreamingDataset(tmp_path)
 for rid in [0,1,126,127,3000,7776,12002]:
  q={0:int(rows[rid,0]),1:int(rows[rid,1]),2:int(rows[rid,2])}
  hits,checked=ds.query_count(q)
  truth=int(np.sum((rows[:,0]==q[0])&(rows[:,1]==q[1])&(rows[:,2]==q[2])))
  assert hits==truth
  assert checked<=len(rows)

def test_impossible_query(tmp_path):
 rows=np.zeros((1000,3),dtype=np.uint8); cards=[2,2,2]
 build_streaming([rows],tmp_path,cards,[HierarchySpec((0,1))],page_rows=64,max_sort_records=5)
 ds=StreamingDataset(tmp_path)
 hits,checked=ds.query_count({0:1,1:1})
 assert hits==0
 assert checked==0
