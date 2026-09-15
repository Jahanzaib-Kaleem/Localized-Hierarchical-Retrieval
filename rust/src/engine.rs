use crate::{Hierarchy,Segment,intersect_sorted,mixed_radix_key};
use serde::Deserialize;
use std::{collections::HashMap,fs,path::{Path,PathBuf},io};

#[derive(Clone,Copy,Debug)]pub struct Predicate{pub column:usize,pub value:u64}
#[derive(Deserialize)]struct Manifest{page_rows:usize,pages:u32,cardinalities:Vec<u64>,segments:Vec<SegMeta>,hierarchies:Vec<HMeta>}
#[derive(Deserialize)]struct SegMeta{file:String,first_page:u32}
#[derive(Deserialize)]struct HMeta{file:String,columns:Vec<usize>}
struct LoadedHierarchy{columns:Vec<usize>,data:Hierarchy}
struct LoadedSegment{first_page:u32,data:Segment}
pub struct Engine{page_rows:usize,pages:u32,card:Vec<u64>,hier:Vec<LoadedHierarchy>,segments:Vec<LoadedSegment>}
impl Engine{
 pub fn open(root:impl AsRef<Path>)->io::Result<Self>{let root=root.as_ref();let raw=fs::read(root.join("manifest.json"))?;let m:Manifest=serde_json::from_slice(&raw).map_err(|e|io::Error::new(io::ErrorKind::InvalidData,e))?;
  let mut hier=Vec::new();for h in m.hierarchies{hier.push(LoadedHierarchy{columns:h.columns,data:Hierarchy::open(root.join("routing").join(h.file))?});}
  let mut segments=Vec::new();for s in m.segments{segments.push(LoadedSegment{first_page:s.first_page,data:Segment::open(root.join("canonical").join(s.file))?});}
  Ok(Self{page_rows:m.page_rows,pages:m.pages,card:m.cardinalities,hier,segments})}
 pub fn candidate_pages(&self,p:&[Predicate])->Vec<u32>{let q:HashMap<usize,u64>=p.iter().map(|x|(x.column,x.value)).collect();let mut lists:Vec<Vec<u32>>=Vec::new();
  for h in &self.hier{if h.columns.iter().all(|c|q.contains_key(c)){let vals:Vec<_>=h.columns.iter().map(|&c|(c,q[&c])).collect();if let Some(k)=mixed_radix_key(&vals,&self.card){lists.push(h.data.pages(k));}}}
  if lists.is_empty(){return(0..self.pages).collect()}lists.sort_by_key(Vec::len);let mut out=lists.remove(0);for x in lists{out=intersect_sorted(&out,&x);if out.is_empty(){break}}out}
 pub fn query_count(&self,p:&[Predicate])->(u64,u64){let pages=self.candidate_pages(p);let pred:Vec<_>=p.iter().map(|x|(x.column,x.value)).collect();let(mut hits,mut checked)=(0,0);
  for s in &self.segments{let pc=((s.data.rows()+self.page_rows-1)/self.page_rows) as u32;let lo=pages.partition_point(|&x|x<s.first_page);let hi=pages.partition_point(|&x|x<s.first_page+pc);for &pid in &pages[lo..hi]{let start=(pid-s.first_page) as usize*self.page_rows;let end=(start+self.page_rows).min(s.data.rows());checked+=(end-start) as u64;hits+=s.data.count_page(start,end,&pred);}}
  (hits,checked)}
}
