"""Bounded-memory LHR writer and exact disk-backed reader."""
from __future__ import annotations
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence
import json, struct
import numpy as np
from .external_sort import external_sort

REC=np.dtype([('key','<u8'),('page','<u4')])

@dataclass(frozen=True)
class HierarchySpec:
    columns: tuple[int,...]

def _mixed_key(rows,cols,card):
    key=np.zeros(len(rows),dtype=np.uint64)
    for c in cols: key=key*np.uint64(card[c])+rows[:,c].astype(np.uint64)
    return key

def build_streaming(batches:Iterable[np.ndarray],path:str|Path,cardinalities:Sequence[int],hierarchies:Sequence[HierarchySpec],page_rows:int=4096,max_sort_records:int=500_000)->None:
    root=Path(path); canon=root/'canonical'; routing=root/'routing'; temp=root/'temp'
    for p in (canon,routing,temp): p.mkdir(parents=True,exist_ok=True)
    spool={h.columns:(temp/f'h{i:04d}.raw') for i,h in enumerate(hierarchies)}
    handles={k:p.open('wb') for k,p in spool.items()}
    segments=[]; row_base=page_id=0; dtype=None; ncols=None; carry=None
    try:
        for batch in batches:
            if batch.ndim!=2: raise ValueError('each batch must be 2D')
            if dtype is None: dtype=batch.dtype; ncols=batch.shape[1]
            if batch.shape[1]!=ncols: raise ValueError('column count changed')
            work=batch if carry is None else np.concatenate((carry,batch))
            full=(len(work)//page_rows)*page_rows; ready=work[:full]
            carry=work[full:].copy() if full<len(work) else None
            if len(ready):
                name=f'segment-{len(segments):06d}.npy'; np.save(canon/name,ready)
                segments.append({'file':name,'row_start':row_base,'rows':len(ready),'first_page':page_id})
                for off in range(0,len(ready),page_rows):
                    page=ready[off:off+page_rows]
                    for h in hierarchies:
                        f=handles[h.columns]
                        for k in np.unique(_mixed_key(page,h.columns,cardinalities)):
                            f.write(struct.pack('<QI',int(k),page_id))
                    page_id+=1
                row_base+=len(ready)
        if carry is not None and len(carry):
            name=f'segment-{len(segments):06d}.npy'; np.save(canon/name,carry)
            segments.append({'file':name,'row_start':row_base,'rows':len(carry),'first_page':page_id})
            for h in hierarchies:
                for k in np.unique(_mixed_key(carry,h.columns,cardinalities)):
                    handles[h.columns].write(struct.pack('<QI',int(k),page_id))
            page_id+=1; row_base+=len(carry)
    finally:
        for f in handles.values(): f.close()

    meta=[]
    for i,h in enumerate(hierarchies):
        raw=spool[h.columns]
        def records():
            with raw.open('rb') as f:
                while True:
                    b=f.read(12)
                    if not b: break
                    if len(b)!=12: raise IOError('truncated hierarchy spool')
                    yield struct.unpack('<QI',b)
        binary=routing/f'h{i:04d}.bin'
        entries=external_sort(records(),binary,max_records=max_sort_records)
        meta.append({'file':binary.name,'columns':list(h.columns),'entries':entries})
        raw.unlink(missing_ok=True)
    manifest={'format':'LHR/0-stream','rows':row_base,'columns':ncols,'dtype':str(dtype),'page_rows':page_rows,'pages':page_id,'cardinalities':list(map(int,cardinalities)),'segments':segments,'hierarchies':meta}
    (root/'manifest.json').write_text(json.dumps(manifest,indent=2))

class StreamingDataset:
    def __init__(self,path:str|Path):
        self.root=Path(path); self.manifest=json.loads((self.root/'manifest.json').read_text())
        self.card=self.manifest['cardinalities']; self.page_rows=self.manifest['page_rows']
        self.h=[(tuple(m['columns']),np.memmap(self.root/'routing'/m['file'],dtype=REC,mode='r',shape=(m['entries'],))) for m in self.manifest['hierarchies']]
        self.segments=[(m,np.load(self.root/'canonical'/m['file'],mmap_mode='r')) for m in self.manifest['segments']]
    def _key(self,q,cols):
        k=0
        for c in cols: k=k*self.card[c]+q[c]
        return k
    def candidate_pages(self,q):
        choices=[]
        for cols,rec in self.h:
            if all(c in q for c in cols):
                k=self._key(q,cols); keys=rec['key']; lo=np.searchsorted(keys,k,'left'); hi=np.searchsorted(keys,k,'right')
                choices.append(np.asarray(rec['page'][lo:hi],dtype=np.uint32))
        if not choices:return np.arange(self.manifest['pages'],dtype=np.uint32)
        choices.sort(key=len); out=choices[0]
        for x in choices[1:]:
            out=np.intersect1d(out,x,assume_unique=True)
            if not len(out):break
        return out
    def query_count(self,q):
        pages=self.candidate_pages(q); checked=hits=0
        # Candidate page IDs are sorted. Walk only relevant segment/page ranges.
        for meta,seg in self.segments:
            first=int(meta['first_page']); count=(len(seg)+self.page_rows-1)//self.page_rows
            lo=np.searchsorted(pages,first,'left'); hi=np.searchsorted(pages,first+count,'left')
            for pid in pages[lo:hi]:
                off=(int(pid)-first)*self.page_rows; block=seg[off:off+self.page_rows]
                keep=np.ones(len(block),bool)
                for c,v in q.items(): keep &= block[:,c]==v
                checked+=len(block); hits+=int(keep.sum())
        return hits,checked
