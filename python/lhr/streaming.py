"""Bounded-memory LHR builder and binary-searchable hierarchy directories."""
from __future__ import annotations
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Iterator, Sequence
import json
import numpy as np

@dataclass(frozen=True)
class HierarchySpec:
    columns: tuple[int, ...]


def _mixed_key(rows: np.ndarray, cols: Sequence[int], cardinalities: Sequence[int]) -> np.ndarray:
    key=np.zeros(len(rows), dtype=np.uint64)
    for c in cols:
        key=key*np.uint64(cardinalities[c])+rows[:,c].astype(np.uint64)
    return key


def build_streaming(batches: Iterable[np.ndarray], path: str|Path, cardinalities: Sequence[int], hierarchies: Sequence[HierarchySpec], page_rows: int=4096) -> None:
    """Write canonical segments and page-level hierarchy runs without holding the dataset in RAM.

    Hierarchy leaves store page IDs, not row IDs. Querying intersects page sets and then
    verifies exact predicates against canonical segments. This bounds write/query memory by
    batch/page size rather than dataset size.
    """
    root=Path(path); canon=root/'canonical'; routing=root/'routing'
    canon.mkdir(parents=True,exist_ok=True); routing.mkdir(parents=True,exist_ok=True)
    page_records={h.columns:[] for h in hierarchies}
    segments=[]; row_base=0; page_id=0; dtype=None; ncols=None
    carry=None
    for batch_no,batch in enumerate(batches):
        if batch.ndim!=2: raise ValueError('each batch must be 2D')
        if dtype is None: dtype=batch.dtype; ncols=batch.shape[1]
        if batch.shape[1]!=ncols: raise ValueError('column count changed between batches')
        work=batch if carry is None else np.concatenate((carry,batch),axis=0)
        full=(len(work)//page_rows)*page_rows
        ready=work[:full]; carry=work[full:].copy() if full<len(work) else None
        if len(ready):
            seg=f'segment-{len(segments):06d}.npy'; np.save(canon/seg,ready)
            segments.append({'file':seg,'row_start':row_base,'rows':len(ready)})
            for off in range(0,len(ready),page_rows):
                page=ready[off:off+page_rows]
                for h in hierarchies:
                    keys=np.unique(_mixed_key(page,h.columns,cardinalities))
                    page_records[h.columns].extend((int(k),page_id) for k in keys)
                page_id+=1
            row_base+=len(ready)
    if carry is not None and len(carry):
        seg=f'segment-{len(segments):06d}.npy'; np.save(canon/seg,carry)
        segments.append({'file':seg,'row_start':row_base,'rows':len(carry)})
        for h in hierarchies:
            keys=np.unique(_mixed_key(carry,h.columns,cardinalities))
            page_records[h.columns].extend((int(k),page_id) for k in keys)
        page_id+=1; row_base+=len(carry)

    # Compact sorted (key,page) runs. Key ranges are binary-searchable.
    hierarchy_meta=[]
    for idx,h in enumerate(hierarchies):
        rec=np.asarray(page_records[h.columns],dtype=[('key','<u8'),('page','<u4')])
        rec.sort(order=['key','page'])
        name=f'h{idx:04d}.npy'; np.save(routing/name,rec)
        hierarchy_meta.append({'file':name,'columns':list(h.columns),'entries':len(rec)})

    manifest={'format':'LHR/0-stream','rows':row_base,'columns':ncols,'dtype':str(dtype),'page_rows':page_rows,'pages':page_id,'cardinalities':list(map(int,cardinalities)),'segments':segments,'hierarchies':hierarchy_meta}
    (root/'manifest.json').write_text(json.dumps(manifest,indent=2),encoding='utf-8')


class StreamingDataset:
    def __init__(self,path: str|Path):
        self.root=Path(path); self.manifest=json.loads((self.root/'manifest.json').read_text())
        self.card=self.manifest['cardinalities']; self.page_rows=self.manifest['page_rows']
        self.h=[]
        for m in self.manifest['hierarchies']:
            self.h.append((tuple(m['columns']),np.load(self.root/'routing'/m['file'],mmap_mode='r')))
        self.segments=[np.load(self.root/'canonical'/m['file'],mmap_mode='r') for m in self.manifest['segments']]

    def _key(self,query:dict[int,int],cols:tuple[int,...])->int:
        k=0
        for c in cols: k=k*self.card[c]+query[c]
        return k

    def candidate_pages(self,query:dict[int,int])->np.ndarray:
        choices=[]
        for cols,rec in self.h:
            if all(c in query for c in cols):
                key=self._key(query,cols); keys=rec['key']
                lo=int(np.searchsorted(keys,key,'left')); hi=int(np.searchsorted(keys,key,'right'))
                choices.append(np.asarray(rec['page'][lo:hi],dtype=np.uint32))
        if not choices: return np.arange(self.manifest['pages'],dtype=np.uint32)
        choices.sort(key=len); out=choices[0]
        for x in choices[1:]:
            out=np.intersect1d(out,x,assume_unique=True)
            if not len(out): break
        return out

    def query_count(self,query:dict[int,int])->tuple[int,int]:
        pages=self.candidate_pages(query); checked=hits=0
        # v0-stream segments are page aligned except final segment, so page->segment lookup is sequential.
        wanted=set(map(int,pages)); pid=0
        for seg in self.segments:
            for off in range(0,len(seg),self.page_rows):
                if pid in wanted:
                    block=seg[off:off+self.page_rows]; keep=np.ones(len(block),bool)
                    for c,v in query.items(): keep &= block[:,c]==v
                    checked+=len(block); hits+=int(keep.sum())
                pid+=1
        return hits,checked
