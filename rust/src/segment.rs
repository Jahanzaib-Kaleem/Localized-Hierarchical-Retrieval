use memmap2::Mmap;
use std::{fs::File,io::{self,Write},path::Path};

const MAGIC:&[u8;8]=b"LHRSEG01";
const HEADER:usize=24;

pub struct Segment{map:Mmap,rows:usize,cols:usize,width:usize}
impl Segment{
 pub fn write(path:impl AsRef<Path>,rows:u64,cols:u32,width:u32,data:&[u8])->io::Result<()> {
  let expected=(rows as usize).checked_mul(cols as usize).and_then(|x|x.checked_mul(width as usize)).ok_or_else(||io::Error::new(io::ErrorKind::InvalidInput,"segment size overflow"))?;
  if data.len()!=expected{return Err(io::Error::new(io::ErrorKind::InvalidInput,"payload length mismatch"));}
  let mut f=File::create(path)?;f.write_all(MAGIC)?;f.write_all(&rows.to_le_bytes())?;f.write_all(&cols.to_le_bytes())?;f.write_all(&width.to_le_bytes())?;f.write_all(data)?;f.sync_all()
 }
 pub fn open(path:impl AsRef<Path>)->io::Result<Self>{
  let f=File::open(path)?;let map=unsafe{Mmap::map(&f)?};
  if map.len()<HEADER||&map[..8]!=MAGIC{return Err(io::Error::new(io::ErrorKind::InvalidData,"bad LHR segment"));}
  let rows=u64::from_le_bytes(map[8..16].try_into().unwrap()) as usize;let cols=u32::from_le_bytes(map[16..20].try_into().unwrap()) as usize;let width=u32::from_le_bytes(map[20..24].try_into().unwrap()) as usize;
  if !matches!(width,1|2|4|8){return Err(io::Error::new(io::ErrorKind::InvalidData,"unsupported token width"));}
  let expected=HEADER.checked_add(rows.checked_mul(cols).and_then(|x|x.checked_mul(width)).ok_or_else(||io::Error::new(io::ErrorKind::InvalidData,"segment overflow"))?).ok_or_else(||io::Error::new(io::ErrorKind::InvalidData,"segment overflow"))?;
  if map.len()!=expected{return Err(io::Error::new(io::ErrorKind::InvalidData,"segment length mismatch"));}
  Ok(Self{map,rows,cols,width})
 }
 pub fn rows(&self)->usize{self.rows}
 pub fn cols(&self)->usize{self.cols}
 pub fn value(&self,row:usize,col:usize)->Option<u64>{
  if row>=self.rows||col>=self.cols{return None} let o=HEADER+(row*self.cols+col)*self.width;let b=&self.map[o..o+self.width];Some(match self.width{1=>b[0] as u64,2=>u16::from_le_bytes(b.try_into().unwrap()) as u64,4=>u32::from_le_bytes(b.try_into().unwrap()) as u64,8=>u64::from_le_bytes(b.try_into().unwrap()),_=>unreachable!()})
 }
 pub fn count_page(&self,start:usize,end:usize,pred:&[(usize,u64)])->u64{
  let mut n=0;for r in start..end.min(self.rows){if pred.iter().all(|&(c,v)|self.value(r,c)==Some(v)){n+=1}}n
 }
}
#[cfg(test)]mod tests{use super::*;#[test]fn roundtrip(){let f=tempfile::NamedTempFile::new().unwrap();Segment::write(f.path(),3,2,1,&[1,2,3,4,1,4]).unwrap();let s=Segment::open(f.path()).unwrap();assert_eq!(s.value(1,1),Some(4));assert_eq!(s.count_page(0,3,&[(0,1)]),2);}}
