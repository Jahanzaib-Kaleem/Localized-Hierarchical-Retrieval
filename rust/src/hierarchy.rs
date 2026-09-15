use memmap2::Mmap;
use std::{fs::File, io, path::Path};

pub const RECORD_BYTES: usize = 12;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct Record { pub key:u64, pub page:u32 }

pub struct Hierarchy { map:Mmap, records:usize }

impl Hierarchy {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file=File::open(path)?;
        let len=file.metadata()?.len() as usize;
        if len % RECORD_BYTES != 0 { return Err(io::Error::new(io::ErrorKind::InvalidData,"LHR hierarchy length is not divisible by 12")); }
        let map=unsafe { Mmap::map(&file)? };
        Ok(Self{map,records:len/RECORD_BYTES})
    }
    pub fn len(&self)->usize { self.records }
    pub fn is_empty(&self)->bool { self.records==0 }
    fn record(&self,i:usize)->Record {
        let o=i*RECORD_BYTES;
        let key=u64::from_le_bytes(self.map[o..o+8].try_into().unwrap());
        let page=u32::from_le_bytes(self.map[o+8..o+12].try_into().unwrap());
        Record{key,page}
    }
    fn lower_bound(&self,key:u64)->usize {
        let(mut l,mut r)=(0,self.records);
        while l<r { let m=l+(r-l)/2; if self.record(m).key<key {l=m+1}else{r=m} }
        l
    }
    fn upper_bound(&self,key:u64)->usize {
        let(mut l,mut r)=(0,self.records);
        while l<r { let m=l+(r-l)/2; if self.record(m).key<=key {l=m+1}else{r=m} }
        l
    }
    fn bounds(&self,key:u64)->(usize,usize){(self.lower_bound(key),self.upper_bound(key))}
    pub fn page_count(&self,key:u64)->usize{let(lo,hi)=self.bounds(key);hi-lo}
    pub fn pages(&self,key:u64)->Vec<u32> {
        let(lo,hi)=self.bounds(key);(lo..hi).map(|i|self.record(i).page).collect()
    }
    pub fn intersect_pages(&self,key:u64,seed:&[u32])->Vec<u32>{
        let(lo,hi)=self.bounds(key);let mut out=Vec::with_capacity(seed.len().min(hi-lo));let(mut i,mut j)=(0usize,lo);
        while i<seed.len()&&j<hi{let p=self.record(j).page;match seed[i].cmp(&p){std::cmp::Ordering::Less=>i+=1,std::cmp::Ordering::Greater=>j+=1,std::cmp::Ordering::Equal=>{out.push(p);i+=1;j+=1;}}}out
    }
}

#[cfg(test)]
mod tests {
 use super::*; use std::io::Write;
 #[test]
 fn binary_searches_pages(){
  let mut f=tempfile::NamedTempFile::new().unwrap();
  for (k,p) in [(2u64,1u32),(5,3),(5,8),(5,11),(9,2)] { f.write_all(&k.to_le_bytes()).unwrap();f.write_all(&p.to_le_bytes()).unwrap(); }
  f.flush().unwrap(); let h=Hierarchy::open(f.path()).unwrap();
  assert_eq!(h.page_count(5),3);assert_eq!(h.pages(5),vec![3,8,11]);assert!(h.pages(6).is_empty());
  assert_eq!(h.intersect_pages(5,&[1,3,4,8,12]),vec![3,8]);
 }
}
