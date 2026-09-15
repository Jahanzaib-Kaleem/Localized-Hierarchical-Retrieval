import numpy as np
from python.lhr.streaming import HierarchySpec, StreamingDataset, build_streaming


def test_streaming_exact(tmp_path):
    rng=np.random.default_rng(17)
    rows=np.column_stack([rng.integers(0,k,25000,dtype=np.uint8) for k in (8,12,6,16)])
    batches=(rows[i:i+777] for i in range(0,len(rows),777))
    specs=[HierarchySpec((0,1)),HierarchySpec((1,2)),HierarchySpec((0,2,3))]
    build_streaming(batches,tmp_path,[8,12,6,16],specs,page_rows=256)
    ds=StreamingDataset(tmp_path)
    for q in ({0:3,1:5},{1:2,2:4},{0:1,2:3,3:7}):
        hits,checked=ds.query_count(q)
        mask=np.ones(len(rows),bool)
        for c,v in q.items(): mask &= rows[:,c]==v
        assert hits==int(mask.sum())
        assert checked<=len(rows)


def test_batch_boundaries_do_not_change_results(tmp_path):
    rng=np.random.default_rng(18)
    rows=np.column_stack([rng.integers(0,k,10003,dtype=np.uint8) for k in (5,7,9)])
    specs=[HierarchySpec((0,1)),HierarchySpec((1,2))]
    build_streaming((rows[i:i+113] for i in range(0,len(rows),113)),tmp_path,[5,7,9],specs,page_rows=128)
    ds=StreamingDataset(tmp_path)
    for q in ({0:2,1:3},{1:4,2:8}):
        hits,_=ds.query_count(q)
        mask=np.ones(len(rows),bool)
        for c,v in q.items(): mask &= rows[:,c]==v
        assert hits==int(mask.sum())
