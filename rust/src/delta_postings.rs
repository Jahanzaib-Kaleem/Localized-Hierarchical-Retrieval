use memmap2::Mmap;
use std::{fs::File,io::{self,BufReader,BufWriter,Read,Seek,Write},path::Path};
// v3 stores consecutive row gaps instead of offsets from the block base.
const MAGIC:&[u8;8]=b"LHRDPB3\0";const HEADER:usize=48;const BLOCK:usize=128;
fn read_record<R:Read>(r:&mut R)->io::Result<Option<(u64,u32)>>{let mut b=[0u8;12];let mut n=0;while n<12{match r.read(&mut b[n..])?{0 if n==0=>return Ok(None),0=>return Err(io::Error::new(io::ErrorKind::UnexpectedEof,"truncated postings")),x=>n+=x}}Ok(Some((u64::from_le_bytes(b[..8].try_into().unwrap()),u32::from_le_bytes(b[8..].try_into().unwrap()))))}
fn bits(x:u32)->u8{if x==0{0}else{(32-x.leading_zeros())as u8}}
fn pack(vals:&[u32],bw:u8,w:&mut impl Write)->io::Result<()> {if bw==0{return Ok(())}let mut acc=0u64;let mut have=0u32;for&v in vals{acc|=(v as u64)<<have;have+=bw as u32;while have>=8{w.write_all(&[acc as u8])?;acc>>=8;have-=8;}}if have>0{w.write_all(&[acc as u8])?}Ok(())}
fn unpack(buf:&[u8],p:&mut usize,n:usize,bw:u8)->Option<Vec<u32>>{if bw==0{return Some(vec![0;n])}let mask=if bw==32{u64::MAX}else{(1u64<<bw)-1};let mut out=Vec::with_capacity(n);let(mut acc,mut have)=(0u64,0u32);for _ in 0..n{while have<bw as u32{acc|=(*buf.get(*p)? as u64)<<have;*p+=1;have+=8;}out.push((acc&mask)as u32);acc>>=bw;have-=bw as u32;}Some(out)}
fn packed_bytes(n:usize,bw:u8)->usize{(n*bw as usize+7)/8}
fn write_block(rows:&[u32],w:&mut BufWriter<File>)->io::Result<()> {let base=rows[0];let gaps:Vec<u32>=rows.windows(2).map(|x|x[1]-x[0]).collect();let bw=gaps.iter().copied().map(bits).max().unwrap_or(0);w.write_all(&base.to_le_bytes())?;w.write_all(&(rows.len()as u16).to_le_bytes())?;w.write_all(&[bw,0])?;pack(&gaps,bw,w)}
pub struct DeltaPostingHierarchy{map:Mmap,keys:u64,dir:usize,body:usize}
impl DeltaPostingHierarchy{
 pub fn build_from_sorted(sorted:impl AsRef<Path>,output:impl AsRef<Path>)->io::Result<()> {let mut r=BufReader::new(File::open(sorted)?);let tmp=output.as_ref().with_extension("delta.tmp");let mut body=BufWriter::new(File::create(&tmp)?);let mut groups=Vec::<(u64,u64,u32)>::new();let(mut cur,mut rows,mut off)=(None,Vec::<u32>::new(),0u64);let flush=|k:u64,rows:&mut Vec<u32>,body:&mut BufWriter<File>,groups:&mut Vec<(u64,u64,u32)>,off:&mut u64|->io::Result<()>{let count=rows.len()as u32;for chunk in rows.chunks(BLOCK){write_block(chunk,body)?}groups.push((k,*off,count));body.flush()?;*off=body.stream_position()?;rows.clear();Ok(())};while let Some((k,row))=read_record(&mut r)?{if let Some(old)=cur{if old!=k{flush(old,&mut rows,&mut body,&mut groups,&mut off)?;cur=Some(k)}}else{cur=Some(k)}if rows.last().map_or(false,|&x|row<x){return Err(io::Error::new(io::ErrorKind::InvalidData,"rows not sorted"))}rows.push(row);}if let Some(k)=cur{flush(k,&mut rows,&mut body,&mut groups,&mut off)?}body.flush()?;let body_start=HEADER as u64+groups.len()as u64*20;let mut w=BufWriter::new(File::create(output)?);w.write_all(MAGIC)?;w.write_all(&(groups.len()as u64).to_le_bytes())?;w.write_all(&(HEADER as u64).to_le_bytes())?;w.write_all(&body_start.to_le_bytes())?;w.write_all(&(BLOCK as u64).to_le_bytes())?;w.write_all(&0u64.to_le_bytes())?;for(k,o,c)in &groups{w.write_all(&k.to_le_bytes())?;w.write_all(&o.to_le_bytes())?;w.write_all(&c.to_le_bytes())?}let mut t=BufReader::new(File::open(&tmp)?);io::copy(&mut t,&mut w)?;w.flush()?;let _=std::fs::remove_file(tmp);Ok(())}
 pub fn open(path:impl AsRef<Path>)->io::Result<Self>{let f=File::open(path)?;let map=unsafe{Mmap::map(&f)?};if map.len()<HEADER||&map[..8]!=MAGIC{return Err(io::Error::new(io::ErrorKind::InvalidData,"bad packed postings"))}let keys=u64::from_le_bytes(map[8..16].try_into().unwrap());let dir=u64::from_le_bytes(map[16..24].try_into().unwrap())as usize;let body=u64::from_le_bytes(map[24..32].try_into().unwrap())as usize;if dir!=HEADER||body!=HEADER+keys as usize*20||body>map.len(){return Err(io::Error::new(io::ErrorKind::InvalidData,"bad packed layout"))}Ok(Self{map,keys,dir,body})}
 fn entry(&self,i:usize)->(u64,u64,u32){let o=self.dir+i*20;(u64::from_le_bytes(self.map[o..o+8].try_into().unwrap()),u64::from_le_bytes(self.map[o+8..o+16].try_into().unwrap()),u32::from_le_bytes(self.map[o+16..o+20].try_into().unwrap()))}
 fn find(&self,key:u64)->Option<(u64,u32)>{let(mut l,mut r)=(0,self.keys as usize);while l<r{let m=(l+r)/2;if self.entry(m).0<key{l=m+1}else{r=m}}if l<self.keys as usize{let(k,o,c)=self.entry(l);if k==key{return Some((o,c))}}None}
 pub fn row_count(&self,key:u64)->usize{self.find(key).map(|x|x.1 as usize).unwrap_or(0)}
 pub fn rows(&self,key:u64)->Vec<u32>{let Some((off,count))=self.find(key)else{return vec![]};let mut p=self.body+off as usize;let mut out=Vec::with_capacity(count as usize);while out.len()<count as usize{if p+8>self.map.len(){return vec![]}let base=u32::from_le_bytes(self.map[p..p+4].try_into().unwrap());let n=u16::from_le_bytes(self.map[p+4..p+6].try_into().unwrap())as usize;let bw=self.map[p+6];p+=8;if n==0||n>BLOCK{return vec![]}out.push(base);let Some(gaps)=unpack(&self.map,&mut p,n-1,bw)else{return vec![]};let mut row=base;for gap in gaps{let Some(next)=row.checked_add(gap)else{return vec![]};row=next;out.push(row)}if out.len()>count as usize{return vec![]}}out}
 pub fn rows_from(&self,key:u64,first_row:u32,limit:usize)->Vec<u32>{
  if limit==0{return vec![]}let Some((off,count))=self.find(key)else{return vec![]};let mut p=self.body+off as usize;let mut seen=0usize;let mut out=Vec::with_capacity(limit.min(count as usize));
  while seen<count as usize&&out.len()<limit{
   if p+8>self.map.len(){return vec![]}let base=u32::from_le_bytes(self.map[p..p+4].try_into().unwrap());let n=u16::from_le_bytes(self.map[p+4..p+6].try_into().unwrap())as usize;let bw=self.map[p+6];p+=8;
   if n==0||n>BLOCK||seen+n>count as usize{return vec![]}let bytes=packed_bytes(n-1,bw);if p+bytes>self.map.len(){return vec![]}let next_p=p+bytes;
   if seen+n<count as usize{if next_p+8>self.map.len(){return vec![]}let next_base=u32::from_le_bytes(self.map[next_p..next_p+4].try_into().unwrap());if next_base<=first_row{p=next_p;seen+=n;continue}}
   let mut q=p;let mut row=base;if row>=first_row{out.push(row);if out.len()==limit{return out}}
   let Some(gaps)=unpack(&self.map,&mut q,n-1,bw)else{return vec![]};for gap in gaps{let Some(next)=row.checked_add(gap)else{return vec![]};row=next;if row>=first_row{out.push(row);if out.len()==limit{return out}}}
   p=next_p;seen+=n;
  }out
 }
 /// Intersect directly against compressed blocks. Blocks wholly before the current seed can be
 /// skipped from their headers alone; only overlapping blocks decode their packed gaps.
 pub fn intersect_rows(&self,key:u64,seed:&[u32])->Vec<u32>{
  if seed.is_empty(){return vec![]}let Some((off,count))=self.find(key)else{return vec![]};
  let mut p=self.body+off as usize;let mut seen=0usize;let mut si=0usize;let mut out=Vec::with_capacity(seed.len().min(count as usize));let seed_max=*seed.last().unwrap();
  while seen<count as usize&&si<seed.len(){
   if p+8>self.map.len(){return vec![]}let base=u32::from_le_bytes(self.map[p..p+4].try_into().unwrap());let n=u16::from_le_bytes(self.map[p+4..p+6].try_into().unwrap())as usize;let bw=self.map[p+6];p+=8;
   if n==0||n>BLOCK||seen+n>count as usize{return vec![]}let bytes=packed_bytes(n-1,bw);if p+bytes>self.map.len(){return vec![]}let next_p=p+bytes;
   if base>seed_max{break}while si<seed.len()&&seed[si]<base{si+=1}if si>=seed.len(){break}
   if seen+n<count as usize{if next_p+8>self.map.len(){return vec![]}let next_base=u32::from_le_bytes(self.map[next_p..next_p+4].try_into().unwrap());if seed[si]>=next_base{p=next_p;seen+=n;continue}}
   let mut q=p;let mut row=base;if seed[si]==row{out.push(row);si+=1}
   let Some(gaps)=unpack(&self.map,&mut q,n-1,bw)else{return vec![]};for gap in gaps{let Some(next)=row.checked_add(gap)else{return vec![]};row=next;while si<seed.len()&&seed[si]<row{si+=1}if si>=seed.len(){break}if seed[si]==row{out.push(row);si+=1}}
   p=next_p;seen+=n;
  }out
 }
}
#[cfg(test)]mod tests{use super::*;#[test]fn roundtrip_blocks(){let d=tempfile::tempdir().unwrap();let s=d.path().join("s");let o=d.path().join("o");let mut f=File::create(&s).unwrap();for i in 0..400u32{let k=if i<300{2u64}else{7};let row=if k==2{100+i*3}else{5000+(i-300)*1001};f.write_all(&k.to_le_bytes()).unwrap();f.write_all(&row.to_le_bytes()).unwrap()}drop(f);DeltaPostingHierarchy::build_from_sorted(&s,&o).unwrap();let x=DeltaPostingHierarchy::open(o).unwrap();assert_eq!(x.row_count(2),300);assert_eq!(x.rows(2)[299],997);assert_eq!(x.row_count(7),100);assert_eq!(x.rows(7)[99],104099);assert_eq!(x.rows_from(2, 700, 3), vec![700,703,706]);assert_eq!(x.intersect_rows(2,&[99,100,103,997,999]),vec![100,103,997]);assert_eq!(x.intersect_rows(7,&[1,5000,6001,104099,200000]),vec![5000,6001,104099]);}}
