use memmap2::Mmap;
use std::{fs::File, io::{self, BufReader, BufWriter, Read, Seek, Write}, path::Path};

const MAGIC:&[u8;8]=b"LHRDPST1";
const HEADER:usize=48;

fn read_record<R:Read>(r:&mut R)->io::Result<Option<(u64,u32)>>{let mut b=[0u8;12];let mut n=0;while n<12{match r.read(&mut b[n..])?{0 if n==0=>return Ok(None),0=>return Err(io::Error::new(io::ErrorKind::UnexpectedEof,"truncated postings")),x=>n+=x}}Ok(Some((u64::from_le_bytes(b[..8].try_into().unwrap()),u32::from_le_bytes(b[8..].try_into().unwrap()))))}
fn put_varint(mut x:u32,w:&mut impl Write)->io::Result<usize>{let mut n=0;loop{let mut b=(x&0x7f)as u8;x>>=7;if x!=0{b|=0x80}w.write_all(&[b])?;n+=1;if x==0{return Ok(n)}}}
fn get_varint(buf:&[u8],p:&mut usize)->Option<u32>{let(mut x,mut s)=(0u32,0);loop{let b=*buf.get(*p)?;*p+=1;x|=((b&0x7f)as u32)<<s;if b&0x80==0{return Some(x)}s+=7;if s>28{return None}}}

/// Directory: (key:u64, byte_offset:u64, count:u32) = 20 bytes/key.
/// Body per key: absolute first row u32 followed by varint deltas.
pub struct DeltaPostingHierarchy{map:Mmap,keys:u64,dir:usize,body:usize}
impl DeltaPostingHierarchy{
 pub fn build_from_sorted(sorted:impl AsRef<Path>,output:impl AsRef<Path>)->io::Result<()> {
  let sorted=sorted.as_ref();let mut r=BufReader::new(File::open(sorted)?);let mut groups=Vec::<(u64,u64,u32)>::new();let tmp=output.as_ref().with_extension("delta.tmp");let mut body=BufWriter::new(File::create(&tmp)?);let(mut cur,mut count,mut prev,mut off)=(None,0u32,0u32,0u64);
  while let Some((k,row))=read_record(&mut r)?{if cur!=Some(k){if let Some(old)=cur{groups.push((old,off,count));off=body.stream_position()?;}cur=Some(k);count=0;prev=row;body.write_all(&row.to_le_bytes())?;}else{put_varint(row.checked_sub(prev).ok_or_else(||io::Error::new(io::ErrorKind::InvalidData,"rows not sorted"))?,&mut body)?;prev=row;}count+=1;}
  if let Some(k)=cur{groups.push((k,off,count));}body.flush()?;
  let body_start=HEADER as u64+groups.len()as u64*20;let mut w=BufWriter::new(File::create(output)?);w.write_all(MAGIC)?;w.write_all(&(groups.len()as u64).to_le_bytes())?;w.write_all(&(HEADER as u64).to_le_bytes())?;w.write_all(&body_start.to_le_bytes())?;w.write_all(&0u64.to_le_bytes())?;w.write_all(&0u64.to_le_bytes())?;
  for(k,o,c)in &groups{w.write_all(&k.to_le_bytes())?;w.write_all(&o.to_le_bytes())?;w.write_all(&c.to_le_bytes())?;}let mut t=BufReader::new(File::open(&tmp)?);io::copy(&mut t,&mut w)?;w.flush()?;drop(t);let _=std::fs::remove_file(tmp);Ok(())
 }
 pub fn open(path:impl AsRef<Path>)->io::Result<Self>{let f=File::open(path)?;let map=unsafe{Mmap::map(&f)?};if map.len()<HEADER||&map[..8]!=MAGIC{return Err(io::Error::new(io::ErrorKind::InvalidData,"bad delta postings"))}let keys=u64::from_le_bytes(map[8..16].try_into().unwrap());let dir=u64::from_le_bytes(map[16..24].try_into().unwrap())as usize;let body=u64::from_le_bytes(map[24..32].try_into().unwrap())as usize;if dir!=HEADER||body!=HEADER+keys as usize*20||body>map.len(){return Err(io::Error::new(io::ErrorKind::InvalidData,"bad delta layout"))}Ok(Self{map,keys,dir,body})}
 fn entry(&self,i:usize)->(u64,u64,u32){let o=self.dir+i*20;(u64::from_le_bytes(self.map[o..o+8].try_into().unwrap()),u64::from_le_bytes(self.map[o+8..o+16].try_into().unwrap()),u32::from_le_bytes(self.map[o+16..o+20].try_into().unwrap()))}
 fn find(&self,key:u64)->Option<(u64,u32)>{let(mut l,mut r)=(0,self.keys as usize);while l<r{let m=(l+r)/2;if self.entry(m).0<key{l=m+1}else{r=m}}if l<self.keys as usize{let(k,o,c)=self.entry(l);if k==key{return Some((o,c))}}None}
 pub fn row_count(&self,key:u64)->usize{self.find(key).map(|x|x.1 as usize).unwrap_or(0)}
 pub fn rows(&self,key:u64)->Vec<u32>{let Some((off,c))=self.find(key)else{return vec![]};if c==0{return vec![]}let mut p=self.body+off as usize;if p+4>self.map.len(){return vec![]}let mut row=u32::from_le_bytes(self.map[p..p+4].try_into().unwrap());p+=4;let mut out=Vec::with_capacity(c as usize);out.push(row);for _ in 1..c{let Some(d)=get_varint(&self.map,&mut p)else{return vec![]};row=match row.checked_add(d){Some(x)=>x,None=>return vec![]};out.push(row)}out}
 pub fn intersect_rows(&self,key:u64,seed:&[u32])->Vec<u32>{let rows=self.rows(key);crate::intersect_sorted(&rows,seed)}
}

#[cfg(test)]mod tests{use super::*;#[test]fn roundtrip(){let d=tempfile::tempdir().unwrap();let s=d.path().join("s");let o=d.path().join("o");let mut f=File::create(&s).unwrap();for(k,r)in[(2u64,100u32),(2,101),(2,140),(7,9),(7,10009)]{f.write_all(&k.to_le_bytes()).unwrap();f.write_all(&r.to_le_bytes()).unwrap()}drop(f);DeltaPostingHierarchy::build_from_sorted(&s,&o).unwrap();let x=DeltaPostingHierarchy::open(o).unwrap();assert_eq!(x.rows(2),vec![100,101,140]);assert_eq!(x.rows(7),vec![9,10009]);assert_eq!(x.row_count(3),0);assert_eq!(x.intersect_rows(2,&[1,101,140,999]),vec![101,140]);}}
